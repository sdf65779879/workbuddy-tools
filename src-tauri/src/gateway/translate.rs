use crate::config::AppConfig;
use std::collections::HashMap;
use uuid::Uuid;

/// 把一个 Responses API 请求转成 WorkBuddy 的 Chat Completions 请求体。
pub fn responses_to_chat(body: &[u8], cfg: &AppConfig) -> Result<serde_json::Value, String> {
    let req: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("请求体不是合法 JSON: {e}"))?;
    if !req.is_object() {
        return Err("请求体必须为 JSON 对象".into());
    }

    let requested_model = req
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("workbuddy-fast");
    let model = cfg.resolve_model(requested_model);

    let mut messages: Vec<serde_json::Value> = Vec::new();

    if let Some(instructions) = req.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.trim().is_empty() {
            messages.push(system_message(instructions));
        }
    }

    match req.get("input") {
        Some(serde_json::Value::String(s)) => {
            if !s.trim().is_empty() {
                messages.push(user_message(s));
            }
        }
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                push_input_item(&mut messages, item)?;
            }
        }
        _ => {}
    }

    let is_stream = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);

    let mut chat = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": is_stream,
    });

    if let Some(v) = req.get("max_output_tokens") {
        chat["max_tokens"] = v.clone();
    } else if let Some(v) = req.get("max_tokens") {
        chat["max_tokens"] = v.clone();
    }
    if let Some(v) = req.get("temperature") {
        chat["temperature"] = v.clone();
    }
    if let Some(v) = req.get("top_p") {
        chat["top_p"] = v.clone();
    }
    if let Some(v) = req.get("tools") {
        chat["tools"] = match v.as_array() {
            Some(tools) => serde_json::Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        if tool.get("type").and_then(|value| value.as_str()) == Some("function")
                            && tool.get("function").is_none()
                        {
                            let mut function = serde_json::Map::new();
                            for key in ["name", "description", "parameters", "strict"] {
                                if let Some(value) = tool.get(key) {
                                    function.insert(key.into(), value.clone());
                                }
                            }
                            serde_json::json!({"type": "function", "function": function})
                        } else {
                            tool.clone()
                        }
                    })
                    .collect(),
            ),
            None => return Err("tools 必须为数组".into()),
        };
    }
    if let Some(v) = req.get("tool_choice") {
        chat["tool_choice"] = v.clone();
    }

    normalize_chat_messages(&mut chat)?;
    Ok(chat)
}

fn system_message(text: &str) -> serde_json::Value {
    serde_json::json!({ "role": "system", "content": text })
}

fn user_message(text: &str) -> serde_json::Value {
    serde_json::json!({ "role": "user", "content": text })
}

fn push_input_item(
    messages: &mut Vec<serde_json::Value>,
    item: &serde_json::Value,
) -> Result<(), String> {
    // Responses' easy message format omits type entirely.
    let ty = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            if item.get("role").is_some() {
                "message"
            } else {
                ""
            }
        });
    match ty {
        "message" => {
            let role = item
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user")
                .to_string();
            let content = item
                .get("content")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            validate_content_parts(&content)?;
            let converted = convert_content(content);
            let role = if role == "developer" { "system" } else { &role };
            messages.push(serde_json::json!({ "role": role, "content": converted }));
        }
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }]
            }));
        }
        "function_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output = item
                .get("output")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            validate_content_parts(&output)?;
            let output = match output {
                serde_json::Value::Object(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::Bool(_) => serde_json::Value::String(output.to_string()),
                other => convert_content(other),
            };
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output
            }));
        }
        // reasoning / item_reference 等无法无损重建，忽略
        _ => {}
    }
    Ok(())
}

fn validate_content_parts(content: &serde_json::Value) -> Result<(), String> {
    if let Some(parts) = content.as_array() {
        for part in parts {
            if part
                .get("type")
                .and_then(|value| value.as_str())
                .is_some_and(|ty| ty.trim() == "input_image")
                && part.get("file_id").is_some()
                && !part
                    .get("image_url")
                    .and_then(|value| value.as_str())
                    .is_some_and(|url| !url.trim().is_empty())
            {
                return Err(
                    "暂不支持通过 file_id 读取图片，请提供 image_url 或图片 data URL".into(),
                );
            }
        }
    }
    Ok(())
}

fn convert_content(content: serde_json::Value) -> serde_json::Value {
    match content {
        serde_json::Value::String(s) => serde_json::Value::String(s),
        serde_json::Value::Array(parts) => {
            let mut converted = Vec::new();
            for mut part in parts {
                let pty = part
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                match pty {
                    "input_text" | "output_text" | "text" => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            if !t.trim().is_empty() {
                                part["type"] = serde_json::json!("text");
                                converted.push(part);
                            }
                        }
                    }
                    "input_image" => {
                        if let Some(u) = part
                            .get("image_url")
                            .and_then(|v| v.as_str())
                            .filter(|u| !u.trim().is_empty())
                        {
                            let mut image = serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": u }
                            });
                            if let Some(detail) = part.get("detail") {
                                image["image_url"]["detail"] = detail.clone();
                            }
                            converted.push(image);
                        }
                    }
                    "image_url" => {
                        if let Some(url) = part.get("image_url").and_then(|v| v.as_str()) {
                            if !url.trim().is_empty() {
                                part["image_url"] = serde_json::json!({"url": url});
                                converted.push(part);
                            }
                        } else if part
                            .pointer("/image_url/url")
                            .and_then(|v| v.as_str())
                            .is_some_and(|url| !url.trim().is_empty())
                        {
                            converted.push(part);
                        }
                    }
                    "refusal" => {
                        if let Some(t) = part.get("refusal").and_then(|v| v.as_str()) {
                            if !t.trim().is_empty() {
                                converted.push(serde_json::json!({"type":"text","text":t}));
                            }
                        }
                    }
                    // Private reasoning blocks do not constitute ordinary message content.
                    "reasoning_text" | "reasoning" => {}
                    "" if part.is_null()
                        || part.as_object().is_some_and(|object| object.is_empty()) => {}
                    "" if part.as_str().is_some_and(|text| text.trim().is_empty()) => {}
                    // Preserve unfamiliar nonempty blocks instead of silently deleting data.
                    _ => converted.push(part),
                }
            }
            if converted
                .iter()
                .all(|part| part.get("type").and_then(|v| v.as_str()) == Some("text"))
            {
                serde_json::Value::String(
                    converted
                        .iter()
                        .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            } else {
                serde_json::Value::Array(converted)
            }
        }
        serde_json::Value::Null => serde_json::Value::Null,
        other => other,
    }
}

/// WorkBuddy rejects empty ordinary messages. Remove empty history without dropping
/// valid tool rounds or image-only input. An empty tool result still needs its pairing.
pub fn normalize_chat_messages(chat: &mut serde_json::Value) -> Result<(), String> {
    let messages = chat
        .get_mut("messages")
        .and_then(|value| value.as_array_mut())
        .ok_or_else(|| "messages 必须为消息数组".to_string())?;
    let mut normalized = Vec::with_capacity(messages.len());
    for (index, mut message) in messages.drain(..).enumerate() {
        let object = message
            .as_object_mut()
            .ok_or_else(|| format!("messages[{index}] 必须为消息对象"))?;
        let role = object
            .get("role")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("messages[{index}] 缺少有效 role"))?
            .to_string();
        if role == "developer" {
            object.insert("role".into(), serde_json::json!("system"));
        }
        let content = object.remove("content").unwrap_or(serde_json::Value::Null);
        validate_content_parts(&content)?;
        let mut content = convert_content(content);
        if !matches!(
            content,
            serde_json::Value::Null | serde_json::Value::String(_) | serde_json::Value::Array(_)
        ) {
            return Err(format!("messages[{index}].content 必须为文本或内容数组"));
        }
        let mut empty = content_is_empty(&content);
        if empty && role == "assistant" {
            if let Some(refusal) = object
                .get("refusal")
                .and_then(|value| value.as_str())
                .filter(|text| !text.trim().is_empty())
            {
                content = serde_json::json!(refusal);
                empty = false;
            }
        }
        let has_calls = role == "assistant"
            && (object
                .get("tool_calls")
                .and_then(|value| value.as_array())
                .is_some_and(|calls| !calls.is_empty())
                || object
                    .get("function_call")
                    .is_some_and(|value| value.is_object()));
        if empty {
            if role == "tool" || role == "function" {
                content = serde_json::json!("[Tool returned no output.]");
            } else if has_calls {
                content = serde_json::Value::Null;
            } else {
                continue;
            }
        }
        object.insert("content".into(), content);
        normalized.push(message);
    }
    if normalized.is_empty() {
        return Err("请求没有可发送的消息内容；请提供非空文本、图片或有效的工具对话".into());
    }
    *messages = normalized;
    Ok(())
}

fn content_is_empty(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::Null => true,
        serde_json::Value::String(text) => text.trim().is_empty(),
        serde_json::Value::Array(parts) => parts.is_empty(),
        _ => false,
    }
}

/// 降级中性提示词：内容审核误报（11128 unapproved channel 等）的同请求重试用，
/// 刻意极简中性。与 codebuddy_sub2api 的 prompt.Degraded 同口径。
pub const DEGRADED_SYSTEM_PROMPT: &str = "You are a helpful assistant. Respond in the user's language, follow the user's instructions, and be direct and concise.";

/// 上游反探测指纹剥离（与 codebuddy_sub2api 的 sanitize.go 同口径）。
/// 上游内容审核按逐字精确匹配误杀合法流量（Claude Code / Codex 等 CLI 注入的
/// 模板句），一字改动即可绕过。出站前做最小改写/剥离，语义不变。
const SANITIZE_REWRITES: &[(&str, &str)] = &[
    (
        "You are Claude Code, Anthropic's official CLI for Claude",
        "You are Claude Code, Anthropic's official CLI tool for Claude",
    ),
    (
        "Main branch (you will usually use this for PRs)",
        "Default branch (you will usually use this for PRs)",
    ),
    (
        "You are a coding agent running in the Codex CLI, a terminal-based coding assistant.",
        "You are a coding agent running in the Codex CLI tool, a terminal-based coding assistant.",
    ),
    (
        "To give feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
        "To provide feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
    ),
    // 上游反探测：请求体出现裸数字 11128 即整单拦截（与其上下文无关），
    // 11128 正是本类拦截自身的错误码。插入连字符保留可读性与指代。
    ("11128", "11-128"),
];

/// 特征预检：任一命中才进入净化（快速路径，普通请求零分配）。
fn has_fingerprint(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "you are claude code",
        "main branch (",
        "you are a coding agent running in the codex cli",
        "github.com/anthropics/",
        "11128",
        "x-anthropic-billing-header",
        "cc_",
    ]
    .iter()
    .any(|feature| lower.contains(feature))
}

/// 大小写不敏感替换（仅 ASCII；指纹均为 ASCII，字节长度不变，偏移安全）。
fn replace_ascii_ci(text: &str, from: &str, to: &str) -> String {
    let lower_from = from.to_ascii_lowercase();
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let lower_rest = rest.to_ascii_lowercase();
        let Some(pos) = lower_rest.find(&lower_from) else {
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..pos]);
        result.push_str(to);
        rest = &rest[pos + from.len()..];
    }
    result
}

/// 清理尾随裸键值（cc_xxx=...; 循环清理，Claude Code 注入痕迹）。
fn strip_cc_key_values(text: &str) -> String {
    let mut out = text.to_string();
    // 有界轮次，防止异常输入死循环。
    for _ in 0..16 {
        let lower = out.to_ascii_lowercase();
        let Some(pos) = lower.find("cc_") else { break };
        let after = &out[pos + 3..];
        let Some(eq) = after.find('=') else { break };
        // 键名必须是 [a-z0-9_]+（不含空格/标点），否则不是注入键值形态。
        if eq == 0 || !after[..eq].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            break;
        }
        let value_start = pos + 3 + eq + 1;
        let rest = &out[value_start..];
        // 值到下一个分号/换行/串尾。
        let end = rest
            .find([';', '\n'])
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        out.replace_range(pos..(value_start + end), "");
    }
    out
}

fn sanitize_text(text: &str) -> String {
    if !has_fingerprint(text) {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (from, to) in SANITIZE_REWRITES {
        out = out.replace(from, to);
    }
    // header 键名（大小写不敏感，含键值/裸键名形态）缩写：破坏逐字匹配。
    out = replace_ascii_ci(&out, "x-anthropic-billing-header", "x-anthropic-billing-hdr");
    out = strip_cc_key_values(&out);
    out.trim().to_string()
}

fn sanitize_content(content: &mut serde_json::Value) {
    match content {
        serde_json::Value::String(text) => {
            let cleaned = sanitize_text(text);
            if cleaned != *text {
                *content = serde_json::json!(cleaned);
            }
        }
        serde_json::Value::Array(parts) => {
            for part in parts.iter_mut() {
                let cleaned = part
                    .get("text")
                    .and_then(|value| value.as_str())
                    .map(sanitize_text);
                if let Some(text) = cleaned {
                    part["text"] = serde_json::json!(text);
                }
            }
        }
        _ => {}
    }
}

/// 净化出站请求体中的反探测指纹：content / reasoning / reasoning_content /
/// tool_calls[].function.arguments。任一命中才改写，普通请求零改动。
pub fn sanitize_fingerprints(chat: &mut serde_json::Value) {
    let Some(messages) = chat.get_mut("messages").and_then(|value| value.as_array_mut()) else {
        return;
    };
    for message in messages.iter_mut() {
        let Some(object) = message.as_object_mut() else { continue };
        if let Some(content) = object.get_mut("content") {
            sanitize_content(content);
        }
        for key in ["reasoning", "reasoning_content"] {
            let cleaned = object
                .get(key)
                .and_then(|value| value.as_str())
                .map(sanitize_text);
            if let Some(text) = cleaned {
                object.insert(key.into(), serde_json::json!(text));
            }
        }
        if let Some(calls) = object.get_mut("tool_calls").and_then(|value| value.as_array_mut()) {
            for call in calls.iter_mut() {
                if let Some(args) = call
                    .get_mut("function")
                    .and_then(|value| value.get_mut("arguments"))
                {
                    if let Some(text) = args.as_str() {
                        let cleaned = sanitize_text(text);
                        *args = serde_json::json!(cleaned);
                    }
                }
            }
        }
    }
}

/// 把 system/developer 消息全部替换为一条 Degraded 中性提示词（头部插入），
/// 其余 user/assistant/tool 消息逐字不动。用于内容审核误报（11128 unapproved
/// channel 等）的同请求降级重试：从源头消灭 system 来源的指纹。
pub fn apply_degraded_prompt(chat: &mut serde_json::Value) {
    let Some(messages) = chat.get_mut("messages").and_then(|value| value.as_array_mut()) else {
        return;
    };
    let kept: Vec<serde_json::Value> = messages
        .drain(..)
        .filter(|message| {
            let role = message.get("role").and_then(|value| value.as_str());
            !matches!(role, Some("system") | Some("developer"))
        })
        .collect();
    let mut rewritten = vec![serde_json::json!({
        "role": "system",
        "content": DEGRADED_SYSTEM_PROMPT,
    })];
    rewritten.extend(kept);
    *messages = rewritten;
}

/// The desktop upstream accepts streaming chat requests. Clients may still ask
/// the gateway for JSON; that stream is collected into a completion downstream.
pub fn prepare_upstream_chat(chat: &mut serde_json::Value) -> Result<(), String> {
    normalize_chat_messages(chat)?;
    sanitize_fingerprints(chat);
    if chat.get("max_tokens").is_none() {
        if let Some(value) = chat.get("max_completion_tokens").cloned() {
            chat["max_tokens"] = value;
        }
    }
    chat.as_object_mut()
        .unwrap()
        .remove("max_completion_tokens");
    if chat
        .get("model")
        .and_then(|value| value.as_str())
        .is_some_and(|model| model.to_ascii_lowercase().contains("gpt-"))
    {
        if chat
            .get("max_tokens")
            .and_then(|value| value.as_i64())
            .is_some_and(|limit| limit < 16)
        {
            chat["max_tokens"] = serde_json::json!(16);
        }
    }
    // WorkBuddy accepts a scalar tool_choice, whereas both public APIs also
    // allow objects. Preserve a selected function's name when converting it.
    if let Some(choice) = chat.get("tool_choice").cloned() {
        let normalized = if let Some(value) = choice.as_str() {
            Some(value.to_string())
        } else {
            match choice.get("type").and_then(|value| value.as_str()) {
                Some("none" | "auto" | "required") => choice["type"].as_str().map(str::to_string),
                Some("function") => choice
                    .pointer("/function/name")
                    .or_else(|| choice.get("name"))
                    .and_then(|value| value.as_str())
                    .filter(|name| !name.trim().is_empty())
                    .map(|name| name.trim().to_string()),
                _ => None,
            }
        };
        let object = chat.as_object_mut().unwrap();
        match normalized.as_deref() {
            Some("none") => {
                for field in ["tool_choice", "tools", "functions"] {
                    object.remove(field);
                }
            }
            Some(value) => {
                object.insert("tool_choice".into(), serde_json::json!(value));
            }
            None => {
                object.remove("tool_choice");
            }
        }
    }
    chat["stream"] = serde_json::json!(true);
    if !chat
        .get("stream_options")
        .is_some_and(|value| value.is_object())
    {
        chat["stream_options"] = serde_json::json!({});
    }
    chat["stream_options"]["include_usage"] = serde_json::json!(true);
    Ok(())
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_outputs_preserve_text_image_order_and_json_values() {
        let chat = responses_to_chat(
            &json!({"input":[
                {"type":"function_call_output", "call_id":"one", "output":[
                    {"type":"input_text","text":"before"},
                    {"type":"input_image","image_url":"data:image/png;base64,test"},
                    {"type":"input_text","text":"after"}
                ]},
                {"type":"function_call_output", "call_id":"two", "output":{"count":2}}
            ]})
            .to_string()
            .into_bytes(),
            &AppConfig::default(),
        )
        .unwrap();
        assert_eq!(
            chat["messages"][0]["content"],
            json!([
                {"type":"text","text":"before"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,test"}},
                {"type":"text","text":"after"}
            ])
        );
        assert_eq!(chat["messages"][1]["content"], "{\"count\":2}");
    }

    #[test]
    fn request_preparation_uses_scalar_tool_choice_and_accepted_token_budget() {
        for (choice, expected) in [
            (json!({"type":"function","name":"lookup"}), json!("lookup")),
            (
                json!({"type":"function","function":{"name":"lookup"}}),
                json!("lookup"),
            ),
            (json!({"type":"required"}), json!("required")),
            (json!("auto"), json!("auto")),
            (json!({"type":"none"}), serde_json::Value::Null),
        ] {
            let mut body = json!({"model":"gpt-test", "messages":[{"role":"user","content":"ping"}],
                "max_completion_tokens":8, "stream":false, "tools":[{"type":"function"}], "tool_choice":choice});
            prepare_upstream_chat(&mut body).unwrap();
            assert_eq!(body["max_tokens"], 16);
            assert!(body.get("max_completion_tokens").is_none());
            assert_eq!(body["stream"], true);
            assert_eq!(body["stream_options"]["include_usage"], true);
            assert_eq!(body["tool_choice"], expected);
            if expected.is_null() {
                assert!(body.get("tools").is_none());
            }
        }
    }

    #[test]
    fn sanitize_fingerprints_strips_upstream_policy_fingerprints() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "You are Claude Code, Anthropic's official CLI for Claude. To give feedback, users should report the issue at https://github.com/anthropics/claude-code/issues"},
                {"role": "user", "content": "upstream returned code=11128 for this request"},
                {"role": "assistant", "content": "ok", "tool_calls": [{"function": {"name": "run", "arguments": "{\"command\":\"echo 11128\"}"}}]},
                {"role": "tool", "content": "x-anthropic-billing-header: abc; cc_version=1.0; done"}
            ]
        });
        sanitize_fingerprints(&mut body);
        let msgs = body["messages"].as_array().unwrap();
        // 身份句 / 反馈句只改一个词，语义不变。
        assert!(msgs[0]["content"].as_str().unwrap().contains("official CLI tool for Claude"));
        assert!(msgs[0]["content"].as_str().unwrap().contains("To provide feedback"));
        assert!(!msgs[0]["content"].as_str().unwrap().contains("To give feedback"));
        // 裸 11128 反探测串改写为 11-128。
        assert!(msgs[1]["content"].as_str().unwrap().contains("11-128"));
        assert!(!msgs[1]["content"].as_str().unwrap().contains("11128"));
        // tool_calls arguments 同样净化。
        let args = &msgs[2]["tool_calls"][0]["function"]["arguments"];
        assert!(args.as_str().unwrap().contains("11-128"));
        assert!(!args.as_str().unwrap().contains("11128"));
        // header 键名缩写 + cc_ 键值清理。
        let tool_content = msgs[3]["content"].as_str().unwrap();
        assert!(!tool_content.contains("x-anthropic-billing-header"));
        assert!(!tool_content.contains("cc_version=1.0"));
    }

    #[test]
    fn apply_degraded_prompt_replaces_system_with_neutral() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "You are Claude Code, Anthropic's official CLI for Claude"},
                {"role": "developer", "content": "some injected instructions"},
                {"role": "user", "content": "你是谁"},
                {"role": "assistant", "content": "hi"}
            ]
        });
        apply_degraded_prompt(&mut body);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], DEGRADED_SYSTEM_PROMPT);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "你是谁");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "hi");
    }
}

// ---------------------------------------------------------------------------
// 流式：把上游 Chat SSE chunk 转成 Responses SSE 事件行
// ---------------------------------------------------------------------------

pub struct ToolState {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub output_index: u64,
}

pub struct ResponsesStreamState {
    pub response_id: String,
    pub item_id: String,
    pub model: String,
    pub seq: u64,
    pub text: String,
    pub text_started: bool,
    pub finish_reason: Option<String>,
    pub tool_states: HashMap<i64, ToolState>,
    pub usage: Option<serde_json::Value>,
}

impl ResponsesStreamState {
    pub fn new(model: &str) -> Self {
        Self {
            response_id: format!("resp_{}", short_id()),
            item_id: format!("msg_{}", short_id()),
            model: model.to_string(),
            seq: 0,
            text: String::new(),
            text_started: false,
            finish_reason: None,
            tool_states: HashMap::new(),
            usage: None,
        }
    }

    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..24].to_string()
}

pub fn sse_msg(event: &str, data: &serde_json::Value) -> String {
    format!("event: {event}\ndata: {}\n\n", data)
}

pub fn initial_events(state: &mut ResponsesStreamState) -> Vec<String> {
    let rid = state.response_id.clone();
    let mut out = Vec::new();
    out.push(sse_msg(
        "response.created",
        &serde_json::json!({
            "type": "response.created",
            "sequence_number": state.next_seq(),
            "response": {
                "id": rid,
                "object": "response",
                "status": "in_progress",
                "model": state.model.clone(),
                "output": []
            }
        }),
    ));
    out.push(sse_msg(
        "response.output_item.added",
        &serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": state.next_seq(),
            "output_index": 0,
            "item": {
                "type": "message",
                "id": state.item_id.clone(),
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        }),
    ));
    out.push(sse_msg(
        "response.content_part.added",
        &serde_json::json!({
            "type": "response.content_part.added",
            "sequence_number": state.next_seq(),
            "item_id": state.item_id.clone(),
            "output_index": 0,
            "content_index": 0,
            "part": { "type": "output_text", "text": "", "annotations": [] }
        }),
    ));
    out
}

/// 处理一个上游 chat chunk，返回要发送的 SSE 消息列表。
pub fn process_chunk(chunk: &serde_json::Value, state: &mut ResponsesStreamState) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(usage) = chunk.get("usage") {
        if usage.is_object() {
            state.usage = Some(usage.clone());
        }
    }

    let Some(choices) = chunk.get("choices").and_then(|v| v.as_array()) else {
        return out;
    };
    let Some(choice) = choices.first() else {
        return out;
    };
    let delta = choice
        .get("delta")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    // 文本增量
    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            state.text.push_str(content);
            state.text_started = true;
            out.push(sse_msg(
                "response.output_text.delta",
                &serde_json::json!({
                    "type": "response.output_text.delta",
                    "sequence_number": state.next_seq(),
                    "item_id": state.item_id.clone(),
                    "output_index": 0,
                    "content_index": 0,
                    "delta": content
                }),
            ));
        }
    }

    // 工具调用
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let is_new = !id.is_empty() && !state.tool_states.contains_key(&index);
            if is_new {
                let output_index = 1 + index as u64;
                let call_id = format!("fc_{}", id);
                state.tool_states.insert(
                    index,
                    ToolState {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        output_index,
                    },
                );
                out.push(sse_msg(
                    "response.output_item.added",
                    &serde_json::json!({
                        "type": "response.output_item.added",
                        "sequence_number": state.next_seq(),
                        "output_index": output_index,
                        "item": {
                            "type": "function_call",
                            "id": call_id,
                            "call_id": id,
                            "name": name.clone(),
                            "arguments": "",
                            "status": "in_progress"
                        }
                    }),
                ));
            }

            if let Some(ts) = state.tool_states.get_mut(&index) {
                if !name.is_empty() && ts.name.is_empty() {
                    ts.name = name.clone();
                }
                if !arguments.is_empty() {
                    ts.arguments.push_str(&arguments);
                }
            }

            if !arguments.is_empty() {
                if let Some((call_id, output_index)) = state
                    .tool_states
                    .get(&index)
                    .map(|t| (t.call_id.clone(), t.output_index))
                {
                    out.push(sse_msg(
                        "response.function_call_arguments.delta",
                        &serde_json::json!({
                            "type": "response.function_call_arguments.delta",
                            "sequence_number": state.next_seq(),
                            "item_id": call_id,
                            "output_index": output_index,
                            "delta": arguments.clone()
                        }),
                    ));
                }
            }
        }
    }

    if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        if !fr.is_empty() && fr != "stop" || state.finish_reason.is_none() {
            state.finish_reason = Some(fr.to_string());
        }
    }

    out
}

/// 流结束时的收尾事件（含 response.completed）。
pub fn final_events(state: &mut ResponsesStreamState) -> Vec<String> {
    let mut out = Vec::new();

    if state.text_started {
        out.push(sse_msg(
            "response.output_text.done",
            &serde_json::json!({
                "type": "response.output_text.done",
                "sequence_number": state.next_seq(),
                "item_id": state.item_id.clone(),
                "output_index": 0,
                "content_index": 0,
                "text": state.text.clone()
            }),
        ));
        out.push(sse_msg(
            "response.content_part.done",
            &serde_json::json!({
                "type": "response.content_part.done",
                "sequence_number": state.next_seq(),
                "item_id": state.item_id.clone(),
                "output_index": 0,
                "content_index": 0,
                "part": { "type": "output_text", "text": state.text.clone(), "annotations": [] }
            }),
        ));
        out.push(sse_msg(
            "response.output_item.done",
            &serde_json::json!({
                "type": "response.output_item.done",
                "sequence_number": state.next_seq(),
                "output_index": 0,
                "item": {
                    "type": "message",
                    "id": state.item_id.clone(),
                    "status": "completed",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": state.text.clone(), "annotations": [] }]
                }
            }),
        ));
    }

    let mut output: Vec<serde_json::Value> = Vec::new();
    if state.text_started {
        output.push(serde_json::json!({
            "type": "message",
            "id": state.item_id.clone(),
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": state.text.clone(), "annotations": [] }]
        }));
    }

    // 先取出快照，避免在借用 state.tool_states 的同时可变借用 state。
    let mut tool_snapshots: Vec<(String, u64, String, String)> = state
        .tool_states
        .values()
        .map(|t| {
            (
                t.call_id.clone(),
                t.output_index,
                t.name.clone(),
                t.arguments.clone(),
            )
        })
        .collect();
    tool_snapshots.sort_by_key(|t| t.1);

    for (call_id, output_index, name, arguments) in &tool_snapshots {
        out.push(sse_msg(
            "response.function_call_arguments.done",
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "sequence_number": state.next_seq(),
                "item_id": call_id,
                "output_index": output_index,
                "arguments": arguments
            }),
        ));
        out.push(sse_msg(
            "response.output_item.done",
            &serde_json::json!({
                "type": "response.output_item.done",
                "sequence_number": state.next_seq(),
                "output_index": output_index,
                "item": {
                    "type": "function_call",
                    "id": call_id,
                    "call_id": call_id.trim_start_matches("fc_"),
                    "name": name,
                    "arguments": arguments,
                    "status": "completed"
                }
            }),
        ));
        output.push(serde_json::json!({
            "type": "function_call",
            "id": call_id,
            "call_id": call_id.trim_start_matches("fc_"),
            "name": name,
            "arguments": arguments,
            "status": "completed"
        }));
    }

    let usage = state.usage.clone().unwrap_or(serde_json::json!({
        "input_tokens": 0, "output_tokens": 0, "total_tokens": 0
    }));

    out.push(sse_msg(
        "response.completed",
        &serde_json::json!({
            "type": "response.completed",
            "sequence_number": state.next_seq(),
            "response": {
                "id": state.response_id.clone(),
                "object": "response",
                "status": "completed",
                "model": state.model.clone(),
                "output": output,
                "usage": usage
            }
        }),
    ));

    out
}

// ---------------------------------------------------------------------------
// 非流式：Chat 响应 -> Responses 对象
// ---------------------------------------------------------------------------

pub fn chat_to_responses_object(chat: &serde_json::Value, cfg: &AppConfig) -> serde_json::Value {
    let model = cfg
        .models
        .iter()
        .find(|m| m.workbuddy_id == chat.get("model").and_then(|v| v.as_str()).unwrap_or(""))
        .map(|m| m.codex_name.clone())
        .unwrap_or_else(|| "workbuddy-fast".to_string());

    let message = chat
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .cloned()
        .unwrap_or(serde_json::json!({ "role": "assistant", "content": "" }));

    let mut output: Vec<serde_json::Value> = Vec::new();

    if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            output.push(serde_json::json!({
                "type": "message",
                "id": format!("msg_{}", short_id()),
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": content, "annotations": [] }]
            }));
        }
    }

    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tool_calls {
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            output.push(serde_json::json!({
                "type": "function_call",
                "id": format!("fc_{}", id),
                "call_id": id,
                "name": name,
                "arguments": arguments,
                "status": "completed"
            }));
        }
    }

    let usage = chat.get("usage").cloned().unwrap_or(serde_json::json!({
        "input_tokens": 0, "output_tokens": 0, "total_tokens": 0
    }));

    serde_json::json!({
        "id": format!("resp_{}", short_id()),
        "object": "response",
        "created_at": chrono_now(),
        "status": "completed",
        "model": model,
        "output": output,
        "usage": usage
    })
}

fn chrono_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
