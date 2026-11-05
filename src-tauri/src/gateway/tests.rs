use super::{build_router, upstream::GatewayState, Ctx};
use crate::config::{AccountConfig, AppConfig};
use crate::pool::Pool;
use crate::reqlog::{ArchiveConfig, Recorder};
use crate::usage::UsageRecorder;
use axum::body::{to_bytes, Body, Bytes};
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

type Recorder2 = Arc<Mutex<Vec<Value>>>;

fn temp_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("wb-gw-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 起一个假的 WorkBuddy 上游：记录收到的请求，并按 stream 返回 SSE / JSON。
async fn spawn_mock_upstream() -> (String, Recorder2) {
    let rec: Recorder2 = Arc::new(Mutex::new(Vec::new()));
    let state = rec.clone();

    let app = Router::new()
        .route(
            "/v2/chat/completions",
            post(
                |State(rec): State<Recorder2>, body: Bytes| async move {
                    let v: Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
                    rec.lock().unwrap().push(v.clone());
                    // Emulate WorkBuddy's validation rather than accepting broken history.
                    let has_empty_message = v["messages"].as_array().map_or(true, |messages| messages.is_empty() || messages.iter().any(|message| {
                        if message["role"] == "assistant" && message["tool_calls"].as_array().is_some_and(|calls| !calls.is_empty()) {
                            return false;
                        }
                        match &message["content"] {
                            Value::String(text) => text.trim().is_empty(),
                            Value::Array(parts) => parts.is_empty(),
                            _ => true,
                        }
                    }));
                    if has_empty_message {
                        return Response::builder().status(StatusCode::BAD_REQUEST)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(r#"{"code":11151,"msg":"a message has empty content, please check the conversation history and retry"}"#)).unwrap();
                    }
                    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
                    if stream {
                        let sse = concat!(
                            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"},\"finish_reason\":null}]}\n\n",
                            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":80}}}\n\n",
                            "data: [DONE]\n\n"
                        );
                        Response::builder()
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            .body(Body::from(sse))
                            .unwrap()
                    } else {
                        Response::builder()
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                r#"{"id":"c1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
                            ))
                            .unwrap()
                    }
                },
            ),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), rec)
}

fn test_ctx(base_url: String) -> Ctx {
    let dir = temp_dir();
    let cfg = AppConfig {
        base_url,
        accounts: vec![AccountConfig {
            uid: "test-uid".into(),
            workbuddy_uid: None,
            nickname: "测试号".into(),
            token: "test-token".into(),
            refresh_token: String::new(),
            email: String::new(),
            login_ip: None,
            import_ip: None,
            enterprise_id: String::new(),
            enterprise_name: String::new(),
            domain: String::new(),
            expires_at: None,
            realm: "cn".into(),
            enabled: true,
            paused: false,
        }],
        ..Default::default()
    };
    let shared: super::SharedConfig = Arc::new(tokio::sync::RwLock::new(cfg));
    let keys = Arc::new(crate::apikey::Store::new_or_empty(
        dir.join("apikeys.json").display().to_string(),
    ));
    let pool = Pool::new(shared.clone());
    let reqlog = Recorder::new(ArchiveConfig {
        enabled: false,
        dir: dir.join("request-logs"),
        retention_days: 7,
        max_bytes: 0,
    });
    let usage = Arc::new(UsageRecorder::new(
        dir.join("usage.json").display().to_string(),
    ));
    Arc::new(GatewayState {
        cfg: shared,
        pool,
        keys,
        reqlog,
        usage,
        client: super::upstream::new_client().unwrap(),
    })
}

#[tokio::test]
async fn healthz_and_models_work() {
    let (base_url, _rec) = spawn_mock_upstream().await;
    let app = build_router(test_ctx(base_url));

    let resp = app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("\"ok\":true"));

    let resp = app
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("workbuddy-fast"));
    assert!(text.contains("deepseek-v4-pro"));
}

#[tokio::test]
async fn responses_stream_is_translated() {
    let (base_url, rec) = spawn_mock_upstream().await;
    let ctx = test_ctx(base_url);
    let app = build_router(ctx.clone());

    let req = Request::post("/v1/responses")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"model":"workbuddy-fast","input":"你好","stream":true}"#,
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);

    assert!(
        text.contains("event: response.created"),
        "缺 response.created"
    );
    assert!(
        text.contains("event: response.output_text.delta"),
        "缺 output_text.delta"
    );
    assert!(text.contains("\"delta\":\"你好\""), "增量内容不对: {text}");
    assert!(text.contains("event: response.completed"), "缺 completed");

    let recorded = rec.lock().unwrap();
    assert_eq!(
        recorded[0]["model"].as_str(),
        Some("fast-model"),
        "模型未重写: {}",
        recorded[0]
    );
    assert_eq!(recorded[0]["stream"].as_bool(), Some(true));
    let snapshot = ctx.usage.snapshot();
    assert_eq!(snapshot.total_requests, 1);
    assert_eq!(snapshot.prompt_tokens, 100);
    assert_eq!(snapshot.completion_tokens, 20);
    assert_eq!(
        snapshot.total_tokens, 120,
        "stream usage falls back to prompt + completion"
    );
    assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
}

#[tokio::test]
async fn chat_stream_records_usage_and_preserves_wire_bytes() {
    let (base_url, rec) = spawn_mock_upstream().await;
    let ctx = test_ctx(base_url);
    let response = build_router(ctx.clone()).oneshot(Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"model":"workbuddy-fast","messages":[{"role":"user","content":"ping"}],"stream":true}"#)).unwrap()).await.unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("你好"));
    let dashboard = ctx.usage.dashboard(&crate::usage::UsageQuery::default());
    assert_eq!(dashboard.snapshot.total_requests, 1);
    assert_eq!(dashboard.snapshot.total_tokens, 120);
    assert_eq!(dashboard.cache_hit_rate, Some(80.0));
    assert_eq!(dashboard.live_rpm, 1);
    assert_eq!(dashboard.live_tpm, 120);
    assert_eq!(
        rec.lock().unwrap()[0]["stream_options"]["include_usage"],
        true
    );
    assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
    let logs = ctx.reqlog.read_archive(
        10,
        crate::reqlog::Filter {
            account_uid: "test-uid".into(),
            ..Default::default()
        },
    );
    assert_eq!(
        logs.len(),
        1,
        "live logs are available when archives are disabled"
    );
    assert_eq!(logs[0].total_tokens, 120);
}

#[tokio::test]
async fn upstream_failures_without_tokens_are_counted() {
    let ctx = test_ctx("http://127.0.0.1:1".into());
    let response = build_router(ctx.clone())
        .oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"model":"workbuddy-fast","messages":[{"role":"user","content":"ping"}],"stream":false}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_server_error());
    let snapshot = ctx.usage.snapshot();
    assert!(snapshot.total_requests > 0);
    assert_eq!(snapshot.total_requests, snapshot.total_errors);
    assert_eq!(snapshot.total_tokens, 0);
    assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
}

#[tokio::test]
async fn client_disconnect_releases_the_account_and_completes_accounting() {
    let (base_url, _) = spawn_mock_upstream().await;
    let ctx = test_ctx(base_url);
    let response = build_router(ctx.clone())
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"model":"workbuddy-fast","input":"ping","stream":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if ctx.reqlog.snapshot().completed == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("disconnect must finish accounting");
    assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
    let config = ctx.cfg.read().await;
    let statuses = ctx.pool.statuses(&config.accounts);
    assert_eq!(statuses[0].1.in_flight, 0);
    assert_eq!(ctx.usage.snapshot().total_requests, 1);
    assert_eq!(
        ctx.reqlog.snapshot().recent[0].outcome,
        crate::reqlog::OUTCOME_INTERRUPTED
    );
}

#[tokio::test]
async fn chat_completions_passthrough_works() {
    let (base_url, rec) = spawn_mock_upstream().await;
    let app = build_router(test_ctx(base_url));

    let req = Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"model":"workbuddy-balanced","messages":[{"role":"user","content":"ping"}],"stream":false}"#,
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let chat: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(chat["choices"][0]["message"]["content"], "你好");
    assert_eq!(chat["usage"]["prompt_tokens"], 100);
    assert_eq!(chat["usage"]["completion_tokens"], 20);

    let recorded = rec.lock().unwrap();
    assert_eq!(recorded[0]["model"].as_str(), Some("balanced-model"));
    assert_eq!(
        recorded[0]["stream"], true,
        "upstream still needs streaming when the client requests JSON"
    );
}

#[tokio::test]
async fn api_key_auth_required_when_key_exists() {
    let (base_url, _rec) = spawn_mock_upstream().await;
    let ctx = test_ctx(base_url);
    // 先创建一个 key：此时无主密钥、有托管 key → 请求必须带正确 key
    ctx.keys
        .create(crate::apikey::CreateRequest {
            name: "test".into(),
            custom_key: "sk-test-key-12345678".into(),
            ..Default::default()
        })
        .unwrap();
    let app = build_router(ctx);

    let resp = app
        .clone()
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .oneshot(
            Request::get("/v1/models")
                .header(header::AUTHORIZATION, "Bearer sk-test-key-12345678")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn responses_easy_messages_and_empty_history_reach_upstream_with_real_content() {
    for stream in [false, true] {
        let (base, recorded) = spawn_mock_upstream().await;
        let response = build_router(test_ctx(base)).oneshot(Request::post("/v1/responses")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({
                "model":"workbuddy-fast", "stream":stream,
                "input":[
                    {"type":"message","role":"system","content":""},
                    {"role":"assistant","content":null},
                    {"role":"assistant","content":[{"type":"reasoning_text","text":"private reasoning"}]},
                    {"role":"user","content":[{"type":"input_text","text":"hello from easy input"}]}
                ]
            }).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let wire = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&wire).contains("你好"));
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]["messages"],
            serde_json::json!([
                {"role":"user","content":"hello from easy input"}
            ])
        );
        assert_eq!(requests[0]["stream"], true);
    }
}

#[tokio::test]
async fn chat_and_responses_keep_tool_rounds_and_normalize_empty_tool_results() {
    let call = serde_json::json!({"id":"call-test","type":"function","function":{"name":"lookup","arguments":"{}"}});
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for stream in [false, true] {
            let (base, recorded) = spawn_mock_upstream().await;
            let payload = if path.ends_with("responses") {
                serde_json::json!({"model":"workbuddy-fast", "stream":stream,
                "tools":[{"type":"function","name":"lookup","description":"Find a result","parameters":{"type":"object"}}],
                "input":[
                    {"role":"user","content":"look up a result"},
                    {"type":"function_call","call_id":"call-test","name":"lookup","arguments":"{}"},
                    {"type":"function_call_output","call_id":"call-test","output":[]},
                    {"role":"user","content":"continue"}
                ]})
            } else {
                serde_json::json!({"model":"workbuddy-fast", "stream":stream,
                "messages":[
                    {"role":"developer","content":" "},
                    {"role":"user","content":"look up a result"},
                    {"role":"assistant","content":[],"tool_calls":[call]},
                    {"role":"tool","tool_call_id":"call-test","content":null},
                    {"role":"assistant","content":" "},
                    {"role":"user","content":"continue"}
                ]})
            };
            let response = build_router(test_ctx(base))
                .oneshot(
                    Request::post(path)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let messages = requests[0]["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[1]["role"], "assistant");
            assert!(messages[1]["content"].is_null());
            assert_eq!(messages[1]["tool_calls"], serde_json::json!([call]));
            assert_eq!(
                messages[2],
                serde_json::json!({
                    "role":"tool", "tool_call_id":"call-test", "content":"[Tool returned no output.]"
                })
            );
            if path.ends_with("responses") {
                assert_eq!(requests[0]["tools"][0]["function"]["name"], "lookup");
                assert_eq!(
                    requests[0]["tools"][0]["function"]["parameters"],
                    serde_json::json!({"type":"object"})
                );
            }
        }
    }
}

#[tokio::test]
async fn image_only_messages_and_tool_output_parts_preserve_images_and_order() {
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let (base, recorded) = spawn_mock_upstream().await;
        let message = if path.ends_with("responses") {
            serde_json::json!({"role":"user","content":[
                {"type":"input_text","text":" "},
                {"type":"input_image","image_url":"data:image/png;base64,test","detail":"low"}
            ]})
        } else {
            serde_json::json!({"role":"user","content":[
                {"type":"text","text":""},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,test","detail":"low"}}
            ]})
        };
        let mut payload = serde_json::json!({"model":"workbuddy-fast","stream":false});
        payload[if path.ends_with("responses") {
            "input"
        } else {
            "messages"
        }] = serde_json::json!([message]);
        let response = build_router(test_ctx(base))
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let requests = recorded.lock().unwrap();
        assert_eq!(
            requests[0]["messages"][0]["content"],
            serde_json::json!([
                {"type":"image_url","image_url":{"url":"data:image/png;base64,test","detail":"low"}}
            ])
        );
    }
}

#[tokio::test]
async fn completely_empty_requests_fail_locally_without_selecting_an_account() {
    let (base, recorded) = spawn_mock_upstream().await;
    let ctx = test_ctx(base);
    for (path, payload) in [
        ("/v1/chat/completions", serde_json::json!({"messages":[]})),
        (
            "/v1/chat/completions",
            serde_json::json!({"messages":[{"role":"user","content":"  "},{"role":"assistant","content":null}]}),
        ),
        (
            "/v1/chat/completions",
            serde_json::json!({"messages":[{"role":"assistant","content":[{"type":"reasoning_text","text":"thinking only"}]}]}),
        ),
        (
            "/v1/chat/completions",
            serde_json::json!({"messages":[{"role":"user","content":[null,{}," "]}]}),
        ),
        ("/v1/chat/completions", serde_json::json!(null)),
        ("/v1/responses", serde_json::json!({"input":" "})),
        ("/v1/responses", serde_json::json!({"input":[]})),
        (
            "/v1/responses",
            serde_json::json!({"input":[{"role":"user","content":[]}]}),
        ),
        (
            "/v1/responses",
            serde_json::json!({"input":[{"type":"reasoning","summary":[]}]}),
        ),
        (
            "/v1/responses",
            serde_json::json!({"input":[{"role":"user","content":[{"type":"input_text","text":"describe this"},{"type":"input_image","file_id":"test-file-id"}]}]}),
        ),
        ("/v1/responses", serde_json::json!([])),
    ] {
        let response = build_router(ctx.clone())
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{path}: {payload}"
        );
        let wire = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!wire.is_empty());
    }
    assert!(recorded.lock().unwrap().is_empty());
    assert_eq!(ctx.usage.snapshot().total_requests, 0);
    assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
    let config = ctx.cfg.read().await;
    assert!(ctx
        .pool
        .statuses(&config.accounts)
        .iter()
        .all(|(_, status)| status.in_flight == 0));
}

async fn rotation_upstream(
    failure: &'static str,
    success_index: usize,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let observed = attempts.clone();
    let app = Router::new().route("/v2/chat/completions", post(move |headers: axum::http::HeaderMap, body: Bytes| {
        let observed = observed.clone();
        async move {
            let token = headers[header::AUTHORIZATION].to_str().unwrap().to_string();
            observed.lock().unwrap().push(token.clone());
            let body: Value = serde_json::from_slice(&body).unwrap();
            let stream = body["stream"].as_bool().unwrap_or(false);
            let succeeds = token == format!("Bearer rotation-token-{success_index}");
            if !succeeds {
                return match failure {
                    "business" => Response::builder().header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"code":10001,"msg":"积分不足","data":null}"#)).unwrap(),
                    "sse" => Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(concat!("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
                            "data: {\"error\":{\"code\":\"insufficient_quota\",\"message\":\"quota exhausted\"}}\n\n"))).unwrap(),
                    "partial" => Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(concat!("data: {\"choices\":[{\"delta\":{\"content\":\"partial answer\"}}]}\n\n",
                            "data: {\"error\":{\"code\":\"insufficient_quota\",\"message\":\"quota exhausted\"}}\n\n"))).unwrap(),
                    "unauthorized" => Response::builder().status(StatusCode::UNAUTHORIZED)
                        .body(Body::from(r#"{"code":10001,"msg":"token expired"}"#)).unwrap(),
                    _ => Response::builder().status(StatusCode::PAYMENT_REQUIRED)
                        .body(Body::from(r#"{"error":{"code":"insufficient_quota","message":"quota exhausted"}}"#)).unwrap(),
                };
            }
            if stream {
                Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(concat!("data: {\"choices\":[{\"delta\":{\"content\":\"rotation success\"}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
                        "data: [DONE]\n\n"))).unwrap()
            } else {
                Response::builder().header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"choices":[{"message":{"role":"assistant","content":"rotation success"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#)).unwrap()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), attempts)
}

async fn rotation_context(base_url: String, accounts: usize) -> Ctx {
    let ctx = test_ctx(base_url);
    {
        let mut config = ctx.cfg.write().await;
        let template = config.accounts[0].clone();
        config.accounts = (0..accounts)
            .map(|index| AccountConfig {
                uid: format!("rotation-{index}"),
                token: format!("rotation-token-{index}"),
                ..template.clone()
            })
            .collect();
        config.pool.strategy = "order".into();
        config.pool.sticky_enabled = true;
    }
    ctx.keys
        .create(crate::apikey::CreateRequest {
            name: "rotation test".into(),
            custom_key: "sk-rotation-test-12345678".into(),
            ..Default::default()
        })
        .unwrap();
    ctx
}

async fn rotation_request(ctx: Ctx, path: &str, stream: bool) -> String {
    let response = build_router(ctx).oneshot(Request::post(path)
        .header(header::AUTHORIZATION, "Bearer sk-rotation-test-12345678")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::json!({"model":"workbuddy-fast","messages":[{"role":"user","content":"test"}],"input":"test","stream":stream,"conversation_id":"same-conversation"}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test]
async fn api_key_uses_all_accounts_when_credits_are_exhausted() {
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for stream in [false, true] {
            let (base, attempts) = rotation_upstream("http", 4).await;
            let ctx = rotation_context(base, 5).await;
            let wire = rotation_request(ctx.clone(), path, stream).await;
            assert!(wire.contains("rotation success"));
            assert_eq!(attempts.lock().unwrap().len(), 5);
            assert_eq!(ctx.usage.snapshot().total_requests, 5);
            assert_eq!(ctx.usage.snapshot().total_errors, 4);
            let accounts = ctx.cfg.read().await.accounts.clone();
            let statuses = ctx.pool.statuses(&accounts);
            assert!(statuses[..4]
                .iter()
                .all(|(_, status)| status.cool_kind == "hard_credit" && status.in_flight == 0));
            assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
        }
    }
}

#[tokio::test]
async fn api_key_retries_business_errors_and_error_frames_before_output() {
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for (failure, stream) in [
            ("business", false),
            ("business", true),
            ("sse", true),
            ("unauthorized", false),
        ] {
            let (base, attempts) = rotation_upstream(failure, 1).await;
            let ctx = rotation_context(base, 2).await;
            assert!(rotation_request(ctx.clone(), path, stream)
                .await
                .contains("rotation success"));
            assert_eq!(
                *attempts.lock().unwrap(),
                ["Bearer rotation-token-0", "Bearer rotation-token-1"]
            );
            assert_eq!(ctx.usage.snapshot().total_errors, 1);
            assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
            if failure == "unauthorized" {
                let accounts = ctx.cfg.read().await.accounts.clone();
                assert_ne!(ctx.pool.statuses(&accounts)[0].1.cool_kind, "hard_credit");
            }
        }
    }
}

#[tokio::test]
async fn stream_error_after_output_does_not_replay_generation_on_another_account() {
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let (base, attempts) = rotation_upstream("partial", 1).await;
        let ctx = rotation_context(base, 2).await;
        let wire = rotation_request(ctx.clone(), path, true).await;
        assert!(wire.contains("partial answer"));
        assert!(!wire.contains("rotation success"));
        assert!(!wire.contains("response.completed"));
        assert_eq!(attempts.lock().unwrap().len(), 1);
        assert_eq!(ctx.usage.snapshot().total_errors, 1);
        let accounts = ctx.cfg.read().await.accounts.clone();
        assert_eq!(ctx.pool.statuses(&accounts)[0].1.cool_kind, "hard_credit");
    }
}

mod channel_protocol_tests {
    use super::*;
    use axum::http::HeaderMap;

    type ChannelRequests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

    fn header_text<'a>(headers: &'a HeaderMap, key: &str) -> &'a str {
        headers
            .get(key)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    }

    fn hex_id(value: &str, length: usize) -> bool {
        value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    // This fake upstream models protocol rejection, rather than returning success to
    // every request. It accepts no real identities, credentials or network endpoints.
    fn contract_error(headers: &HeaderMap, body: &Value, realm: &str) -> Option<&'static str> {
        for (name, expected) in [
            ("x-agent-purpose", "conversation"),
            ("x-ide-name", "WorkBuddy"),
            ("x-ide-type", "WorkBuddy"),
            ("x-ide-version", "5.5.4"),
            ("x-product", "WorkBuddy"),
            ("x-codebuddy-request", "1"),
        ] {
            if header_text(headers, name) != expected {
                return Some(name);
            }
        }
        for name in ["x-machine-id", "x-session-id"] {
            if !hex_id(header_text(headers, name), 36) {
                return Some(name);
            }
        }
        if !header_text(headers, "x-user-id").starts_with("official-channel-") {
            return Some("x-user-id");
        }
        if headers.contains_key("x-refresh-token") {
            return Some("x-refresh-token must be absent");
        }
        if !header_text(headers, "accept").contains("text/event-stream") {
            return Some("accept");
        }
        if header_text(headers, "content-type") != "application/json" {
            return Some("content-type");
        }
        let (origin, user_agent, language) = if realm == "global" {
            (
                "https://www.workbuddy.ai",
                "WorkBuddy/5.5.4 WorkBuddy AI/5.5.4 CLI/2.137.1",
                "en-US",
            )
        } else {
            (
                "https://www.codebuddy.cn",
                "WorkBuddy/5.5.4 WorkBuddy/5.5.4 CLI/2.137.1",
                "zh-CN",
            )
        };
        if header_text(headers, "origin") != origin {
            return Some("origin");
        }
        if header_text(headers, "referer") != format!("{origin}/") {
            return Some("referer");
        }
        if header_text(headers, "user-agent") != user_agent {
            return Some("user-agent");
        }
        if header_text(headers, "accept-language") != language {
            return Some("accept-language");
        }
        if realm == "global" {
            if header_text(headers, "x-domain") != "www.workbuddy.ai" {
                return Some("x-domain");
            }
            if header_text(headers, "x-no-enterprise-id") != "1" {
                return Some("x-no-enterprise-id");
            }
            if headers.contains_key("x-enterprise-id") {
                return Some("global enterprise header must be absent");
            }
        } else {
            if header_text(headers, "x-enterprise-id") != "channel-enterprise" {
                return Some("x-enterprise-id");
            }
            if header_text(headers, "x-domain") != "channel-domain" {
                return Some("x-domain");
            }
        }
        for name in [
            "x-conversation-id",
            "x-conversation-request-id",
            "x-trace-id",
        ] {
            if header_text(headers, name).is_empty() {
                return Some(name);
            }
        }
        if !hex_id(header_text(headers, "x-conversation-message-id"), 32) {
            return Some("x-conversation-message-id");
        }
        if header_text(headers, "x-request-id") != header_text(headers, "x-conversation-message-id")
        {
            return Some("x-request-id");
        }
        if header_text(headers, "x-root-request-id")
            != header_text(headers, "x-conversation-request-id")
        {
            return Some("x-root-request-id");
        }
        let trace = header_text(headers, "x-b3-traceid");
        if !hex_id(trace, 32) && !hex_id(trace, 16) {
            return Some("x-b3-traceid");
        }
        if !hex_id(header_text(headers, "x-b3-spanid"), 16) {
            return Some("x-b3-spanid");
        }
        if body["stream"] != true || body["stream_options"]["include_usage"] != true {
            return Some("stream configuration");
        }
        if realm == "global" && body["messages"][0]["role"] != "system" {
            return Some("global first system message");
        }
        None
    }

    async fn channel_upstream(
        realm: &'static str,
        reject_first_account: bool,
    ) -> (String, ChannelRequests, tokio::task::JoinHandle<()>) {
        let requests: ChannelRequests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let app = Router::new().route("/v2/chat/completions", post(move |headers: HeaderMap, body: Bytes| {
            let recorded = recorded.clone();
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap();
                recorded.lock().unwrap().push((headers.clone(), body.clone()));
                if let Some(field) = contract_error(&headers, &body, realm) {
                    return Response::builder().status(StatusCode::BAD_REQUEST)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(serde_json::json!({"code":11128,"msg":"request illegal","displayTips":{"en":format!("missing protocol field: {field}")}}).to_string())).unwrap();
                }
                if reject_first_account && header_text(&headers, "authorization") == "Bearer channel-token-one" {
                    return Response::builder().status(StatusCode::PAYMENT_REQUIRED)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"code":14018,"msg":"credits exhausted"}"#)).unwrap();
                }
                Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(concat!(
                        "data: {\"id\":\"channel-test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"channel ok\"}}]}\n\n",
                        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                        "data: [DONE]\n\n"
                    ))).unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), requests, server)
    }

    async fn channel_context(base: String, realm: &str, second_account: bool) -> Ctx {
        let ctx = test_ctx(base);
        {
            let mut config = ctx.cfg.write().await;
            let account = &mut config.accounts[0];
            account.uid = "acct-channel-local-one".into();
            account.workbuddy_uid = Some("official-channel-one".into());
            account.token = "channel-token-one".into();
            account.refresh_token = "fake-refresh-must-not-leak".into();
            account.enterprise_id = "channel-enterprise".into();
            account.domain = "channel-domain".into();
            account.realm = realm.into();
            if second_account {
                let mut second = account.clone();
                second.uid = "acct-channel-local-two".into();
                second.workbuddy_uid = Some("official-channel-two".into());
                second.token = "channel-token-two".into();
                config.accounts.push(second);
            }
            config.pool.strategy = "order".into();
        }
        ctx
    }

    fn client_payload(path: &str, stream: bool) -> Value {
        let mut payload = serde_json::json!({"model":"workbuddy-fast", "stream":stream,
            "metadata":{"conversation_id":"metadata-channel-conversation"},
            "conversation_id":"root-channel-conversation", "conversationId":"camel-channel-conversation"});
        payload[if path.ends_with("responses") {
            "input"
        } else {
            "messages"
        }] = serde_json::json!([
            {"role":"user","content":"ping"}
        ]);
        payload
    }

    async fn channel_request(ctx: Ctx, path: &str, payload: Value) -> String {
        let response = build_router(ctx)
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("X-Conversation-Request-ID", "incoming-channel-turn")
                    .header("X-Trace-ID", "incoming-channel-trace")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let wire = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(status, StatusCode::OK, "{path}: {wire}");
        assert!(wire.contains("channel ok"), "{path}: {wire}");
        wire
    }

    #[tokio::test]
    async fn gateway_routes_chat_and_responses_through_channel_protocol_for_both_realms() {
        for realm in ["cn", "global"] {
            for path in ["/v1/chat/completions", "/v1/responses"] {
                for stream in [false, true] {
                    let (base, recorded, server) = channel_upstream(realm, false).await;
                    let ctx = channel_context(base, realm, false).await;
                    channel_request(ctx, path, client_payload(path, stream)).await;
                    server.abort();
                    let requests = recorded.lock().unwrap();
                    assert_eq!(requests.len(), 1, "{realm} {path} {stream}");
                    let (headers, body) = &requests[0];
                    assert_eq!(header_text(headers, "x-user-id"), "official-channel-one");
                    assert_eq!(
                        header_text(headers, "x-conversation-id"),
                        "metadata-channel-conversation"
                    );
                    assert_eq!(
                        header_text(headers, "x-conversation-request-id"),
                        "incoming-channel-turn"
                    );
                    assert_eq!(header_text(headers, "x-trace-id"), "incoming-channel-trace");
                    assert_eq!(
                        body["messages"].as_array().unwrap().len(),
                        if realm == "global" { 2 } else { 1 }
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn connection_probe_uses_the_same_channel_contract_and_does_not_duplicate_system() {
        for realm in ["cn", "global"] {
            let (base, recorded, server) = channel_upstream(realm, false).await;
            let ctx = channel_context(base, realm, false).await;
            let config = ctx.cfg.read().await.clone();
            let result = crate::commands::test_connection(config).await.unwrap();
            assert!(result.ok, "{realm}: {:?}", result.error);
            for path in ["/v1/chat/completions", "/v1/responses"] {
                let mut payload = client_payload(path, false);
                payload[if path.ends_with("responses") {
                    "input"
                } else {
                    "messages"
                }] = serde_json::json!([
                    {"role":"system","content":"client system is preserved"},
                    {"role":"user","content":"ping"}
                ]);
                channel_request(ctx.clone(), path, payload).await;
            }
            server.abort();
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 3);
            for (headers, body) in &requests[1..] {
                assert_eq!(body["messages"].as_array().unwrap().len(), 2);
                assert_eq!(body["messages"][0]["content"], "client system is preserved");
                assert_eq!(
                    header_text(headers, "x-machine-id"),
                    header_text(&requests[0].0, "x-machine-id")
                );
                assert_eq!(
                    header_text(headers, "x-session-id"),
                    header_text(&requests[0].0, "x-session-id")
                );
            }
        }
    }

    #[tokio::test]
    async fn client_camel_case_and_header_only_conversations_are_preserved() {
        let (base, recorded, server) = channel_upstream("cn", false).await;
        let ctx = channel_context(base, "cn", false).await;
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for (field, conversation) in [
                ("conversationId", "camel-only-conversation"),
                ("header", "header-only-conversation"),
            ] {
                let mut payload = serde_json::json!({"model":"workbuddy-fast", "stream":false});
                payload[if path.ends_with("responses") {
                    "input"
                } else {
                    "messages"
                }] = serde_json::json!([
                    {"role":"user","content":"ping"}
                ]);
                if field != "header" {
                    payload[field] = serde_json::json!(conversation);
                }
                let response = build_router(ctx.clone())
                    .oneshot(
                        Request::post(path)
                            .header(header::CONTENT_TYPE, "application/json")
                            .header("X-Conversation-ID", conversation)
                            .body(Body::from(payload.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            }
        }
        server.abort();
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 4);
        for (index, (headers, _)) in requests.iter().enumerate() {
            assert_eq!(
                header_text(headers, "x-conversation-id"),
                if index % 2 == 0 {
                    "camel-only-conversation"
                } else {
                    "header-only-conversation"
                }
            );
        }
    }

    #[tokio::test]
    async fn account_rotation_preserves_turn_identity_and_uses_distinct_account_device_ids() {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let (base, recorded, server) = channel_upstream("cn", true).await;
            let ctx = channel_context(base, "cn", true).await;
            channel_request(ctx.clone(), path, client_payload(path, false)).await;
            server.abort();
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for name in [
                "x-conversation-id",
                "x-conversation-request-id",
                "x-root-request-id",
                "x-trace-id",
            ] {
                assert_eq!(
                    header_text(&requests[0].0, name),
                    header_text(&requests[1].0, name)
                );
            }
            assert_ne!(
                header_text(&requests[0].0, "x-request-id"),
                header_text(&requests[1].0, "x-request-id")
            );
            assert_ne!(
                header_text(&requests[0].0, "x-machine-id"),
                header_text(&requests[1].0, "x-machine-id")
            );
            assert_ne!(
                header_text(&requests[0].0, "x-session-id"),
                header_text(&requests[1].0, "x-session-id")
            );
            assert_eq!(
                header_text(&requests[1].0, "x-user-id"),
                "official-channel-two"
            );
            assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
        }
    }

    #[tokio::test]
    async fn strict_upstream_rejects_missing_protocol_and_accepts_shared_probe() {
        let (base, _, server) = channel_upstream("cn", false).await;
        let old_request = reqwest::Client::new().post(format!("{base}/v2/chat/completions"))
            .bearer_auth("channel-token-one")
            .json(&serde_json::json!({"model":"fast-model","messages":[{"role":"user","content":"ping"}],"stream":true}))
            .send().await.unwrap();
        assert_eq!(old_request.status(), StatusCode::BAD_REQUEST);
        let error: Value = old_request.json().await.unwrap();
        assert_eq!(error["code"], 11128);
        let ctx = channel_context(base, "cn", false).await;
        let config = ctx.cfg.read().await.clone();
        let result = crate::commands::test_connection(config).await.unwrap();
        server.abort();
        assert!(result.ok, "{:?}", result.error);
    }

    #[tokio::test]
    async fn policy_rejections_remain_failed_requests_without_cooling_any_account() {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for client_stream in [false, true] {
                for (status, sse, message) in [
                    (StatusCode::BAD_REQUEST, false, "request illegal"),
                    (StatusCode::FORBIDDEN, false, "request illegal"),
                    (StatusCode::OK, false, "request illegal"),
                    (StatusCode::OK, true, "request illegal"),
                    (StatusCode::OK, true, "blocked by security policy"),
                ] {
                    // Long messages catch premature diagnostic truncation that loses
                    // the structured code before account-health classification.
                    let message = format!("{} {message}", "details ".repeat(80));
                    let error = serde_json::json!({"code":"11-128","msg":message});
                    let wire = if sse {
                        format!("data: {error}\n\n")
                    } else {
                        error.to_string()
                    };
                    let app = Router::new().route(
                        "/v2/chat/completions",
                        post(move || {
                            let wire = wire.clone();
                            async move {
                                Response::builder()
                                    .status(status)
                                    .header(
                                        header::CONTENT_TYPE,
                                        if sse {
                                            "text/event-stream"
                                        } else {
                                            "application/json"
                                        },
                                    )
                                    .body(Body::from(wire))
                                    .unwrap()
                            }
                        }),
                    );
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let address = listener.local_addr().unwrap();
                    let server =
                        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                    let ctx = channel_context(format!("http://{address}"), "cn", true).await;
                    ctx.cfg.write().await.pool.breaker_threshold = 1;
                    for _ in 0..2 {
                        let response = build_router(ctx.clone())
                            .oneshot(
                                Request::post(path)
                                    .header(header::CONTENT_TYPE, "application/json")
                                    .body(Body::from(
                                        client_payload(path, client_stream).to_string(),
                                    ))
                                    .unwrap(),
                            )
                            .await
                            .unwrap();
                        assert_ne!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                    }
                    server.abort();
                    let snapshot = ctx.usage.snapshot();
                    assert_eq!(
                        snapshot.total_requests, 4,
                        "{path} {client_stream} {status} SSE={sse}"
                    );
                    assert_eq!(snapshot.total_errors, 4);
                    assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
                    let accounts = ctx.cfg.read().await.accounts.clone();
                    for (_, health) in ctx.pool.statuses(&accounts) {
                        assert_eq!(health.in_flight, 0);
                        assert_eq!(health.err_total, 0);
                        assert_eq!(health.consecutive_fails, 0);
                        assert!(health.cooling_until.is_none());
                        assert!(health.cool_kind.is_empty());
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn content_blocked_retries_once_with_degraded_prompt_then_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let seen: ChannelRequests = Arc::new(Mutex::new(Vec::new()));
        let hits2 = hits.clone();
        let seen2 = seen.clone();
        let app = Router::new().route(
            "/v2/chat/completions",
            post(move |body: Bytes| {
                let hits = hits2.clone();
                let seen = seen2.clone();
                async move {
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    seen.lock().unwrap().push((HeaderMap::new(), body.clone()));
                    if n == 0 {
                        // 首次：返回 11128 内容审核误报（unapproved channel）。
                        return Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                r#"{"code":11128,"msg":"Illegal API invocation from an unapproved channel"}"#,
                            ))
                            .unwrap();
                    }
                    Response::builder()
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(concat!(
                            "data: {\"id\":\"degraded-test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"degraded ok\"}}]}\n\n",
                            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                            "data: [DONE]\n\n"
                        )))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ctx = channel_context(format!("http://{address}"), "cn", false).await;
        let payload = serde_json::json!({
            "model": "workbuddy-fast",
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are Claude Code, Anthropic's official CLI for Claude"},
                {"role": "user", "content": "你是谁"}
            ]
        });
        let response = build_router(ctx)
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let wire =
            String::from_utf8(to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec())
                .unwrap();
        assert!(wire.contains("degraded ok"));
        server.abort();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let seen = seen.lock().unwrap();
        // 第一次：原始 system 已被出站 sanitize 改写（仍非中性提示词）。
        assert_eq!(seen[0].1["messages"][0]["role"], "system");
        assert!(seen[0].1["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("official CLI tool"));
        // 第二次：降级重试，system 替换为中性提示词。
        assert_eq!(seen[1].1["messages"][0]["role"], "system");
        assert_eq!(
            seen[1].1["messages"][0]["content"],
            crate::gateway::translate::DEGRADED_SYSTEM_PROMPT
        );
    }
}
