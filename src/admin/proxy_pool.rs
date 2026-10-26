//! 代理 IP 池管理
//!
//! 独立于凭据管理，存储为 proxy_pool.json
//!
//! 除增删改查外，还提供主动健康检查：周期性（或按需）通过每个代理请求一个
//! 轻量公网探测端点，记录连通性与延迟；连续探测失败达阈值的代理会被自动禁用。

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use futures::stream::{self, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// 健康检查探测端点：返回 204 No Content 的轻量公网地址，不依赖上游 Kiro。
const PROXY_HEALTH_CHECK_URL: &str = "https://www.gstatic.com/generate_204";
/// 单次探测超时（秒）
const PROXY_PROBE_TIMEOUT_SECS: u64 = 8;
/// 连续探测失败阈值：达到后自动禁用（与凭据的 MAX_FAILURES_PER_CREDENTIAL 对齐）
const MAX_PROXY_PROBE_FAILURES: u32 = 3;
/// 纯净度检查默认每 100 分钟执行一次。
pub const DEFAULT_QUALITY_INTERVAL_SECS: u64 = 6_000;
pub const DEFAULT_QUALITY_CONCURRENCY: usize = 10;
pub const DEFAULT_QUALITY_MIN_SCORE: u8 = 50;
const DEFAULT_IP_ECHO_URL: &str = "https://api.ipify.org?format=json";
const DEFAULT_QUALITY_URL: &str = "https://proxycheck.io/v2/{ip}?vpn=1&asn=1";
const SID_PLACEHOLDER: &str = "{sid}";

/// 代理健康状态
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyHealth {
    /// 尚未探测
    #[default]
    Unknown,
    /// 最近一次探测成功
    Healthy,
    /// 最近一次探测失败
    Unhealthy,
}

/// 出口 IP 纯净度检查状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyQualityStatus {
    #[default]
    Unknown,
    Clean,
    Dirty,
    Error,
}

/// 持久化的代理条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyEntry {
    pub id: u64,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 健康状态（健康检查结果）
    #[serde(default)]
    pub health: ProxyHealth,
    /// 最近一次成功探测的延迟（毫秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 最近一次探测时间（RFC3339）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    /// 连续探测失败计数（成功后清零）
    #[serde(default)]
    pub consecutive_failures: u32,
    /// 是否由健康检查自动禁用（区别于用户手动禁用）
    #[serde(default)]
    pub auto_disabled: bool,
    /// 经该代理访问公网后观测到的出口 IP。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_ip: Option<String>,
    /// 质量评分（0-100，越高越干净）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_score: Option<u8>,
    #[serde(default)]
    pub quality_status: ProxyQualityStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_quality_checked_at: Option<String>,
    /// 从代理 URL 推导出的可生成模板，例如 sid-{sid}。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_template: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 纯净度插件配置，单独落盘以避免破坏主 config.json 的兼容性。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyQualityConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_quality_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_quality_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_quality_min_score")]
    pub min_score: u8,
    /// 预检时至少保持的干净且出口 IP 唯一的备用数量。
    #[serde(default)]
    pub precheck_min_ips: usize,
    #[serde(default = "default_ip_echo_url")]
    pub ip_echo_url: String,
    #[serde(default = "default_quality_url")]
    pub quality_url: String,
    #[serde(default = "default_true")]
    pub auto_assign: bool,
}

fn default_quality_interval() -> u64 { DEFAULT_QUALITY_INTERVAL_SECS }
fn default_quality_concurrency() -> usize { DEFAULT_QUALITY_CONCURRENCY }
fn default_quality_min_score() -> u8 { DEFAULT_QUALITY_MIN_SCORE }
fn default_ip_echo_url() -> String { DEFAULT_IP_ECHO_URL.to_string() }
fn default_quality_url() -> String { DEFAULT_QUALITY_URL.to_string() }

impl Default for ProxyQualityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: DEFAULT_QUALITY_INTERVAL_SECS,
            concurrency: DEFAULT_QUALITY_CONCURRENCY,
            min_score: DEFAULT_QUALITY_MIN_SCORE,
            precheck_min_ips: 0,
            ip_echo_url: default_ip_echo_url(),
            quality_url: default_quality_url(),
            auto_assign: true,
        }
    }
}

impl ProxyQualityConfig {
    pub fn normalized(mut self) -> Self {
        self.interval_secs = self.interval_secs.clamp(60, 86_400);
        self.concurrency = self.concurrency.clamp(1, 64);
        self.min_score = self.min_score.min(100);
        self.precheck_min_ips = self.precheck_min_ips.min(100_000);
        if self.ip_echo_url.trim().is_empty() { self.ip_echo_url = default_ip_echo_url(); }
        if self.quality_url.trim().is_empty() { self.quality_url = default_quality_url(); }
        self
    }
}

/// 代理分配结果
pub enum GetUrlResult {
    /// 代理存在且已启用，返回 URL
    Ok(String),
    /// 代理不存在
    NotFound,
    /// 代理存在但已被禁用
    Disabled,
}

/// 一次全量健康检查的摘要
#[derive(Debug, Clone, Default)]
pub struct CheckSummary {
    /// 探测成功数
    pub healthy: usize,
    /// 探测失败数
    pub unhealthy: usize,
    /// 本轮新增的自动禁用数
    pub auto_disabled: usize,
}

/// 一次纯净度调度的统计结果。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityRunSummary {
    pub checked: usize,
    pub clean: usize,
    pub dirty: usize,
    pub errors: usize,
    pub disabled: usize,
    pub generated: usize,
}

/// 单个代理探测结果
enum ProbeResult {
    Ok { latency_ms: u32 },
    Err { error: String },
}

#[derive(Debug, Clone, Default)]
struct QualityResult {
    ip: Option<String>,
    score: Option<u8>,
    dirty: bool,
    reason: Option<String>,
}

pub struct ProxyPoolManager {
    entries: Mutex<Vec<ProxyEntry>>,
    // 仅需原子自增，不需要与 entries 联锁；约定独立使用，无锁顺序问题
    next_id: AtomicU64,
    path: Option<PathBuf>,
    /// TLS 后端，构建探测用 HTTP client 时需要
    tls_backend: TlsBackend,
    /// 可生成的代理模板，独立于当前已通过检查的条目保存。
    templates: Mutex<Vec<String>>,
    templates_path: Option<PathBuf>,
    quality_config: Mutex<ProxyQualityConfig>,
    quality_config_path: Option<PathBuf>,
}

/// 校验代理 URL 的 scheme 是否合法
fn validate_proxy_url(url: &str) -> anyhow::Result<()> {
    let valid_schemes = ["http://", "https://", "socks5://", "socks4://"];
    if !valid_schemes.iter().any(|s| url.starts_with(s)) {
        anyhow::bail!(
            "代理 URL scheme 无效，支持: http/https/socks4/socks5（收到: {}）",
            url
        );
    }
    // 简单检查 host:port 存在
    let after_scheme = valid_schemes
        .iter()
        .find(|s| url.starts_with(*s))
        .map(|s| &url[s.len()..])
        .unwrap_or(url);
    // after_scheme 可能是 user:pass@host:port 或 host:port
    let host_part = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    if !host_part.contains(':') {
        anyhow::bail!("代理 URL 缺少端口号: {}", url);
    }
    Ok(())
}

/// 把示例中的 `sid-XXXXXXXX` 变成可轮换模板；无 sid 的代理仍可手工使用，
/// 但不会被自动复制生成。
fn derive_template(url: &str) -> Option<String> {
    let bytes = url.as_bytes();
    let marker = b"sid-";
    let start = bytes.windows(marker.len()).position(|w| w.eq_ignore_ascii_case(marker))?;
    let value_start = start + marker.len();
    let value_end = bytes[value_start..]
        .iter()
        .position(|b| !b.is_ascii_alphanumeric())
        .map(|offset| value_start + offset)
        .unwrap_or(bytes.len());
    if value_end.saturating_sub(value_start) < 4 { return None; }
    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..value_start]);
    out.push_str(SID_PLACEHOLDER);
    out.push_str(&url[value_end..]);
    Some(out)
}

fn generate_from_template(template: &str) -> String {
    let sid: String = (0..8).map(|_| {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        ALPHABET[fastrand::usize(..ALPHABET.len())] as char
    }).collect();
    template.replace(SID_PLACEHOLDER, &sid)
}

impl ProxyPoolManager {
    pub fn new(path: Option<PathBuf>, tls_backend: TlsBackend) -> Self {
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<ProxyEntry>>(&s).ok())
            .unwrap_or_default();

        let templates_path = path.as_ref().map(|p| p.with_file_name("proxy_templates.json"));
        let mut templates = templates_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();
        for entry in &entries {
            if let Some(template) = entry.source_template.clone().or_else(|| derive_template(&entry.url)) {
                if !templates.iter().any(|existing| existing == &template) {
                    templates.push(template);
                }
            }
        }
        let quality_config_path = path.as_ref().map(|p| p.with_file_name("proxy_quality.json"));
        let quality_config = quality_config_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<ProxyQualityConfig>(&s).ok())
            .unwrap_or_default()
            .normalized();

        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;

        Self {
            entries: Mutex::new(entries),
            next_id: AtomicU64::new(next_id),
            path,
            tls_backend,
            templates: Mutex::new(templates),
            templates_path,
            quality_config: Mutex::new(quality_config),
            quality_config_path,
        }
    }

    pub fn list(&self) -> Vec<ProxyEntry> {
        self.entries.lock().clone()
    }

    pub fn add(&self, url: String, label: Option<String>) -> anyhow::Result<ProxyEntry> {
        let url = url.trim().to_string();
        if url.is_empty() {
            anyhow::bail!("代理 URL 不能为空");
        }
        validate_proxy_url(&url)?;

        let mut entries = self.entries.lock();

        if entries.iter().any(|e| e.url == url) {
            anyhow::bail!("代理 URL 已存在: {}", url);
        }

        let source_template = derive_template(&url);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = ProxyEntry {
            id,
            url,
            label,
            enabled: true,
            health: ProxyHealth::Unknown,
            latency_ms: None,
            last_checked_at: None,
            consecutive_failures: 0,
            auto_disabled: false,
            observed_ip: None,
            quality_score: None,
            quality_status: ProxyQualityStatus::Unknown,
            quality_reason: None,
            last_quality_checked_at: None,
            source_template: source_template,
        };
        self.remember_template(entry.source_template.clone());
        entries.push(entry.clone());
        drop(entries);

        self.persist()?;
        Ok(entry)
    }

    /// 批量添加：在单次加锁内完成所有插入，最后统一持久化一次
    pub fn batch_add(&self, urls: Vec<String>) -> (Vec<ProxyEntry>, Vec<String>) {
        let mut added = vec![];
        let mut errors = vec![];

        let mut entries = self.entries.lock();
        for url in urls {
            let url = url.trim().to_string();
            if url.is_empty() || url.starts_with('#') {
                continue;
            }
            if let Err(e) = validate_proxy_url(&url) {
                errors.push(e.to_string());
                continue;
            }
            if entries.iter().any(|e| e.url == url) {
                errors.push(format!("代理 URL 已存在: {}", url));
                continue;
            }
            let source_template = derive_template(&url);
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let entry = ProxyEntry {
                id,
                url,
                label: None,
                enabled: true,
                health: ProxyHealth::Unknown,
                latency_ms: None,
                last_checked_at: None,
                consecutive_failures: 0,
                auto_disabled: false,
                observed_ip: None,
                quality_score: None,
                quality_status: ProxyQualityStatus::Unknown,
                quality_reason: None,
                last_quality_checked_at: None,
                source_template,
            };
            if let Some(template) = entry.source_template.clone() {
                if !self.templates.lock().iter().any(|existing| existing == &template) {
                    self.templates.lock().push(template);
                }
            }
            entries.push(entry.clone());
            added.push(entry);
        }
        drop(entries);

        if !added.is_empty() {
            if let Err(e) = self.persist() {
                tracing::warn!("批量添加代理后持久化失败: {}", e);
            }
        }

        (added, errors)
    }

    pub fn delete(&self, id: u64) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let len_before = entries.len();
        entries.retain(|e| e.id != id);
        if entries.len() == len_before {
            anyhow::bail!("代理不存在: {}", id);
        }
        drop(entries);
        self.persist()?;
        Ok(())
    }

    /// 设置代理启用/禁用状态
    ///
    /// 用户手动启用时清除「健康检查自动禁用」标记与连续失败计数，
    /// 让该代理重新参与健康检查与分配。
    pub fn set_enabled(&self, id: u64, enabled: bool) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
        entry.enabled = enabled;
        if enabled {
            entry.auto_disabled = false;
            entry.consecutive_failures = 0;
        }
        drop(entries);
        self.persist()?;
        Ok(())
    }

    /// 获取代理 URL，区分"不存在"和"已禁用"两种情况
    pub fn get_url(&self, id: u64) -> GetUrlResult {
        match self.entries.lock().iter().find(|e| e.id == id) {
            None => GetUrlResult::NotFound,
            Some(e) if !e.enabled => GetUrlResult::Disabled,
            Some(e) => GetUrlResult::Ok(e.url.clone()),
        }
    }

    /// 获取所有「可用于分配」的代理 URL：已启用且非 Unhealthy
    pub fn assignable_urls(&self) -> Vec<String> {
        let quality_enabled = self.quality_config.lock().enabled;
        self.entries
            .lock()
            .iter()
            .filter(|e| {
                e.enabled
                    && e.health != ProxyHealth::Unhealthy
                    && (!quality_enabled || e.quality_status == ProxyQualityStatus::Clean)
            })
            .map(|e| e.url.clone())
            .collect()
    }

    fn persist(&self) -> anyhow::Result<()> {
        let path = match &self.path {
            Some(p) => p,
            None => return Ok(()),
        };
        let entries = self.entries.lock();
        let json = serde_json::to_string_pretty(&*entries)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

// ============ 健康检查 ============

impl ProxyPoolManager {
    /// 探测单个代理 URL 的连通性与延迟。
    ///
    /// 通过该代理请求 `PROXY_HEALTH_CHECK_URL`，成功（HTTP 2xx/3xx）即视为连通，
    /// 返回往返延迟；任何网络错误或非预期状态码视为失败。
    async fn probe_one(&self, url: &str) -> ProbeResult {
        let proxy = ProxyConfig::new(url);
        let client = match build_client(Some(&proxy), PROXY_PROBE_TIMEOUT_SECS, self.tls_backend) {
            Ok(c) => c,
            Err(e) => {
                return ProbeResult::Err {
                    error: format!("构建探测 client 失败: {}", e),
                };
            }
        };

        let started = Instant::now();
        match client.get(PROXY_HEALTH_CHECK_URL).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || status.is_redirection() {
                    ProbeResult::Ok {
                        latency_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
                    }
                } else {
                    ProbeResult::Err {
                        error: format!("探测端点返回非预期状态: {}", status),
                    }
                }
            }
            Err(e) => ProbeResult::Err {
                error: e.to_string(),
            },
        }
    }

    /// 将一次探测结果回写到指定条目，并按需触发自动禁用。
    ///
    /// 返回 `(变为不健康, 本次新自动禁用)` 供摘要统计。
    fn apply_probe_result(entry: &mut ProxyEntry, result: &ProbeResult) -> (bool, bool) {
        entry.last_checked_at = Some(chrono::Utc::now().to_rfc3339());
        match result {
            ProbeResult::Ok { latency_ms } => {
                entry.health = ProxyHealth::Healthy;
                entry.latency_ms = Some(*latency_ms);
                entry.consecutive_failures = 0;
                (false, false)
            }
            ProbeResult::Err { error } => {
                entry.health = ProxyHealth::Unhealthy;
                entry.latency_ms = None;
                entry.consecutive_failures += 1;
                tracing::warn!(
                    "代理 #{} 探测失败（{}/{}）: {}",
                    entry.id,
                    entry.consecutive_failures,
                    MAX_PROXY_PROBE_FAILURES,
                    error
                );
                let mut newly_disabled = false;
                if entry.consecutive_failures >= MAX_PROXY_PROBE_FAILURES && entry.enabled {
                    entry.enabled = false;
                    entry.auto_disabled = true;
                    newly_disabled = true;
                    tracing::error!(
                        "代理 #{} 连续探测失败 {} 次，已自动禁用",
                        entry.id,
                        entry.consecutive_failures
                    );
                }
                (true, newly_disabled)
            }
        }
    }

    /// 全量健康检查：并发探测所有「已启用」代理，回写结果并持久化一次。
    ///
    /// 仅探测当前 enabled 的条目；用户/自动禁用的条目跳过（手动重新启用会清零计数）。
    pub async fn check_all(&self) -> CheckSummary {
        // 快照待探测的 (id, url)，避免长时间持锁
        let targets: Vec<(u64, String)> = self
            .entries
            .lock()
            .iter()
            .filter(|e| e.enabled)
            .map(|e| (e.id, e.url.clone()))
            .collect();

        if targets.is_empty() {
            return CheckSummary::default();
        }

        let probes = targets
            .iter()
            .map(|(id, url)| async move { (*id, self.probe_one(url).await) });
        let results = futures::future::join_all(probes).await;

        let mut summary = CheckSummary::default();
        {
            let mut entries = self.entries.lock();
            for (id, result) in &results {
                if let Some(entry) = entries.iter_mut().find(|e| e.id == *id) {
                    let (unhealthy, newly_disabled) = Self::apply_probe_result(entry, result);
                    if unhealthy {
                        summary.unhealthy += 1;
                    } else {
                        summary.healthy += 1;
                    }
                    if newly_disabled {
                        summary.auto_disabled += 1;
                    }
                }
            }
        }

        if let Err(e) = self.persist() {
            tracing::warn!("健康检查后持久化失败: {}", e);
        }
        summary
    }

    /// 单个代理即时探测（供 UI「测试」按钮调用），回写结果并持久化。
    pub async fn check_one(&self, id: u64) -> anyhow::Result<ProxyEntry> {
        let url = self
            .entries
            .lock()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.url.clone())
            .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;

        let result = self.probe_one(&url).await;

        let entry = {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
            Self::apply_probe_result(entry, &result);
            entry.clone()
        };

        self.persist()?;
        Ok(entry)
    }

    fn remember_template(&self, template: Option<String>) {
        let Some(template) = template else { return };
        let mut templates = self.templates.lock();
        if templates.iter().any(|existing| existing == &template) { return; }
        templates.push(template);
        drop(templates);
        if let Err(e) = self.persist_templates() {
            tracing::warn!("保存代理生成模板失败: {}", e);
        }
    }

    fn persist_templates(&self) -> anyhow::Result<()> {
        let Some(path) = &self.templates_path else { return Ok(()) };
        let templates = self.templates.lock();
        std::fs::write(path, serde_json::to_string_pretty(&*templates)?)?;
        Ok(())
    }

    fn persist_quality_config(&self) -> anyhow::Result<()> {
        let Some(path) = &self.quality_config_path else { return Ok(()) };
        let config = self.quality_config.lock();
        std::fs::write(path, serde_json::to_string_pretty(&*config)?)?;
        Ok(())
    }

    pub fn quality_config(&self) -> ProxyQualityConfig {
        self.quality_config.lock().clone()
    }

    pub fn set_quality_config(&self, config: ProxyQualityConfig) -> anyhow::Result<ProxyQualityConfig> {
        let config = config.normalized();
        *self.quality_config.lock() = config.clone();
        self.persist_quality_config()?;
        Ok(config)
    }

    async fn inspect_quality(&self, url: &str, config: &ProxyQualityConfig) -> anyhow::Result<QualityResult> {
        let proxy = ProxyConfig::new(url);
        let via_proxy = build_client(Some(&proxy), PROXY_PROBE_TIMEOUT_SECS, self.tls_backend)?;
        let ip_response = via_proxy.get(&config.ip_echo_url).send().await?;
        if !ip_response.status().is_success() {
            anyhow::bail!("出口 IP 服务返回 {}", ip_response.status());
        }
        let ip_body = ip_response.text().await?;
        let ip = serde_json::from_str::<Value>(&ip_body)
            .ok()
            .and_then(|v| v.get("ip").and_then(Value::as_str).map(str::to_owned))
            .or_else(|| {
                let value = ip_body.trim();
                (!value.is_empty() && value.len() <= 128).then(|| value.to_string())
            })
            .ok_or_else(|| anyhow::anyhow!("出口 IP 服务响应中没有 ip 字段"))?;

        let quality_url = config.quality_url.replace("{ip}", &ip);
        let direct = build_client(None, PROXY_PROBE_TIMEOUT_SECS, self.tls_backend)?;
        let quality_response = direct.get(quality_url).send().await?;
        if !quality_response.status().is_success() {
            anyhow::bail!("纯净度服务返回 {}", quality_response.status());
        }
        let body: Value = quality_response.json().await?;
        let details = body.get(&ip).or_else(|| body.get("data")).unwrap_or(&body);
        let score = details
            .get("score").or_else(|| details.get("risk"))
            .and_then(parse_score)
            .map(|value| if details.get("score").is_some() { value } else { 100u8.saturating_sub(value) });
        let proxy_flag = quality_flag(details, &["proxy", "is_proxy"]);
        let vpn = quality_flag(details, &["vpn", "is_vpn"]);
        let tor = quality_flag(details, &["tor", "is_tor"]);
        let hosting = quality_flag(details, &["hosting", "is_hosting", "datacenter"]);
        let mut reasons = Vec::new();
        if proxy_flag { reasons.push("proxy"); }
        if vpn { reasons.push("vpn"); }
        if tor { reasons.push("tor"); }
        if hosting { reasons.push("hosting"); }
        if let Some(score) = score.filter(|value| *value < config.min_score) {
            reasons.push("score_low");
            return Ok(QualityResult { ip: Some(ip), score: Some(score), dirty: true, reason: Some(reasons.join(",")) });
        }
        Ok(QualityResult {
            ip: Some(ip),
            score,
            dirty: !reasons.is_empty(),
            reason: (!reasons.is_empty()).then(|| reasons.join(",")),
        })
    }

    pub async fn quality_check_urls(&self, only_urls: Option<&std::collections::HashSet<String>>) -> QualityRunSummary {
        let config = self.quality_config();
        let targets: Vec<(u64, String)> = self.entries.lock().iter()
            .filter(|entry| entry.enabled && only_urls.map(|urls| urls.contains(&entry.url)).unwrap_or(true))
            .map(|entry| (entry.id, entry.url.clone()))
            .collect();
        if targets.is_empty() { return QualityRunSummary::default(); }

        let concurrency = config.concurrency.clamp(1, 64);
        let results: Vec<(u64, anyhow::Result<QualityResult>)> = stream::iter(targets)
            .map(|(id, url)| {
                let config = config.clone();
                async move { (id, self.inspect_quality(&url, &config).await) }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        let mut summary = QualityRunSummary { checked: results.len(), ..Default::default() };
        let mut observed_ips: HashMap<String, u64> = HashMap::new();
        {
            let mut entries = self.entries.lock();
            for (id, result) in results {
                let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else { continue };
                entry.last_quality_checked_at = Some(chrono::Utc::now().to_rfc3339());
                match result {
                    Ok(result) => {
                        entry.observed_ip = result.ip.clone();
                        entry.quality_score = result.score;
                        entry.quality_reason = result.reason.clone();
                        if result.dirty {
                            entry.quality_status = ProxyQualityStatus::Dirty;
                            if entry.enabled { entry.enabled = false; entry.auto_disabled = true; summary.disabled += 1; }
                            summary.dirty += 1;
                        } else if let Some(ip) = result.ip {
                            if observed_ips.insert(ip, entry.id).is_some() {
                                entry.quality_status = ProxyQualityStatus::Dirty;
                                entry.quality_reason = Some("duplicate_exit_ip".to_string());
                                if entry.enabled { entry.enabled = false; entry.auto_disabled = true; summary.disabled += 1; }
                                summary.dirty += 1;
                            } else {
                                entry.quality_status = ProxyQualityStatus::Clean;
                                summary.clean += 1;
                            }
                        } else {
                            entry.quality_status = ProxyQualityStatus::Error;
                            entry.quality_reason = Some("missing_exit_ip".to_string());
                            summary.errors += 1;
                        }
                    }
                    Err(error) => {
                        entry.quality_status = ProxyQualityStatus::Error;
                        entry.quality_reason = Some(error.to_string());
                        summary.errors += 1;
                    }
                }
            }
        }
        if let Err(error) = self.persist() { tracing::warn!("纯净度检查后保存代理池失败: {}", error); }
        summary
    }

    /// 从已记录的 sid 模板生成候选链接；候选先入池，随后由质量检查决定是否启用。
    pub fn generate_candidates(&self, count: usize) -> usize {
        if count == 0 { return 0; }
        let templates = self.templates.lock().clone();
        if templates.is_empty() { return 0; }
        let existing: std::collections::HashSet<String> = self.entries.lock().iter().map(|entry| entry.url.clone()).collect();
        let mut generated = 0;
        for index in 0..count.saturating_mul(4).max(count) {
            let url = generate_from_template(&templates[index % templates.len()]);
            if existing.contains(&url) { continue; }
            if self.add(url, Some("auto-generated".to_string())).is_ok() { generated += 1; }
            if generated >= count { break; }
        }
        generated
    }

    /// 取尚未参与本轮检查的备用链接，数量按缺口限制，避免无意义地探测整个池。
    pub fn candidate_urls(&self, excluded: &std::collections::HashSet<String>, limit: usize) -> Vec<String> {
        self.entries.lock().iter()
            .filter(|entry| entry.enabled && !excluded.contains(&entry.url))
            .map(|entry| entry.url.clone())
            .take(limit)
            .collect()
    }

    pub fn clean_entries(&self) -> Vec<ProxyEntry> {
        self.entries.lock().iter()
            .filter(|entry| entry.enabled && entry.health != ProxyHealth::Unhealthy && entry.quality_status == ProxyQualityStatus::Clean)
            .cloned().collect()
    }
}

fn parse_score(value: &Value) -> Option<u8> {
    value.as_u64().or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok())).map(|value| value.min(100) as u8)
}

fn quality_flag(value: &Value, keys: &[&str]) -> bool {
    keys.iter().find_map(|key| value.get(*key)).is_some_and(|value| match value {
        Value::Bool(flag) => *flag,
        Value::String(text) => matches!(text.to_ascii_lowercase().as_str(), "yes" | "true" | "1" | "y"),
        Value::Number(number) => number.as_u64().is_some_and(|number| number > 0),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(url: &str) -> ProxyEntry {
        ProxyEntry {
            id: 1,
            url: url.to_string(),
            label: None,
            enabled: true,
            health: ProxyHealth::Unknown,
            latency_ms: None,
            last_checked_at: None,
            consecutive_failures: 0,
            auto_disabled: false,
            observed_ip: None,
            quality_score: None,
            quality_status: ProxyQualityStatus::Unknown,
            quality_reason: None,
            last_quality_checked_at: None,
            source_template: derive_template(url),
        }
    }

    #[test]
    fn old_json_without_new_fields_deserializes() {
        // 旧格式 JSON 只有 id/url/label/enabled，新字段应由 serde default 补全
        let json = r#"[{"id":1,"url":"socks5://127.0.0.1:1080","enabled":true}]"#;
        let entries: Vec<ProxyEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.health, ProxyHealth::Unknown);
        assert_eq!(e.latency_ms, None);
        assert_eq!(e.consecutive_failures, 0);
        assert!(!e.auto_disabled);
        assert_eq!(e.quality_status, ProxyQualityStatus::Unknown);
        assert_eq!(e.source_template, None);
    }

    #[test]
    fn sid_url_derives_generation_template() {
        let url = "socks5://user:pass@host:3010/path-sid-AbC12345-t-5";
        assert_eq!(derive_template(url), Some("socks5://user:pass@host:3010/path-sid-{sid}-t-5".to_string()));
    }

    #[test]
    fn quality_config_normalizes_limits_and_defaults() {
        let config = ProxyQualityConfig {
            interval_secs: 1,
            concurrency: 1000,
            min_score: 255,
            ip_echo_url: String::new(),
            quality_url: String::new(),
            ..Default::default()
        }.normalized();
        assert_eq!(config.interval_secs, 60);
        assert_eq!(config.concurrency, 64);
        assert_eq!(config.min_score, 100);
        assert_eq!(config.ip_echo_url, DEFAULT_IP_ECHO_URL);
        assert_eq!(config.quality_url, DEFAULT_QUALITY_URL);
    }

    #[test]
    fn probe_failure_increments_and_auto_disables_at_threshold() {
        let mut entry = make_entry("socks5://127.0.0.1:1080");
        let err = ProbeResult::Err {
            error: "connection refused".to_string(),
        };
        // 前两次失败：计数累加，仍启用
        for n in 1..MAX_PROXY_PROBE_FAILURES {
            let (unhealthy, disabled) = ProxyPoolManager::apply_probe_result(&mut entry, &err);
            assert!(unhealthy);
            assert!(!disabled);
            assert_eq!(entry.consecutive_failures, n);
            assert!(entry.enabled);
            assert!(!entry.auto_disabled);
        }
        // 第 N 次失败：自动禁用
        let (_, disabled) = ProxyPoolManager::apply_probe_result(&mut entry, &err);
        assert!(disabled);
        assert_eq!(entry.consecutive_failures, MAX_PROXY_PROBE_FAILURES);
        assert!(!entry.enabled);
        assert!(entry.auto_disabled);
    }

    #[test]
    fn probe_success_clears_failures_and_marks_healthy() {
        let mut entry = make_entry("socks5://127.0.0.1:1080");
        entry.consecutive_failures = 2;
        entry.health = ProxyHealth::Unhealthy;
        let ok = ProbeResult::Ok { latency_ms: 123 };
        let (unhealthy, disabled) = ProxyPoolManager::apply_probe_result(&mut entry, &ok);
        assert!(!unhealthy);
        assert!(!disabled);
        assert_eq!(entry.consecutive_failures, 0);
        assert_eq!(entry.health, ProxyHealth::Healthy);
        assert_eq!(entry.latency_ms, Some(123));
    }

    #[test]
    fn set_enabled_true_clears_auto_disable_state() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let entry = mgr.add("socks5://127.0.0.1:1080".to_string(), None).unwrap();
        // 模拟自动禁用状态
        {
            let mut entries = mgr.entries.lock();
            let e = entries.iter_mut().find(|e| e.id == entry.id).unwrap();
            e.enabled = false;
            e.auto_disabled = true;
            e.consecutive_failures = MAX_PROXY_PROBE_FAILURES;
        }
        mgr.set_enabled(entry.id, true).unwrap();
        let list = mgr.list();
        let e = list.iter().find(|e| e.id == entry.id).unwrap();
        assert!(e.enabled);
        assert!(!e.auto_disabled);
        assert_eq!(e.consecutive_failures, 0);
    }
}
