//! 网关共享状态、鉴权与上游转发管线。

use super::translate;
use crate::apikey::{self, ApiKey};
use crate::config::AppConfig;
use crate::pool::{ErrorKind, Pool};
use crate::reqlog::{self, Event};
use crate::usage::{UsageDelta, UsageRecorder};
use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_stream::wrappers::ReceiverStream;

pub type SharedConfig = Arc<RwLock<AppConfig>>;

pub struct GatewayState {
    pub cfg: SharedConfig,
    pub pool: Arc<Pool>,
    pub keys: Arc<apikey::Store>,
    pub reqlog: Arc<reqlog::Recorder>,
    pub usage: Arc<UsageRecorder>,
    pub client: reqwest::Client,
}

pub type Ctx = Arc<GatewayState>;
const UPSTREAM_TIMEOUT_SECS: u64 = 600;

pub fn new_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("workbuddy-codex-gateway/0.2")
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(UPSTREAM_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))
}

async fn send_chat(
    client: &reqwest::Client,
    configured_base: &str,
    account: &crate::config::AccountConfig,
    body: &serde_json::Value,
    session: &super::request::ChatSession,
) -> Result<reqwest::Response, String> {
    let url = super::request::chat_url(configured_base, &account.realm)?;
    super::request::chat_post_with_session(client, &url, account, body, session)
        .send()
        .await
        .map_err(|error| error.to_string())
}

pub fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    let bytes = Bytes::from(value.to_string());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap()
}

pub fn err_response(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        serde_json::json!({ "error": { "message": message, "type": "gateway_error" } }),
    )
}

pub struct AuthInfo {
    pub key: Option<ApiKey>,
    pub key_value: String,
    pub client_ip: String,
    pub user_agent: String,
}

pub fn authenticate(
    ctx: &Ctx,
    headers: &HeaderMap,
    master_key: &str,
) -> Result<AuthInfo, Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let client_ip = client_ip(headers);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let (result, ok) = ctx
        .keys
        .verify(&token, master_key, &client_ip, reqlog::now_ms());
    if !ok {
        return Err(err_response(StatusCode::UNAUTHORIZED, &result.reason));
    }
    Ok(AuthInfo {
        key: result.key,
        key_value: token,
        client_ip,
        user_agent,
    })
}

fn client_ip(headers: &HeaderMap) -> String {
    if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            return first.trim().to_string();
        }
    }
    if let Some(v) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        return v.trim().to_string();
    }
    "127.0.0.1".to_string()
}

pub fn parse_model_realm(body: &[u8]) -> (String, String) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ("".to_string(), "cn".to_string());
    };
    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let (realm, bare) = match model.split_once(':') {
        Some((r, rest)) => (r.to_string(), rest.to_string()),
        None => ("cn".to_string(), model.clone()),
    };
    (bare, realm)
}

pub fn parse_conv_id(body: &[u8]) -> Option<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return None;
    };
    super::request::conversation_id(&v).map(str::to_string)
}

pub struct ReqMeta {
    pub request_id: String,
    pub path: String,
    pub model: String,
    pub uid: String,
    pub account_label: String,
    pub auth: AuthInfo,
    pub attempts: i64,
    accounted: AtomicBool,
}

struct PendingAttempt<'a> {
    ctx: Ctx,
    meta: &'a ReqMeta,
    started: std::time::Instant,
    usage: UsageObservation,
    handed_off: bool,
}

impl Drop for PendingAttempt<'_> {
    fn drop(&mut self) {
        if !self.handed_off && !self.meta.accounted.swap(true, Ordering::AcqRel) {
            record_accounting(
                &self.ctx,
                self.meta,
                reqlog::OUTCOME_INTERRUPTED,
                0,
                false,
                self.started.elapsed().as_millis() as i64,
                None,
                self.usage,
                0.0,
                false,
            );
        }
    }
}

async fn finish_accounting(
    ctx: &Ctx,
    meta: &ReqMeta,
    outcome: &str,
    status: u16,
    ok: bool,
    duration_ms: i64,
    ttfb_ms: Option<i64>,
    tokens: impl Into<UsageObservation>,
    credit: f64,
    credit_known: bool,
) {
    if meta.accounted.swap(true, Ordering::AcqRel) {
        return;
    }
    let usage = tokens.into();
    record_accounting(
        ctx,
        meta,
        outcome,
        status,
        ok,
        duration_ms,
        ttfb_ms,
        usage,
        credit,
        credit_known,
    );
    if ok {
        ctx.pool.note_success(&meta.uid, None).await;
    }
}

fn record_accounting(
    ctx: &Ctx,
    meta: &ReqMeta,
    outcome: &str,
    status: u16,
    ok: bool,
    duration_ms: i64,
    ttfb_ms: Option<i64>,
    usage: UsageObservation,
    credit: f64,
    credit_known: bool,
) {
    let (prompt, completion, total) = usage.tokens();
    ctx.reqlog.record(Event {
        time: 0,
        request_id: meta.request_id.clone(),
        path: meta.path.clone(),
        account: meta.account_label.clone(),
        account_uid: meta.uid.clone(),
        model: meta.model.clone(),
        status,
        ok,
        outcome: outcome.to_string(),
        duration_ms,
        ttfb_ms,
        attempts: Some(meta.attempts),
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        credit,
        credit_known,
        client_ip: meta.auth.client_ip.clone(),
        user_agent: meta.auth.user_agent.clone(),
    });

    // Failed and zero-usage attempts are still requests; skipping them inflated success rates.
    ctx.usage.record(UsageDelta {
        realm: "cn".to_string(),
        uid: meta.uid.clone(),
        model: meta.model.clone(),
        ok,
        latency_ms: duration_ms,
        ttfb_ms,
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        credit,
        credit_known,
        cache_hit_tokens: usage.cache_hit,
        cache_miss_tokens: usage.cache_miss,
        cache_known: usage.cache_known,
    });

    if let Some(key) = &meta.auth.key {
        ctx.keys.record_usage(&key.key, credit, reqlog::now_ms());
    }
    ctx.pool.release(&meta.uid);
}

pub fn classify_upstream_error(status: StatusCode, text: &str) -> ErrorKind {
    if let Some(kind) = request_policy_error(text) {
        return kind;
    }
    match status.as_u16() {
        429 => ErrorKind::SoftRate,
        402 | 403 => ErrorKind::HardCredit,
        404 => ErrorKind::NotFound,
        _ => {
            let text = text.to_lowercase();
            if [
                "余额不足",
                "积分不足",
                "额度不足",
                "积分已用完",
                "insufficient_quota",
                "insufficient credit",
                "insufficient balance",
                "quota exhausted",
                "quota exceeded",
                "credit exhausted",
                "credits exhausted",
                "out of credits",
            ]
            .iter()
            .any(|marker| text.contains(marker))
            {
                ErrorKind::HardCredit
            } else if text.contains("rate_limit")
                || text.contains("rate limit")
                || text.contains("限流")
            {
                ErrorKind::SoftRate
            } else {
                ErrorKind::Other
            }
        }
    }
}

/// A business code must be present in a JSON field, not merely quoted in a message
/// or request ID. The same 11128 code carries several request-level rejections.
fn request_policy_error(text: &str) -> Option<ErrorKind> {
    fn inspect(value: &serde_json::Value, channel_code: &mut bool, messages: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(code) = object.get("code") {
                    *channel_code |= code.as_i64() == Some(11128)
                        || code
                            .as_str()
                            .is_some_and(|code| matches!(code.trim(), "11128" | "11-128"));
                }
                for key in ["msg", "message"] {
                    if let Some(message) = object.get(key).and_then(|value| value.as_str()) {
                        messages.push(message.to_lowercase());
                    }
                }
                for child in object.values() {
                    inspect(child, channel_code, messages);
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    inspect(child, channel_code, messages);
                }
            }
            _ => {}
        }
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut channel_code = false;
    let mut messages = Vec::new();
    inspect(&value, &mut channel_code, &mut messages);
    if !channel_code {
        return None;
    }
    if messages.iter().any(|message| {
        [
            "blocked by security policy",
            "unapproved channel",
            "illegal api invocation",
        ]
        .iter()
        .any(|marker| message.contains(marker))
    }) {
        return Some(ErrorKind::ContentBlocked);
    }
    if messages.iter().any(|message| {
        [
            "request illegal",
            "channel verification",
            "conversation id missing",
        ]
        .iter()
        .any(|marker| message.contains(marker))
    }) {
        return Some(ErrorKind::ChannelRefused);
    }
    None
}

fn error_fields(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let allowed = [
                "code",
                "msg",
                "message",
                "type",
                "error",
                "data",
                "extError",
                "cause",
                "innerError",
                "errCode",
                "error_code",
            ];
            serde_json::Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| allowed.contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), error_fields(value)))
                    .collect(),
            )
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(error_fields).collect())
        }
        value => value.clone(),
    }
}

pub(super) fn business_error(value: &serde_json::Value) -> Option<String> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        // Keep the envelope's exact code/message for later HTTP-200/SSE error
        // classification; extracting only the nested error can lose its code.
        let mut envelope = serde_json::json!({"error": error_fields(error)});
        for key in [
            "code",
            "msg",
            "message",
            "data",
            "extError",
            "cause",
            "innerError",
        ] {
            if let Some(field) = value.get(key) {
                envelope[key] = error_fields(field);
            }
        }
        return Some(envelope.to_string());
    }
    let code = value.get("code")?;
    let code = code.as_i64().or_else(|| code.as_str()?.parse().ok());
    if matches!(code, Some(0 | 200)) {
        return None;
    }
    let message = value
        .get("msg")
        .or_else(|| value.get("message"))
        .and_then(|value| value.as_str())
        .unwrap_or("Upstream business error");
    let mut envelope = serde_json::json!({
        "code": error_fields(&value["code"]), "message": message,
        "diagnostic": format!("code={}", error_fields(&value["code"]))
    });
    for key in ["data", "extError", "cause", "innerError"] {
        if let Some(field) = value
            .get(key)
            .filter(|value| value.is_object() || value.is_array())
        {
            envelope[key] = error_fields(field);
        }
    }
    Some(envelope.to_string())
}

/// 非流式上游调用已内联到 handle_chat / handle_responses，这里只保留工具函数。
#[derive(Default, Clone, Copy)]
struct UsageObservation {
    prompt: i64,
    completion: i64,
    total: Option<i64>,
    cache_hit: i64,
    cache_miss: i64,
    cache_known: bool,
}

impl From<(i64, i64, i64)> for UsageObservation {
    fn from((prompt, completion, total): (i64, i64, i64)) -> Self {
        Self {
            prompt,
            completion,
            total: Some(total),
            ..Default::default()
        }
    }
}

impl UsageObservation {
    fn tokens(&self) -> (i64, i64, i64) {
        (
            self.prompt,
            self.completion,
            self.total.unwrap_or(self.prompt + self.completion),
        )
    }

    fn observe(&mut self, chunk: &serde_json::Value) {
        let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) else {
            return;
        };
        let number = |value: &serde_json::Value| value.as_i64().filter(|n| *n >= 0);
        if let Some(n) = usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(number)
        {
            self.prompt = n;
        }
        if let Some(n) = usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(number)
        {
            self.completion = n;
        }
        if let Some(n) = usage.get("total_tokens").and_then(number) {
            self.total = Some(n);
        }
        // Match CodeBuddy's documented cache aliases. Missing observations stay unknown.
        let hit = [
            "/prompt_cache_hit_tokens",
            "/cache_read_input_tokens",
            "/prompt_tokens_details/cached_tokens",
            "/input_tokens_details/cached_tokens",
            "/cached_tokens",
        ]
        .iter()
        .filter_map(|path| usage.pointer(path).and_then(number))
        .max();
        let miss = ["/prompt_cache_miss_tokens", "/cache_creation_input_tokens"]
            .iter()
            .filter_map(|path| usage.pointer(path).and_then(number))
            .max();
        if hit.is_some() || miss.is_some() {
            self.cache_known = true;
            self.cache_hit = hit.unwrap_or_else(|| (self.prompt - miss.unwrap_or(0)).max(0));
            self.cache_miss = miss.unwrap_or_else(|| (self.prompt - self.cache_hit).max(0));
        }
    }
}

fn usage_from_chat(chat: &serde_json::Value) -> UsageObservation {
    let mut usage = UsageObservation::default();
    usage.observe(chat);
    usage
}

#[derive(Default)]
pub(super) struct StreamAccounting {
    pending: Vec<u8>,
    usage: UsageObservation,
    pub(super) saw_error: bool,
    pub(super) error_message: Option<String>,
    saw_output: bool,
    saw_response_output: bool,
    pub(super) saw_done: bool,
    choices: std::collections::HashMap<i64, bool>,
}

impl StreamAccounting {
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<serde_json::Value> {
        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        let mut consumed = 0;
        while let Some(relative) = self.pending[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + relative;
            let line = &self.pending[consumed..end];
            if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.trim_ascii();
                if !data.iter().all(u8::is_ascii_whitespace) {
                    if data == b"[DONE]" {
                        self.saw_done = true;
                    } else if let Ok(chunk) = serde_json::from_slice::<serde_json::Value>(data) {
                        if self.saw_done {
                            self.saw_error = true;
                        }
                        self.usage.observe(&chunk);
                        if let Some(message) = business_error(&chunk) {
                            self.saw_error = true;
                            self.error_message = Some(message);
                        }
                        if let Some(choices) = chunk.get("choices").and_then(|v| v.as_array()) {
                            for choice in choices {
                                if !choice.get("delta").is_some_and(|delta| delta.is_object())
                                    && !choice
                                        .get("finish_reason")
                                        .is_some_and(|reason| reason.is_string())
                                {
                                    self.saw_error = true;
                                    continue;
                                }
                                let index =
                                    choice.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                                let finished = self.choices.entry(index).or_default();
                                if let Some(reason) =
                                    choice.get("finish_reason").and_then(|v| v.as_str())
                                {
                                    if !reason.is_empty() {
                                        if matches!(
                                            reason,
                                            "stop"
                                                | "length"
                                                | "tool_calls"
                                                | "function_call"
                                                | "content_filter"
                                        ) {
                                            *finished = true;
                                        } else {
                                            self.saw_error = true;
                                        }
                                    }
                                }
                                let delta = choice.get("delta");
                                self.saw_response_output |=
                                    delta.is_some_and(|delta| has_output(delta, false));
                                self.saw_output |=
                                    delta.is_some_and(|delta| has_output(delta, true));
                            }
                        } else if !chunk.get("usage").is_some_and(|v| v.is_object())
                            && !chunk.get("error").is_some_and(|v| !v.is_null())
                        {
                            self.saw_error = true;
                        }
                        chunks.push(chunk);
                    } else {
                        self.saw_error = true;
                    }
                }
            } else if !line.iter().all(u8::is_ascii_whitespace)
                && !line.starts_with(b":")
                && ![b"event:".as_slice(), b"id:", b"retry:"]
                    .iter()
                    .any(|field| line.starts_with(field))
            {
                self.saw_error = true;
            }
            consumed = end + 1;
        }
        self.pending.drain(..consumed);
        // Do not retain arbitrarily large malformed SSE lines in memory.
        if self.pending.len() > 4 * 1024 * 1024 {
            self.pending.clear();
            self.saw_error = true;
        }
        chunks
    }

    pub(super) fn finish(&mut self) -> Vec<serde_json::Value> {
        self.feed(b"\n")
    }

    pub(super) fn completed(&self) -> bool {
        // [DONE] ends the whole stream. Some upstreams close after each observed choice
        // has a finish_reason instead; usage-only and empty streams satisfy neither case.
        !self.saw_error
            && !self.choices.is_empty()
            && (self.saw_done || self.choices.values().all(|finished| *finished))
    }
}

fn has_output(delta: &serde_json::Value, include_reasoning: bool) -> bool {
    let nonempty = |value: Option<&serde_json::Value>| {
        value
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    };
    nonempty(delta.get("content"))
        || (include_reasoning
            && (nonempty(delta.get("refusal"))
                || nonempty(delta.get("reasoning_content"))
                || nonempty(delta.get("reasoning"))))
        || delta
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .is_some_and(|calls| {
                calls.iter().any(|call| {
                    nonempty(call.get("id"))
                        || nonempty(call.pointer("/function/name"))
                        || nonempty(call.pointer("/function/arguments"))
                })
            })
        || (include_reasoning
            && delta
                .get("function_call")
                .is_some_and(|call| nonempty(call.get("name")) || nonempty(call.get("arguments"))))
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

struct PreparedStream {
    stream: UpstreamStream,
    prefix: Vec<Bytes>,
    first_output_ms: Option<i64>,
}

impl PreparedStream {
    fn replay(self) -> (UpstreamStream, Option<i64>) {
        let prefix = futures_util::stream::iter(self.prefix.into_iter().map(Ok));
        (Box::pin(prefix.chain(self.stream)), self.first_output_ms)
    }
}

struct StreamRejection {
    message: String,
    usage: UsageObservation,
}

impl StreamRejection {
    fn new(message: impl Into<String>, usage: UsageObservation) -> Self {
        Self {
            message: message.into(),
            usage,
        }
    }
}

/// Keep role/usage frames private until a usable result arrives. This leaves room to
/// retry a different account without duplicating text or tool calls at the client.
async fn prepare_stream(
    response: reqwest::Response,
    started: std::time::Instant,
    responses_api: bool,
    pending: &mut PendingAttempt<'_>,
) -> Result<PreparedStream, StreamRejection> {
    const MAX_PREFIX_BYTES: usize = 2 * 1024 * 1024;
    let valid_content_type = is_event_stream(&response);
    let mut stream: UpstreamStream = Box::pin(response.bytes_stream());
    // Reasoning models can think before producing client-visible output. Keep the
    // same overall deadline as the upstream request instead of imposing a shorter one.
    let deadline = tokio::time::Instant::from_std(started)
        + std::time::Duration::from_secs(UPSTREAM_TIMEOUT_SECS);
    let mut prefix = Vec::new();
    let mut size = 0usize;
    let mut stats = StreamAccounting::default();
    loop {
        let item = tokio::time::timeout_at(deadline, stream.next())
            .await
            .map_err(|_| {
                StreamRejection::new("Upstream timed out before its first output", stats.usage)
            })?;
        let Some(item) = item else {
            if !valid_content_type {
                let bytes: Vec<u8> = prefix
                    .iter()
                    .flat_map(|chunk: &Bytes| chunk.iter().copied())
                    .collect();
                let message = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|value| business_error(&value))
                    .unwrap_or_else(|| "Upstream did not return text/event-stream".into());
                return Err(StreamRejection::new(message, stats.usage));
            }
            stats.finish();
            pending.usage = stats.usage;
            if stats.completed() {
                return Ok(PreparedStream {
                    stream,
                    prefix,
                    first_output_ms: None,
                });
            }
            return Err(StreamRejection::new(
                stats
                    .error_message
                    .as_deref()
                    .unwrap_or("Upstream event stream ended without a completion signal"),
                stats.usage,
            ));
        };
        let bytes = item.map_err(|error| StreamRejection::new(error.to_string(), stats.usage))?;
        size = size.saturating_add(bytes.len());
        if size > MAX_PREFIX_BYTES {
            return Err(StreamRejection::new(
                "Upstream sent too much data before its first output",
                stats.usage,
            ));
        }
        prefix.push(bytes.clone());
        if !valid_content_type {
            continue;
        }
        let chunks = stats.feed(&bytes);
        pending.usage = stats.usage;
        for chunk in chunks {
            if let Some(message) = business_error(&chunk) {
                return Err(StreamRejection::new(message, stats.usage));
            }
            let output = chunk
                .get("choices")
                .and_then(|choices| choices.as_array())
                .is_some_and(|choices| {
                    choices.iter().any(|choice| {
                        choice
                            .get("delta")
                            .is_some_and(|delta| has_output(delta, !responses_api))
                    })
                });
            if output {
                return Ok(PreparedStream {
                    stream,
                    prefix,
                    first_output_ms: Some(started.elapsed().as_millis() as i64),
                });
            }
        }
        if stats.completed() {
            return Ok(PreparedStream {
                stream,
                prefix,
                first_output_ms: None,
            });
        }
        if stats.saw_error || stats.saw_done {
            return Err(StreamRejection::new(
                stats
                    .error_message
                    .as_deref()
                    .unwrap_or("Upstream returned an invalid or failed event stream"),
                stats.usage,
            ));
        }
    }
}

fn stream_error_response(responses_api: bool, model: &str, message: &str) -> Response {
    let error = serde_json::json!({"message": truncate(message, 200), "code": "incomplete_stream", "type": "upstream_stream_error"});
    let wire = if responses_api {
        let mut state = translate::ResponsesStreamState::new(model);
        let mut events = translate::initial_events(&mut state);
        events.push(responses_failed_event(&mut state, Some(&error)));
        events.join("")
    } else {
        format!("data: {}\n\n", serde_json::json!({"error": error}))
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(wire))
        .unwrap()
}

fn estimate_credit(cfg: &AppConfig, upstream_model: &str, total_tokens: i64) -> (f64, bool) {
    let mult = cfg.multiplier_of(upstream_model);
    if mult > 0.0 && total_tokens > 0 {
        (total_tokens as f64 / 1000.0 * mult, true)
    } else {
        (0.0, false)
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------------------
// Chat Completions 处理
// ---------------------------------------------------------------------------

pub async fn handle_chat(ctx: Ctx, headers: HeaderMap, body: Vec<u8>) -> Response {
    let master_key = ctx.cfg.read().await.master_key.clone();
    let auth = match authenticate(&ctx, &headers, &master_key) {
        Ok(a) => a,
        Err(resp) => return resp,
    };

    let (bare_model, _realm) = parse_model_realm(&body);
    let cfg = ctx.cfg.read().await.clone();
    let upstream_model = cfg.resolve_model(&bare_model);
    let model_display = format!("cn:{bare_model}");
    let conv = parse_conv_id(&body);
    let session = super::request::ChatSession::with_headers(conv.as_deref(), &headers);

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                &format!("请求体不是合法 JSON: {e}"),
            )
        }
    };
    if !value.is_object() {
        return err_response(StatusCode::BAD_REQUEST, "请求体必须为 JSON 对象");
    }
    value["model"] = serde_json::json!(upstream_model.clone());
    let is_stream = value
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Err(error) = translate::prepare_upstream_chat(&mut value) {
        return err_response(StatusCode::BAD_REQUEST, &error);
    }

    let mut tried = std::collections::HashSet::new();
    let mut attempts: i64 = 0;
    let mut degraded_applied = false;
    let mut max_attempts = cfg
        .accounts
        .iter()
        .filter(|account| account.enabled && !account.paused)
        .count()
        .max(1);

    while attempts < max_attempts as i64 {
        attempts += 1;
        let Some(account) = ctx.pool.pick(&tried, conv.as_deref()).await else {
            return err_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "账号池没有可用账号（可能都在冷却中）",
            );
        };
        tried.insert(account.uid.clone());

        let meta = ReqMeta {
            request_id: reqlog::new_request_id(),
            path: "/v1/chat/completions".to_string(),
            model: model_display.clone(),
            uid: account.uid.clone(),
            account_label: account_label(&account),
            auth: AuthInfo {
                key: auth.key.clone(),
                key_value: auth.key_value.clone(),
                client_ip: auth.client_ip.clone(),
                user_agent: auth.user_agent.clone(),
            },
            attempts,
            accounted: AtomicBool::new(false),
        };

        let started = std::time::Instant::now();
        ctx.reqlog.begin();
        let mut pending = PendingAttempt {
            ctx: ctx.clone(),
            meta: &meta,
            started,
            usage: UsageObservation::default(),
            handed_off: false,
        };
        let resp = match send_chat(&ctx.client, &cfg.base_url, &account, &value, &session).await {
            Ok(r) => r,
            Err(e) => {
                let duration = started.elapsed().as_millis() as i64;
                finish_accounting(
                    &ctx,
                    &meta,
                    reqlog::OUTCOME_HTTP_ERROR,
                    0,
                    false,
                    duration,
                    None,
                    (0, 0, 0),
                    0.0,
                    false,
                )
                .await;
                ctx.pool
                    .note_error(&account.uid, ErrorKind::Other, &e.to_string())
                    .await;
                if attempts < max_attempts as i64 {
                    continue;
                }
                return err_response(StatusCode::BAD_GATEWAY, &format!("无法连接上游: {e}"));
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let duration = started.elapsed().as_millis() as i64;
            let kind = classify_upstream_error(status, &text);
            // 内容审核误报（11128 unapproved channel 等）：system 来源的指纹被上游
            // 逐字审核误杀，账号健康。换 Degraded 中性提示词同请求内重试一次。
            let mut degraded_this_attempt = false;
            if kind == ErrorKind::ContentBlocked && !degraded_applied {
                degraded_applied = true;
                degraded_this_attempt = true;
                translate::apply_degraded_prompt(&mut value);
                // 降级重试不计入轮转预算：单账号池（max_attempts=1）也拿得到这次机会。
                max_attempts += 1;
            }
            ctx.pool
                .note_error(&account.uid, kind, &format!("{status}"))
                .await;
            finish_accounting(
                &ctx,
                &meta,
                reqlog::OUTCOME_HTTP_ERROR,
                status.as_u16(),
                false,
                duration,
                None,
                (0, 0, 0),
                0.0,
                false,
            )
            .await;
            if degraded_this_attempt {
                // 单账号池也拿得到重试机会：把当前账号放回候选。
                tried.remove(&account.uid);
                continue;
            }
            if attempts < max_attempts as i64 {
                continue;
            }
            return err_response(
                status,
                &format!("WorkBuddy 上游返回 {status}: {}", truncate(&text, 200)),
            );
        }

        if !is_stream {
            // 聚合非流式：复用上游响应
            let chat = match super::probe::collect_chat_response(resp).await {
                Ok(chat) => chat,
                Err(error) => {
                    let duration = started.elapsed().as_millis() as i64;
                    ctx.pool
                        .note_error(
                            &account.uid,
                            classify_upstream_error(StatusCode::OK, &error),
                            &error,
                        )
                        .await;
                    finish_accounting(
                        &ctx,
                        &meta,
                        reqlog::OUTCOME_STREAM_ERROR,
                        200,
                        false,
                        duration,
                        None,
                        (0, 0, 0),
                        0.0,
                        false,
                    )
                    .await;
                    if attempts < max_attempts as i64 {
                        continue;
                    }
                    return err_response(
                        StatusCode::BAD_GATEWAY,
                        &format!("读取上游响应失败: {error}"),
                    );
                }
            };
            let duration = started.elapsed().as_millis() as i64;
            let tokens = usage_from_chat(&chat);
            if let Some(message) = business_error(&chat) {
                let kind = classify_upstream_error(StatusCode::OK, &message);
                ctx.pool.note_error(&account.uid, kind, &message).await;
                let (credit, known) = estimate_credit(&cfg, &upstream_model, tokens.tokens().2);
                finish_accounting(
                    &ctx,
                    &meta,
                    reqlog::OUTCOME_HTTP_ERROR,
                    200,
                    false,
                    duration,
                    None,
                    tokens,
                    credit,
                    known,
                )
                .await;
                if attempts < max_attempts as i64 {
                    continue;
                }
                return err_response(StatusCode::BAD_GATEWAY, &truncate(&message, 200));
            }
            let (credit, credit_known) = estimate_credit(&cfg, &upstream_model, tokens.tokens().2);
            finish_accounting(
                &ctx,
                &meta,
                reqlog::OUTCOME_SUCCESS,
                200,
                true,
                duration,
                None,
                tokens,
                credit,
                credit_known,
            )
            .await;
            let ct = "application/json";
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(chat.to_string()))
                .unwrap();
        }

        let prepared = match prepare_stream(resp, started, false, &mut pending).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let kind = classify_upstream_error(StatusCode::OK, &error.message);
                ctx.pool
                    .note_error(&account.uid, kind, &error.message)
                    .await;
                let (credit, known) =
                    estimate_credit(&cfg, &upstream_model, error.usage.tokens().2);
                finish_accounting(
                    &ctx,
                    &meta,
                    reqlog::OUTCOME_STREAM_ERROR,
                    200,
                    false,
                    started.elapsed().as_millis() as i64,
                    None,
                    error.usage,
                    credit,
                    known,
                )
                .await;
                if attempts < max_attempts as i64 {
                    continue;
                }
                return stream_error_response(false, &bare_model, &error.message);
            }
        };
        pending.handed_off = true;
        drop(pending);
        return spawn_chat_stream(ctx.clone(), meta, prepared, started, cfg, upstream_model).await;
    }

    err_response(StatusCode::SERVICE_UNAVAILABLE, "账号池没有可用账号")
}

async fn spawn_chat_stream(
    ctx: Ctx,
    meta: ReqMeta,
    prepared: PreparedStream,
    started: std::time::Instant,
    cfg: AppConfig,
    upstream_model: String,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(128);
    tokio::spawn(async move {
        let (mut stream, mut ttfb) = prepared.replay();
        let mut stats = StreamAccounting::default();
        let mut interrupted = false;
        while !stats.saw_error {
            let item = tokio::select! {
                biased;
                _ = tx.closed() => { interrupted = true; break; }
                item = stream.next() => item,
            };
            let Some(item) = item else {
                break;
            };
            match item {
                Ok(b) => {
                    stats.feed(&b);
                    if ttfb.is_none() && stats.saw_output {
                        ttfb = Some(started.elapsed().as_millis() as i64);
                    }
                    if tx.send(Ok(Bytes::from(b.to_vec()))).await.is_err() {
                        interrupted = true;
                        break;
                    }
                    if stats.saw_done {
                        break;
                    }
                }
                Err(e) => {
                    let duration = started.elapsed().as_millis() as i64;
                    ctx.pool
                        .note_error(&meta.uid, ErrorKind::Other, &e.to_string())
                        .await;
                    let (credit, known) =
                        estimate_credit(&cfg, &upstream_model, stats.usage.tokens().2);
                    finish_accounting(
                        &ctx,
                        &meta,
                        reqlog::OUTCOME_STREAM_ERROR,
                        0,
                        false,
                        duration,
                        ttfb,
                        stats.usage,
                        credit,
                        known,
                    )
                    .await;
                    let _ = tx
                        .send(Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            e.to_string(),
                        )))
                        .await;
                    return;
                }
            }
        }
        stats.finish();
        if ttfb.is_none() && stats.saw_output {
            ttfb = Some(started.elapsed().as_millis() as i64);
        }
        let duration = started.elapsed().as_millis() as i64;
        let ok = !interrupted && stats.completed();
        let outcome = if interrupted {
            reqlog::OUTCOME_INTERRUPTED
        } else if !ok {
            reqlog::OUTCOME_STREAM_ERROR
        } else {
            reqlog::OUTCOME_SUCCESS
        };
        let (credit, known) = estimate_credit(&cfg, &upstream_model, stats.usage.tokens().2);
        finish_accounting(
            &ctx,
            &meta,
            outcome,
            200,
            ok,
            duration,
            ttfb,
            stats.usage,
            credit,
            known,
        )
        .await;
        if !interrupted && !ok {
            let message = stats
                .error_message
                .as_deref()
                .unwrap_or("Upstream event stream failed or ended without a completion signal");
            ctx.pool
                .note_error(
                    &meta.uid,
                    classify_upstream_error(StatusCode::OK, message),
                    message,
                )
                .await;
            let error = serde_json::json!({"error": {
                "type": "upstream_stream_error", "code": "incomplete_stream",
                "message": "Upstream event stream failed or ended without a completion signal"
            }});
            let _ = tx
                .send(Ok(Bytes::from(format!("\n\ndata: {error}\n\n"))))
                .await;
        }
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap()
}

fn account_label(a: &crate::config::AccountConfig) -> String {
    let short: String = a.uid.chars().take(8).collect();
    format!("{}({short})", a.nickname)
}

// ---------------------------------------------------------------------------
// Responses 处理
// ---------------------------------------------------------------------------

pub async fn handle_responses(ctx: Ctx, headers: HeaderMap, body: Vec<u8>) -> Response {
    let master_key = ctx.cfg.read().await.master_key.clone();
    let auth = match authenticate(&ctx, &headers, &master_key) {
        Ok(a) => a,
        Err(resp) => return resp,
    };

    let (bare_model, _realm) = parse_model_realm(&body);
    let cfg = ctx.cfg.read().await.clone();
    let upstream_model = cfg.resolve_model(&bare_model);
    let model_display = format!("cn:{bare_model}");
    let conv = parse_conv_id(&body);
    let session = super::request::ChatSession::with_headers(conv.as_deref(), &headers);

    let mut chat_body = match translate::responses_to_chat(&body, &cfg) {
        Ok(v) => v,
        Err(e) => return err_response(StatusCode::BAD_REQUEST, &e),
    };
    let is_stream = chat_body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if let Err(error) = translate::prepare_upstream_chat(&mut chat_body) {
        return err_response(StatusCode::BAD_REQUEST, &error);
    }

    let mut tried = std::collections::HashSet::new();
    let max_attempts = cfg
        .accounts
        .iter()
        .filter(|account| account.enabled && !account.paused)
        .count()
        .max(1);

    for attempt in 1..=max_attempts {
        let Some(account) = ctx.pool.pick(&tried, conv.as_deref()).await else {
            return err_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "账号池没有可用账号（可能都在冷却中）",
            );
        };
        tried.insert(account.uid.clone());

        let meta = ReqMeta {
            request_id: reqlog::new_request_id(),
            path: "/v1/responses".to_string(),
            model: model_display.clone(),
            uid: account.uid.clone(),
            account_label: account_label(&account),
            auth: AuthInfo {
                key: auth.key.clone(),
                key_value: auth.key_value.clone(),
                client_ip: auth.client_ip.clone(),
                user_agent: auth.user_agent.clone(),
            },
            attempts: attempt as i64,
            accounted: AtomicBool::new(false),
        };

        let started = std::time::Instant::now();
        ctx.reqlog.begin();
        let mut pending = PendingAttempt {
            ctx: ctx.clone(),
            meta: &meta,
            started,
            usage: UsageObservation::default(),
            handed_off: false,
        };
        let resp = match send_chat(&ctx.client, &cfg.base_url, &account, &chat_body, &session).await
        {
            Ok(r) => r,
            Err(e) => {
                let duration = started.elapsed().as_millis() as i64;
                finish_accounting(
                    &ctx,
                    &meta,
                    reqlog::OUTCOME_HTTP_ERROR,
                    0,
                    false,
                    duration,
                    None,
                    (0, 0, 0),
                    0.0,
                    false,
                )
                .await;
                ctx.pool
                    .note_error(&account.uid, ErrorKind::Other, &e.to_string())
                    .await;
                if attempt < max_attempts {
                    continue;
                }
                return err_response(StatusCode::BAD_GATEWAY, &format!("无法连接上游: {e}"));
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let duration = started.elapsed().as_millis() as i64;
            let kind = classify_upstream_error(status, &text);
            ctx.pool
                .note_error(&account.uid, kind, &format!("{status}"))
                .await;
            finish_accounting(
                &ctx,
                &meta,
                reqlog::OUTCOME_HTTP_ERROR,
                status.as_u16(),
                false,
                duration,
                None,
                (0, 0, 0),
                0.0,
                false,
            )
            .await;
            if attempt < max_attempts {
                continue;
            }
            return err_response(
                status,
                &format!("WorkBuddy 上游返回 {status}: {}", truncate(&text, 200)),
            );
        }

        if !is_stream {
            let chat = match super::probe::collect_chat_response(resp).await {
                Ok(v) => v,
                Err(e) => {
                    let duration = started.elapsed().as_millis() as i64;
                    ctx.pool
                        .note_error(
                            &account.uid,
                            classify_upstream_error(StatusCode::OK, &e),
                            &e,
                        )
                        .await;
                    finish_accounting(
                        &ctx,
                        &meta,
                        reqlog::OUTCOME_STREAM_ERROR,
                        0,
                        false,
                        duration,
                        None,
                        (0, 0, 0),
                        0.0,
                        false,
                    )
                    .await;
                    if attempt < max_attempts {
                        continue;
                    }
                    return err_response(
                        StatusCode::BAD_GATEWAY,
                        &format!("解析上游响应失败: {e}"),
                    );
                }
            };
            let tokens = usage_from_chat(&chat);
            if let Some(message) = business_error(&chat) {
                let kind = classify_upstream_error(StatusCode::OK, &message);
                ctx.pool.note_error(&account.uid, kind, &message).await;
                let (credit, known) = estimate_credit(&cfg, &upstream_model, tokens.tokens().2);
                finish_accounting(
                    &ctx,
                    &meta,
                    reqlog::OUTCOME_HTTP_ERROR,
                    200,
                    false,
                    started.elapsed().as_millis() as i64,
                    None,
                    tokens,
                    credit,
                    known,
                )
                .await;
                if attempt < max_attempts {
                    continue;
                }
                return err_response(StatusCode::BAD_GATEWAY, &truncate(&message, 200));
            }
            let (credit, credit_known) = estimate_credit(&cfg, &upstream_model, tokens.tokens().2);
            let duration = started.elapsed().as_millis() as i64;
            finish_accounting(
                &ctx,
                &meta,
                reqlog::OUTCOME_SUCCESS,
                200,
                true,
                duration,
                None,
                tokens,
                credit,
                credit_known,
            )
            .await;
            let obj = translate::chat_to_responses_object(&chat, &cfg);
            return json_response(StatusCode::OK, obj);
        }

        let prepared = match prepare_stream(resp, started, true, &mut pending).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let kind = classify_upstream_error(StatusCode::OK, &error.message);
                ctx.pool
                    .note_error(&account.uid, kind, &error.message)
                    .await;
                let (credit, known) =
                    estimate_credit(&cfg, &upstream_model, error.usage.tokens().2);
                finish_accounting(
                    &ctx,
                    &meta,
                    reqlog::OUTCOME_STREAM_ERROR,
                    200,
                    false,
                    started.elapsed().as_millis() as i64,
                    None,
                    error.usage,
                    credit,
                    known,
                )
                .await;
                if attempt < max_attempts {
                    continue;
                }
                return stream_error_response(true, &bare_model, &error.message);
            }
        };
        pending.handed_off = true;
        drop(pending);
        return spawn_responses_stream(
            ctx.clone(),
            meta,
            prepared,
            started,
            upstream_model.clone(),
            &cfg,
            bare_model.clone(),
        )
        .await;
    }

    err_response(StatusCode::SERVICE_UNAVAILABLE, "账号池没有可用账号")
}

async fn spawn_responses_stream(
    ctx: Ctx,
    meta: ReqMeta,
    prepared: PreparedStream,
    started: std::time::Instant,
    upstream_model: String,
    cfg: &AppConfig,
    bare_model: String,
) -> Response {
    let mut state = translate::ResponsesStreamState::new(&bare_model);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(128);
    let upstream_model_c = upstream_model.clone();
    let cfg_c = cfg.clone();

    tokio::spawn(async move {
        let mut stats = StreamAccounting::default();
        let (mut stream, mut ttfb) = prepared.replay();
        let mut interrupted = false;
        let mut transport_error = false;
        let mut upstream_error = None;
        for line in translate::initial_events(&mut state) {
            if tx.send(Ok(Bytes::from(line))).await.is_err() {
                interrupted = true;
                break;
            }
        }
        if !interrupted && !stats.saw_error {
            'upstream: loop {
                let item = tokio::select! {
                    biased;
                    _ = tx.closed() => { interrupted = true; break; }
                    item = stream.next() => item,
                };
                let Some(item) = item else {
                    break;
                };
                let bytes = match item {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        transport_error = true;
                        ctx.pool
                            .note_error(&meta.uid, ErrorKind::Other, &error.to_string())
                            .await;
                        let _ = tx
                            .send(Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                error.to_string(),
                            )))
                            .await;
                        break;
                    }
                };
                let chunks = stats.feed(&bytes);
                if ttfb.is_none() && stats.saw_response_output {
                    ttfb = Some(started.elapsed().as_millis() as i64);
                }
                for chunk in chunks {
                    if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
                        upstream_error = Some(error.clone());
                        break 'upstream;
                    }
                    for message in translate::process_chunk(&chunk, &mut state) {
                        if tx.send(Ok(Bytes::from(message))).await.is_err() {
                            interrupted = true;
                            break 'upstream;
                        }
                    }
                }
                if stats.saw_error || stats.saw_done {
                    break;
                }
            }
        }
        if !interrupted && !transport_error {
            if upstream_error.is_none() {
                for chunk in stats.finish() {
                    if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
                        upstream_error = Some(error.clone());
                        break;
                    }
                    for message in translate::process_chunk(&chunk, &mut state) {
                        if tx.send(Ok(Bytes::from(message))).await.is_err() {
                            interrupted = true;
                            break;
                        }
                    }
                }
            }
            if !interrupted {
                let events = if !stats.completed() {
                    vec![responses_failed_event(&mut state, upstream_error.as_ref())]
                } else {
                    translate::final_events(&mut state)
                };
                for line in events {
                    if tx.send(Ok(Bytes::from(line))).await.is_err() {
                        interrupted = true;
                        break;
                    }
                }
            }
        }
        let duration = started.elapsed().as_millis() as i64;
        if ttfb.is_none() && stats.saw_response_output {
            ttfb = Some(started.elapsed().as_millis() as i64);
        }
        let ok = !interrupted && !transport_error && stats.completed();
        let outcome = if interrupted {
            reqlog::OUTCOME_INTERRUPTED
        } else if !ok {
            reqlog::OUTCOME_STREAM_ERROR
        } else {
            reqlog::OUTCOME_SUCCESS
        };
        let (credit, credit_known) =
            estimate_credit(&cfg_c, &upstream_model_c, stats.usage.tokens().2);
        finish_accounting(
            &ctx,
            &meta,
            outcome,
            if transport_error { 0 } else { 200 },
            ok,
            duration,
            ttfb,
            stats.usage,
            credit,
            credit_known,
        )
        .await;
        if !interrupted && !transport_error && !ok {
            let message = stats
                .error_message
                .as_deref()
                .unwrap_or("Upstream event stream failed or ended without a completion signal");
            ctx.pool
                .note_error(
                    &meta.uid,
                    classify_upstream_error(StatusCode::OK, message),
                    message,
                )
                .await;
        }
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap()
}

fn responses_failed_event(
    state: &mut translate::ResponsesStreamState,
    upstream_error: Option<&serde_json::Value>,
) -> String {
    let message = upstream_error
        .and_then(|error| {
            error
                .get("message")
                .and_then(|value| value.as_str())
                .or_else(|| error.as_str())
        })
        .unwrap_or("Upstream returned an invalid or failed event stream");
    let code = upstream_error
        .and_then(|error| error.get("code").and_then(|value| value.as_str()))
        .unwrap_or("upstream_error");
    translate::sse_msg(
        "response.failed",
        &serde_json::json!({
            "type": "response.failed",
            "sequence_number": state.next_seq(),
            "response": {
                "id": state.response_id,
                "object": "response",
                "status": "failed",
                "model": state.model,
                "output": [],
                "error": { "code": code, "message": message }
            }
        }),
    )
}

#[cfg(test)]
mod accounting_tests {
    use super::*;

    #[test]
    fn channel_policy_classification_requires_structured_code_and_matching_reason() {
        for code in [
            serde_json::json!(11128),
            serde_json::json!("11128"),
            serde_json::json!("11-128"),
        ] {
            for status in [
                StatusCode::BAD_REQUEST,
                StatusCode::FORBIDDEN,
                StatusCode::OK,
            ] {
                for message in [
                    "request illegal",
                    "This call failed channel verification",
                    "conversation id missing",
                ] {
                    let body = serde_json::json!({"code":code, "msg":message});
                    assert_eq!(
                        classify_upstream_error(status, &body.to_string()),
                        ErrorKind::ChannelRefused
                    );
                    let preserved = business_error(&body).unwrap();
                    assert!(serde_json::from_str::<serde_json::Value>(&preserved).is_ok());
                    assert_eq!(
                        classify_upstream_error(status, &preserved),
                        ErrorKind::ChannelRefused
                    );
                }
                let blocked = serde_json::json!({"error":{"code":code,"message":"request illegal: blocked by security policy"}});
                assert_eq!(
                    classify_upstream_error(status, &blocked.to_string()),
                    ErrorKind::ContentBlocked
                );
                assert_eq!(
                    classify_upstream_error(status, &business_error(&blocked).unwrap()),
                    ErrorKind::ContentBlocked
                );
            }
        }
        for body in [
            r#"{"code":111280,"msg":"request illegal"}"#,
            r#"{"code":11140,"msg":"request illegal"}"#,
            r#"{"code":14018,"msg":"request illegal"}"#,
            r#"{"msg":"request illegal code 11128"}"#,
            r#"{"requestId":"11128","msg":"request illegal"}"#,
            r#"{"code":11128,"msg":"unrelated rejection"}"#,
        ] {
            assert_eq!(
                classify_upstream_error(StatusCode::FORBIDDEN, body),
                ErrorKind::HardCredit,
                "{body}"
            );
            assert_eq!(
                classify_upstream_error(StatusCode::BAD_REQUEST, body),
                ErrorKind::Other,
                "{body}"
            );
        }
        assert_eq!(
            classify_upstream_error(
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"code":11140,"msg":"The model provider is rate-limiting requests."}"#
            ),
            ErrorKind::SoftRate
        );
        assert_eq!(
            classify_upstream_error(
                StatusCode::BAD_REQUEST,
                r#"{"code":14018,"msg":"credits exhausted"}"#
            ),
            ErrorKind::HardCredit
        );
    }

    #[test]
    fn nested_and_long_business_errors_keep_policy_fields_without_credential_fields() {
        let message = format!("{} request illegal", "x".repeat(600));
        let body = serde_json::json!({"code":10000, "msg":"request failed", "data":{
            "extError":{"code":"11-128","message":message},
            "accessToken":"must-not-appear", "refresh_token":"must-not-appear"
        }});
        let preserved = business_error(&body).unwrap();
        assert!(!preserved.contains("must-not-appear"));
        assert_eq!(
            classify_upstream_error(StatusCode::OK, &preserved),
            ErrorKind::ChannelRefused
        );
        let sibling_error = serde_json::json!({
            "error":{"message":"request failed"},
            "extError":{"code":"11-128","message":"request illegal"},
            "accessToken":"must-not-appear"
        });
        let preserved = business_error(&sibling_error).unwrap();
        assert!(!preserved.contains("must-not-appear"));
        assert_eq!(
            classify_upstream_error(StatusCode::OK, &preserved),
            ErrorKind::ChannelRefused
        );
    }
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    fn stream_test_context(base_url: String) -> Ctx {
        let account = serde_json::from_value(serde_json::json!({
            "uid": "stream-test", "nickname": "Test", "token": "test-token",
            "realm": "cn", "enabled": true, "paused": false
        }))
        .unwrap();
        let cfg = Arc::new(RwLock::new(AppConfig {
            base_url,
            accounts: vec![account],
            ..Default::default()
        }));
        Arc::new(GatewayState {
            cfg: cfg.clone(),
            pool: Pool::new(cfg),
            keys: Arc::new(apikey::Store::new_or_empty(String::new())),
            reqlog: reqlog::Recorder::new(reqlog::ArchiveConfig {
                enabled: false,
                dir: std::path::PathBuf::new(),
                retention_days: 1,
                max_bytes: 0,
            }),
            usage: Arc::new(UsageRecorder::new(String::new())),
            client: new_client().unwrap(),
        })
    }

    async fn accounting_upstream(body: Option<String>) -> String {
        accounting_upstream_content_type(body, "text/event-stream").await
    }

    async fn accounting_upstream_content_type(
        body: Option<String>,
        content_type: &'static str,
    ) -> String {
        let app = axum::Router::new().route(
            "/v2/chat/completions",
            axum::routing::post(move || {
                let body = body.clone();
                async move {
                    let body = match body {
                        Some(body) => Body::from(body),
                        None => {
                            // Headers and a comment arrive, then upstream remains silent forever.
                            let stream = futures_util::stream::once(async {
                                Ok::<_, std::io::Error>(Bytes::from_static(b": ready\n\n"))
                            })
                            .chain(futures_util::stream::pending());
                            Body::from_stream(stream)
                        }
                    };
                    Response::builder()
                        .header(header::CONTENT_TYPE, content_type)
                        .body(body)
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    async fn stream_request(ctx: Ctx, path: &str) -> String {
        let response = crate::gateway::build_router(ctx)
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"model":"workbuddy-fast","input":"ping","messages":[{"role":"user","content":"ping"}],"stream":true}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn empty_invalid_and_truncated_streams_fail_for_both_apis() {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for (content_type, body) in [
                ("text/event-stream", ""),
                ("application/json", r#"{"message":"upstream unavailable"}"#),
                ("application/json", "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"),
                ("text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"),
                ("text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\ndata: broken-json\n\ndata: [DONE]\n\n"),
                ("text/event-stream", "data: [DONE]\n\n"),
            ] {
                let base = accounting_upstream_content_type(Some(body.to_owned()), content_type).await;
                let ctx = stream_test_context(base);
                let wire = stream_request(ctx.clone(), path).await;
                if path == "/v1/responses" {
                    assert!(wire.contains("response.failed"), "{content_type}: {wire}");
                    assert!(!wire.contains("response.completed"), "{wire}");
                    assert!(!wire.contains("response.output_text.done"), "{wire}");
                } else {
                    assert!(wire.contains("upstream_stream_error"), "{wire}");
                }
                let usage = ctx.usage.snapshot();
                assert_eq!(usage.total_requests, 1);
                assert_eq!(usage.total_errors, 1, "{content_type}: {wire}");
                let logs = ctx.reqlog.snapshot();
                assert_eq!(logs.in_flight, 0);
                assert!(!logs.recent[0].ok);
                let accounts = ctx.cfg.read().await.accounts.clone();
                assert_eq!(ctx.pool.statuses(&accounts)[0].1.in_flight, 0);
            }
        }
    }

    #[tokio::test]
    async fn valid_terminal_signals_succeed_and_chat_preserves_wire_bytes() {
        for ending in [
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ] {
            let body = format!("{}{}{}", 
                "data: {\"choices\":[{\"delta\":{\"content\":\"complete\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n", ending);
            for path in ["/v1/chat/completions", "/v1/responses"] {
                let ctx = stream_test_context(accounting_upstream(Some(body.clone())).await);
                let wire = stream_request(ctx.clone(), path).await;
                if path == "/v1/chat/completions" {
                    assert_eq!(wire, body);
                } else {
                    assert!(wire.contains("response.completed"));
                    assert!(!wire.contains("response.failed"));
                }
                assert_eq!(ctx.usage.snapshot().total_errors, 0);
                assert_eq!(ctx.usage.snapshot().total_tokens, 12);
            }
        }
    }

    #[tokio::test]
    async fn role_frames_do_not_start_first_output_latency() {
        let app = axum::Router::new().route("/v2/chat/completions", axum::routing::post(|| async {
            let frames = [
                (0, "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n"),
                (160, "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n"),
                (40, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":20}}\n\ndata: [DONE]\n\n"),
            ];
            let stream = futures_util::stream::iter(frames).then(|(delay, frame)| async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                Ok::<_, std::io::Error>(Bytes::from_static(frame.as_bytes()))
            });
            Response::builder().header(header::CONTENT_TYPE, "text/event-stream").body(Body::from_stream(stream)).unwrap()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let ctx = stream_test_context(format!("http://{address}"));
            stream_request(ctx.clone(), path).await;
            let logs = ctx.reqlog.snapshot();
            assert!(
                logs.recent[0].ttfb_ms.unwrap() >= 140,
                "role-only frame must not count as output"
            );
            let dashboard = ctx.usage.dashboard(&crate::usage::UsageQuery::default());
            let generation_ms = logs.recent[0].duration_ms - logs.recent[0].ttfb_ms.unwrap();
            assert!(generation_ms > 0);
            assert_eq!(
                dashboard.avg_tokens_per_second,
                Some(20_000.0 / generation_ms as f64)
            );
        }
    }

    #[tokio::test]
    async fn tool_only_completion_succeeds_and_role_only_has_no_first_output() {
        for (delta, finish_reason, has_first_output) in [
            (
                serde_json::json!({"tool_calls":[{"index":0,"id":"call-1","function":{"name":"lookup","arguments":"{}"}}]}),
                "tool_calls",
                true,
            ),
            (serde_json::json!({"role":"assistant"}), "stop", false),
        ] {
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"delta":delta}]}),
                serde_json::json!({"choices":[{"delta":{},"finish_reason":finish_reason}]})
            );
            for path in ["/v1/chat/completions", "/v1/responses"] {
                let ctx = stream_test_context(accounting_upstream(Some(body.clone())).await);
                let wire = stream_request(ctx.clone(), path).await;
                if path == "/v1/responses" {
                    assert!(wire.contains("response.completed"));
                    if has_first_output {
                        assert!(wire.contains("response.function_call_arguments.delta"));
                    }
                }
                assert_eq!(ctx.usage.snapshot().total_errors, 0);
                assert_eq!(
                    ctx.reqlog.snapshot().recent[0].ttfb_ms.is_some(),
                    has_first_output
                );
            }
        }
    }

    #[test]
    fn output_and_completion_require_meaningful_frames() {
        let mut stats = StreamAccounting::default();
        stats.feed(b": ready\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: {\"usage\":{\"completion_tokens\":2}}\n\n");
        assert!(!stats.saw_output);
        assert!(!stats.completed());
        stats.feed(b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n");
        assert!(stats.saw_output);
        assert!(!stats.saw_response_output);
        stats.feed(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"tool-1\",\"function\":{\"name\":\"lookup\"}}]}}]}\n\n");
        assert!(stats.saw_response_output);
        stats.feed(b"data: [DONE]\n\n");
        assert!(stats.completed());
        let mut no_output = StreamAccounting::default();
        no_output.feed(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        assert!(no_output.completed());
        assert!(!no_output.saw_output);
    }

    #[tokio::test]
    async fn disconnect_during_silent_upstream_releases_chat_and_responses_accounts() {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let ctx = stream_test_context(accounting_upstream(None).await);
            let router = crate::gateway::build_router(ctx.clone());
            let request = tokio::spawn(async move {
                router.oneshot(
                    Request::post(path).header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"model":"workbuddy-fast","input":"ping","messages":[{"role":"user","content":"ping"}],"stream":true}"#)).unwrap()
                ).await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while ctx.reqlog.snapshot().in_flight == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("request must enter its pending stream preflight");
            assert_eq!(ctx.reqlog.snapshot().in_flight, 1);
            request.abort();
            let _ = request.await;
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    let accounts = ctx.cfg.read().await.accounts.clone();
                    if ctx.reqlog.snapshot().completed == 1
                        && ctx.pool.statuses(&accounts)[0].1.in_flight == 0
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("disconnect must release the account without waiting for another chunk");
            assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
            assert_eq!(
                ctx.reqlog.snapshot().recent[0].outcome,
                reqlog::OUTCOME_INTERRUPTED
            );
            assert_eq!(ctx.usage.snapshot().total_requests, 1);
            assert_eq!(ctx.usage.snapshot().total_errors, 1);
        }
    }

    #[tokio::test]
    async fn responses_error_frames_emit_failed_without_success_completion() {
        for ending in ["\n\n", ""] {
            let body = format!("{}{}{}{}", 
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
                "data: {\"error\":{\"message\":\"quota exhausted\",\"code\":\"insufficient_quota\"}}", ending);
            let ctx = stream_test_context(accounting_upstream(Some(body)).await);
            let response = crate::gateway::build_router(ctx.clone())
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
            let wire = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let wire = String::from_utf8(wire.to_vec()).unwrap();
            assert!(
                wire.contains("response.output_text.delta"),
                "previous text deltas must survive"
            );
            assert!(wire.contains("event: response.failed"));
            assert!(wire.contains("quota exhausted"));
            assert!(wire.contains("insufficient_quota"));
            assert!(!wire.contains("response.completed"));
            assert!(!wire.contains("response.output_text.done"));
            let snapshot = ctx.usage.snapshot();
            assert_eq!(snapshot.total_requests, 1);
            assert_eq!(snapshot.total_errors, 1);
            assert_eq!(snapshot.total_tokens, 12);
            assert_eq!(ctx.reqlog.snapshot().in_flight, 0);
        }
    }

    #[test]
    fn sse_split_utf8_last_usage_and_error_frames() {
        let body = concat!(
            ": keepalive\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\r\n\r\n",
            "data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"prompt_cache_miss_tokens\":25}}\n\n",
            "data: {\"usage\":{\"completion_tokens\":20}}\n\n",
            "data: {\"error\":{\"message\":\"rate limit\"}}"
        );
        let mut stats = StreamAccounting::default();
        let mut chunks = Vec::new();
        // Every byte boundary, including the middle of UTF-8 characters, must survive parsing.
        for byte in body.as_bytes() {
            chunks.extend(stats.feed(&[*byte]));
        }
        chunks.extend(stats.finish());
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "你好");
        assert_eq!(stats.usage.tokens(), (100, 20, 120));
        assert_eq!(stats.usage.cache_hit, 75);
        assert_eq!(stats.usage.cache_miss, 25);
        assert!(stats.saw_error);
        assert!(stats.saw_output);
    }

    #[test]
    fn missing_cache_is_distinct_from_explicit_zero_hit() {
        let absent = usage_from_chat(&serde_json::json!({"usage":{"prompt_tokens":100}}));
        assert!(!absent.cache_known);
        let zero = usage_from_chat(
            &serde_json::json!({"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":0}}}),
        );
        assert!(zero.cache_known);
        assert_eq!(zero.cache_miss, 100);
    }
}
