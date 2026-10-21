//! 直连 Claude 端点的出站整形
//!
//! `/v1/messages` 与 `/cc/v1/messages` 的响应在发给客户端之前经过这里，按官方
//! API 的键顺序重新序列化。serde_json 没开 `preserve_order`，`json!` 构造的对象
//! 一律按字母序输出（`content` 排在 `id` 前面），和真实 Claude 不一样。
//!
//! 只调整顺序，不增删任何字段：`usage` 里的 Kiro 计费字段（`credit_*`）和缓存
//! TTL 拆分都要原样到达客户端，下游计费系统靠它们核算成本。
//!
//! 非 JSON 的响应（如请求体解析失败时 axum 的纯文本拒绝）原样放行。

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::{Map, Value};

use super::types::ErrorResponse;

/// 消息对象（非流式响应体、`message_start.message`）。
const MESSAGE_ORDER: &[&str] = &[
    "id",
    "type",
    "role",
    "model",
    "content",
    "stop_reason",
    "stop_sequence",
    "usage",
];

const USAGE_ORDER: &[&str] = &[
    "input_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "cache_creation",
    "output_tokens",
    "server_tool_use",
    "service_tier",
];

/// 流式事件、内容块、delta 与错误对象：`type` 打头，其余按官方示例的先后。
const EVENT_ORDER: &[&str] = &[
    "type",
    "index",
    "id",
    "tool_use_id",
    "name",
    "input",
    "content_block",
    "delta",
    "message",
    "error",
    "text",
    "thinking",
    "signature",
    "data",
    "partial_json",
    "citation",
    "citations",
    "content",
    "stop_reason",
    "stop_sequence",
    "usage",
];

/// `usage.cache_creation`：按 TTL 拆分的缓存写入量。
const CACHE_CREATION_ORDER: &[&str] =
    &["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"];

const WEB_SEARCH_RESULT_ORDER: &[&str] = &["type", "title", "url", "encrypted_content", "page_age"];

const CITATION_ORDER: &[&str] = &["type", "cited_text", "url", "title", "encrypted_index"];

/// 按 Claude 的键序重写响应体。
pub(crate) async fn into_claude_wire(response: Response) -> Response {
    let (is_sse, is_json) = {
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        (
            content_type.starts_with("text/event-stream"),
            content_type.starts_with("application/json"),
        )
    };

    if is_sse {
        let (parts, body) = response.into_parts();
        return Response::from_parts(parts, Body::from_stream(rewrite_sse_body(body)));
    }
    if !is_json {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!("failed to read response body for Claude wire rewrite: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "api_error",
                    "Failed to read response body",
                )),
            )
                .into_response();
        }
    };
    let body = match rewrite_json(&bytes) {
        Some(rewritten) => Bytes::from(rewritten),
        None => bytes,
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(body))
}

fn rewrite_json(bytes: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    Some(to_claude_json(&value))
}

fn to_claude_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None);
    out
}

fn write_value(out: &mut String, value: &Value, key: Option<&str>) {
    match value {
        // 工具参数保持模型给出的内容不动
        _ if key == Some("input") => out.push_str(&value.to_string()),
        Value::Object(map) => write_object(out, map, key_order(map, key)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item, None);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn key_order(map: &Map<String, Value>, key: Option<&str>) -> &'static [&'static str] {
    match key {
        Some("usage") => return USAGE_ORDER,
        Some("cache_creation") => return CACHE_CREATION_ORDER,
        _ => {}
    }
    match map.get("type").and_then(Value::as_str) {
        Some("message") => MESSAGE_ORDER,
        Some("web_search_result") => WEB_SEARCH_RESULT_ORDER,
        Some("web_search_result_location") => CITATION_ORDER,
        _ => EVENT_ORDER,
    }
}

/// 先按 `order` 输出列出的键，未列出的键排在后面、保持原有顺序。
fn write_object(out: &mut String, map: &Map<String, Value>, order: &[&str]) {
    let listed = order
        .iter()
        .filter_map(|key| map.get(*key).map(|value| (*key, value)));
    let rest = map
        .iter()
        .filter(|(key, _)| !order.contains(&key.as_str()))
        .map(|(key, value)| (key.as_str(), value));

    out.push('{');
    for (i, (key, value)) in listed.chain(rest).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&Value::from(key).to_string());
        out.push(':');
        write_value(out, value, Some(key));
    }
    out.push('}');
}

fn rewrite_sse_body(body: Body) -> impl Stream<Item = Result<Bytes, axum::Error>> {
    stream::unfold(
        Some((body.into_data_stream(), SseRewriter::default())),
        |state| async move {
            let (mut body, mut rewriter) = state?;
            loop {
                match body.next().await {
                    Some(Ok(chunk)) => {
                        let out = rewriter.push(&chunk);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), Some((body, rewriter))));
                        }
                    }
                    Some(Err(e)) => return Some((Err(e), None)),
                    None => {
                        let rest = rewriter.finish();
                        return (!rest.is_empty()).then(|| (Ok(Bytes::from(rest)), None));
                    }
                }
            }
        },
    )
}

/// 逐个 SSE 事件重写；跨分片的事件攒齐再处理。
#[derive(Default)]
struct SseRewriter {
    pending: Vec<u8>,
}

impl SseRewriter {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self
            .pending
            .windows(2)
            .position(|window| window == b"\n\n")
            .map(|i| i + 2)
        {
            let event: Vec<u8> = self.pending.drain(..end).collect();
            out.extend(rewrite_sse_event(&event));
        }
        out
    }

    /// 流结束时剩下的不完整事件原样发出。
    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

fn rewrite_sse_event(event: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(event) else {
        return event.to_vec();
    };
    // ping 保持官方的 `{"type": "ping"}` 写法
    if text.starts_with("event: ping\n") {
        return event.to_vec();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (content, line_end) = match line.strip_suffix('\n') {
            Some(content) => (content, "\n"),
            None => (line, ""),
        };
        match content
            .strip_prefix("data: ")
            .and_then(|data| rewrite_json(data.as_bytes()))
        {
            Some(json) => {
                out.push_str("data: ");
                out.push_str(&json);
                out.push_str(line_end);
            }
            None => out.push_str(line),
        }
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::convert::Infallible;

    async fn body_string(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn json_message_keeps_all_usage_fields_and_uses_claude_key_order() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                { "type": "text", "text": "hi" },
                { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls", "cwd": "/" } }
            ],
            "model": "claude-sonnet-4-5",
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 2,
                "cache_creation": { "ephemeral_1h_input_tokens": 1, "ephemeral_5m_input_tokens": 2 },
                "credit_usage": 0.42,
                "credit_unit": "credit",
                "credit_unit_plural": "credits"
            }
        });
        let response = into_claude_wire(Json(body).into_response()).await;
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json"
        );
        assert_eq!(
            body_string(response).await,
            concat!(
                r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","#,
                r#""content":[{"type":"text","text":"hi"},"#,
                r#"{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls","cwd":"/"}}],"#,
                r#""stop_reason":"tool_use","stop_sequence":null,"#,
                r#""usage":{"input_tokens":10,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"#,
                r#""cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":1},"#,
                r#""output_tokens":5,"credit_unit":"credit","credit_unit_plural":"credits","#,
                r#""credit_usage":0.42}}"#
            )
        );
    }

    #[tokio::test]
    async fn json_error_keeps_status() {
        let response = (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", "bad")),
        )
            .into_response();
        let response = into_claude_wire(response).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_string(response).await;
        assert!(
            body.starts_with(
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"},"request_id":"req_"#
            ),
            "{body}"
        );
    }

    #[tokio::test]
    async fn non_json_response_passes_through() {
        let response = (StatusCode::UNSUPPORTED_MEDIA_TYPE, "plain text").into_response();
        let response = into_claude_wire(response).await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(body_string(response).await, "plain text");
    }

    #[tokio::test]
    async fn sse_events_are_rewritten_across_chunk_boundaries() {
        let message_start = json!({
            "type": "message_start",
            "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "content": [],
                "model": "claude-sonnet-4-5", "stop_reason": null, "stop_sequence": null,
                "usage": { "input_tokens": 10, "output_tokens": 1, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 }
            }
        });
        let message_delta = json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": {
                "input_tokens": 10, "output_tokens": 7,
                "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                "credit_usage": 0.42, "credit_unit": "credit", "credit_unit_plural": "credits"
            }
        });
        let raw = format!(
            "event: message_start\ndata: {message_start}\n\n\
             event: ping\ndata: {{\"type\": \"ping\"}}\n\n\
             event: content_block_start\ndata: {{\"content_block\":{{\"text\":\"\",\"type\":\"text\"}},\"index\":0,\"type\":\"content_block_start\"}}\n\n\
             event: message_delta\ndata: {message_delta}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        // 故意在事件中间切分
        let chunks: Vec<Result<Bytes, Infallible>> = raw
            .as_bytes()
            .chunks(37)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(Body::from_stream(stream::iter(chunks)))
            .unwrap();

        let response = into_claude_wire(response).await;
        assert_eq!(response.headers()["x-accel-buffering"], "no");
        assert_eq!(
            body_string(response).await,
            concat!(
                "event: message_start\n",
                r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}"#,
                "\n\n",
                "event: ping\ndata: {\"type\": \"ping\"}\n\n",
                "event: content_block_start\n",
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                "\n\n",
                "event: message_delta\n",
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":7,"credit_unit":"credit","credit_unit_plural":"credits","credit_usage":0.42}}"#,
                "\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            )
        );
    }

    #[test]
    fn incomplete_trailing_event_is_flushed_unchanged() {
        let mut rewriter = SseRewriter::default();
        let out = rewriter.push(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\nevent: err");
        assert_eq!(out, b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(rewriter.finish(), b"event: err");
    }

    #[test]
    fn web_search_blocks_follow_claude_order() {
        let block = json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_1",
            "content": [{
                "type": "web_search_result",
                "title": "Example",
                "url": "https://example.com",
                "encrypted_content": "abc",
                "page_age": null
            }]
        });
        assert_eq!(
            to_claude_json(&block),
            concat!(
                r#"{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"#,
                r#""type":"web_search_result","title":"Example","url":"https://example.com","#,
                r#""encrypted_content":"abc","page_age":null}]}"#
            )
        );
    }
}
