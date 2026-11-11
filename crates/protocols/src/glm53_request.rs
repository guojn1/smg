//! GLM-5.3 wire compatibility verified against the official API (2026-11-11).
//! Model-scoped parameter/media compatibility; tool history and schemas stay intact.
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Chat,
    Messages,
    Responses,
}

fn error(api: Api, code: &str, _field: &str, message: &str) -> Response {
    let code = if api == Api::Responses {
        "invalid_request"
    } else {
        code
    };
    let body = match api {
        Api::Chat => json!({"error": {"code": code, "message": message}}),
        Api::Messages => json!({"type": "error", "error": {
            "type": "invalid_request_error", "code": code, "message": message}}),
        Api::Responses => json!({"error": {"code": code, "message": message,
            "type": "invalid_request", "param": null}}),
    };
    let mut response = (StatusCode::BAD_REQUEST, Json(body)).into_response();
    response
        .extensions_mut()
        .insert(crate::validated::RejectionCode(code.into()));
    response
}

fn numeric_string(fields: &mut Map<String, Value>, field: &str) {
    let Some(text) = fields.get(field).and_then(Value::as_str) else {
        return;
    };
    if let Ok(number) = text.parse::<f64>() {
        if number.is_finite() {
            fields.insert(field.into(), json!(number));
        }
    }
}

// Official Messages ignores these media blocks. Responses represents images
// with a text omission marker and ignores audio. Only the verified user
// text-plus-media shape is normalized; pure media and nested tool data stay intact.
fn normalize_media(fields: &mut Map<String, Value>, api: Api) -> bool {
    let (key, text_type, image_type, audio_type) = match api {
        Api::Messages => ("messages", "text", "image", "audio"),
        Api::Responses => ("input", "input_text", "input_image", "input_audio"),
        Api::Chat => return false,
    };
    let Some(messages) = fields.get_mut(key).and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if !parts.iter().any(|part| {
            part.get("type").and_then(Value::as_str) == Some(text_type)
                && part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
        }) {
            continue;
        }
        parts.retain_mut(|part| {
            let kind = part.get("type").and_then(Value::as_str);
            if kind == Some(image_type) && api == Api::Responses {
                *part = json!({"type": "input_text", "text": "image content omitted because you do not support image input"});
                changed = true;
                true
            } else if kind == Some(image_type) || kind == Some(audio_type) {
                changed = true;
                false
            } else { true }
        });
    }
    changed
}

pub fn prepare(body: &[u8], api: Api) -> Result<Option<Vec<u8>>, Response> {
    let Some(mut value) = crate::chat_compat::parse_compat_body(body) else {
        return Ok(None); // Preserve the ordinary malformed-body rejection.
    };
    let Some(model) = value.get("model").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !is_glm53(model) {
        return Ok(None);
    }
    // Snapshot only small parameters, never duplicate a potentially multi-MiB history.
    let keys = [
        "temperature",
        "presence_penalty",
        "top_p",
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
        "stream",
        "stream_options",
        "n",
        "top_logprobs",
        "logprobs",
        "response_format",
        "tool_choice",
    ];
    let original: Vec<_> = keys.iter().map(|key| value.get(*key).cloned()).collect();
    let Some(fields) = value.as_object_mut() else {
        return Ok(None);
    };
    // Acceptance-only compatibility: retain retrieval declarations as an
    // opaque extension. They are not callable functions or executed searches.
    let media_changed = normalize_media(fields, api);
    let mut retrieval_changed = false;
    if api == Api::Chat {
        if let Some(tools) = fields.get_mut("tools").and_then(Value::as_array_mut) {
            let is_retrieval = |tool: &Value| {
                tool.get("type").and_then(Value::as_str) == Some("retrieval")
                    && tool.get("retrieval").is_some_and(Value::is_object)
            };
            let retrieval: Vec<_> = tools
                .iter()
                .filter(|tool| is_retrieval(tool))
                .cloned()
                .collect();
            if !retrieval.is_empty() {
                tools.retain(|tool| !is_retrieval(tool));
                let saved = fields
                    .entry("glm53_retrieval_tools")
                    .or_insert_with(|| json!([]));
                if let Some(existing) = saved.as_array_mut() {
                    existing.extend(retrieval);
                } else {
                    // Preserve an opaque pre-existing extension rather than reject it.
                    let previous = saved.take();
                    *saved = Value::Array(std::iter::once(previous).chain(retrieval).collect());
                }
                retrieval_changed = true;
            }
        }
    }
    // Hosted tools need gateway orchestration. Keep their declarations as
    // opaque data, outside the function parser; never invent tool results.
    if api == Api::Chat {
        if let Some(tools) = fields.get_mut("tools").and_then(Value::as_array_mut) {
            let is_hosted = |tool: &Value| {
                matches!(
                    tool.get("type").and_then(Value::as_str),
                    Some("web_search" | "mcp")
                ) && tool
                    .get(tool["type"].as_str().unwrap())
                    .is_some_and(Value::is_object)
            };
            let hosted: Vec<_> = tools
                .iter()
                .filter(|tool| is_hosted(tool))
                .cloned()
                .collect();
            for tool in &hosted {
                if tool
                    .get("web_search")
                    .and_then(|v| v.get("enable"))
                    .and_then(Value::as_str)
                    .is_some_and(|v| {
                        !v.eq_ignore_ascii_case("true") && !v.eq_ignore_ascii_case("false")
                    })
                {
                    return Err(error(
                        api,
                        "400",
                        "tools",
                        "tools[].web_search.enable must be a boolean.",
                    ));
                }
            }
            if !hosted.is_empty() {
                tools.retain(|tool| !is_hosted(tool));
                let saved = fields
                    .entry("glm53_hosted_tools")
                    .or_insert_with(|| json!([]));
                if let Some(existing) = saved.as_array_mut() {
                    existing.extend(hosted);
                } else {
                    let previous = saved.take();
                    *saved = Value::Array(std::iter::once(previous).chain(hosted).collect());
                }
                retrieval_changed = true;
            }
        }
    }
    for name in ["temperature", "top_p"] {
        numeric_string(fields, name);
    }
    let limits: &[&str] = match api {
        Api::Chat => &["max_tokens", "max_completion_tokens"],
        Api::Messages => &["max_tokens"],
        Api::Responses => &["max_output_tokens"],
    };
    for &name in limits {
        if !fields.contains_key(name) {
            continue;
        }
        numeric_string(fields, name);
        if let Some(number) = fields.get(name).and_then(Value::as_f64) {
            // The official API truncates 1.5 to one generated token.
            if number >= 0.0 && number <= u32::MAX as f64 {
                fields.insert(name.into(), json!(number as u32));
            }
            if number.trunc() <= 0.0
                || (api == Api::Chat && name == "max_tokens" && number > 131072.0)
            {
                return Err(error(
                    api,
                    "1210",
                    name,
                    "max_tokens参数非法：限制数值范围[1,131072]",
                ));
            }
        }
    }
    if let Some(text) = fields.get("stream").and_then(Value::as_str) {
        if let Ok(stream) = text.parse::<bool>() {
            fields.insert("stream".into(), json!(stream));
        }
    }
    if let Some(value @ (0 | 1)) = fields.get("stream").and_then(Value::as_i64) {
        fields.insert("stream".into(), json!(value == 1));
    }
    if api == Api::Chat
        && fields
            .get("stream")
            .is_none_or(|v| v.is_null() || v == &json!(false))
    {
        // Official accepts this non-streaming request; usage options have no effect.
        fields.remove("stream_options");
    }
    // Official GLM-5.3 ignores n, including zero, negative and n=2.
    if api == Api::Chat && fields.get("n").is_some_and(Value::is_number) {
        fields.insert("n".into(), json!(1));
    }
    // Match official GLM-5.3 request acceptance while keeping the sampler's
    // probability threshold valid. This does not assert provider internals.
    if fields
        .get("top_p")
        .and_then(Value::as_f64)
        .is_some_and(|value| value.is_finite() && value > 1.0)
    {
        fields.insert("top_p".into(), json!(1.0));
    }
    if api == Api::Chat {
        // Official accepts these negative values. Defer to existing model
        // defaults instead of forwarding invalid sampler thresholds.
        for (key, lower) in [
            ("temperature", 0.0),
            ("top_p", 0.0),
            ("presence_penalty", -2.0),
        ] {
            if fields
                .get(key)
                .and_then(Value::as_f64)
                .is_some_and(|v| v.is_finite() && v < lower)
            {
                fields.remove(key);
            }
        }
        // Official probes: 4 and 99 both succeed without logprob data, including
        // streaming. Treat these unsupported public fields as ignored.
        fields.remove("logprobs");
        fields.remove("top_logprobs");
        if fields.get("stop").is_some_and(Value::is_string) {
            return Err(error(
                api,
                "400",
                "stop",
                "stop must be an array of strings.",
            ));
        }
        for (field, max) in [("user_id", 128), ("request_id", 64)] {
            if let Some(text) = fields.get(field).and_then(Value::as_str) {
                if !(6..=max).contains(&text.chars().count()) {
                    return Err(error(
                        api,
                        "1214",
                        field,
                        if field == "user_id" {
                            "user_id:The unique ID of the user must be between 6 and 128 characters in length."
                        } else {
                            "request_id:The request task ID length must be between 6 and 64 characters."
                        },
                    ));
                }
            }
        }
        if fields.get("response_format").is_some_and(|v| {
            v.as_object().is_some_and(|o| o.is_empty())
                || v.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| !["text", "json_object", "json_schema"].contains(&kind))
        }) {
            fields.remove("response_format");
        }
        if fields.get("enable_thinking") == Some(&json!(false))
            || fields
                .get("thinking")
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                == Some("disabled")
        {
            return Err(error(
                api,
                "1210",
                "thinking",
                "该模型始终思考，不支持关闭思考；请使用 low、high 或 max。",
            ));
        }
        if fields
            .get("thinking")
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| !["enabled", "disabled"].contains(&kind))
        {
            return Err(error(
                api,
                "1210",
                "thinking",
                "该模型始终思考，不支持关闭思考；请使用 low、high 或 max。",
            ));
        }
        if fields
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .is_some_and(|effort| !["low", "high", "max"].contains(&effort))
        {
            return Err(error(
                api,
                "1210",
                "reasoning_effort",
                "该模型始终思考，不支持关闭思考；请使用 low、high 或 max。",
            ));
        }
        check_chat_shape(fields)?;
        // Only the isolated empty user prompt was verified; do not reject
        // assistant null content, multimodal parts, or incomplete tool history.
        if let Some(messages) = fields.get("messages").and_then(Value::as_array) {
            if let [message] = messages.as_slice() {
                if message.get("role") == Some(&json!("user"))
                    && (message.get("content").is_none_or(Value::is_null)
                        || message.get("content") == Some(&json!([]))
                        || message.get("content") == Some(&json!("")))
                {
                    return Err(error(api, "1213", "messages", "未正常接收到prompt参数。"));
                }
            }
        }
    }
    let invalid_choice = match api {
        Api::Messages => fields
            .get("tool_choice")
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| !["auto", "any", "tool", "none"].contains(&kind)),
        Api::Responses => fields
            .get("tool_choice")
            .and_then(Value::as_str)
            .is_some_and(|kind| !["auto", "required", "none"].contains(&kind)),
        Api::Chat => false,
    };
    if invalid_choice {
        fields.remove("tool_choice");
    }
    if api == Api::Responses
        && fields.get("input") == Some(&json!(""))
        && fields
            .get("previous_response_id")
            .is_none_or(Value::is_null)
        && fields.get("conversation").is_none_or(Value::is_null)
    {
        return Err(error(api, "invalid_request", "input", "输入不能为空"));
    }
    if !media_changed
        && !retrieval_changed
        && keys
            .iter()
            .zip(&original)
            .all(|(key, old)| value.get(*key) == old.as_ref())
    {
        return Ok(None);
    }
    // Value only contains valid JSON; serialization cannot fail for these types.
    Ok(serde_json::to_vec(&value).ok())
}

pub fn is_glm53(model: &str) -> bool {
    model.to_ascii_lowercase().replace(['-', '.'], "") == "glm53"
}

fn check_chat_shape(fields: &Map<String, Value>) -> Result<(), Response> {
    let reject = |code, message: &str| error(Api::Chat, code, "messages", message);
    if let (Some(name), Some(tools)) = (
        fields
            .get("tool_choice")
            .and_then(|v| v.get("function"))
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str),
        fields.get("tools").and_then(Value::as_array),
    ) {
        if !tools.iter().any(|tool| {
            tool.get("function")
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                == Some(name)
        }) {
            return Err(reject("1210", "API 调用参数有误，请检查文档。"));
        }
    }
    let Some(messages) = fields.get("messages").and_then(Value::as_array) else {
        return Ok(());
    };
    if messages.len() == 1 && messages[0].get("role").and_then(Value::as_str) == Some("system") {
        return Err(reject("1214", "messages 参数非法。请检查文档。"));
    }
    // Do not inspect historical tool IDs, function names, arguments or pairing.
    for (i, message) in messages.iter().enumerate() {
        if !message.is_object() {
            continue;
        }
        let role = message.get("role");
        if role.is_none_or(Value::is_null)
            || role
                .and_then(Value::as_str)
                .is_some_and(|r| r.trim().is_empty())
        {
            return Err(reject("1214", "角色信息不能为空"));
        }
        if role.and_then(Value::as_str).is_some_and(|r| {
            ![
                "user",
                "assistant",
                "system",
                "developer",
                "tool",
                "function",
                "latest_reminder",
            ]
            .contains(&r.to_ascii_lowercase().as_str())
        }) {
            return Err(reject("1214", "角色信息不正确"));
        }
        if let Some(parts) = message.get("content").and_then(Value::as_array) {
            for (j, part) in parts.iter().enumerate() {
                if part.is_number() {
                    return Err(reject(
                        "1214",
                        &format!("messages[{i}].content[{j}].type:不能为空"),
                    ));
                }
                if part.get("type").and_then(Value::as_str) == Some("image_url") {
                    return Err(reject(
                        "1210",
                        "messages.content.type 参数非法，取值范围 ['text']",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Non-streaming Chat only; malformed/unrelated responses are passed through.
/// Counts and text come from the worker. No token counts are recomputed.
pub fn normalize_chat_response(body: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    if !value
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(is_glm53)
        || value.get("object").and_then(Value::as_str) != Some("chat.completion")
    {
        return None;
    }
    let fields = value.as_object_mut()?;
    fields.remove("metadata");
    if fields
        .get("request_id")
        .is_none_or(|v| v.is_null() || v == "")
    {
        if let Some(id) = fields.get("id").cloned() {
            fields.insert("request_id".into(), id);
        }
    }
    if let Some(choices) = fields.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices.iter_mut().filter_map(Value::as_object_mut) {
            choice.remove("logprobs");
            choice.remove("matched_stop");
        }
    }
    if let Some(usage) = fields.get_mut("usage").and_then(Value::as_object_mut) {
        if let Some(count) = usage.remove("reasoning_tokens").filter(|v| !v.is_null()) {
            let details = usage
                .entry("completion_tokens_details")
                .or_insert_with(|| json!({}));
            if !details.is_object() {
                *details = json!({});
            }
            if let Some(details) = details.as_object_mut() {
                details.entry("reasoning_tokens").or_insert(count);
            }
        }
        // Missing cache details are unknown; only the worker knows the count.
    }
    serde_json::to_vec(&value).ok()
}

#[cfg(test)]
mod xunmeng_tests {
    use super::*;
    #[tokio::test]
    async fn anyuxin_hosted_tools_and_negative_parameters() {
        let body = json!({"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],
            "temperature":-1,"top_p":-0.1,"presence_penalty":-5,
            "tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object"}}},
                {"type":"web_search","web_search":{"enable":true}},
                {"type":"mcp","mcp":{"server_label":"dummy"}}]});
        let fixed: Value = serde_json::from_slice(
            &prepare(&serde_json::to_vec(&body).unwrap(), Api::Chat)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(fixed["tools"], json!([body["tools"][0].clone()]));
        assert_eq!(
            fixed["glm53_hosted_tools"],
            json!([body["tools"][1].clone(), body["tools"][2].clone()])
        );
        assert_eq!(fixed["messages"], body["messages"]);
        for key in ["temperature", "top_p", "presence_penalty"] {
            assert!(fixed.get(key).is_none());
        }
        assert!(prepare(&serde_json::to_vec(&fixed).unwrap(), Api::Chat)
            .unwrap()
            .is_none());
        let mut other = body.clone();
        other["model"] = json!("qwen3");
        assert!(prepare(&serde_json::to_vec(&other).unwrap(), Api::Chat)
            .unwrap()
            .is_none());
        let mut bad = body;
        bad["tools"][1]["web_search"]["enable"] = json!("NOT_A_BOOLEAN");
        let response = prepare(&serde_json::to_vec(&bad).unwrap(), Api::Chat).unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["error"]["code"], "400");
    }

    #[tokio::test]
    async fn captured_official_error_cases() {
        let cases: Value =
            serde_json::from_str(include_str!("glm53_xunmeng_official_errors.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let request = serde_json::to_vec(&case["request"]).unwrap();
            let response = prepare(&request, Api::Chat).expect_err(case["id"].as_str().unwrap());
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", case["id"]);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"], case["error"], "{}", case["id"]);
        }
    }

    #[test]
    fn output_preserves_text_and_actual_counts_and_is_scoped() {
        let body = json!({"object":"chat.completion", "model":"glm-5.3", "id":"r1",
            "choices":[{"message":{"content":"hi","reasoning_content":"think"},"matched_stop":1,"logprobs":null}],
            "metadata":{"weight_version":"v1"},
            "usage":{"completion_tokens":150,"reasoning_tokens":110,"prompt_tokens_details":null}});
        let fixed: Value = serde_json::from_slice(
            &normalize_chat_response(&serde_json::to_vec(&body).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            fixed["choices"][0]["message"],
            body["choices"][0]["message"]
        );
        assert!(fixed.get("metadata").is_none());
        assert!(fixed["choices"][0].get("matched_stop").is_none());
        assert_eq!(fixed["usage"]["completion_tokens"], 150);
        assert_eq!(
            fixed["usage"]["completion_tokens_details"]["reasoning_tokens"],
            110
        );
        assert!(fixed["usage"]["prompt_tokens_details"].is_null());
        let mut cached = fixed.clone();
        cached["usage"]["prompt_tokens_details"] = json!({"cached_tokens": 80});
        let preserved: Value = serde_json::from_slice(
            &normalize_chat_response(&serde_json::to_vec(&cached).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            preserved["usage"]["prompt_tokens_details"]["cached_tokens"],
            80
        );
        assert_eq!(
            serde_json::from_slice::<Value>(
                &normalize_chat_response(&serde_json::to_vec(&fixed).unwrap()).unwrap()
            )
            .unwrap(),
            fixed
        );
        for (model, object) in [
            ("qwen3", "chat.completion"),
            ("glm-5.3", "response"),
            ("glm-5.3", "chat.completion.chunk"),
        ] {
            let mut other = body.clone();
            other["model"] = json!(model);
            other["object"] = json!(object);
            assert!(normalize_chat_response(&serde_json::to_vec(&other).unwrap()).is_none());
        }
    }
}
