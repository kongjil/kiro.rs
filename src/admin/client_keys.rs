//! 客户端 API Key 管理
//!
//! 管理中转站下发的客户端 Key。生成值以 `sk-` 开头；鉴权不校验前缀，只按完整值匹配。
//!
//! 与上游 Kiro 凭据（`KiroCredentials`，`ksk_*`）相互独立：
//! - 上游凭据池：服务对接 Kiro 的"出口"
//! - 客户端 Key：中转站对外的"入口"
//!
//! 持久化为 `client_api_keys.json`（与 `credentials.json` 同目录）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

/// 单条客户端 Key
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientKey {
    pub id: u64,
    /// 明文 Key（中转站场景，校验需原值，不做 hash）
    pub key: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub total_calls: u64,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    /// 累计 credit 计费量（meteringEvent.usage 累加）
    #[serde(default)]
    pub total_credits: f64,
    /// 累计积分（credit）使用上限。None 表示不限制。
    ///
    /// 达到上限后该 Key 的后续请求会被鉴权层拒绝（HTTP 429）。上限基于累计
    /// `total_credits`，通过「重置统计」清零后可重新计费。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_credits: Option<f64>,
    /// 绑定的账号分组名（可选）
    ///
    /// 设置后，用该 Key 发起的请求只会调度到 groups 包含此分组名的上游账号（严格隔离）。
    /// None 表示不绑定分组，可使用全部账号。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 缓存计费系数（0.0~1.0）。None 等价于 1.0（满缓存）。
    ///
    /// 只影响本 Key 上报给下游的 usage 拆分：最终缓存量（本地 CacheMeter 模拟值或
    /// 上游 tokenUsage 精确值）乘以此系数，被砍掉的部分转入 input 全价计；
    /// 0 表示缓存 token 归零。不影响发往上游的真实请求与 credit 消耗。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ratio: Option<f64>,
    /// 普通 input 的计费用量倍率；空值表示 raw 模式下的默认 1.0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_multiplier: Option<f64>,
    /// cache creation/write 的计费用量倍率。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_multiplier: Option<f64>,
    /// cache read 的计费用量倍率。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_multiplier: Option<f64>,
    /// 目标有效缓存命中率（0~1）；设置后动态求解 read multiplier。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_effective_hit_rate: Option<f64>,
    /// 下游卖出倍率；只用于内部销售统计，不改变请求响应。
    #[serde(default, skip_serializing_if = "is_one_f64")]
    pub sell_multiplier: f64,
    /// 系统 Key（由 config.json apiKey 同步，不可删除、可轮换）。
    /// 老数据无此字段，默认 false。
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_system: bool,
}

/// 鉴权结果：区分「命中」「超额」「未命中」，供中间件返回不同 HTTP 状态。
#[derive(Debug, Clone, PartialEq)]
pub enum KeyAuth {
    /// 命中且未超额，携带 Key id
    Ok(u64),
    /// 命中但已达积分上限
    OverLimit { id: u64, used: f64, limit: f64 },
    /// 未匹配到任何启用的 Key
    NotFound,
}

/// `by_key` 仅用于判重；鉴权扫描 `entries` 并做常量时间比较。
pub struct ClientKeyManager {
    inner: RwLock<Inner>,
    path: Option<PathBuf>,
}

struct Inner {
    entries: HashMap<u64, ClientKey>,
    by_key: HashMap<String, u64>,
    next_id: u64,
}

impl ClientKeyManager {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                entries: HashMap::new(),
                by_key: HashMap::new(),
                next_id: 1,
            }),
            path: None,
        }
    }

    /// 从文件加载（不存在时返回空管理器）
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let entries: Vec<ClientKey> = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            if content.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&content)?
            }
        } else {
            Vec::new()
        };

        let mut by_key = HashMap::with_capacity(entries.len());
        let mut by_id = HashMap::with_capacity(entries.len());
        let mut max_id = 0u64;
        for ck in entries {
            max_id = max_id.max(ck.id);
            by_key.insert(ck.key.clone(), ck.id);
            by_id.insert(ck.id, ck);
        }

        Ok(Self {
            inner: RwLock::new(Inner {
                entries: by_id,
                by_key,
                next_id: max_id + 1,
            }),
            path: Some(path),
        })
    }

    fn save_locked(&self, inner: &Inner) {
        let path = match &self.path {
            Some(p) => p,
            None => return,
        };
        let mut list: Vec<&ClientKey> = inner.entries.values().collect();
        list.sort_by_key(|k| k.id);
        match serde_json::to_string_pretty(&list) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("写入客户端 Key 文件失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化客户端 Key 失败: {}", e),
        }
    }

    /// 列表（按 id 升序）
    pub fn list(&self) -> Vec<ClientKey> {
        let inner = self.inner.read();
        let mut list: Vec<ClientKey> = inner.entries.values().cloned().collect();
        list.sort_by_key(|k| k.id);
        list
    }

    /// 生成并保存新 Key。
    pub fn create(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
    ) -> ClientKey {
        self.create_with_key(name, description, group, generate_client_key())
    }

    /// 使用指定明文创建 Key；明文已存在时返回原条目。
    pub fn create_with_key(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
        plaintext: String,
    ) -> ClientKey {
        let mut inner = self.inner.write();
        if let Some(&id) = inner.by_key.get(&plaintext) {
            return inner.entries.get(&id).cloned().expect("by_key 与 entries 应一致");
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let entry = ClientKey {
            id,
            key: plaintext.clone(),
            name,
            description,
            disabled: false,
            created_at: Utc::now().to_rfc3339(),
            last_used_at: None,
            total_calls: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
            total_credits: 0.0,
            max_credits: None,
            group: group.filter(|g| !g.trim().is_empty()),
            cache_ratio: None,
            input_multiplier: None,
            cache_creation_multiplier: None,
            cache_read_multiplier: None,
            target_effective_hit_rate: None,
            sell_multiplier: 1.0,
            is_system: false,
        };
        inner.by_key.insert(plaintext, id);
        inner.entries.insert(id, entry.clone());
        self.save_locked(&inner);
        entry
    }

    /// 将 `config.apiKey` 同步为唯一的 `id=0` 系统 Key。配置值变化时保留元数据与统计、
    /// 重新启用新值，并删除与新旧明文冲突的非系统条目，使旧值立即失效。
    pub fn sync_system_key(&self, name: String, description: Option<String>, plaintext: String) {
        let mut inner = self.inner.write();
        let previous_key = inner.entries.get(&0).map(|entry| entry.key.clone());
        let mut changed = false;

        if let Some(entry) = inner.entries.get_mut(&0) {
            if entry.key != plaintext {
                entry.key = plaintext.clone();
                entry.disabled = false;
                changed = true;
            }
            if !entry.is_system {
                entry.is_system = true;
                changed = true;
            }
        } else {
            inner.entries.insert(
                0,
                ClientKey {
                    id: 0,
                    key: plaintext.clone(),
                    name,
                    description,
                    disabled: false,
                    created_at: Utc::now().to_rfc3339(),
                    last_used_at: None,
                    total_calls: 0,
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    total_cache_creation_tokens: 0,
                    total_cache_read_tokens: 0,
                    total_credits: 0.0,
                    max_credits: None,
                    group: None,
                    cache_ratio: None,
                    input_multiplier: None,
                    cache_creation_multiplier: None,
                    cache_read_multiplier: None,
                    target_effective_hit_rate: None,
                    sell_multiplier: 1.0,
                    is_system: true,
                },
            );
            changed = true;
        }

        let entries_before = inner.entries.len();
        inner.entries.retain(|id, entry| {
            *id == 0
                || (entry.key != plaintext
                    && previous_key
                        .as_deref()
                        .map(|old_key| entry.key != old_key)
                        .unwrap_or(true))
        });
        changed |= inner.entries.len() != entries_before;

        for (id, entry) in inner.entries.iter_mut() {
            if *id != 0 && entry.is_system {
                entry.is_system = false;
                changed = true;
            }
        }

        let by_key: HashMap<String, u64> = inner
            .entries
            .iter()
            .map(|(id, entry)| (entry.key.clone(), *id))
            .collect();
        changed |= inner.by_key != by_key;
        inner.by_key = by_key;

        if changed {
            self.save_locked(&inner);
        }
    }

    pub fn delete(&self, id: u64) -> bool {
        let mut inner = self.inner.write();
        // 系统 Key 拒绝删除
        if inner.entries.get(&id).map(|e| e.is_system).unwrap_or(false) {
            return false;
        }
        let removed = match inner.entries.remove(&id) {
            Some(e) => {
                inner.by_key.remove(&e.key);
                true
            }
            None => false,
        };
        if removed {
            self.save_locked(&inner);
        }
        removed
    }

    pub fn set_disabled(&self, id: u64, disabled: bool) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.disabled = disabled;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    pub fn update_meta(
        &self,
        id: u64,
        name: Option<String>,
        description: Option<Option<String>>,
        group: Option<Option<String>>,
    ) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                if let Some(n) = name {
                    e.name = n;
                }
                if let Some(d) = description {
                    e.description = d;
                }
                if let Some(g) = group {
                    e.group = g.filter(|s| !s.trim().is_empty());
                }
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 返回指定 Key 绑定的分组名（None 表示未绑定或 Key 不存在）
    pub fn group_of(&self, id: u64) -> Option<String> {
        let legacy = self.inner.read().entries.get(&id).and_then(|e| e.group.clone());
        Some(normalize_group_scope(None, legacy))
    }

    /// 返回指定 Key 的有效缓存计费系数（未设置或 Key 不存在时为 1.0）
    pub fn cache_ratio_of(&self, id: u64) -> f64 {
        normalize_cache_ratio(self.inner.read().entries.get(&id).and_then(|e| e.cache_ratio))
    }

    /// 返回该 Key 的完整计费策略字段；缺省值保持 raw 行为。
    pub fn billing_policy_of(
        &self,
        id: u64,
    ) -> (
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        f64,
    ) {
        let inner = self.inner.read();
        let Some(entry) = inner.entries.get(&id) else {
            return (None, None, None, None, None, 1.0);
        };
        (
            entry.cache_ratio,
            entry.input_multiplier,
            entry.cache_creation_multiplier,
            entry.cache_read_multiplier,
            entry.target_effective_hit_rate,
            normalize_sell_multiplier(entry.sell_multiplier),
        )
    }

    pub fn sell_multiplier_of(&self, id: u64) -> f64 {
        self.inner
            .read()
            .entries
            .get(&id)
            .map(|entry| normalize_sell_multiplier(entry.sell_multiplier))
            .unwrap_or(1.0)
    }

    /// 列出所有当前被引用的分组名（仅去重，不带计数）。
    pub fn used_group_names(&self) -> Vec<String> {
        let inner = self.inner.read();
        let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in inner.entries.values() {
            if let Some(g) = &e.group {
                for group in split_group_scope(Some(g)) { set.insert(group); }
            }
        }
        let mut list: Vec<String> = set.into_iter().collect();
        list.sort();
        list
    }

    /// 统计指定分组被多少把 Key 绑定（用于分组管理页 / 删除前提示）。
    pub fn count_with_group(&self, group: &str) -> usize {
        self.inner
            .read()
            .entries
            .values()
            .filter(|e| split_group_scope(e.group.as_deref()).iter().any(|g| g == group))
            .count()
    }

    /// 指定 id 的 Key 是否为系统 Key（不存在也返回 false）。
    pub fn is_system(&self, id: u64) -> bool {
        self.inner
            .read()
            .entries
            .get(&id)
            .map(|e| e.is_system)
            .unwrap_or(false)
    }

    /// 把所有引用 `old` 的 Key 的 group 字段改为 `new`（分组改名级联用）。
    /// 返回受影响的 Key 数。
    pub fn rename_group(&self, old: &str, new: &str) -> usize {
        let mut inner = self.inner.write();
        let mut affected = 0usize;
        for entry in inner.entries.values_mut() {
            let groups = split_group_scope(entry.group.as_deref());
            if groups.iter().any(|g| g == old) {
                entry.group = Some(
                    groups
                        .into_iter()
                        .map(|g| if g == old { new.to_string() } else { g })
                        .collect::<Vec<_>>()
                        .join("|"),
                );
                affected += 1;
            }
        }
        if affected > 0 {
            self.save_locked(&inner);
        }
        affected
    }

    /// 把所有引用 `name` 的 Key 的 group 字段清空（强删分组级联用）。
    /// 返回受影响的 Key 数。
    pub fn clear_group(&self, name: &str) -> usize {
        let mut inner = self.inner.write();
        let mut affected = 0usize;
        for entry in inner.entries.values_mut() {
            let groups = split_group_scope(entry.group.as_deref());
            if groups.iter().any(|g| g == name) {
                let kept: Vec<String> = groups.into_iter().filter(|g| g != name).collect();
                entry.group = Some(if kept.is_empty() { "default".to_string() } else { kept.join("|") });
                affected += 1;
            }
        }
        if affected > 0 {
            self.save_locked(&inner);
        }
        affected
    }

    /// 生成新明文并保留 id、元数据、分组、统计及状态；旧明文立即失效。
    /// 系统 Key 的调用方必须同步 `config.apiKey`，否则重启时配置值会覆盖轮换结果。
    pub fn rotate(&self, id: u64) -> Option<ClientKey> {
        let new_key = generate_client_key();
        let mut inner = self.inner.write();
        let old_key = inner.entries.get(&id).map(|e| e.key.clone())?;
        inner.by_key.remove(&old_key);
        let entry = inner.entries.get_mut(&id)?;
        entry.key = new_key.clone();
        let snapshot = entry.clone();
        inner.by_key.insert(new_key, id);
        self.save_locked(&inner);
        Some(snapshot)
    }

    /// 重置计数（保留 Key 与名称）
    pub fn reset_stats(&self, id: u64) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.total_calls = 0;
                e.total_input_tokens = 0;
                e.total_output_tokens = 0;
                e.total_cache_creation_tokens = 0;
                e.total_cache_read_tokens = 0;
                e.total_credits = 0.0;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 不校验前缀，常量时间匹配所有启用 Key；命中后更新使用记录。
    ///
    /// 仅返回命中的 Key id（不含额度判定）。需要额度拦截时用 [`Self::verify_and_touch_ex`]。
    #[allow(dead_code)] // 保留的兼容接口，生产路径已切换到 verify_and_touch_ex
    pub fn verify_and_touch(&self, presented: &str) -> Option<u64> {
        match self.verify_and_touch_ex(presented) {
            KeyAuth::Ok(id) => Some(id),
            // 兼容旧语义：超额与未命中都视为 None
            KeyAuth::OverLimit { .. } | KeyAuth::NotFound => None,
        }
    }

    /// 常量时间匹配所有启用 Key；命中后若已达积分上限则返回 [`KeyAuth::OverLimit`]，
    /// 否则累加调用次数并返回 [`KeyAuth::Ok`]。
    pub fn verify_and_touch_ex(&self, presented: &str) -> KeyAuth {
        let mut inner = self.inner.write();
        let mut hit_id: Option<u64> = None;
        for (id, ck) in inner.entries.iter() {
            if ck.disabled {
                continue;
            }
            if ck.key.as_bytes().ct_eq(presented.as_bytes()).into() {
                hit_id = Some(*id);
                // 不 break，继续完整扫描以保持常量时间
            }
        }
        let Some(id) = hit_id else {
            return KeyAuth::NotFound;
        };
        if let Some(entry) = inner.entries.get_mut(&id) {
            // 达到积分上限：拒绝，不累加调用次数
            if let Some(limit) = entry.max_credits {
                if entry.total_credits >= limit {
                    return KeyAuth::OverLimit {
                        id,
                        used: entry.total_credits,
                        limit,
                    };
                }
            }
            entry.total_calls += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
        // 不在每次请求都落盘（高频写入），由 record_usage / 定期 flush 持久化
        KeyAuth::Ok(id)
    }

    /// 设置或清除单个 Key 的积分使用上限。`None` 表示取消限制。
    ///
    /// 传入 `Some(v)` 时会归一为非负有限值；小于 0 或非有限值一律拒绝为无效。
    /// 返回 `false` 表示 Key 不存在或入参非法。
    pub fn set_max_credits(&self, id: u64, max_credits: Option<f64>) -> bool {
        if let Some(v) = max_credits {
            if !v.is_finite() || v < 0.0 {
                return false;
            }
        }
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.max_credits = max_credits;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 设置或清除单个 Key 的缓存计费系数。`None` 或 `>= 1.0` 表示满缓存（清除字段）。
    ///
    /// 非有限值拒绝为无效，其余值归一到 0.0~1.0。返回 `false` 表示 Key 不存在或入参非法。
    pub fn set_cache_ratio(&self, id: u64, cache_ratio: Option<f64>) -> bool {
        if cache_ratio.is_some_and(|v| !v.is_finite()) {
            return false;
        }
        let cache_ratio = cache_ratio
            .map(|v| v.clamp(0.0, 1.0))
            .filter(|v| *v < 1.0);
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.cache_ratio = cache_ratio;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 设置 Key 的新计费策略字段。`None` 表示回到 raw 模式默认值。
    pub fn set_billing_policy(
        &self,
        id: u64,
        input_multiplier: Option<f64>,
        cache_creation_multiplier: Option<f64>,
        cache_read_multiplier: Option<f64>,
        target_effective_hit_rate: Option<f64>,
        sell_multiplier: Option<f64>,
    ) -> bool {
        let valid_multiplier = |v: Option<f64>| {
            v.map(|x| x.is_finite() && x >= 0.0).unwrap_or(true)
        };
        let valid_target = |v: Option<f64>| {
            v.map(|x| x.is_finite() && (0.0..=1.0).contains(&x))
                .unwrap_or(true)
        };
        if !valid_multiplier(input_multiplier)
            || !valid_multiplier(cache_creation_multiplier)
            || !valid_multiplier(cache_read_multiplier)
            || !valid_multiplier(sell_multiplier)
            || !valid_target(target_effective_hit_rate)
        {
            return false;
        }
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(entry) => {
                entry.input_multiplier = input_multiplier;
                entry.cache_creation_multiplier = cache_creation_multiplier;
                entry.cache_read_multiplier = cache_read_multiplier;
                entry.target_effective_hit_rate = target_effective_hit_rate;
                if let Some(value) = sell_multiplier {
                    entry.sell_multiplier = value;
                }
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 按字段增量更新计费策略；外层 None 表示保持，内层 None 表示清除该字段。
    pub fn update_billing_policy(
        &self,
        id: u64,
        input_multiplier: Option<Option<f64>>,
        cache_creation_multiplier: Option<Option<f64>>,
        cache_read_multiplier: Option<Option<f64>>,
        target_effective_hit_rate: Option<Option<f64>>,
        sell_multiplier: Option<Option<f64>>,
    ) -> bool {
        let valid_multiplier = |v: Option<Option<f64>>| {
            v.flatten()
                .map(|x| x.is_finite() && x >= 0.0)
                .unwrap_or(true)
        };
        let valid_target = |v: Option<Option<f64>>| {
            v.flatten()
                .map(|x| x.is_finite() && (0.0..=1.0).contains(&x))
                .unwrap_or(true)
        };
        if !valid_multiplier(input_multiplier)
            || !valid_multiplier(cache_creation_multiplier)
            || !valid_multiplier(cache_read_multiplier)
            || !valid_multiplier(sell_multiplier)
            || !valid_target(target_effective_hit_rate)
        {
            return false;
        }
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(entry) => {
                if let Some(value) = input_multiplier { entry.input_multiplier = value; }
                if let Some(value) = cache_creation_multiplier { entry.cache_creation_multiplier = value; }
                if let Some(value) = cache_read_multiplier { entry.cache_read_multiplier = value; }
                if let Some(value) = target_effective_hit_rate { entry.target_effective_hit_rate = value; }
                if let Some(value) = sell_multiplier { entry.sell_multiplier = value.unwrap_or(1.0); }
                true
            }
            None => false,
        };
        if updated { self.save_locked(&inner); }
        updated
    }

    /// 在请求结束时累计 Token 用量并落盘
    pub fn record_usage(
        &self,
        id: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
        credits: f64,
    ) {
        let mut inner = self.inner.write();
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.total_input_tokens += input_tokens;
            entry.total_output_tokens += output_tokens;
            entry.total_cache_creation_tokens += cache_creation_tokens;
            entry.total_cache_read_tokens += cache_read_tokens;
            if credits.is_finite() && credits > 0.0 {
                entry.total_credits += credits;
            }
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
        self.save_locked(&inner);
    }

    /// 获取统计后的 active Key 数（未禁用）
    pub fn active_count(&self) -> usize {
        self.inner.read().entries.values().filter(|e| !e.disabled).count()
    }
}

/// 规范化 Key 的分组 scope：default 始终包含，额外分组用 `|` 连接以兼容
/// 旧版单字符串 `group` 持久化字段。
pub fn normalize_group_scope(groups: Option<Vec<String>>, legacy: Option<String>) -> String {
    let mut out = vec!["default".to_string()];
    let mut add = |value: String| {
        let value = value.trim().to_string();
        if !value.is_empty() && !out.iter().any(|g| g == &value) {
            out.push(value);
        }
    };
    if let Some(values) = groups {
        for value in values { add(value); }
    }
    if let Some(value) = legacy {
        for part in value.split('|') { add(part.to_string()); }
    }
    out.join("|")
}

pub fn split_group_scope(scope: Option<&str>) -> Vec<String> {
    let scope = scope.unwrap_or("default");
    let mut out: Vec<String> = scope
        .split('|')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if out.is_empty() { out.push("default".to_string()); }
    if !out.iter().any(|g| g == "default") { out.insert(0, "default".to_string()); }
    out
}

impl Default for ClientKeyManager {
    fn default() -> Self {
        Self::new()
    }
}

fn is_false(b: &bool) -> bool {
    !b
}

fn is_one_f64(v: &f64) -> bool {
    (*v - 1.0).abs() < f64::EPSILON
}

pub fn normalize_sell_multiplier(v: f64) -> f64 {
    if v.is_finite() && v >= 0.0 { v } else { 1.0 }
}

/// 把存储的缓存计费系数归一为有效值：未设置 / 非有限值为 1.0，其余夹到 0.0~1.0。
pub fn normalize_cache_ratio(ratio: Option<f64>) -> f64 {
    match ratio {
        Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
        _ => 1.0,
    }
}

/// 生成 `sk-` 前缀 + 32 位 base62 随机字符串
pub fn generate_client_key() -> String {
    const CHARSET: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let body: String = (0..32)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    format!("sk-{}", body)
}

/// 脱敏展示：保留前 8 个字符（含前缀）和后 4 个字符
pub fn mask_client_key(key: &str) -> String {
    let char_count = key.chars().count();
    if char_count <= 12 {
        return key.to_string();
    }
    let start: String = key.chars().take(8).collect();
    let end: String = key.chars().skip(char_count - 4).collect();
    format!("{start}...{end}")
}

pub fn default_path_in(dir: &Path) -> PathBuf {
    dir.join("client_api_keys.json")
}

pub type SharedClientKeyManager = Arc<ClientKeyManager>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_verify() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        assert!(entry.key.starts_with("sk-"));
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
        assert_eq!(mgr.verify_and_touch("nope"), None);
    }

    #[test]
    fn disabled_key_rejected() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        mgr.set_disabled(entry.id, true);
        assert_eq!(mgr.verify_and_touch(&entry.key), None);
        mgr.set_disabled(entry.id, false);
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
    }

    #[test]
    fn record_usage_accumulates() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        mgr.record_usage(entry.id, 100, 50, 0, 0, 0.0);
        mgr.record_usage(entry.id, 200, 30, 5, 10, 1.5);
        let list = mgr.list();
        let e = list.iter().find(|x| x.id == entry.id).unwrap();
        assert_eq!(e.total_input_tokens, 300);
        assert_eq!(e.total_output_tokens, 80);
        assert_eq!(e.total_cache_creation_tokens, 5);
        assert_eq!(e.total_cache_read_tokens, 10);
    }

    #[test]
    fn max_credits_blocks_when_exceeded() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        assert!(mgr.set_max_credits(entry.id, Some(2.0)));

        // 未超额：正常放行
        assert_eq!(mgr.verify_and_touch_ex(&entry.key), KeyAuth::Ok(entry.id));

        // 累计到达上限后拒绝
        mgr.record_usage(entry.id, 0, 0, 0, 0, 2.5);
        match mgr.verify_and_touch_ex(&entry.key) {
            KeyAuth::OverLimit { id, used, limit } => {
                assert_eq!(id, entry.id);
                assert!((used - 2.5).abs() < 1e-9);
                assert!((limit - 2.0).abs() < 1e-9);
            }
            other => panic!("expected OverLimit, got {other:?}"),
        }
        // 旧接口兼容：超额返回 None
        assert_eq!(mgr.verify_and_touch(&entry.key), None);

        // 重置统计后恢复放行
        assert!(mgr.reset_stats(entry.id));
        assert_eq!(mgr.verify_and_touch_ex(&entry.key), KeyAuth::Ok(entry.id));

        // 清除上限：不再拦截
        mgr.record_usage(entry.id, 0, 0, 0, 0, 999.0);
        assert!(mgr.set_max_credits(entry.id, None));
        assert_eq!(mgr.verify_and_touch_ex(&entry.key), KeyAuth::Ok(entry.id));
    }

    #[test]
    fn set_max_credits_rejects_invalid() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        assert!(!mgr.set_max_credits(entry.id, Some(-1.0)));
        assert!(!mgr.set_max_credits(entry.id, Some(f64::NAN)));
        assert!(!mgr.set_max_credits(999, Some(5.0)));
        assert!(mgr.set_max_credits(entry.id, Some(0.0)));
    }

    #[test]
    fn cache_ratio_defaults_to_full_and_round_trips() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        assert_eq!(entry.cache_ratio, None);
        assert_eq!(mgr.cache_ratio_of(entry.id), 1.0);

        assert!(mgr.set_cache_ratio(entry.id, Some(0.5)));
        assert_eq!(mgr.cache_ratio_of(entry.id), 0.5);
        assert!(mgr.set_cache_ratio(entry.id, Some(0.0)));
        assert_eq!(mgr.cache_ratio_of(entry.id), 0.0);

        // 越界值夹到 0~1；>= 1 视为满缓存并清除字段
        assert!(mgr.set_cache_ratio(entry.id, Some(-3.0)));
        assert_eq!(mgr.cache_ratio_of(entry.id), 0.0);
        assert!(mgr.set_cache_ratio(entry.id, Some(7.0)));
        assert_eq!(mgr.list()[0].cache_ratio, None);
        assert_eq!(mgr.cache_ratio_of(entry.id), 1.0);

        assert!(!mgr.set_cache_ratio(entry.id, Some(f64::NAN)));
        assert!(!mgr.set_cache_ratio(999, Some(0.5)));
        assert_eq!(mgr.cache_ratio_of(999), 1.0);
    }

    #[test]
    fn cache_ratio_is_omitted_from_json_when_unset() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        let json = serde_json::to_value(&entry).unwrap();
        assert!(json.get("cacheRatio").is_none());

        mgr.set_cache_ratio(entry.id, Some(0.25));
        let json = serde_json::to_value(&mgr.list()[0]).unwrap();
        assert_eq!(json["cacheRatio"], serde_json::json!(0.25));

        // 旧文件无此字段时反序列化为 None
        let legacy: ClientKey = serde_json::from_value(serde_json::json!({
            "id": 1, "key": "sk-x", "name": "old", "createdAt": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(legacy.cache_ratio, None);
    }

    #[test]
    fn normalize_cache_ratio_handles_invalid_values() {
        assert_eq!(normalize_cache_ratio(None), 1.0);
        assert_eq!(normalize_cache_ratio(Some(f64::INFINITY)), 1.0);
        assert_eq!(normalize_cache_ratio(Some(f64::NAN)), 1.0);
        assert_eq!(normalize_cache_ratio(Some(-0.2)), 0.0);
        assert_eq!(normalize_cache_ratio(Some(0.3)), 0.3);
        assert_eq!(normalize_cache_ratio(Some(1.5)), 1.0);
    }

    #[test]
    fn mask_format() {
        assert_eq!(mask_client_key("sk-abcdefghijklmnop"), "sk-abcde...mnop");
        assert_eq!(mask_client_key("short"), "short");
        assert_eq!(mask_client_key("密钥🔐测试abcdefgh"), "密钥🔐测试abc...efgh");
    }

    #[test]
    fn rotate_replaces_key_but_keeps_metadata_and_stats() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("kb".to_string(), Some("desc".into()), Some("groupA".into()));
        mgr.record_usage(entry.id, 100, 50, 5, 10, 1.5);
        let old_key = entry.key.clone();
        let rotated = mgr.rotate(entry.id).expect("rotate should succeed");
        assert_ne!(rotated.key, old_key);
        assert!(rotated.key.starts_with("sk-"));
        assert_eq!(rotated.id, entry.id);
        assert_eq!(rotated.name, "kb");
        assert_eq!(rotated.description.as_deref(), Some("desc"));
        assert_eq!(rotated.group.as_deref(), Some("groupA"));
        assert_eq!(rotated.total_input_tokens, 100);
        assert_eq!(rotated.total_output_tokens, 50);
        assert_eq!(mgr.verify_and_touch(&old_key), None);
        assert_eq!(mgr.verify_and_touch(&rotated.key), Some(entry.id));
    }

    #[test]
    fn rotate_unknown_id_returns_none() {
        let mgr = ClientKeyManager::new();
        assert!(mgr.rotate(999).is_none());
    }

    #[test]
    fn sync_system_key_uses_id_zero() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert!(mgr.is_system(0));
        assert_eq!(mgr.list().first().map(|k| k.id), Some(0));
        assert_eq!(mgr.verify_and_touch("custom-api-key"), Some(0));
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert_eq!(mgr.list().iter().filter(|k| k.is_system).count(), 1);
    }

    #[test]
    fn sync_system_key_replaces_config_value_and_revokes_old_key() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), Some("初始描述".into()), "custom-a".into());
        mgr.update_meta(
            0,
            Some("保留名称".into()),
            Some(Some("保留描述".into())),
            Some(Some("group-a".into())),
        );
        mgr.record_usage(0, 100, 50, 5, 10, 1.5);
        assert_eq!(mgr.verify_and_touch("custom-a"), Some(0));
        mgr.set_disabled(0, true);

        let conflicting = mgr.create_with_key(
            "冲突密钥".into(),
            None,
            None,
            "custom-b".into(),
        );
        assert_ne!(conflicting.id, 0);

        mgr.sync_system_key("默认密钥".into(), None, "custom-b".into());

        assert_eq!(mgr.verify_and_touch("custom-a"), None);
        assert_eq!(mgr.verify_and_touch("custom-b"), Some(0));
        let entries = mgr.list();
        assert_eq!(entries.len(), 1);
        let system = &entries[0];
        assert_eq!(system.id, 0);
        assert!(system.is_system);
        assert_eq!(system.name, "保留名称");
        assert_eq!(system.description.as_deref(), Some("保留描述"));
        assert_eq!(system.group.as_deref(), Some("group-a"));
        assert!(!system.disabled);
        assert_eq!(system.total_input_tokens, 100);
        assert_eq!(system.total_output_tokens, 50);
    }

    #[test]
    fn system_key_cannot_be_deleted() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert!(!mgr.delete(0), "系统密钥 id=0 不可删除");
        assert!(mgr.is_system(0));
    }
}
