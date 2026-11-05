//! Shared, validated collection of streamed chat responses for probes and non-stream clients.

use super::{
    request,
    upstream::{business_error, StreamAccounting},
};
use crate::config::AppConfig;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

fn short(message: &str) -> String {
    message.chars().take(400).collect()
}

fn valid_finish(reason: &Value) -> bool {
    matches!(
        reason.as_str(),
        Some("stop" | "length" | "tool_calls" | "function_call" | "content_filter")
    )
}

fn has_output(message: &Value) -> bool {
    ["content", "refusal", "reasoning_content", "reasoning"]
        .iter()
        .any(|key| {
            message.get(*key).is_some_and(|value| match value {
                Value::String(text) => !text.trim().is_empty(),
                Value::Array(parts) => !parts.is_empty(),
                _ => false,
            })
        })
        || message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| {
                calls.iter().any(|call| {
                    call.pointer("/function/name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !name.trim().is_empty())
                })
            })
        || message
            .pointer("/function_call/name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.trim().is_empty())
}

fn validate_chat_json(value: Value) -> Result<Value, String> {
    if let Some(message) = business_error(&value) {
        // Keep structured fields intact for the gateway's error policy. Only
        // shorten at the final UI boundary; truncating here can corrupt the JSON.
        return Err(message);
    }
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| !choices.is_empty())
        .ok_or("上游响应缺少有效 choices")?;
    if choices.iter().any(|choice| {
        !choice.get("message").is_some_and(Value::is_object)
            || !has_output(&choice["message"])
            || !valid_finish(&choice["finish_reason"])
    }) {
        return Err("上游未返回有内容且正常结束的聊天结果".into());
    }
    Ok(value)
}

#[derive(Default)]
struct Choice {
    message: Map<String, Value>,
    tools: BTreeMap<i64, Value>,
    finish_reason: Option<Value>,
}

fn append_field(target: &mut Map<String, Value>, key: &str, value: &Value) {
    if let Some(fragment) = value.as_str() {
        let entry = target.entry(key.to_string()).or_insert_with(|| json!(""));
        if let Some(text) = entry.as_str() {
            *entry = json!(format!("{text}{fragment}"));
        }
    } else if let Some(parts) = value.as_array() {
        let entry = target.entry(key.to_string()).or_insert_with(|| json!([]));
        if let Some(array) = entry.as_array_mut() {
            array.extend(parts.iter().cloned());
        }
    }
}

fn merge_function(target: &mut Value, fragment: &Value) {
    if !target.is_object() {
        *target = json!({});
    }
    let object = target.as_object_mut().unwrap();
    for key in ["name", "arguments"] {
        if let Some(value) = fragment.get(key) {
            append_field(object, key, value);
        }
    }
}

#[derive(Default)]
struct ChatCollector {
    metadata: Map<String, Value>,
    choices: BTreeMap<i64, Choice>,
}

impl ChatCollector {
    fn observe(&mut self, chunk: Value) {
        for key in ["id", "created", "model", "system_fingerprint", "usage"] {
            if let Some(value) = chunk.get(key).filter(|value| !value.is_null()) {
                self.metadata.insert(key.into(), value.clone());
            }
        }
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return;
        };
        for (position, choice) in choices.iter().enumerate() {
            let index = choice
                .get("index")
                .and_then(Value::as_i64)
                .unwrap_or(position as i64);
            let collected = self.choices.entry(index).or_default();
            if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
                collected.finish_reason = Some(reason.clone());
            }
            let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
                continue;
            };
            for key in ["content", "refusal", "reasoning_content", "reasoning"] {
                if let Some(value) = delta.get(key) {
                    append_field(&mut collected.message, key, value);
                }
            }
            if let Some(function) = delta.get("function_call") {
                merge_function(
                    collected
                        .message
                        .entry("function_call".to_string())
                        .or_insert(Value::Null),
                    function,
                );
            }
            if let Some(tools) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, tool) in tools.iter().enumerate() {
                    let index = tool
                        .get("index")
                        .and_then(Value::as_i64)
                        .unwrap_or(position as i64);
                    let target = collected
                        .tools
                        .entry(index)
                        .or_insert_with(|| json!({"type":"function"}));
                    let target = target.as_object_mut().unwrap();
                    for key in ["id", "type"] {
                        if let Some(value) = tool.get(key).filter(|value| !value.is_null()) {
                            if key == "id" {
                                append_field(target, key, value);
                            } else {
                                target.insert(key.into(), value.clone());
                            }
                        }
                    }
                    if let Some(function) = tool.get("function") {
                        merge_function(
                            target.entry("function".to_string()).or_insert(Value::Null),
                            function,
                        );
                    }
                }
            }
        }
    }

    fn finish(mut self) -> Result<Value, String> {
        self.metadata
            .insert("object".into(), json!("chat.completion"));
        let choices: Vec<Value> = self
            .choices
            .into_iter()
            .map(|(index, mut choice)| {
                choice.message.insert("role".into(), json!("assistant"));
                choice
                    .message
                    .entry("content".to_string())
                    .or_insert(Value::Null);
                if !choice.tools.is_empty() {
                    choice.message.insert(
                        "tool_calls".into(),
                        Value::Array(choice.tools.into_values().collect()),
                    );
                }
                let reason = choice.finish_reason.unwrap_or_else(|| {
                    json!(if choice.message.contains_key("tool_calls") {
                        "tool_calls"
                    } else {
                        "stop"
                    })
                });
                json!({"index":index, "message":choice.message, "finish_reason":reason})
            })
            .collect();
        self.metadata
            .insert("choices".into(), Value::Array(choices));
        validate_chat_json(Value::Object(self.metadata))
    }
}

async fn limited_body(response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("读取上游响应失败: {error}"))?;
        if output.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("上游响应超出大小限制".into());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

pub(crate) async fn collect_chat_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let event_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    if !status.is_success() {
        let body = limited_body(response).await?;
        let detail = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|value| {
                business_error(&value)
                    .or_else(|| value.get("message").and_then(Value::as_str).map(short))
            })
            .map(|message| format!(": {}", short(&message)))
            .unwrap_or_default();
        return Err(format!("HTTP {status}{detail}"));
    }
    if !event_stream {
        let body = limited_body(response).await?;
        let value =
            serde_json::from_slice(&body).map_err(|_| "上游未返回有效 JSON 或 SSE".to_string())?;
        return validate_chat_json(value);
    }
    let mut stats = StreamAccounting::default();
    let mut collector = ChatCollector::default();
    let mut stream = response.bytes_stream();
    let mut received = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("读取上游响应失败: {error}"))?;
        received = received.saturating_add(chunk.len());
        if received > MAX_RESPONSE_BYTES {
            return Err("上游响应超出大小限制".into());
        }
        for chunk in stats.feed(&chunk) {
            collector.observe(chunk);
        }
        if stats.saw_error || stats.saw_done {
            break;
        }
    }
    for chunk in stats.finish() {
        collector.observe(chunk);
    }
    if !stats.completed() {
        return Err(stats
            .error_message
            .clone()
            .unwrap_or_else(|| "上游 SSE 为空、格式无效或在完成前提前结束".into()));
    }
    collector.finish()
}

pub(crate) async fn test_connection(
    cfg: &AppConfig,
) -> Result<crate::commands::TestResult, String> {
    let account = cfg
        .accounts
        .iter()
        .find(|account| account.enabled && !account.paused && !account.token.trim().is_empty())
        .ok_or("请先在账号池里添加一个启用、未暂停且带 Token 的账号")?;
    let model = cfg
        .models
        .first()
        .map(|model| model.workbuddy_id.as_str())
        .unwrap_or("fast-model");
    let body = json!({"model":model,"messages":[{"role":"user","content":"Reply briefly with pong."}],
        "max_tokens":64,"stream":true,"stream_options":{"include_usage":true}});
    let url = request::chat_url(&cfg.base_url, &account.realm)?;
    let client = super::upstream::new_client()?;
    let started = std::time::Instant::now();
    let response = request::chat_post(&client, &url, account, &body)
        .timeout(std::time::Duration::from_secs(45))
        .send()
        .await;
    let mut status = 0;
    let result = match response {
        Ok(response) => {
            status = response.status().as_u16();
            collect_chat_response(response).await
        }
        Err(error) => Err(format!("无法连接上游: {error}")),
    };
    Ok(match result {
        Ok(chat) => crate::commands::TestResult {
            ok: true,
            status,
            latency_ms: started.elapsed().as_millis(),
            content: chat
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .map(short)
                .unwrap_or_default(),
            error: None,
        },
        Err(error) => crate::commands::TestResult {
            ok: false,
            status,
            latency_ms: started.elapsed().as_millis(),
            content: String::new(),
            error: Some(short(&error)),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountConfig;
    use axum::{
        body::{to_bytes, Body},
        extract::Request,
        http::{header, HeaderMap, Method, StatusCode},
        response::Response,
        routing::any,
        Router,
    };
    use std::sync::{Arc, Mutex};

    type Requests = Arc<Mutex<Vec<(Method, String, HeaderMap, Value)>>>;

    async fn mock_upstream(
        status: StatusCode,
        content_type: &'static str,
        chunks: Vec<Vec<u8>>,
    ) -> (String, Requests, tokio::task::JoinHandle<()>) {
        let seen: Requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let app = Router::new().fallback(any(move |request: Request| {
            let recorded = recorded.clone();
            let chunks = chunks.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = to_bytes(body, 8192).await.unwrap();
                recorded.lock().unwrap().push((
                    parts.method,
                    parts.uri.path().to_string(),
                    parts.headers,
                    serde_json::from_slice(&body).unwrap_or(Value::Null),
                ));
                let stream =
                    futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
                Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen, server)
    }

    fn account() -> AccountConfig {
        serde_json::from_value(json!({"uid":"acct-local-test","workbuddy_uid":"official-test-uid","nickname":"Test",
            "token":"test-token","refresh_token":"must-not-be-sent","realm":"cn","enabled":true,"paused":false,
            "enterprise_id":"test-enterprise","domain":"test-domain"})).unwrap()
    }

    #[tokio::test]
    async fn connection_probe_uses_streaming_chat_protocol_and_skips_paused_accounts() {
        let wire = concat!(
            "data: {\"id\":\"test-completion\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"pong\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, recorded, server) = mock_upstream(
            StatusCode::OK,
            "text/event-stream",
            vec![wire.as_bytes().to_vec()],
        )
        .await;
        let mut cfg = AppConfig {
            base_url: base,
            accounts: vec![account()],
            ..Default::default()
        };
        cfg.models[0].workbuddy_id = "gpt-test".into();
        let mut paused = account();
        paused.paused = true;
        paused.token = "paused-must-not-use".into();
        cfg.accounts.insert(0, paused);
        let result = crate::commands::test_connection(cfg).await.unwrap();
        server.abort();
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(result.content, "pong");
        assert_eq!(result.status, 200);
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let (method, path, headers, body) = &requests[0];
        assert_eq!(*method, Method::POST);
        assert_eq!(path, "/v2/chat/completions");
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert_eq!(headers["x-user-id"], "official-test-uid");
        assert_eq!(headers["x-codebuddy-request"], "1");
        assert_eq!(headers["x-enterprise-id"], "test-enterprise");
        assert_eq!(headers["x-domain"], "test-domain");
        assert!(!headers.contains_key("x-refresh-token"));
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body["max_tokens"].as_i64().unwrap() >= 16);
        assert!(!body["messages"][0]["content"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn connection_probe_does_not_mistake_http_200_or_incomplete_sse_for_success() {
        for (content_type, body) in [
            ("application/json", ""),
            ("application/json", "{}"),
            (
                "application/json",
                r#"{"code":10001,"msg":"token expired"}"#,
            ),
            ("text/html", "<html>not a completion</html>"),
            (
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            ),
            (
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: [DONE]\n\n",
            ),
            (
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            ),
            (
                "text/event-stream",
                "data: {\"error\":{\"message\":\"token expired\"}}\n\ndata: [DONE]\n\n",
            ),
        ] {
            let (base, _, server) =
                mock_upstream(StatusCode::OK, content_type, vec![body.as_bytes().to_vec()]).await;
            let cfg = AppConfig {
                base_url: base,
                accounts: vec![account()],
                ..Default::default()
            };
            let result = crate::commands::test_connection(cfg).await.unwrap();
            server.abort();
            assert!(!result.ok, "accepted invalid reply: {body}");
            assert!(result.error.is_some());
        }
    }

    #[tokio::test]
    async fn connection_probe_preserves_structured_http_rejection_without_dumping_html() {
        for (body, expected) in [
            (
                r#"{"code":11151,"msg":"a message has empty content"}"#,
                "code=11151",
            ),
            ("<html>internal proxy diagnostic</html>", "HTTP 400"),
        ] {
            let (base, _, server) = mock_upstream(
                StatusCode::BAD_REQUEST,
                "application/json",
                vec![body.as_bytes().to_vec()],
            )
            .await;
            let cfg = AppConfig {
                base_url: base,
                accounts: vec![account()],
                ..Default::default()
            };
            let result = crate::commands::test_connection(cfg).await.unwrap();
            server.abort();
            assert!(!result.ok);
            assert_eq!(result.status, 400);
            let error = result.error.unwrap();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("<html>"));
        }
    }

    #[tokio::test]
    async fn streamed_chat_collection_preserves_utf8_tools_and_usage() {
        let wire = concat!(
            "data: {\"id\":\"chat-test\",\"created\":100,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你好\",\"reasoning_content\":\"think\",\"tool_calls\":[{\"index\":0,\"id\":\"call_test\",\"type\":\"function\",\"function\":{\"name\":\"look\",\"arguments\":\"{\\\"key\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"世界\",\"reasoning_content\":\"ing\",\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"up\",\"arguments\":\"1}\"}},{\"index\":1,\"id\":\"call_two\",\"function\":{\"name\":\"second\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n"
        );
        let chunks = wire
            .as_bytes()
            .chunks(3)
            .map(|chunk| chunk.to_vec())
            .collect();
        let (base, _, server) =
            mock_upstream(StatusCode::OK, "text/event-stream; charset=utf-8", chunks).await;
        let response = reqwest::Client::new().get(&base).send().await.unwrap();
        let chat = collect_chat_response(response).await.unwrap();
        server.abort();
        assert_eq!(chat["object"], "chat.completion");
        assert_eq!(chat["id"], "chat-test");
        assert_eq!(chat["choices"][0]["message"]["content"], "你好世界");
        assert_eq!(
            chat["choices"][0]["message"]["reasoning_content"],
            "thinking"
        );
        assert_eq!(
            chat["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        assert_eq!(
            chat["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"key\":1}"
        );
        assert_eq!(
            chat["choices"][0]["message"]["tool_calls"][1]["id"],
            "call_two"
        );
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chat["usage"]["total_tokens"], 15);
    }

    #[tokio::test]
    async fn compatible_json_completion_is_accepted_but_empty_or_unfinished_objects_fail() {
        for (body, ok) in [
            (
                r#"{"id":"test","choices":[{"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}]}"#,
                true,
            ),
            (
                r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"t1","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
                true,
            ),
            (
                r#"{"choices":[{"message":{"role":"assistant","content":""},"finish_reason":"stop"}]}"#,
                false,
            ),
            (
                r#"{"choices":[{"message":{"role":"assistant","content":"partial"},"finish_reason":null}]}"#,
                false,
            ),
            (
                r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{}]},"finish_reason":"tool_calls"}]}"#,
                false,
            ),
        ] {
            let (base, _, server) = mock_upstream(
                StatusCode::OK,
                "application/json",
                vec![body.as_bytes().to_vec()],
            )
            .await;
            let response = reqwest::Client::new().get(&base).send().await.unwrap();
            let result = collect_chat_response(response).await;
            server.abort();
            assert_eq!(result.is_ok(), ok, "{body}: {result:?}");
        }
    }
}
