//! Opt-in wire errors for a dedicated Zhipu-compatible GLM-5.3 endpoint.
//! Missing model and failed authentication cannot be scoped by request.model.
//! Enable only on a dedicated listener with SMG_GLM53_OFFICIAL_ERROR_COMPAT=1.

use axum::{
    body::{Body, Bytes},
    extract::{FromRequest, Request},
    http::{header, Method},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, value::RawValue};

pub const ENV: &str = "SMG_GLM53_OFFICIAL_ERROR_COMPAT";

pub fn enabled() -> bool {
    std::env::var(ENV).is_ok_and(|value| value == "1")
}

/// Called only after the existing serving-key check has rejected a request.
/// Never installs authentication, validates a provider key, or permits a caller.
pub fn auth_rejection(request: &Request, enabled: bool) -> Option<Response> {
    if !enabled || request.method() != Method::POST || request.uri().path() != "/v1/responses" {
        return None;
    }
    let error = if request.headers().contains_key(header::AUTHORIZATION) {
        json!({"code": 401, "msg": "令牌已过期或验证不正确", "success": false})
    } else {
        json!({"code": 1001, "msg": "Header中未收到Authorization参数，无法进行身份验证。", "success": false})
    };
    Some(Json(error).into_response())
}

#[derive(Deserialize)]
struct ModelOnly<'a> {
    // Borrow only the model; histories are skipped, never cloned or rewritten.
    #[serde(borrow)]
    model: Option<&'a RawValue>,
}

fn missing_model(bytes: &[u8]) -> bool {
    // A struct can also deserialize from a JSON sequence: explicitly require
    // an object so [] / [null] retain their existing rejection behavior.
    bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{')
        && serde_json::from_slice::<ModelOnly<'_>>(bytes).is_ok_and(|body| body.model.is_none())
}

/// Runs after authentication/admission and before raw HTTP forwarding. The
/// caller wraps it in the existing body watchdog; Bytes uses the ORIGINAL
/// request extensions, including the configured payload cap (not 2 MiB).
pub async fn chat_missing_model(request: Request, next: Next) -> Response {
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| {
            let value = value.trim().to_ascii_lowercase();
            value == "application/json"
                || (value.starts_with("application/") && value.ends_with("+json"))
        });
    if request.method() != Method::POST
        || request.uri().path() != "/v1/chat/completions"
        || !is_json
    {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let bytes = match Bytes::from_request(Request::from_parts(parts.clone(), body), &()).await {
        Ok(bytes) => bytes,
        Err(error) => return error.into_response(),
    };
    if missing_model(&bytes) {
        return Json(json!({"error": {"code": "500"}})).into_response();
    }
    // Preserve every byte, including duplicate keys and historical arguments.
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// Install only on the explicitly configured dedicated GLM-5.3 listener.
/// The default router keeps its original routes and method handling.
pub fn compact_routes<S: Clone + Send + Sync + 'static>(enabled: bool) -> axum::Router<S> {
    let routes = axum::Router::new();
    if enabled {
        routes.route(
            "/v1/responses/compact",
            axum::routing::post(compact_unsupported),
        )
    } else {
        routes
    }
}

/// SMG does not implement Responses compaction. No inference is attempted.
pub async fn compact_unsupported() -> Response {
    (
        axum::http::StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": {
            "code": "not_implemented", "message": "Compact is not implemented yet",
            "param": null, "type": "not_implemented"
        }})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::{
        body::to_bytes, extract::DefaultBodyLimit, http::StatusCode, middleware::from_fn,
        routing::post, Router,
    };
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn compact_returns_official_unsupported_response() {
        let response = compact_unsupported().await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(
            value,
            json!({"error": {"code": "not_implemented", "message": "Compact is not implemented yet", "param": null, "type": "not_implemented"}})
        );
    }

    #[tokio::test]
    async fn compact_route_is_only_installed_on_dedicated_listener() {
        for (enabled, status) in [
            (false, StatusCode::METHOD_NOT_ALLOWED),
            (true, StatusCode::NOT_IMPLEMENTED),
        ] {
            let app = Router::new()
                .route(
                    "/v1/responses/{response_id}",
                    axum::routing::get(|| async { "existing retrieval" }),
                )
                .merge(compact_routes(enabled));
            let response = app
                .oneshot(request("/v1/responses/compact", "{}"))
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
    }

    fn app(limit: usize) -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: Bytes| async move { body }),
            )
            .route("/v1/responses", post(|body: Bytes| async move { body }))
            .layer(from_fn(chat_missing_model))
            .layer(DefaultBodyLimit::max(limit))
    }

    fn request(path: &str, body: &str) -> Request {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    #[tokio::test]
    async fn three_missing_model_cases_stop_before_inference() {
        for body in [
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            r#"{"model":null,"messages":[{"role":"user","content":"hi"}]}"#,
            "{}",
        ] {
            let response = app(1024)
                .oneshot(request("/v1/chat/completions", body))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(&bytes[..], br#"{"error":{"code":"500"}}"#);
        }
    }

    #[tokio::test]
    async fn histories_and_unrelated_inputs_are_byte_preserved() {
        for body in [
            r#"{"model":"glm-5.3","messages":[{"role":"assistant","tool_calls":[{"id":null,"function":{"name":"","arguments":"{}"}},{"id":"c1","function":{"name":"bash","arguments":"\"x\":1}"}}]}],"reasoning_content":"a","reasoning_content":"b"}"#,
            r#"{"model":"qwen3","messages":[]}"#,
            r#"{"model":"","messages":[]}"#,
            r#"{"model":123}"#,
            r#"{"model":null,"model":"glm-5.3"}"#,
            "{",
            "null",
            "[]",
            "[null]",
        ] {
            let response = app(4096)
                .oneshot(request("/v1/chat/completions", body))
                .await
                .unwrap();
            assert_eq!(
                to_bytes(response.into_body(), 4096).await.unwrap().as_ref(),
                body.as_bytes()
            );
        }
        let response = app(1024)
            .oneshot(request("/v1/responses", "{}"))
            .await
            .unwrap();
        assert_eq!(
            &to_bytes(response.into_body(), 1024).await.unwrap()[..],
            b"{}"
        );
    }

    #[tokio::test]
    async fn original_payload_limit_survives_body_reconstruction() {
        let body = format!(
            r#"{{"model":"glm-5.3","padding":"{}"}}"#,
            "x".repeat(3 * 1024 * 1024)
        );
        let response = app(4 * 1024 * 1024)
            .oneshot(request("/v1/chat/completions", &body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap()
                .len(),
            body.len()
        );
        let response = app(1024)
            .oneshot(request("/v1/chat/completions", &body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn auth_format_is_scoped_to_opted_in_responses_post() {
        let mut req = request("/v1/responses", "{}");
        assert!(auth_rejection(&req, false).is_none());
        for (header, expected) in [(None, 1001), (Some("Bearer invalid"), 401)] {
            if let Some(value) = header {
                req.headers_mut()
                    .insert(header::AUTHORIZATION, value.parse().unwrap());
            }
            let response = auth_rejection(&req, true).unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let value: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap())
                    .unwrap();
            assert_eq!(value["code"], expected);
            assert_eq!(value["success"], false);
            assert!(value.get("error").is_none());
        }
        *req.method_mut() = Method::GET;
        assert!(auth_rejection(&req, true).is_none());
        assert!(auth_rejection(&request("/v1/chat/completions", "{}"), true).is_none());
    }
}
