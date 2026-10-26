//! Anthropic API Handler 函数

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Instant;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{
    SharedTraceStore, TraceAttempt, TraceKeySource, TraceRecord, TraceRoute, TraceSink, outcome,
    usage_source,
};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder, UsageRecord};
use crate::kiro::model::available_models::{TokenLimits, UpstreamModel};
use crate::kiro::model::events::{Event, TokenUsage};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::token_manager::ModelDiscoveryError;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Extension, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::billing::BillingOutcome;
use super::converter::{convert_request_with_mode, get_context_window_size};
use super::middleware::{AppState, KeyContext};
use super::stream::{SseEvent, StreamContext};
use super::truncation::TruncationRetry;
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

/// 请求结束时记录用量的钩子
///
/// 在 handler 入口构造，调用 [`Self::record`] 时把当次请求的 input/output token、
/// 命中的上游凭据 ID、状态写入：
/// - `usage_log.YYYY-MM-DD.jsonl`（持久化历史）
/// - 内存聚合器（仪表盘趋势）
/// - 客户端 Key 计数（按 Key 累计）
#[derive(Clone)]
pub(crate) struct UsageRecordHook {
    pub recorder: Option<SharedRecorder>,
    pub aggregator: Option<SharedAggregator>,
    pub client_keys: Option<SharedClientKeyManager>,
    pub key_id: u64,
    pub model: String,
    pub started_at: Instant,
    pub billing_prices: std::sync::Arc<std::collections::HashMap<String, crate::model::config::BillingPrice>>,
    pub token_manager: Option<std::sync::Arc<crate::kiro::token_manager::MultiTokenManager>>,
}

impl UsageRecordHook {
    pub fn from_state(state: &AppState, key_id: u64, model: String) -> Self {
        Self {
            recorder: state.usage_recorder.clone(),
            aggregator: state.usage_aggregator.clone(),
            client_keys: state.client_keys.clone(),
            key_id,
            model,
            started_at: Instant::now(),
            billing_prices: state.billing_prices.clone(),
            token_manager: state
                .kiro_provider
                .as_ref()
                .map(|provider| provider.token_manager().clone()),
        }
    }

    /// 记录一次没有计费策略参与的用量（错误早退等场景），原始与计费后相同。
    pub fn record(
        &self,
        credential_id: u64,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_tokens: i32,
        cache_read_tokens: i32,
        credits: f64,
        status: &str,
    ) {
        let usage = TokenUsage {
            uncached_input_tokens: input_tokens,
            output_tokens,
            cache_write_input_tokens: cache_creation_tokens,
            cache_read_input_tokens: cache_read_tokens,
        };
        let credits = if credits.is_finite() && credits > 0.0 { credits } else { 0.0 };
        self.record_outcome(
            credential_id,
            &BillingOutcome {
                raw: usage.sanitized(),
                raw_credits: credits,
                billed: usage.sanitized(),
                billed_credits: credits,
            },
            status,
        );
    }

    /// 记录一次结算结果：原始与计费后两套口径一起落盘。
    pub fn record_outcome(&self, credential_id: u64, outcome: &BillingOutcome, status: &str) {
        let raw = outcome.raw.sanitized();
        let billed = outcome.billed.sanitized();
        let sell_multiplier = self
            .client_keys
            .as_ref()
            .map(|manager| manager.sell_multiplier_of(self.key_id))
            .unwrap_or(1.0);
        let cost_multiplier = self
            .token_manager
            .as_ref()
            .and_then(|manager| manager.clone_credential(credential_id))
            .and_then(|credential| {
                credential
                    .metadata
                    .extra
                    .get("_billing_cost_multiplier")
                    .and_then(serde_json::Value::as_f64)
            })
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(1.0);
        let rec = UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: self.key_id,
            credential_id,
            model: self.model.clone(),
            input_tokens: billed.uncached_input_tokens as u64,
            output_tokens: billed.output_tokens as u64,
            cache_creation_tokens: billed.cache_write_input_tokens as u64,
            cache_read_tokens: billed.cache_read_input_tokens as u64,
            credits: outcome.billed_credits,
            raw_input_tokens: raw.uncached_input_tokens as u64,
            raw_output_tokens: raw.output_tokens as u64,
            raw_cache_creation_tokens: raw.cache_write_input_tokens as u64,
            raw_cache_read_tokens: raw.cache_read_input_tokens as u64,
            raw_credits: outcome.raw_credits,
            account_cost: outcome.raw_credits * cost_multiplier,
            sell_consumption: outcome.billed_credits * sell_multiplier,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            status: status.to_string(),
        };
        if let Some(r) = &self.recorder {
            r.record(&rec);
        }
        if let Some(a) = &self.aggregator {
            a.ingest(&rec);
        }
        if status == "success" && self.key_id != 0 {
            if let Some(m) = &self.client_keys {
                m.record_usage(
                    self.key_id,
                    rec.input_tokens,
                    rec.output_tokens,
                    rec.cache_creation_tokens,
                    rec.cache_read_tokens,
                    rec.credits,
                );
            }
        }
    }
}

/// 单次请求的链路追踪器
///
/// 在 handler 入口构造，作为 [`TraceSink`] 传入 provider；provider 在重试循环里
/// 每跳调用 [`on_attempt`](TraceSink::on_attempt) 累积一条 [`TraceAttempt`]。
/// 请求结束时调用 [`Self::finalize`] 组装 [`TraceRecord`] 并写入 SQLite。
///
/// `store` 为 None（未启用 Admin / trace）时所有方法都是空操作，零开销。
pub(crate) struct RequestTracer {
    store: Option<SharedTraceStore>,
    trace_id: String,
    ts: String,
    key_id: u64,
    key_source: TraceKeySource,
    client_ip: Option<String>,
    model: String,
    is_stream: bool,
    started_at: Instant,
    /// 首个上游 chunk 到达时刻（仅流式标记；取第一次）
    first_token_at: parking_lot::Mutex<Option<Instant>>,
    attempts: parking_lot::Mutex<Vec<TraceAttempt>>,
    /// 首次选号的路由决策。web_search 一条 trace 内多次 provider 调用，
    /// 「是否沿用了上一轮账号」只看第一次。
    route: parking_lot::Mutex<Option<TraceRoute>>,
    /// 是否已落库。[`finalize`](Self::finalize) 首次进入即置位，保证同一条 trace
    /// 只写一行；[`Drop`] 兜底时据此判断「本请求从未正常收尾」。
    finalized: std::sync::atomic::AtomicBool,
    /// provider 成功返回 `reqwest::Response`，即已收到上游 HTTP 响应头。
    /// 不能用「尚未 finalize」代替这个状态：下游可能在读取 body 前取消。
    upstream_headers_received: std::sync::atomic::AtomicBool,
}

/// usage 三项的来源，落到 trace 行便于区分「上游真值」与「本地估算」。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum UsageSource {
    /// 错误早退等无用量场景
    #[default]
    Unknown,
    /// 上游 metadataEvent.tokenUsage
    Provider,
    /// 本地 CacheMeter 按断点估算
    Simulated,
    /// 无断点 / 计量关闭
    None,
}

impl UsageSource {
    fn as_db(self) -> Option<&'static str> {
        match self {
            Self::Unknown => Option::None,
            Self::Provider => Some(usage_source::PROVIDER),
            Self::Simulated => Some(usage_source::SIMULATED),
            Self::None => Some(usage_source::NONE),
        }
    }

    /// 由「上游是否给了精确用量」与「本地模拟是否覆盖到前缀」推断来源。
    pub fn resolve(has_provider_usage: bool, cache_usage: &super::cache_metering::CacheUsage) -> Self {
        if has_provider_usage {
            Self::Provider
        } else if cache_usage.cache_covered_est > 0 {
            Self::Simulated
        } else {
            Self::None
        }
    }
}

/// 本次请求的用量快照（落入 trace 行，与 usage_log 同源）。
///
/// 顶层字段是计费后口径（下游看到的值），`raw` 是未应用 Key 计费策略的原始口径。
#[derive(Clone, Copy, Default)]
pub(crate) struct TraceUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub credits: f64,
    pub raw: crate::admin::trace_db::TraceRawUsage,
    pub source: UsageSource,
}

impl TraceUsage {
    /// 错误早退等无用量场景
    pub fn zero() -> Self {
        Self::default()
    }

    pub fn from_outcome(outcome: &BillingOutcome, source: UsageSource) -> Self {
        let tokens = |v: i32| v.max(0) as u64;
        let billed = outcome.billed;
        let raw = outcome.raw;
        Self {
            input_tokens: tokens(billed.uncached_input_tokens),
            output_tokens: tokens(billed.output_tokens),
            cache_creation_tokens: tokens(billed.cache_write_input_tokens),
            cache_read_tokens: tokens(billed.cache_read_input_tokens),
            credits: outcome.billed_credits,
            raw: crate::admin::trace_db::TraceRawUsage {
                input_tokens: tokens(raw.uncached_input_tokens),
                output_tokens: tokens(raw.output_tokens),
                cache_creation_tokens: tokens(raw.cache_write_input_tokens),
                cache_read_tokens: tokens(raw.cache_read_input_tokens),
                credits: outcome.raw_credits,
            },
            source,
        }
    }
}

struct RequestTraceOptions {
    key_ctx: KeyContext,
    model: String,
    is_stream: bool,
}

impl RequestTracer {
    fn new(state: &AppState, options: RequestTraceOptions) -> Self {
        Self {
            store: state.trace_store.clone(),
            trace_id: Uuid::new_v4().to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: options.key_ctx.key_id,
            key_source: options.key_ctx.key_source,
            client_ip: options.key_ctx.client_ip,
            model: options.model,
            is_stream: options.is_stream,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 标记 provider 已成功返回响应头。应在取得 `reqwest::Response` 后立刻调用，
    /// 在创建/轮询下游 response body 之前完成。
    pub(super) fn mark_upstream_headers_received(&self) {
        self.upstream_headers_received
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 标记首个可发送的内容到达（幂等，仅记录第一次）。
    pub fn mark_first_token(&self) {
        if !self.is_stream {
            return;
        }
        let mut slot = self.first_token_at.lock();
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }

    fn mark_first_content(&self, events: &[SseEvent]) {
        if events.iter().any(|event| match event.event.as_str() {
            "content_block_start" => {
                event.data.pointer("/content_block/type")
                    .and_then(serde_json::Value::as_str) == Some("tool_use")
            }
            "content_block_delta" => ["text", "thinking", "partial_json"].iter().any(|field| {
                event.data.get("delta")
                    .and_then(|delta| delta.get(*field))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            }),
            _ => false,
        }) {
            self.mark_first_token();
        }
    }

    /// 组装并落库一条完整链路。store 为 None 时不做任何事。
    pub fn finalize(
        &self,
        final_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
        usage: TraceUsage,
    ) {
        // 幂等：首次进入置位，防止显式收尾后 Drop 兜底再写一次（insert 按 trace_id 覆盖）。
        if self
            .finalized
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let Some(store) = &self.store else { return };
        let attempts = std::mem::take(&mut *self.attempts.lock());
        // 最终凭据：最后一跳的命中凭据（成功跳即命中凭据，失败跳即最后尝试的凭据）
        let final_credential_id = attempts.last().map(|a| a.credential_id).unwrap_or(0);
        let first_token_ms = self
            .first_token_at
            .lock()
            .map(|t| t.duration_since(self.started_at).as_millis() as u64);
        let route = self.route.lock().take();
        let rec = TraceRecord {
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            key_id: self.key_id,
            key_source: self.key_source,
            model: self.model.clone(),
            is_stream: self.is_stream,
            final_status: final_status.to_string(),
            final_credential_id,
            error_type: error_type.map(|s| s.to_string()),
            error_message: error_message.map(|s| s.to_string()),
            total_attempts: attempts.len() as u32,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            interrupted_after_bytes,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            credits: usage.credits,
            raw_input_tokens: Some(usage.raw.input_tokens),
            raw_output_tokens: Some(usage.raw.output_tokens),
            raw_cache_creation_tokens: Some(usage.raw.cache_creation_tokens),
            raw_cache_read_tokens: Some(usage.raw.cache_read_tokens),
            raw_credits: Some(usage.raw.credits),
            first_token_ms,
            session_id: route.as_ref().and_then(|r| r.session_id.clone()),
            sticky_outcome: route.as_ref().map(|r| r.sticky_outcome.to_string()),
            previous_credential_id: route.as_ref().and_then(|r| r.previous_credential_id),
            usage_source: usage.source.as_db().map(|s| s.to_string()),
            client_ip: self.client_ip.clone(),
            attempts,
        };
        store.insert(&rec);
    }
}

/// 兜底落库：请求被取消、走不到任何显式 finalize 时补一行。
///
/// `upstream_headers_received=false` 才表示请求仍在等待上游响应头；若 headers 已到
/// 但 body 尚未被读取/收尾，则应归为流中断，不能误报为等头超时。
///
/// 只记录可观测到的事实（头未到达即被取消），不推断原因：此处无法区分前置 SLB 等头超时、
/// 客户端读超时、用户主动中断、客户端进程退出等——它们在这一层长得完全一样。原因需结合
/// `duration_ms`（如贴近 SLB 的等头超时值）与下游日志判断。
///
/// 已正常收尾的请求已置位 `finalized`，幂等守卫使此处成为空操作，不会双写或覆盖。
impl Drop for RequestTracer {
    fn drop(&mut self) {
        let headers_received = self
            .upstream_headers_received
            .load(std::sync::atomic::Ordering::SeqCst);
        let (error_type, message) = if headers_received {
            (
                outcome::STREAM_INTERRUPTED,
                "request was cancelled after upstream response headers arrived",
            )
        } else {
            (
                outcome::ABANDONED_BEFORE_HEADERS,
                "request was cancelled while awaiting upstream response headers",
            )
        };
        // final_status 复用 "interrupted"（前端归入「中断」徽章），靠 error_type 区分具体类型。
        self.finalize(
            "interrupted",
            Some(error_type),
            Some(message),
            None,
            TraceUsage::zero(),
        );
    }
}

impl TraceSink for RequestTracer {
    fn on_attempt(&self, mut attempt: TraceAttempt) {
        let mut attempts = self.attempts.lock();
        // Each provider call numbers retries from zero. A web-search request can make
        // several provider calls under one trace, so assign a request-wide sequence
        // before persisting to the (trace_id, attempt) primary key.
        attempt.attempt = attempts.len() as u32;
        attempts.push(attempt);
    }

    fn on_route(&self, route: TraceRoute) {
        let mut slot = self.route.lock();
        if slot.is_none() {
            *slot = Some(route);
        }
    }
}

/// 取追踪器里最后一跳的 outcome（用于把 provider 的失败分类提升到 record.error_type）。
/// 返回 'static str（outcome 常量），无 attempt 时返回 None。
pub(crate) fn last_attempt_outcome(tracer: &RequestTracer) -> Option<&'static str> {
    let last = tracer.attempts.lock().last()?.outcome.clone();
    Some(canonical_attempt_outcome(&last))
}

fn canonical_attempt_outcome(value: &str) -> &'static str {
    match value {
        outcome::QUOTA_EXHAUSTED => outcome::QUOTA_EXHAUSTED,
        outcome::ACCOUNT_THROTTLED => outcome::ACCOUNT_THROTTLED,
        outcome::ACCOUNT_SUSPENDED => outcome::ACCOUNT_SUSPENDED,
        outcome::AUTH_FAILED => outcome::AUTH_FAILED,
        outcome::TRANSIENT => outcome::TRANSIENT,
        outcome::NETWORK_ERROR => outcome::NETWORK_ERROR,
        outcome::BAD_REQUEST => outcome::BAD_REQUEST,
        _ => outcome::UNKNOWN,
    }
}

/// Image-budget warning threshold (in raw base64 chars, not decoded bytes).
/// Emits a warning when the total base64 char count of all image content in one request exceeds this threshold.
/// The threshold does not reject the request (the upstream makes the final call); it only gives operators more precise diagnostics.
const IMAGE_BUDGET_WARN_BYTES: usize = 800 * 1024;

/// Budget statistics for the image content in one inbound request.
struct ImageBudget {
    count: usize,
    total_b64_bytes: usize,
    largest_b64_bytes: usize,
}

/// Counts the total number of images in the payload and their base64 byte size.
/// Looks only at inline base64 (image source.type == "base64"), skipping url-mode images (which do not
/// go directly into a Bedrock single message body). This is a lightweight O(N) scan that does not decode base64.
fn count_image_budget(payload: &super::types::MessagesRequest) -> ImageBudget {
    let mut count = 0usize;
    let mut total = 0usize;
    let mut largest = 0usize;
    for msg in &payload.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                if item.get("type").and_then(|v| v.as_str()) != Some("image") {
                    continue;
                }
                let Some(src) = item.get("source") else {
                    continue;
                };
                if src.get("type").and_then(|v| v.as_str()) != Some("base64") {
                    continue;
                }
                let n = src
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                count += 1;
                total += n;
                if n > largest {
                    largest = n;
                }
            }
        }
    }
    ImageBudget {
        count,
        total_b64_bytes: total,
        largest_b64_bytes: largest,
    }
}

/// 上下文超限时的错误消息。与官方 `prompt is too long: ...` 同前缀——Claude Code
/// 按这个前缀识别上下文超限并提示 / 触发压缩，自定义文案会让它当成普通 API 错误。
const PROMPT_TOO_LONG_MESSAGE: &str =
    "prompt is too long: the request exceeds the model's context window";

/// 将 KiroProvider 错误映射为 HTTP 响应
pub(super) fn map_provider_error(err: Error) -> Response {
    if err
        .downcast_ref::<crate::kiro::error::UpstreamTimeoutError>()
        .is_some()
    {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            Json(ErrorResponse::new("timeout_error", "Request timed out.")),
        )
            .into_response();
    }
    if let Some(unavailable) =
        err.downcast_ref::<crate::kiro::error::UpstreamModelUnavailableError>()
    {
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("overloaded_error", "Overloaded")),
        )
            .into_response();
        if let Ok(value) = unavailable.retry_after.parse() {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    if err
        .downcast_ref::<crate::kiro::error::UpstreamContextOverflowError>()
        .is_some()
    {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                PROMPT_TOO_LONG_MESSAGE,
            )),
        )
            .into_response();
    }

    if let Some(rate_limit) = err.downcast_ref::<crate::kiro::error::UpstreamRateLimitError>() {
        tracing::warn!(error = %err, "上游限流（映射为 429）");
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new(
                "rate_limit_error",
                "Rate limit exceeded. Please retry later.",
            )),
        )
            .into_response();
        if let Some(value) = rate_limit
            .retry_after()
            .and_then(|value| value.parse::<header::HeaderValue>().ok())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }

    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                PROMPT_TOO_LONG_MESSAGE,
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                PROMPT_TOO_LONG_MESSAGE,
            )),
        )
            .into_response();
    }

    // Bedrock client-side validation errors (tool_use <-> tool_result mismatch, invalid message sequence, etc.)
    // The root cause is the client's own messages array, not an upstream failure, so it must not map to 5xx
    // otherwise it triggers an upstream cooldown that amplifies one client error into a 30+ burst of 503s.
    // Detection is centralized in the endpoint layer (single source of truth for the markers); the provider
    // already bails out without retry on these, and this mapping is the client-facing safety net.
    if crate::kiro::endpoint::default_is_client_validation_error(&err_str) {
        tracing::warn!(
            error = %err,
            "client messages array violates the protocol (Bedrock validation; mapped to 400 to avoid a false cooldown)"
        );
        // Return a stable, client-facing message and avoid echoing the raw upstream
        // error string (which can carry request IDs or internal validation details).
        // The full error is already logged above for diagnostics.
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Invalid message sequence: tool_use and tool_result blocks must be correctly paired and ordered.".to_string(),
            )),
        )
            .into_response();
    }

    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new("api_error", "Internal server error")),
    )
        .into_response()
}

/// 解析普通非流式响应的原始 usage（未应用 Key 计费策略）。
///
/// 精确 provider 快照优先；缺失时才使用 contextUsage/输入估算和本地 CacheMeter 分摊。
fn resolve_raw_non_stream_usage(
    fallback_total_input_tokens: i32,
    context_total_input_tokens: Option<i32>,
    fallback_output_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    provider_usage: Option<TokenUsage>,
) -> TokenUsage {
    if let Some(usage) = provider_usage {
        return usage.sanitized();
    }
    // contextUsage 覆盖生成结束后的整个上下文（含本轮输出），分摊前先扣掉输出。
    let total_input = token::input_tokens_excluding_output(
        context_total_input_tokens,
        fallback_total_input_tokens,
        fallback_output_tokens,
    );
    cache_usage.simulated_usage(total_input, fallback_output_tokens)
}

/// 测试用：`(uncached_input, output, cache_write, cache_read)` 元组形式。
#[cfg(test)]
fn resolve_non_stream_usage(
    fallback_total_input_tokens: i32,
    context_total_input_tokens: Option<i32>,
    fallback_output_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    provider_usage: Option<TokenUsage>,
) -> (i32, i32, i32, i32) {
    let usage = resolve_raw_non_stream_usage(
        fallback_total_input_tokens,
        context_total_input_tokens,
        fallback_output_tokens,
        cache_usage,
        provider_usage,
    );
    (
        usage.uncached_input_tokens,
        usage.output_tokens,
        usage.cache_write_input_tokens,
        usage.cache_read_input_tokens,
    )
}

fn validate_max_tokens(max_tokens: i32) -> Result<(), ErrorResponse> {
    if max_tokens <= 0 {
        Err(ErrorResponse::new(
            "invalid_request_error",
            "max_tokens must be greater than 0",
        ))
    } else {
        Ok(())
    }
}

fn merge_token_limits(target: &mut Option<TokenLimits>, incoming: Option<TokenLimits>) {
    let Some(incoming) = incoming else {
        return;
    };
    match target {
        Some(target) => {
            target.max_input_tokens = target.max_input_tokens.max(incoming.max_input_tokens);
            target.max_output_tokens = target.max_output_tokens.max(incoming.max_output_tokens);
        }
        None => *target = Some(incoming),
    }
}

fn infer_model_owner(model_id: &str) -> &'static str {
    let id = model_id.to_ascii_lowercase();
    if id.starts_with("claude-") {
        "anthropic"
    } else if id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || id.starts_with("o1-")
        || id.starts_with("o3-")
        || id.starts_with("o4-")
    {
        "openai"
    } else {
        "kiro"
    }
}

fn context_window_from_upstream(model_id: &str, token_limits: Option<&TokenLimits>) -> i32 {
    token_limits
        .and_then(|limits| limits.max_input_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or_else(|| get_context_window_size(model_id))
}

fn model_from_upstream(upstream: UpstreamModel) -> Model {
    let max_tokens = upstream
        .token_limits
        .as_ref()
        .and_then(|limits| limits.max_output_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(64_000);
    let context_window =
        context_window_from_upstream(&upstream.model_id, upstream.token_limits.as_ref());
    Model {
        display_name: upstream
            .model_name
            .clone()
            .unwrap_or_else(|| upstream.model_id.clone()),
        owned_by: infer_model_owner(&upstream.model_id).to_string(),
        id: upstream.model_id,
        object: "model".to_string(),
        created: 0,
        model_type: "chat".to_string(),
        context_window,
        max_tokens,
    }
}

fn aggregate_available_models_with_custom(
    upstream_models: Vec<UpstreamModel>,
    custom_models: &[crate::model::config::CustomModel],
) -> Vec<Model> {
    let mut merged_upstream: BTreeMap<String, UpstreamModel> = BTreeMap::new();
    for incoming in upstream_models {
        match merged_upstream.get_mut(&incoming.model_id) {
            Some(existing) => {
                if existing.model_name.is_none() {
                    existing.model_name = incoming.model_name;
                }
                if existing.description.is_none() {
                    existing.description = incoming.description;
                }
                merge_token_limits(&mut existing.token_limits, incoming.token_limits);
            }
            None => {
                merged_upstream.insert(incoming.model_id.clone(), incoming);
            }
        }
    }

    let mut models: BTreeMap<String, Model> = BTreeMap::new();
    for upstream in merged_upstream.into_values() {
        let model = model_from_upstream(upstream);
        models.insert(model.id.clone(), model);
    }

    // 自定义别名最后写入，同名时其展示元数据优先于动态条目。
    for custom in custom_models {
        let model = Model {
            id: custom.id.clone(),
            object: "model".to_string(),
            created: 0,
            owned_by: custom
                .owned_by
                .clone()
                .unwrap_or_else(|| "custom".to_string()),
            display_name: custom
                .display_name
                .clone()
                .unwrap_or_else(|| custom.id.clone()),
            model_type: "chat".to_string(),
            context_window: custom
                .context_window
                .unwrap_or_else(|| get_context_window_size(&custom.id)),
            max_tokens: custom.max_tokens.unwrap_or(64_000),
        };
        models.insert(model.id.clone(), model);
    }

    models.into_values().collect()
}

fn aggregate_available_models(upstream_models: Vec<UpstreamModel>) -> Vec<Model> {
    aggregate_available_models_with_custom(upstream_models, &crate::model::custom_models::all())
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
) -> Response {
    tracing::info!("Received GET /v1/models request");

    let Some(provider) = &state.kiro_provider else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "Service temporarily unavailable",
            )),
        )
            .into_response();
    };

    let upstream = match provider
        .token_manager()
        .discover_models_for_group(key_ctx.group.as_deref())
        .await
    {
        Ok(models) => models,
        Err(ModelDiscoveryError::NoAvailableCredentials) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "No available credentials for this API key",
                )),
            )
                .into_response();
        }
        Err(error @ ModelDiscoveryError::ColdStartFailed { .. }) => {
            tracing::warn!("动态模型列表加载失败: {}", error);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    "Unable to load available models from upstream",
                )),
            )
                .into_response();
        }
    };

    let models = aggregate_available_models(upstream);

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
    .into_response()
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    // Count the image budget on inbound to provide precise diagnostics for later context-window-full errors
    let img_stats = count_image_budget(&payload);
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        image_count = %img_stats.count,
        image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
        image_largest_b64_kb = %(img_stats.largest_b64_bytes / 1024),
        "Received POST /v1/messages request"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    if img_stats.total_b64_bytes > IMAGE_BUDGET_WARN_BYTES {
        tracing::warn!(
            image_count = %img_stats.count,
            image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
            "incoming image payload is large; if upstream rejects with CONTENT_LENGTH_EXCEEDS_THRESHOLD, reduce image count or use lower-resolution screenshots"
        );
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Service temporarily unavailable",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        let (resp, outcome) = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            key_ctx.group.as_deref(),
            &key_ctx.billing_policy,
            &hook.billing_prices,
        )
        .await;
        // WebSearch 路径走 MCP 端点，没有 credential_id 上下文，统一记 0
        match &outcome {
            Some(outcome) if resp.status().is_success() => {
                hook.record_outcome(0, outcome, "success");
            }
            _ => hook.record(0, input_tokens, 0, 0, 0, 0.0, "error"),
        }
        return resp;
    }

    let payload_stream = payload.stream;
    // 服务端工具（web_search / web_fetch）与客户端工具混用，或只用了 web_fetch：
    // 都走内部 agentic 循环——由代理执行抓取 / 搜索并把结果回喂给模型。
    if websearch::needs_server_tool_loop(&payload) {
        tracing::info!("detected server tools (web_search / web_fetch), entering the agentic loop");
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: payload_stream,
            },
        ));
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            tracer,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
            state.cache_meter.clone(),
            key_ctx.key_id,
            key_ctx.billing_policy,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match convert_request_with_mode(&payload, state.tool_compatibility_mode)
    {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = ("invalid_request_error", e.client_message());
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("api_error", "Internal server error")),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    // CacheMeter：根据 cache_control 断点查 / 写中转层提示词缓存。
    // 返回 estimate 口径的覆盖量；真实 input/cache 互斥分摊在拿到 total 真值时进行。
    // 保留原始 CacheMeter 结果；客户端策略在响应结算时统一应用。
    let cache_usage = match state.cache_meter.as_ref() {
        Some(cache) => {
            super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id).await
        }
        None => super::cache_metering::CacheUsage::default(),
    };

    let model = payload.model.clone();
    let is_stream = payload.stream;
    let has_output_config = payload.output_config.is_some();
    // 工具调用被 Kiro 输出上限截断时，在代理内续写（持有原请求用于拼续写上下文）。
    let retry = Some(TruncationRetry::new(
        provider.clone(),
        payload,
        state.tool_compatibility_mode,
        key_ctx.group.clone(),
    ));

    if is_stream {
        // 流式响应
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: model.clone(),
                is_stream: true,
            },
        ));
        handle_stream_request(
            provider,
            &request_body,
            &model,
            total_input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            key_ctx.billing_policy,
            tracer,
            key_ctx.group.clone(),
            retry,
        )
        .await
    } else {
        // Responses reasoning requests must expose native reasoning even when the
        // global Anthropic compatibility flag is disabled; the Responses adapter
        // explicitly opted into it via `thinking`/`output_config`.
        let extract_thinking =
            (state.extract_thinking || has_output_config) && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: model.clone(),
                is_stream: false,
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            key_ctx.billing_policy,
            tracer,
            key_ctx.group.clone(),
            retry,
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    billing_policy: super::billing::BillingPolicy,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    retry: Option<TruncationRetry>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api_stream(request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            // 重试链路全部失败、未开始返回内容：error_type 取最后一跳分类
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&format!("{e:#}")),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    tracer.mark_upstream_headers_received();
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
    );
    ctx.cache_usage = cache_usage;
    ctx.billing_policy = billing_policy;
    ctx.billing_prices = hook.billing_prices.clone();

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(
        response,
        ctx,
        initial_events,
        hook,
        credential_id,
        tracer,
        retry,
    );

    // 返回 SSE 响应
    // `x-accel-buffering: no` 让前置 nginx 对本次响应关闭 proxy_buffering。
    // 若反代开启了响应缓冲，SSE 分片会先攒在缓冲区再转发，拉长增量到达延迟；
    // 该头只作用于当前响应，无需在反代侧改全局配置。
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    retry: Option<TruncationRetry>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(sse_bytes(initial_events));

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let settlement = StreamSettlement::new(hook, credential_id, tracer, &ctx);
    let live = LiveStream {
        body_stream: response.bytes_stream().boxed(),
        ctx,
        decoder: EventStreamDecoder::new(),
        finished: false,
        ping_interval: interval(Duration::from_secs(PING_INTERVAL_SECS)),
        settlement,
        sent_bytes: 0,
        attempt_bytes: 0,
        retry,
    };

    let processing_stream = stream::unfold(live, |mut live| async move {
        if live.finished {
            return None;
        }

        // 使用 select! 同时等待数据和 ping 定时器
        let step = tokio::select! {
            chunk = live.body_stream.next() => Some(chunk),
            _ = live.ping_interval.tick() => None,
        };
        let bytes = match step {
            Some(Some(Ok(chunk))) => live.on_chunk(&chunk),
            Some(Some(Err(e))) => {
                let message = format!("{:#}", anyhow::Error::new(e));
                live.interrupt(Vec::new(), &message)
            }
            Some(None) => live.on_end().await,
            None => {
                tracing::trace!("发送 ping 保活事件");
                vec![Ok(create_ping_sse())]
            }
        };
        Some((stream::iter(bytes), live))
    })
    .flatten();

    initial_stream.chain(processing_stream)
}

fn sse_bytes(events: Vec<SseEvent>) -> Vec<Result<Bytes, Infallible>> {
    events
        .into_iter()
        .map(|e| Ok(Bytes::from(e.to_sse_string())))
        .collect()
}

/// 实时 Messages 流的处理状态。
///
/// 一条客户端流可能对应多次上游响应：工具调用被 Kiro 输出上限截断时，
/// 在流末尾发起续写请求（见 `truncation`），换上新的响应体继续读。
struct LiveStream {
    body_stream: futures::stream::BoxStream<'static, reqwest::Result<Bytes>>,
    ctx: StreamContext,
    decoder: EventStreamDecoder,
    finished: bool,
    ping_interval: tokio::time::Interval,
    settlement: StreamSettlement,
    /// 所有上游响应累计收到的字节数
    sent_bytes: u64,
    /// 当前这次上游响应收到的字节数
    attempt_bytes: u64,
    retry: Option<TruncationRetry>,
}

impl LiveStream {
    fn interrupt(&mut self, events: Vec<SseEvent>, error: &str) -> Vec<Result<Bytes, Infallible>> {
        self.finished = true;
        self.settlement
            .interrupt(&mut self.ctx, events, error, self.sent_bytes)
    }

    fn on_chunk(&mut self, chunk: &[u8]) -> Vec<Result<Bytes, Infallible>> {
        self.sent_bytes += chunk.len() as u64;
        self.attempt_bytes += chunk.len() as u64;
        // 解码事件
        if let Err(e) = self.decoder.feed(chunk) {
            return self.interrupt(Vec::new(), &e.to_string());
        }

        let mut events = Vec::new();
        let mut failure = None;
        for result in self.decoder.decode_iter() {
            let event = match result {
                Ok(frame) => match Event::from_frame(frame) {
                    Ok(event) => event,
                    Err(error) => {
                        failure = Some(error.to_string());
                        break;
                    }
                },
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            };
            if let Some(error) = upstream_event_error(&event) {
                failure = Some(error);
                break;
            }
            let generated = self.ctx.process_kiro_event(&event);
            self.settlement.tracer.mark_first_content(&generated);
            events.extend(generated);
        }
        if let Some(error) = failure {
            return self.interrupt(events, &error);
        }

        self.settlement.update(&self.ctx, self.sent_bytes);
        sse_bytes(events)
    }

    async fn on_end(&mut self) -> Vec<Result<Bytes, Infallible>> {
        let retried = self.sent_bytes > self.attempt_bytes;
        if self.attempt_bytes == 0 || self.decoder.has_pending_data() {
            let error = if self.attempt_bytes == 0 {
                "Upstream returned an empty event stream"
            } else {
                "Upstream ended with an incomplete event frame"
            };
            if !retried {
                return self.interrupt(Vec::new(), error);
            }
            // 续写请求异常结束：之前的内容已完整发出，按 max_tokens 收尾，
            // 由客户端决定是否继续。
            tracing::warn!(error, "truncation retry response ended abnormally");
            self.ctx.state_manager.set_stop_reason("max_tokens");
        }

        let mut events = self.ctx.flush_buffered_output();
        if let Some(truncated) = self.ctx.truncated_tool_call() {
            if let Some(retry) = self.retry.as_mut() {
                let partial_text = self.ctx.attempt_text().to_string();
                if let Some(call) = retry
                    .resume(
                        &truncated,
                        &partial_text,
                        true,
                        Some(self.settlement.tracer.as_ref()),
                    )
                    .await
                {
                    self.settlement.tracer.mark_first_content(&events);
                    self.ctx.prepare_for_retry();
                    self.settlement.update(&self.ctx, self.sent_bytes);
                    self.body_stream = call.response.bytes_stream().boxed();
                    self.decoder = EventStreamDecoder::new();
                    self.attempt_bytes = 0;
                    return sse_bytes(events);
                }
            }
        }

        // 流结束，发送最终事件（generate_final_events 内部会 finish()
        // 累积器，据此判定是否有非法工具调用 JSON；截断的调用已被丢弃）。
        events.extend(self.ctx.generate_final_events());
        self.settlement.tracer.mark_first_content(&events);
        self.settlement.update(&self.ctx, self.sent_bytes);
        if let Some(message) = self.ctx.tool_json_error_message() {
            // 工具调用 JSON 非法：实时流已回 200，无法改状态码，
            // 只能记 error 并让 generate_final_events 补发的 `error` 事件透传给客户端。
            self.settlement.finish(
                "error",
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&message),
                None,
            );
        } else {
            self.settlement
                .finish("success", "success", None, None, None);
        }
        self.finished = true;
        sse_bytes(events)
    }
}

/// Exactly-once settlement for a live Messages stream.
///
/// Responses consumes this stream as an inner body. If the outer client goes
/// away, dropping that body also drops the in-flight unfold future; `Drop`
/// records the latest usage snapshot and closes the trace instead of losing
/// already incurred provider usage.
struct StreamSettlement {
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    /// 最近一次结算（原始 + 计费后），流被取消时按它记账。
    outcome: BillingOutcome,
    source: UsageSource,
    sent_bytes: u64,
    settled: bool,
}

impl StreamSettlement {
    fn interrupt(
        &mut self,
        ctx: &mut StreamContext,
        mut events: Vec<SseEvent>,
        error: &str,
        bytes: u64,
    ) -> Vec<Result<Bytes, Infallible>> {
        tracing::error!(error, "Upstream response stream was interrupted");
        events.extend(
            ctx.generate_error_events("api_error", "Upstream response stream was interrupted"),
        );
        self.update(ctx, bytes);
        self.finish(
            "error",
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            Some(error),
            Some(bytes),
        );
        events
            .into_iter()
            .map(|event| Ok(Bytes::from(event.to_sse_string())))
            .collect()
    }

    fn new(
        hook: UsageRecordHook,
        credential_id: u64,
        tracer: std::sync::Arc<RequestTracer>,
        ctx: &StreamContext,
    ) -> Self {
        Self {
            hook,
            credential_id,
            tracer,
            outcome: ctx.billing(),
            source: stream_usage_source(ctx),
            sent_bytes: 0,
            settled: false,
        }
    }

    fn update(&mut self, ctx: &StreamContext, sent_bytes: u64) {
        self.outcome = ctx.billing();
        self.source = stream_usage_source(ctx);
        self.sent_bytes = sent_bytes;
    }

    fn trace_usage(&self) -> TraceUsage {
        TraceUsage::from_outcome(&self.outcome, self.source)
    }

    fn finish(
        &mut self,
        usage_status: &str,
        trace_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
    ) {
        if self.settled {
            return;
        }
        self.record_usage(usage_status);
        self.tracer.finalize(
            trace_status,
            error_type,
            error_message,
            interrupted_after_bytes,
            self.trace_usage(),
        );
        self.settled = true;
    }

    fn record_usage(&self, status: &str) {
        self.hook
            .record_outcome(self.credential_id, &self.outcome, status);
    }
}

impl Drop for StreamSettlement {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.record_usage("error");
        self.tracer.finalize(
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            Some("response stream was cancelled before completion"),
            Some(self.sent_bytes),
            self.trace_usage(),
        );
        self.settled = true;
    }
}

fn stream_usage_source(ctx: &StreamContext) -> UsageSource {
    UsageSource::resolve(ctx.provider_token_usage.is_some(), &ctx.cache_usage)
}

pub(crate) enum NonStreamExecutionError {
    Provider(Error),
    Response(Response),
}

pub(crate) fn new_non_stream_request_tracer(
    state: &AppState,
    key_ctx: KeyContext,
    model: String,
) -> std::sync::Arc<RequestTracer> {
    std::sync::Arc::new(RequestTracer::new(
        state,
        RequestTraceOptions {
            key_ctx,
            model,
            is_stream: false,
        },
    ))
}

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    billing_policy: super::billing::BillingPolicy,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    retry: Option<TruncationRetry>,
) -> Response {
    match execute_non_stream_request(
        provider,
        request_body,
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
        hook,
        cache_usage,
        billing_policy,
        tracer,
        group,
        retry,
    )
    .await
    {
        Ok(response_body) => (StatusCode::OK, Json(response_body)).into_response(),
        Err(NonStreamExecutionError::Provider(error)) => map_provider_error(error),
        Err(NonStreamExecutionError::Response(response)) => response,
    }
}

fn upstream_event_error(event: &Event) -> Option<String> {
    match event {
        Event::Error {
            error_code,
            error_message,
        } => Some(format!("{error_code}: {error_message}")),
        Event::Exception {
            exception_type,
            message,
        } if exception_type != "ContentLengthExceededException" => {
            Some(format!("{exception_type}: {message}"))
        }
        _ => None,
    }
}

fn non_stream_read_failure(
    hook: &UsageRecordHook,
    tracer: &RequestTracer,
    credential_id: u64,
    input_tokens: i32,
    error: &str,
) -> NonStreamExecutionError {
    hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
    tracer.finalize(
        "interrupted",
        Some(outcome::STREAM_INTERRUPTED),
        Some(error),
        None,
        TraceUsage::zero(),
    );
    NonStreamExecutionError::Response(
        (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "api_error",
                "Invalid or interrupted upstream response stream",
            )),
        )
            .into_response(),
    )
}

/// 一次上游非流式响应读出的全部内容。
struct NonStreamAttempt {
    text: String,
    native_thinking: String,
    native_thinking_signature: Option<String>,
    native_redacted_thinking: Vec<String>,
    tool_uses: Vec<serde_json::Value>,
    stop_reason: String,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    context_input_tokens: Option<i32>,
    /// metadataEvent.tokenUsage 是本次 provider 调用的精确最终快照。
    provider_token_usage: Option<TokenUsage>,
    /// meteringEvent 上报的 credit 计费量（上游真实下发）；
    /// input/cache_* 的互斥分摊在拿到 total 真值后由 cache_usage 完成。
    credits: f64,
    /// 最近一次 meteringEvent 的完整 payload，用于在响应体 usage 中附带
    /// credit_usage / credit_unit / credit_unit_plural 字段（供内部适配层换算），
    /// 与流式（message_delta）行为一致；如果上游多次下发则取最后一次。
    metering: Option<crate::kiro::model::events::MeteringEvent>,
    /// 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，stop 时整体解析。
    /// 非法 JSON 显式暴露为错误（返回 502）；半截 JSON 先尝试续写，不行再丢弃。
    tool_accumulator: super::stream::ToolJsonAccumulator,
    tool_json_error: Option<super::stream::ToolJsonAccumulatorError>,
    /// 上游内容过滤拒答：拦截后只剩 reasoning，必须显式标成 refusal，
    /// 否则会被当成正常 end_turn 的空响应返回。
    refused: bool,
}

impl NonStreamAttempt {
    fn new() -> Self {
        Self {
            text: String::new(),
            native_thinking: String::new(),
            native_thinking_signature: None,
            native_redacted_thinking: Vec::new(),
            tool_uses: Vec::new(),
            stop_reason: "end_turn".to_string(),
            context_input_tokens: None,
            provider_token_usage: None,
            credits: 0.0,
            metering: None,
            tool_accumulator: super::stream::ToolJsonAccumulator::new(),
            tool_json_error: None,
            refused: false,
        }
    }

    /// 本次响应里被输出上限截断、可以续写的工具调用。已经有完整的工具调用、
    /// 拒答、JSON 非法或上下文已满时不续写。
    fn truncated_tool_call(
        &self,
        tool_name_map: &std::collections::HashMap<String, String>,
    ) -> Option<super::truncation::TruncatedToolCall> {
        if self.refused
            || self.tool_json_error.is_some()
            || !self.tool_uses.is_empty()
            || self.stop_reason == "model_context_window_exceeded"
        {
            return None;
        }
        let (kiro_name, partial_input) = self.tool_accumulator.truncated_call()?;
        Some(super::truncation::TruncatedToolCall::from_partial(
            kiro_name,
            partial_input,
            tool_name_map,
        ))
    }

    /// 客户端可见的正文：剥离字面 XML 泄漏与文本形式的 thinking 块。
    fn visible_text(&self, thinking_enabled: bool) -> String {
        let text = crate::kiro::model::events::strip_tool_use_xml_leaks(&self.text);
        if thinking_enabled && self.native_thinking.is_empty() {
            super::stream::extract_thinking_from_complete_text(&text).1
        } else {
            text
        }
    }

    /// 并入一次续写响应（正文由调用方另行拼接）：工具调用与收尾状态以续写为准；
    /// 输入侧用量以首次请求为准，输出与计费累加。
    fn absorb_continuation(&mut self, next: NonStreamAttempt) {
        self.tool_uses = next.tool_uses;
        self.stop_reason = next.stop_reason;
        self.tool_accumulator = next.tool_accumulator;
        self.tool_json_error = next.tool_json_error;
        self.refused = next.refused;
        if let (Some(usage), Some(next_usage)) =
            (self.provider_token_usage.as_mut(), next.provider_token_usage)
        {
            usage.output_tokens += next_usage.output_tokens;
        }
        self.credits += next.credits;
        if let Some(mut metering) = next.metering {
            metering.usage += self.metering.as_ref().map_or(0.0, |m| m.usage);
            self.metering = Some(metering);
        }
    }
}

/// 读完一次上游非流式响应。失败时返回错误描述，由调用方决定如何收尾。
async fn read_non_stream_attempt(
    response: reqwest::Response,
    model: &str,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> Result<NonStreamAttempt, String> {
    let mut body_stream = response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();
    let mut received_data = false;
    let mut attempt = NonStreamAttempt::new();
    while let Some(chunk) = body_stream.next().await {
        let body_bytes = chunk.map_err(|e| format!("{:#}", anyhow::Error::new(e)))?;
        received_data |= !body_bytes.is_empty();
        decoder.feed(&body_bytes).map_err(|e| e.to_string())?;
        for result in decoder.decode_iter() {
            let frame = result.map_err(|e| e.to_string())?;
            let event = Event::from_frame(frame).map_err(|e| e.to_string())?;
            if let Some(error) = upstream_event_error(&event) {
                return Err(error);
            }
            match event {
                Event::AssistantResponse(resp) => {
                    attempt.text.push_str(&resp.content);
                }
                Event::ReasoningContent(reasoning) => {
                    if let Some(text) = reasoning.text
                        && !text.is_empty()
                    {
                        attempt.native_thinking.push_str(&text);
                    }
                    if let Some(signature) = reasoning.signature
                        && !signature.is_empty()
                    {
                        attempt.native_thinking_signature = Some(signature);
                    }
                    if let Some(redacted) = reasoning.redacted_content
                        && !redacted.is_empty()
                    {
                        attempt.native_redacted_thinking.push(redacted);
                    }
                }
                Event::ToolUse(tool_use) => {
                    match attempt.tool_accumulator.push(&tool_use, tool_name_map) {
                        Ok(Some(completed)) => {
                            attempt.tool_uses.push(completed.to_anthropic_block());
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::error!("{}", e);
                            attempt.tool_json_error = Some(e);
                        }
                    }
                }
                Event::Metadata(metadata) => {
                    if let Some(usage) = metadata.token_usage {
                        let usage = usage.sanitized();
                        tracing::debug!(
                            uncached_input_tokens = usage.uncached_input_tokens,
                            cache_write_input_tokens = usage.cache_write_input_tokens,
                            cache_read_input_tokens = usage.cache_read_input_tokens,
                            output_tokens = usage.output_tokens,
                            "收到 metadataEvent.tokenUsage 精确用量"
                        );
                        // 单条 provider 流内是最终快照，重复事件取最后一份。
                        attempt.provider_token_usage = Some(usage);
                    }
                    if let Some(refusal) = metadata.content_filter_refusal() {
                        tracing::warn!("上游内容过滤拒答: {}", refusal);
                        attempt.refused = true;
                    }
                }
                Event::ContextUsage(context_usage) => {
                    // 从上下文使用百分比计算实际的 input_tokens
                    let window_size = get_context_window_size(model);
                    let actual_input_tokens = (context_usage.context_usage_percentage
                        * (window_size as f64)
                        / 100.0) as i32;
                    attempt.context_input_tokens = Some(actual_input_tokens);
                    // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                    if context_usage.context_usage_percentage >= 100.0 {
                        attempt.stop_reason = "model_context_window_exceeded".to_string();
                    }
                    tracing::debug!(
                        "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                        context_usage.context_usage_percentage,
                        actual_input_tokens
                    );
                }
                Event::Metering(event_metering) => {
                    // 上游只下发 credit；token / cache 字段不存在
                    attempt.credits += event_metering.usage;
                    tracing::debug!(
                        usage = event_metering.usage,
                        unit = %event_metering.unit,
                        unit_plural = %event_metering.unit_plural,
                        "metering credits +{:.6}", event_metering.usage
                    );
                    attempt.metering = Some(event_metering);
                }
                Event::Exception { exception_type, .. } => {
                    if exception_type == "ContentLengthExceededException" {
                        attempt.stop_reason = "max_tokens".to_string();
                    }
                }
                _ => {}
            }
        }
    }

    if !received_data {
        return Err("Upstream returned an empty event stream".to_string());
    }
    if decoder.has_pending_data() {
        return Err("Upstream ended with an incomplete event frame".to_string());
    }
    Ok(attempt)
}

pub(crate) async fn execute_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    billing_policy: super::billing::BillingPolicy,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    mut retry: Option<TruncationRetry>,
) -> Result<serde_json::Value, NonStreamExecutionError> {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api(request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&format!("{e:#}")),
                None,
                TraceUsage::zero(),
            );
            return Err(NonStreamExecutionError::Provider(e));
        }
    };
    tracer.mark_upstream_headers_received();
    let credential_id = call_result.credential_id;

    let mut result =
        match read_non_stream_attempt(call_result.response, model, &tool_name_map).await {
            Ok(attempt) => attempt,
            Err(error) => {
                return Err(non_stream_read_failure(
                    &hook,
                    &tracer,
                    credential_id,
                    input_tokens,
                    &error,
                ));
            }
        };

    // 工具调用被 Kiro 输出上限截断：在代理内让上游分段重发，
    // 续写的正文接在同一条回复后面（见 `truncation`）。
    let mut continuations: Vec<String> = Vec::new();
    while let Some(retry) = retry.as_mut() {
        let Some(truncated) = result.truncated_tool_call(&tool_name_map) else {
            break;
        };
        let partial_text = match continuations.last() {
            Some(text) => text.clone(),
            None => result.visible_text(thinking_enabled),
        };
        let Some(call) = retry
            .resume(&truncated, &partial_text, false, Some(tracer.as_ref()))
            .await
        else {
            break;
        };
        match read_non_stream_attempt(call.response, model, &tool_name_map).await {
            Ok(next) => {
                continuations.push(crate::kiro::model::events::strip_tool_use_xml_leaks(
                    &next.text,
                ));
                result.absorb_continuation(next);
            }
            Err(error) => {
                // 续写失败：之前的内容照常返回，半截的调用在下面被丢弃并按 max_tokens 收尾。
                tracing::warn!(error, "truncation retry response failed");
                break;
            }
        }
    }
    let NonStreamAttempt {
        text: text_content,
        native_thinking,
        native_thinking_signature,
        native_redacted_thinking,
        mut tool_uses,
        mut stop_reason,
        context_input_tokens,
        provider_token_usage,
        credits,
        metering,
        mut tool_accumulator,
        mut tool_json_error,
        refused,
    } = result;

    // 收尾：无参工具的 0 字节滞留缓冲在此还原为 {} 补进 content；累积了非空内容
    // 却从未收到 stop=true 的（上游在参数写到一半时截断、且已无法续写）丢弃该调用，
    // 没有其他工具调用时按 max_tokens 收尾，由客户端决定是否继续。
    // 已有错误则保持不变。
    if refused {
        // A filter refusal can cut off tool arguments. Do not recover pending
        // calls or replace the authoritative refusal with their JSON errors.
        tool_json_error = None;
    } else if tool_json_error.is_none() {
        match tool_accumulator.finish(&tool_name_map) {
            Ok(recovered) => {
                for completed in recovered {
                    tool_uses.push(completed.to_anthropic_block());
                }
            }
            Err(e @ super::stream::ToolJsonAccumulatorError::IncompleteJson { .. }) => {
                tracing::warn!("{}", e);
                if tool_uses.is_empty() && stop_reason == "end_turn" {
                    stop_reason = "max_tokens".to_string();
                }
            }
            Err(e) => {
                tracing::error!("{}", e);
                tool_json_error = Some(e);
            }
        }
    }

    // 工具调用 JSON 非法：非流式路径尚未发送任何字节，直接回 502，
    // 明确暴露上游问题，而不是把无法解析的参数当成完整调用返回。
    if let Some(err) = tool_json_error {
        let message = err.message();
        if let Some(usage) = provider_token_usage {
            let outcome = super::billing::settle(
                &billing_policy,
                &hook.billing_prices,
                model,
                usage,
                credits,
            );
            hook.record_outcome(credential_id, &outcome, "error");
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&message),
                None,
                TraceUsage::from_outcome(&outcome, UsageSource::Provider),
            );
        } else {
            // metadata 缺失时保留原有错误口径，不把不完整工具输出估算成已消费量。
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&message),
                None,
                TraceUsage::zero(),
            );
        }
        return Err(NonStreamExecutionError::Response(
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new("upstream_tool_json_error", message)),
            )
                .into_response(),
        ));
    }

    // 剥离混入文本的字面 <tool_use> XML 泄漏（非流式：整段文本已就绪，一次性剥离）。
    let text_content = crate::kiro::model::events::strip_tool_use_xml_leaks(&text_content);

    // 构建响应内容
    let mut content = build_non_stream_content(
        thinking_enabled,
        text_content,
        native_thinking,
        native_thinking_signature,
        native_redacted_thinking,
    );
    for continuation in &continuations {
        append_non_stream_text(&mut content, continuation);
    }
    // 把混在正文里的字面 <invoke> 工具调用捞回成结构化 tool_use（与流式嗅探同一套判定）。
    if !known_tool_names.is_empty() {
        content = content
            .into_iter()
            .flat_map(|block| match block["text"].as_str() {
                Some(text) if block["type"] == "text" => {
                    super::stream::extract_invoke_content_blocks(
                        text,
                        &known_tool_names,
                        &tool_name_map,
                    )
                }
                _ => vec![block],
            })
            .collect();
    }
    content.extend(tool_uses);

    // 确定 stop_reason：拒答是终态，优先于同流内的 max_tokens / 上下文溢出 / tool_use。
    let has_tool_use = content.iter().any(|block| block["type"] == "tool_use");
    if refused {
        stop_reason = super::stream::REFUSAL_STOP_REASON.to_string();
    } else if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // provider 未下发 metadataEvent 时才使用本地输出估算。
    let fallback_output_tokens = token::estimate_output_tokens(&content);
    let raw_usage = resolve_raw_non_stream_usage(
        input_tokens,
        context_input_tokens,
        fallback_output_tokens,
        cache_usage,
        provider_token_usage,
    );
    // 一次结算：响应、用量统计、trace 共用同一份计费后结果。
    let outcome = super::billing::settle(
        &billing_policy,
        &hook.billing_prices,
        model,
        raw_usage,
        credits,
    );
    let billed = outcome.billed;

    // 构建 Anthropic 响应
    let mut usage_json = json!({
        "input_tokens": billed.uncached_input_tokens,
        "output_tokens": billed.output_tokens,
        "cache_creation_input_tokens": billed.cache_write_input_tokens,
        "cache_read_input_tokens": billed.cache_read_input_tokens
    });
    // 按 TTL 拆分的缓存写入量，下游按 5m / 1h 分别计价。
    let creation_ttl = cache_usage.split_creation_ttl(billed.cache_write_input_tokens);
    usage_json["cache_creation"] = json!({
        "ephemeral_5m_input_tokens": creation_ttl.ephemeral_5m_input_tokens,
        "ephemeral_1h_input_tokens": creation_ttl.ephemeral_1h_input_tokens
    });
    // 上游 meteringEvent 的 credit_* 字段（只在收到过 meteringEvent 时追加），
    // 用量换成计费后 credit。
    if let Some(m) = &metering {
        usage_json["credit_usage"] = json!(outcome.billed_credits);
        usage_json["credit_unit"] = json!(m.unit);
        usage_json["credit_unit_plural"] = json!(m.unit_plural);
    }
    let response_body = json!({
        "id": super::ids::message_id(),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage_json
    });

    hook.record_outcome(credential_id, &outcome, "success");
    tracer.finalize(
        "success",
        None,
        None,
        None,
        TraceUsage::from_outcome(
            &outcome,
            UsageSource::resolve(provider_token_usage.is_some(), &cache_usage),
        ),
    );
    Ok(response_body)
}

fn build_non_stream_content(
    thinking_enabled: bool,
    text_content: String,
    native_thinking: String,
    native_thinking_signature: Option<String>,
    native_redacted_thinking: Vec<String>,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    let has_native_thinking = !native_thinking.is_empty();

    if thinking_enabled {
        if has_native_thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": native_thinking.clone(),
                "signature": native_thinking_signature
                    .unwrap_or_else(super::ids::thinking_signature),
            }));
        } else {
            // 从完整文本中提取 thinking 块，兼容旧的 <thinking> 文本路径。
            let (thinking, remaining_text) =
                super::stream::extract_thinking_from_complete_text(&text_content);

            if let Some(thinking_text) = thinking {
                content.push(json!({
                    "type": "thinking",
                    "thinking": thinking_text,
                    "signature": super::ids::thinking_signature(),
                }));
            }

            if !remaining_text.is_empty() {
                content.push(json!({
                    "type": "text",
                    "text": remaining_text
                }));
            }
        }

        for redacted in native_redacted_thinking {
            content.push(json!({
                "type": "redacted_thinking",
                "data": redacted
            }));
        }

        if has_native_thinking && !text_content.is_empty() {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    } else if has_native_thinking {
        content.push(json!({
            "type": "text",
            "text": native_thinking
        }));
    }
    content
}

/// 把截断续写得到的正文接到非流式 content 末尾，分隔规则与流式续写一致。
fn append_non_stream_text(content: &mut Vec<serde_json::Value>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = content.last_mut()
        && last["type"] == "text"
        && let Some(existing) = last["text"].as_str()
    {
        let separator = super::truncation::continuation_separator(existing.chars().last(), text);
        let joined = format!("{existing}{separator}{text}");
        last["text"] = json!(joined);
        return;
    }
    content.push(json!({ "type": "text", "text": text }));
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6 {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    Extension(_key_ctx): Extension<KeyContext>,
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Service temporarily unavailable",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        let (resp, outcome) = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            key_ctx.group.as_deref(),
            &key_ctx.billing_policy,
            &hook.billing_prices,
        )
        .await;
        // WebSearch 路径走 MCP 端点，没有 credential_id 上下文，统一记 0
        match &outcome {
            Some(outcome) if resp.status().is_success() => {
                hook.record_outcome(0, outcome, "success");
            }
            _ => hook.record(0, input_tokens, 0, 0, 0, 0.0, "error"),
        }
        return resp;
    }

    let payload_stream = payload.stream;
    // 服务端工具（web_search / web_fetch）与客户端工具混用，或只用了 web_fetch：
    // 都走内部 agentic 循环——由代理执行抓取 / 搜索并把结果回喂给模型。
    if websearch::needs_server_tool_loop(&payload) {
        tracing::info!("detected server tools (web_search / web_fetch), entering the agentic loop");
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: payload_stream,
            },
        ));
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            tracer,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
            state.cache_meter.clone(),
            key_ctx.key_id,
            key_ctx.billing_policy,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match convert_request_with_mode(&payload, state.tool_compatibility_mode)
    {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = ("invalid_request_error", e.client_message());
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("api_error", "Internal server error")),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 计算总 input tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    // CacheMeter：根据 cache_control 断点查 / 写中转层提示词缓存（estimate 口径），
    // 保留原始 CacheMeter 结果；策略在结算阶段统一应用。
    let cache_usage = match state.cache_meter.as_ref() {
        Some(cache) => {
            super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id).await
        }
        None => super::cache_metering::CacheUsage::default(),
    };

    // 原始请求移交给截断续写上下文，之后只用到模型名等少数字段。
    let model = payload.model.clone();
    let is_stream = payload.stream;
    let retry = Some(TruncationRetry::new(
        provider.clone(),
        payload,
        state.tool_compatibility_mode,
        key_ctx.group.clone(),
    ));

    if is_stream {
        // Stream content immediately; final usage arrives in message_delta.
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: model.clone(),
                is_stream: true,
            },
        ));
        handle_cc_stream_request(
            provider,
            &request_body,
            &model,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            hook,
            total_input_tokens,
            cache_usage,
            key_ctx.billing_policy,
            tracer,
            key_ctx.group.clone(),
            retry,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: model.clone(),
                is_stream: false,
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            key_ctx.billing_policy,
            tracer,
            key_ctx.group.clone(),
            retry,
        )
        .await
    }
}

/// Claude Code uses the live stream too. Initial usage is an estimate;
/// message_delta carries the final provider usage without delaying content.
async fn handle_cc_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    fallback_input_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    billing_policy: super::billing::BillingPolicy,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    retry: Option<TruncationRetry>,
) -> Response {
    handle_stream_request(
        provider,
        request_body,
        model,
        fallback_input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
        hook,
        cache_usage,
        billing_policy,
        tracer,
        group,
        retry,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::ToolCompatibilityMode;

    #[test]
    fn dropped_stream_settles_latest_usage_exactly_once() {
        let aggregator = std::sync::Arc::new(crate::admin::usage_stats::UsageAggregator::new());
        let state = AppState::new(false, ToolCompatibilityMode::Raw).with_usage(
            None,
            None,
            Some(aggregator.clone()),
        );
        let hook = UsageRecordHook::from_state(&state, 0, "test-model".to_string());
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: KeyContext {
                    key_id: 0,
                    group: None,
                    key_source: TraceKeySource::MasterApiKey,
                    client_ip: None,
                    billing_policy: crate::anthropic::billing::BillingPolicy::default(),
                },
                model: "test-model".to_string(),
                is_stream: true,
            },
        ));
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            11,
            false,
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.output_tokens = 7;
        ctx.credits = 0.5;

        let mut settlement = StreamSettlement::new(hook, 42, tracer, &ctx);
        settlement.update(&ctx, 123);
        drop(settlement);

        let overview = aggregator.overview(crate::admin::usage_stats::StatsView::Billed);
        assert_eq!(overview.today_calls, 1);
        assert_eq!(overview.today_errors, 1);
        assert_eq!(overview.today_input_tokens, 11);
        assert_eq!(overview.today_output_tokens, 7);
        assert_eq!(overview.today_credits, 0.5);
    }

    /// CC 实时流在客户端断开时：Drop 必须记账 + 落 trace，
    /// 且已产生的用量不能丢（与 /v1 的 StreamSettlement 行为一致）。
    #[test]
    fn dropped_cc_live_stream_settles_usage_and_trace() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        let aggregator = std::sync::Arc::new(crate::admin::usage_stats::UsageAggregator::new());
        let state = AppState::new(false, ToolCompatibilityMode::Raw)
            .with_usage(None, None, Some(aggregator.clone()))
            .with_trace_store(Some(store.clone()));
        let hook = UsageRecordHook::from_state(&state, 0, "test-model".to_string());
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: KeyContext {
                    key_id: 0,
                    group: None,
                    key_source: TraceKeySource::MasterApiKey,
                    client_ip: None,
                    billing_policy: crate::anthropic::billing::BillingPolicy::default(),
                },
                model: "test-model".to_string(),
                is_stream: true,
            },
        ));
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            11,
            false,
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.cache_usage = super::super::cache_metering::CacheUsage::default();

        let mut settlement = StreamSettlement::new(hook, 42, tracer, &ctx);
        settlement.update(&ctx, 456);
        drop(settlement);

        // 用量必须落账（不能因取消而丢失）
        let overview = aggregator.overview(crate::admin::usage_stats::StatsView::Billed);
        assert_eq!(overview.today_calls, 1);
        assert_eq!(overview.today_errors, 1);

        // trace 必须留痕，且归类为流中断（而非 abandoned_before_headers）
        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1);
        assert_eq!(records[0].final_status, "interrupted");
        assert_eq!(
            records[0].error_type.as_deref(),
            Some(outcome::STREAM_INTERRUPTED)
        );
        assert_eq!(records[0].interrupted_after_bytes, Some(456));
    }

    #[test]
    fn tracer_renumbers_attempts_across_provider_rounds_before_persisting() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        let tracer = RequestTracer {
            store: Some(store.clone()),
            trace_id: "gpt-websearch-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 7,
            key_source: TraceKeySource::ClientKey,
            client_ip: None,
            model: "gpt-5.6-luna".to_string(),
            is_stream: false,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        };

        let attempt = |attempt, credential_id, outcome: &str| TraceAttempt {
            attempt,
            credential_id,
            endpoint: "ide".to_string(),
            http_status: Some(200),
            outcome: outcome.to_string(),
            error_snippet: None,
            duration_ms: 10,
        };

        // First provider round reports local attempts 0,1; the next round starts at 0 again.
        tracer.on_attempt(attempt(0, 11, outcome::TRANSIENT));
        tracer.on_attempt(attempt(1, 12, outcome::SUCCESS));
        tracer.on_attempt(attempt(0, 13, outcome::SUCCESS));
        tracer.finalize(
            "success",
            None,
            None,
            None,
            TraceUsage {
                input_tokens: 101,
                output_tokens: 23,
                cache_creation_tokens: 7,
                cache_read_tokens: 89,
                credits: 0.25,
                source: UsageSource::Simulated,
                ..Default::default()
            },
        );

        let (records, total) = store.query_paged(&TraceQuery {
            model: Some("gpt-5.6-luna".to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.final_credential_id, 13);
        assert_eq!(record.total_attempts, 3);
        assert_eq!(
            record
                .attempts
                .iter()
                .map(|a| a.attempt)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(record.input_tokens, 101);
        assert_eq!(record.output_tokens, 23);
        assert_eq!(record.cache_creation_tokens, 7);
        assert_eq!(record.cache_read_tokens, 89);
        assert_eq!(record.credits, 0.25);
    }

    #[test]
    fn tracer_only_marks_first_token_for_streaming_requests() {
        let mut tracer = RequestTracer {
            store: None,
            trace_id: "first-token-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 0,
            key_source: TraceKeySource::MasterApiKey,
            client_ip: None,
            model: "claude-sonnet-4".to_string(),
            is_stream: false,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        };

        tracer.mark_first_token();
        assert!(tracer.first_token_at.lock().is_none());

        tracer.is_stream = true;
        tracer.mark_first_token();
        let first = *tracer.first_token_at.lock();
        assert!(first.is_some());

        tracer.mark_first_token();
        assert_eq!(*tracer.first_token_at.lock(), first);
    }

    #[test]
    fn tracer_uses_terminal_mcp_attempt_for_failure_fields() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        let tracer = RequestTracer {
            store: Some(store.clone()),
            trace_id: "mcp-failure-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 0,
            key_source: TraceKeySource::MasterApiKey,
            client_ip: None,
            model: "claude-sonnet-4".to_string(),
            is_stream: true,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        };
        let attempt = |credential_id, endpoint: &str, status, attempt_outcome: &str| TraceAttempt {
            attempt: 0,
            credential_id,
            endpoint: endpoint.to_string(),
            http_status: Some(status),
            outcome: attempt_outcome.to_string(),
            error_snippet: None,
            duration_ms: 10,
        };

        tracer.on_attempt(attempt(11, "ide", 200, outcome::SUCCESS));
        tracer.on_attempt(attempt(29, "cli", 503, outcome::TRANSIENT));
        tracer.finalize(
            "error",
            last_attempt_outcome(&tracer),
            Some("MCP request failed"),
            None,
            TraceUsage::zero(),
        );

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1);
        let record = &records[0];
        assert_eq!(record.final_credential_id, 29);
        assert_eq!(record.error_type.as_deref(), Some(outcome::TRANSIENT));
        assert_eq!(record.total_attempts, 2);
        assert_eq!(record.attempts[0].endpoint, "ide");
        assert_eq!(record.attempts[1].endpoint, "cli");
    }

    /// 从未显式 finalize → Drop 兜底补一条 abandoned 记录（attempts=0 / first_token=None）。
    #[test]
    fn tracer_drop_records_abandoned_when_never_finalized() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        {
            let _tracer = RequestTracer {
                store: Some(store.clone()),
                trace_id: "abandoned-trace".to_string(),
                ts: Utc::now().to_rfc3339(),
                key_id: 0,
                key_source: TraceKeySource::MasterApiKey,
                model: "claude-sonnet-5".to_string(),
                is_stream: true,
                started_at: Instant::now(),
                first_token_at: parking_lot::Mutex::new(None),
                attempts: parking_lot::Mutex::new(Vec::new()),
                client_ip: None,
                route: parking_lot::Mutex::new(None),
                finalized: std::sync::atomic::AtomicBool::new(false),
                upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
            };
            // 不调用任何 finalize，直接离开作用域触发 Drop
        }

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1, "Drop 应兜底落一行");
        let record = &records[0];
        assert_eq!(record.final_status, "interrupted");
        assert_eq!(
            record.error_type.as_deref(),
            Some(outcome::ABANDONED_BEFORE_HEADERS)
        );
        assert_eq!(record.total_attempts, 0);
        assert_eq!(record.first_token_ms, None);
        assert_eq!(record.interrupted_after_bytes, None);
    }

    /// provider 已返回 response headers 后、body 收尾前请求被取消：不能误报为等头阶段
    /// 中断。这个覆盖非流式 body 读取，以及未来忘记安装 stream settlement 的新路径。
    #[test]
    fn tracer_drop_records_stream_interrupted_after_headers_arrive() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        {
            let tracer = RequestTracer {
                store: Some(store.clone()),
                trace_id: "headers-arrived-trace".to_string(),
                ts: Utc::now().to_rfc3339(),
                key_id: 0,
                key_source: TraceKeySource::MasterApiKey,
                model: "claude-sonnet-5".to_string(),
                is_stream: true,
                started_at: Instant::now(),
                first_token_at: parking_lot::Mutex::new(None),
                attempts: parking_lot::Mutex::new(Vec::new()),
                client_ip: None,
                route: parking_lot::Mutex::new(None),
                finalized: std::sync::atomic::AtomicBool::new(false),
                upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
            };
            tracer.mark_upstream_headers_received();
            // 不调用 finalize，直接离开作用域模拟 headers 已到后 handler/body 被取消。
        }

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1, "Drop 应兜底落一行");
        assert_eq!(records[0].final_status, "interrupted");
        assert_eq!(
            records[0].error_type.as_deref(),
            Some(outcome::STREAM_INTERRUPTED)
        );
        assert_eq!(records[0].first_token_ms, None);
    }

    /// 已显式 finalize 的请求：Drop 走幂等守卫，不得再写第二行、也不得覆盖原记录。
    #[test]
    fn tracer_drop_is_noop_when_already_finalized() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        {
            let tracer = RequestTracer {
                store: Some(store.clone()),
                trace_id: "finalized-trace".to_string(),
                ts: Utc::now().to_rfc3339(),
                key_id: 0,
                key_source: TraceKeySource::MasterApiKey,
                model: "claude-sonnet-5".to_string(),
                is_stream: false,
                started_at: Instant::now(),
                first_token_at: parking_lot::Mutex::new(None),
                attempts: parking_lot::Mutex::new(Vec::new()),
                client_ip: None,
                route: parking_lot::Mutex::new(None),
                finalized: std::sync::atomic::AtomicBool::new(false),
                upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
            };
            tracer.finalize("success", None, None, None, TraceUsage::zero());
            // 离开作用域触发 Drop —— 应被幂等守卫拦下
        }

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1, "已 finalize 后 Drop 不得再写一行");
        assert_eq!(records[0].final_status, "success");
        assert_eq!(records[0].error_type, None);
    }

    #[test]
    fn account_suspended_attempt_is_preserved_as_request_error_type() {
        assert_eq!(
            canonical_attempt_outcome(outcome::ACCOUNT_SUSPENDED),
            outcome::ACCOUNT_SUSPENDED
        );
    }

    #[test]
    fn bedrock_client_validation_errors_map_to_400() {
        // 客户端校验错误必须映射为 400（而非 5xx），否则会被 provider 当作上游
        // 瞬态错误触发冷却，放大成 503 风暴。识别逻辑集中在 endpoint 层。
        for needle in [
            // 精确 reason（provider 错误串里嵌着上游 body）
            "非流式 API 请求失败: 500 {\"reason\":\"TOOL_USE_RESULT_MISMATCH\"}",
            // message 级特异短语（纯文本报文）
            "Expected toolResult blocks but found none",
        ] {
            let resp = map_provider_error(anyhow::anyhow!(needle.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "错误串 `{needle}` 应映射为 400"
            );
        }
    }

    #[test]
    fn generic_upstream_error_still_maps_to_502() {
        // 回归：普通上游错误不应被新分支误伤，仍应是 502 BAD_GATEWAY。
        let resp = map_provider_error(anyhow::anyhow!("connection reset by peer"));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        // 回归：宽泛的 ValidationException 不再被当作客户端校验错误而误判为 400，
        // 仍按上游错误走 502（避免把可重试故障误杀）。
        let resp = map_provider_error(anyhow::anyhow!(
            "ValidationException: transient backend issue".to_string()
        ));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn upstream_rate_limit_maps_to_429_with_retry_after() {
        let err = crate::kiro::error::UpstreamRateLimitError::new(Some("1800".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "1800");
    }

    #[test]
    fn upstream_rate_limit_drops_invalid_retry_after() {
        let err =
            crate::kiro::error::UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_none());
    }

    #[tokio::test]
    async fn typed_context_overflow_maps_to_stable_400() {
        let resp = map_provider_error(crate::kiro::error::UpstreamContextOverflowError.into());
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("prompt is too long"));
        assert_eq!(body["type"], "error");
        assert!(body["request_id"].as_str().unwrap().starts_with("req_011"));
    }

    #[tokio::test]
    async fn generic_upstream_error_does_not_expose_raw_body() {
        let secret = "aws-account=123456789012 request-id=private-request";
        let resp = map_provider_error(anyhow::anyhow!(secret));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(secret));
        assert!(body.contains("Internal server error"));
    }

    #[test]
    fn non_stream_native_thinking_precedes_redacted_and_text() {
        let content = build_non_stream_content(
            true,
            "final answer".to_string(),
            "native thinking".to_string(),
            Some("real-signature".to_string()),
            vec!["encrypted-thinking".to_string()],
        );

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "native thinking");
        assert_eq!(content[0]["signature"], "real-signature");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "encrypted-thinking");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "final answer");
    }

    #[test]
    fn non_stream_legacy_thinking_extraction_still_works_without_native_reasoning() {
        let content = build_non_stream_content(
            true,
            "<thinking>legacy thinking</thinking>\n\nfinal answer".to_string(),
            String::new(),
            None,
            Vec::new(),
        );

        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "legacy thinking");
        let signature = content[0]["signature"].as_str().expect("signature string");
        assert!(signature.len() > 200, "{signature}");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "final answer");
    }

    #[test]
    fn non_stream_native_thinking_downgrades_to_text_when_thinking_disabled() {
        let content = build_non_stream_content(
            false,
            String::new(),
            "native thinking fallback".to_string(),
            Some("ignored-signature".to_string()),
            vec!["ignored-redacted".to_string()],
        );

        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "native thinking fallback");
    }

    #[test]
    fn dynamic_models_do_not_synthesize_claude_thinking_alias() {
        let models = aggregate_available_models(vec![UpstreamModel {
            model_id: "claude-opus-5".to_string(),
            model_name: Some("Claude Opus 5".to_string()),
            description: None,
            token_limits: None,
        }]);
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-5"));
        assert!(!ids.contains(&"claude-opus-5-thinking"));
    }

    #[test]
    fn count_image_budget_handles_empty() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": []
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.total_b64_bytes, 0);
        assert_eq!(stats.largest_b64_bytes, 0);
    }

    #[test]
    fn count_image_budget_counts_inline_base64() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA1111"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBBBBBBBB"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#).unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_b64_bytes, 18);
        assert_eq!(stats.largest_b64_bytes, 10);
    }

    #[test]
    fn count_image_budget_skips_url_only_images() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
    }

    #[test]
    fn dynamic_models_merge_metadata_and_do_not_use_input_limit_as_output_limit() {
        let models = aggregate_available_models(vec![
            UpstreamModel {
                model_id: "glm-5".to_string(),
                model_name: None,
                description: Some("first".to_string()),
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(200_000),
                    max_output_tokens: None,
                }),
            },
            UpstreamModel {
                model_id: "glm-5".to_string(),
                model_name: Some("GLM 5".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(32_000),
                }),
            },
        ]);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "GLM 5");
        assert_eq!(models[0].owned_by, "kiro");
        assert_eq!(models[0].context_window, 1_000_000);
        assert_eq!(models[0].max_tokens, 32_000);
    }

    #[test]
    fn custom_model_metadata_overrides_dynamic_collision() {
        let custom = crate::model::config::CustomModel {
            id: "gpt-next".to_string(),
            backend_id: "gpt-next".to_string(),
            display_name: Some("Configured GPT".to_string()),
            context_window: Some(500_000),
            max_tokens: Some(12_345),
            supports_reasoning: Some(true),
            owned_by: Some("configured-owner".to_string()),
        };
        let models = aggregate_available_models_with_custom(
            vec![UpstreamModel {
                model_id: "gpt-next".to_string(),
                model_name: Some("Upstream GPT".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(300_000),
                    max_output_tokens: Some(64_000),
                }),
            }],
            &[custom],
        );

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "Configured GPT");
        assert_eq!(models[0].owned_by, "configured-owner");
        assert_eq!(models[0].context_window, 500_000);
        assert_eq!(models[0].max_tokens, 12_345);
    }

    #[test]
    fn non_stream_usage_prefers_sanitized_provider_snapshot() {
        let fallback_cache = super::super::cache_metering::CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
            ..Default::default()
        };
        let provider = TokenUsage {
            uncached_input_tokens: 3,
            output_tokens: 11,
            cache_read_input_tokens: 7,
            cache_write_input_tokens: 4,
        };

        assert_eq!(
            resolve_non_stream_usage(100, Some(80), 9, fallback_cache, Some(provider)),
            (3, 11, 4, 7)
        );
    }

    #[test]
    fn non_stream_usage_falls_back_to_context_and_cache_split() {
        let cache_usage = super::super::cache_metering::CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
            ..Default::default()
        };

        // context 80 - output 9 = 71，再按 50/100 分摊：35 / 18 / 18。
        // output_tokens 字段本身不受扣减影响，仍是 9。
        assert_eq!(
            resolve_non_stream_usage(100, Some(80), 9, cache_usage, None),
            (35, 9, 18, 18)
        );
        let (input, _, creation, read) =
            resolve_non_stream_usage(100, Some(80), 9, cache_usage, None);
        assert_eq!(
            input + creation + read,
            71,
            "分摊必须发生在扣减之后，三份之和等于修正后的 total"
        );
        // output 必须取正数：若传 -9，错实现（对 fallback 分支也扣减）因
        // output.max(0) == 0 而照样得到 100，这条断言就失去鉴别力。
        // 取 9 后，错实现会得到 91，与期望的 100 不符，立刻暴露。
        assert_eq!(
            resolve_non_stream_usage(100, None, 9, Default::default(), None),
            (100, 9, 0, 0)
        );
        // 负输出的 clamp 另行单独覆盖（口径与上一条互不干扰）。
        assert_eq!(
            resolve_non_stream_usage(100, None, -9, Default::default(), None),
            (100, 0, 0, 0)
        );
    }

    #[test]
    fn max_tokens_must_be_positive() {
        assert!(validate_max_tokens(1).is_ok());
        assert!(validate_max_tokens(0).is_err());
        assert!(validate_max_tokens(-1).is_err());
    }

    /// 每个 SSE 响应都要带 `x-accel-buffering: no`。
    ///
    /// 走源码扫描而非构造响应：SSE 端点散落在 7 个模块且构造成本高，而风险点
    /// 恰是"新增端点时忘了带"。
    #[test]
    fn every_sse_response_disables_proxy_buffering() {
        const SSE_CONTENT_TYPE: &str = r#".header(header::CONTENT_TYPE, "text/event-stream")"#;
        const NO_BUFFERING: &str = r#".header("x-accel-buffering", "no")"#;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0;
        let mut missing = Vec::new();

        let mut stack = vec![root];
        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(&path).expect("read src dir") {
                let entry = entry.expect("dir entry");
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&p).expect("read rs file");
                for (idx, _) in src.match_indices(SSE_CONTENT_TYPE) {
                    checked += 1;
                    // 头部块紧邻 content-type；按字符截取，避免切断中文注释的 UTF-8 编码。
                    let tail: String = src[idx..].chars().take(400).collect();
                    if !tail.contains(NO_BUFFERING) {
                        let line = src[..idx].lines().count();
                        missing.push(format!("{}:{}", p.display(), line));
                    }
                }
            }
        }

        assert!(checked > 0, "未扫描到任何 SSE 响应，断言失效");
        assert!(
            missing.is_empty(),
            "以下 SSE 响应缺少 `x-accel-buffering: no`：{missing:?}"
        );
    }
}
