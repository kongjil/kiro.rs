//! Kiro 输出上限截断的代理内续写
//!
//! Kiro 上游对单次回复有输出长度上限，超过时直接结束响应。最常见的受害者是
//! 写大文件的工具调用：参数 JSON 写到一半流就结束了，这个调用既不能执行，也不能
//! 原样转发给客户端。
//!
//! 这里在代理内部处理：丢弃半截的调用，把已经吐出的正文作为 assistant 前文、再附一条
//! 「请分段重发」的说明，重新请求上游（最多 `MAX_TRUNCATION_RETRIES` 次）。续写的
//! 内容接在同一条回复里发给客户端，客户端只看到一次正常的回复。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::converter::{convert_request_with_mode, restore_tool_use_for_client};
use super::types::{Message, MessagesRequest};
use crate::admin::trace_db::TraceSink;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::provider::{KiroCallResult, KiroProvider};
use crate::model::config::ToolCompatibilityMode;

/// 同一个请求内最多续写几次。
pub(crate) const MAX_TRUNCATION_RETRIES: u8 = 2;

/// 被截断的工具调用（客户端可见的名字 + 能从半截参数里认出的目标路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TruncatedToolCall {
    pub name: String,
    pub path: Option<String>,
}

impl TruncatedToolCall {
    pub(crate) fn from_partial(
        kiro_name: &str,
        partial_input: &str,
        tool_name_map: &HashMap<String, String>,
    ) -> Self {
        let (name, _) = restore_tool_use_for_client(kiro_name, json!({}), tool_name_map);
        let path = ["file_path", "path", "notebook_path"]
            .iter()
            .find_map(|key| extract_partial_string_field(partial_input, key));
        Self { name, path }
    }
}

/// 从可能不完整的 JSON 里取出某个已经写完整的字符串字段。
fn extract_partial_string_field(partial: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut search_from = 0;
    while let Some(found) = partial[search_from..].find(&needle) {
        let after_key = search_from + found + needle.len();
        search_from = after_key;
        let rest = partial[after_key..].trim_start();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.starts_with('"') {
            continue;
        }
        let mut escaped = false;
        for (i, c) in rest.char_indices().skip(1) {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => {
                    return serde_json::from_str::<String>(&rest[..=i])
                        .ok()
                        .filter(|s| !s.is_empty());
                }
                _ => {}
            }
        }
        return None;
    }
    None
}

/// 发给上游的续写说明。
pub(crate) fn continuation_note(call: &TruncatedToolCall) -> String {
    let target = call
        .path
        .as_deref()
        .map(|path| format!(" for `{path}`"))
        .unwrap_or_default();
    let advice = match call.name.as_str() {
        "Write" | "fs_write" | "write_file" | "create_file" => {
            "Write only the first part of the file (no more than about 200 lines), then add the remaining parts with follow-up Edit calls of similar size."
        }
        "Edit" | "MultiEdit" | "str_replace" | "apply_patch" | "edit_file" => {
            "Split the change into several smaller edits, each touching a short, unique snippet."
        }
        "Bash" | "execute_bash" | "shell" | "exec_command" => {
            "If it needs a long script or heredoc, write the script to a file in several smaller steps first, then run it."
        }
        _ => "Split the work across several smaller calls so that each call's input stays short.",
    };
    format!(
        "Your previous {name} tool call{target} was cut off by the output length limit before its input was complete, so it was not executed. Re-issue it in smaller pieces. {advice} Do not apologize, mention this limit, or repeat earlier text; continue directly.",
        name = call.name,
    )
}

/// 续写的正文与之前已输出的正文之间的分隔：另起一段，已经换行的不重复补。
pub(crate) fn continuation_separator(last_char: Option<char>, next: &str) -> &'static str {
    match last_char {
        Some(_) if next.starts_with('\n') => "",
        Some('\n') => "\n",
        Some(_) => "\n\n",
        None => "",
    }
}

/// 把已吐出的正文作为 assistant 前文、续写说明作为 user 消息追加到请求末尾。
pub(crate) fn append_continuation(payload: &mut MessagesRequest, partial_text: &str, note: &str) {
    let partial_text = partial_text.trim();
    if !partial_text.is_empty() {
        match payload.messages.last_mut() {
            Some(last) if last.role == "assistant" => append_text_block(last, partial_text),
            _ => payload.messages.push(Message {
                role: "assistant".to_string(),
                content: json!([{ "type": "text", "text": partial_text }]),
            }),
        }
    }
    match payload.messages.last_mut() {
        Some(last) if last.role == "user" => append_text_block(last, note),
        _ => payload.messages.push(Message {
            role: "user".to_string(),
            content: json!([{ "type": "text", "text": note }]),
        }),
    }
}

fn append_text_block(message: &mut Message, text: &str) {
    let block = json!({ "type": "text", "text": text });
    match &mut message.content {
        Value::Array(blocks) => blocks.push(block),
        Value::String(existing) => {
            let existing = std::mem::take(existing);
            message.content = if existing.is_empty() {
                json!([block])
            } else {
                json!([{ "type": "text", "text": existing }, block])
            };
        }
        other => *other = json!([block]),
    }
}

/// 一次请求的续写上下文：持有原始请求，每次续写都在其末尾追加前文与说明。
pub(crate) struct TruncationRetry {
    provider: Arc<KiroProvider>,
    payload: MessagesRequest,
    mode: ToolCompatibilityMode,
    group: Option<String>,
    remaining: u8,
}

impl TruncationRetry {
    pub(crate) fn new(
        provider: Arc<KiroProvider>,
        payload: MessagesRequest,
        mode: ToolCompatibilityMode,
        group: Option<String>,
    ) -> Self {
        Self {
            provider,
            payload,
            mode,
            group,
            remaining: MAX_TRUNCATION_RETRIES,
        }
    }

    pub(crate) fn can_retry(&self) -> bool {
        self.remaining > 0
    }

    /// 发起一次续写请求。任何失败都返回 `None`，由调用方按 `max_tokens` 正常收尾。
    pub(crate) async fn resume(
        &mut self,
        truncated: &TruncatedToolCall,
        partial_text: &str,
        stream: bool,
        sink: Option<&dyn TraceSink>,
    ) -> Option<KiroCallResult> {
        if !self.can_retry() {
            return None;
        }
        self.remaining -= 1;
        tracing::info!(
            tool = %truncated.name,
            path = truncated.path.as_deref().unwrap_or(""),
            remaining = self.remaining,
            "tool call truncated by upstream output limit, re-requesting in smaller pieces"
        );

        append_continuation(&mut self.payload, partial_text, &continuation_note(truncated));
        // 续写的内容接在已发出的正文之后，不能再开新的 thinking 块。
        self.payload.thinking = None;
        self.payload.output_config = None;

        let conversion = match convert_request_with_mode(&self.payload, self.mode) {
            Ok(conversion) => conversion,
            Err(e) => {
                tracing::warn!("truncation retry: request conversion failed: {}", e);
                return None;
            }
        };
        let request = KiroRequest {
            conversation_state: conversion.conversation_state,
            profile_arn: None,
            additional_model_request_fields: conversion.additional_model_request_fields,
        };
        let body = match serde_json::to_string(&request) {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!("truncation retry: request serialization failed: {}", e);
                return None;
            }
        };

        let group = self.group.as_deref();
        let result = if stream {
            self.provider.call_api_stream(&body, sink, group).await
        } else {
            self.provider.call_api(&body, sink, group).await
        };
        match result {
            Ok(result) => Some(result),
            Err(e) => {
                tracing::warn!("truncation retry: upstream call failed: {:#}", e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(messages: Value) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": messages,
        }))
        .unwrap()
    }

    #[test]
    fn partial_path_is_read_from_incomplete_json() {
        let partial = r#"{"path": "/tmp/a \"b\".rs", "text": "fn main() {\n    println!("#;
        assert_eq!(
            extract_partial_string_field(partial, "path").as_deref(),
            Some("/tmp/a \"b\".rs")
        );
        assert_eq!(extract_partial_string_field(partial, "file_path"), None);
        // 路径本身也被截断时不给出半截路径
        assert_eq!(
            extract_partial_string_field(r#"{"path": "/tmp/ab"#, "path"),
            None
        );
        // 键名出现在内容里而不是键位置时跳过
        assert_eq!(
            extract_partial_string_field(r#"{"text": "\"path\" is", "path": "/x"}"#, "path")
                .as_deref(),
            Some("/x")
        );
    }

    #[test]
    fn truncated_call_restores_client_name() {
        let map = HashMap::from([("fs_write".to_string(), "Write".to_string())]);
        let call = TruncatedToolCall::from_partial("fs_write", r#"{"path":"/src/lib.rs","te"#, &map);
        assert_eq!(call.name, "Write");
        assert_eq!(call.path.as_deref(), Some("/src/lib.rs"));

        let note = continuation_note(&call);
        assert!(note.contains("Write tool call for `/src/lib.rs`"), "{note}");
        assert!(note.contains("follow-up Edit calls"), "{note}");
    }

    #[test]
    fn continuation_appends_partial_text_and_note() {
        let mut payload = request(json!([{ "role": "user", "content": "write the file" }]));
        append_continuation(&mut payload, "  I'll create it now.\n", "NOTE");

        assert_eq!(payload.messages.len(), 3);
        assert_eq!(payload.messages[1].role, "assistant");
        assert_eq!(
            payload.messages[1].content,
            json!([{ "type": "text", "text": "I'll create it now." }])
        );
        assert_eq!(payload.messages[2].role, "user");
        assert_eq!(
            payload.messages[2].content,
            json!([{ "type": "text", "text": "NOTE" }])
        );
    }

    #[test]
    fn continuation_without_text_extends_trailing_user_message() {
        let mut payload = request(json!([
            { "role": "user", "content": "go" },
            { "role": "assistant", "content": [{ "type": "tool_use", "id": "toolu_1", "name": "Read", "input": {} }] },
            { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" }] }
        ]));
        append_continuation(&mut payload, "   ", "NOTE");

        assert_eq!(payload.messages.len(), 3);
        let blocks = payload.messages[2].content.as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1], json!({ "type": "text", "text": "NOTE" }));

        // 字符串内容先转为块数组
        let mut payload = request(json!([{ "role": "user", "content": "go" }]));
        append_continuation(&mut payload, "", "NOTE");
        assert_eq!(
            payload.messages[0].content,
            json!([
                { "type": "text", "text": "go" },
                { "type": "text", "text": "NOTE" }
            ])
        );
    }

    #[test]
    fn repeated_continuations_keep_roles_alternating() {
        let mut payload = request(json!([{ "role": "user", "content": "go" }]));
        append_continuation(&mut payload, "part one", "NOTE 1");
        append_continuation(&mut payload, "part two", "NOTE 2");
        let roles: Vec<_> = payload.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant", "user"]);
        assert_eq!(payload.messages[4].content[0]["text"], "NOTE 2");
    }
}
