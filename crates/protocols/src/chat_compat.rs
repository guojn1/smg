//! Narrow compatibility repairs for malformed chat history emitted by clients.
//!
//! This is only used after the ordinary chat request extractor rejects a body.
//! Other duplicate JSON keys stay invalid, as they are in the ordinary path.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::fmt;

#[derive(Clone, Copy)]
enum Position {
    Root,
    Messages,
    Message,
    Other,
}

#[derive(Default)]
struct RepairState {
    changed: bool,
}

struct TreeSeed<'a> {
    state: &'a mut RepairState,
    position: Position,
}

impl<'de> DeserializeSeed<'de> for TreeSeed<'_> {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(TreeVisitor {
            state: self.state,
            position: self.position,
        })
    }
}

struct TreeVisitor<'a> {
    state: &'a mut RepairState,
    position: Position,
}

impl<'de> Visitor<'de> for TreeVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let element_position = if matches!(self.position, Position::Messages) {
            Position::Message
        } else {
            Position::Other
        };
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(TreeSeed {
            state: &mut *self.state,
            position: element_position,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        let mut duplicate_reasoning = false;
        while let Some(key) = map.next_key::<String>()? {
            let child_position = if matches!(self.position, Position::Root) && key == "messages" {
                Position::Messages
            } else {
                Position::Other
            };
            let value = map.next_value_seed(TreeSeed {
                state: &mut *self.state,
                position: child_position,
            })?;
            if values.contains_key(&key) {
                if matches!(self.position, Position::Message)
                    && matches!(key.as_str(), "reasoning_content" | "reasoning_context")
                {
                    duplicate_reasoning = true;
                } else {
                    return Err(de::Error::custom(format!("duplicate field `{key}`")));
                }
            }
            values.insert(key, value);
        }
        if duplicate_reasoning {
            if values.get("role").and_then(Value::as_str) != Some("assistant") {
                return Err(de::Error::custom(
                    "duplicate reasoning field outside an assistant message",
                ));
            }
            self.state.changed = true;
        }
        Ok(Value::Object(values))
    }
}

fn empty_tool_call(call: &Value) -> bool {
    let Some(call) = call.as_object() else {
        return false;
    };
    let Some(function) = call.get("function").and_then(Value::as_object) else {
        return false;
    };
    let empty_name = match function.get("name") {
        None => function.len() == 1,
        Some(Value::String(name)) if name.is_empty() => function.len() == 2,
        _ => false,
    };
    call.len() == 3
        && call.get("id") == Some(&Value::Null)
        && call.get("type").and_then(Value::as_str) == Some("function")
        && empty_name
        && function.get("arguments").and_then(Value::as_str) == Some("{}")
}

// Request-only adaptation: do not weaken shared generated ToolCall types.
// Null/absence use the existing empty-ID sentinel; scalars keep their JSON text.
fn normalize_history_id(fields: &mut Map<String, Value>, key: &str, state: &mut RepairState) {
    let normalized = match fields.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(value @ (Value::Bool(_) | Value::Number(_))) => value.to_string(),
        _ => return, // Keep strings verbatim and let objects/arrays fail normally.
    };
    fields.insert(key.into(), Value::String(normalized));
    state.changed = true;
}

/// Parse without silently collapsing duplicate fields. Parameter compatibility
/// uses the same duplicate-key policy as the existing history repair.
#[cfg(feature = "axum")]
pub(crate) fn parse_compat_body(body: &[u8]) -> Option<Value> {
    let mut state = RepairState::default();
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let value = TreeSeed {
        state: &mut state,
        position: Position::Root,
    }
    .deserialize(&mut deserializer)
    .ok()?;
    deserializer.end().ok()?;
    Some(value)
}

/// Return a repaired body only when a known compatibility defect was found.
/// Unknown duplicate fields and malformed JSON are left to the original error.
pub(crate) fn repair_chat_body(body: &[u8]) -> Option<Vec<u8>> {
    let mut state = RepairState::default();
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let mut value = TreeSeed {
        state: &mut state,
        position: Position::Root,
    }
    .deserialize(&mut deserializer)
    .ok()?;
    deserializer.end().ok()?;

    if let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
                continue;
            };
            // Do not silently erase an assistant turn consisting solely of a
            // placeholder. That history needs an explicit client decision.
            if calls.iter().any(empty_tool_call) && calls.iter().any(|call| !empty_tool_call(call))
            {
                calls.retain(|call| !empty_tool_call(call));
                state.changed = true;
            }
        }
    }

    // Normalize only GLM5.3 historical IDs accepted by the official API.
    // Do not weaken shared ToolCall response types or other model contracts.
    let allow_compatible_ids = value
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| {
            model.to_ascii_lowercase().replace('-', "").replace('.', "") == "glm53"
        });
    if allow_compatible_ids {
        if let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                match message.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        if let Some(calls) =
                            message.get_mut("tool_calls").and_then(Value::as_array_mut)
                        {
                            for call in calls {
                                // Preserve the existing single-stub policy.
                                if empty_tool_call(call) {
                                    continue;
                                }
                                if let Some(call) = call.as_object_mut() {
                                    normalize_history_id(call, "id", &mut state);
                                }
                            }
                        }
                    }
                    Some("tool") => {
                        if let Some(message) = message.as_object_mut() {
                            normalize_history_id(message, "tool_call_id", &mut state);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    state
        .changed
        .then(|| serde_json::to_vec(&value).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::repair_chat_body;
    use crate::chat::ChatCompletionRequest;

    #[test]
    fn repairs_duplicate_reasoning_and_empty_tool_stub_together() {
        let body = br#"{"model":"glm-5.3","messages":[{"role":"assistant","content":null,"reasoning_content":"old","reasoning_content":"new","reasoning_context":"first","reasoning_context":"second","tool_calls":[{"function":{"arguments":"{}","name":""},"id":null,"type":"function"},{"function":{"arguments":"\"command\":\"echo hi\"}","name":"bash"},"id":"call_1","type":"function"}]},{"role":"user","content":"continue"}]}"#;
        let repaired = repair_chat_body(body).expect("known defects should be repaired");
        let value: serde_json::Value = serde_json::from_slice(&repaired).unwrap();
        assert_eq!(value["messages"][0]["reasoning_content"], "new");
        assert_eq!(value["messages"][0]["reasoning_context"], "second");
        assert_eq!(
            value["messages"][0]["tool_calls"].as_array().unwrap().len(),
            1
        );
        serde_json::from_slice::<ChatCompletionRequest>(&repaired).unwrap();
    }

    #[test]
    fn unrelated_duplicates_and_single_stubs_are_not_repaired() {
        let duplicate_model = br#"{"model":"a","model":"b","messages":[]}"#;
        assert!(repair_chat_body(duplicate_model).is_none());

        let duplicate_user_reasoning = br#"{"model":"a","messages":[{"role":"user","content":"hi","reasoning_content":"a","reasoning_content":"b"}]}"#;
        assert!(repair_chat_body(duplicate_user_reasoning).is_none());

        let single_stub = br#"{"model":"a","messages":[{"role":"assistant","content":null,"tool_calls":[{"function":{"arguments":"{}","name":""},"id":null,"type":"function"}]}]}"#;
        assert!(repair_chat_body(single_stub).is_none());

        let two_stubs = br#"{"model":"a","messages":[{"role":"assistant","content":null,"tool_calls":[{"function":{"arguments":"{}","name":""},"id":null,"type":"function"},{"function":{"arguments":"{}","name":""},"id":null,"type":"function"}]}]}"#;
        assert!(repair_chat_body(two_stubs).is_none());
    }

    #[test]
    fn removes_a_placeholder_with_missing_name_when_a_real_call_follows() {
        let body = br#"{"model":"m","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":null,"type":"function","function":{"arguments":"{}"}},{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{}"}}]}]}"#;
        let repaired = repair_chat_body(body).unwrap();
        let request: ChatCompletionRequest = serde_json::from_slice(&repaired).unwrap();
        assert_eq!(request.history_tool_calls_count(), 1);
    }
}
