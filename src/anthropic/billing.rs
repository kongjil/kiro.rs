//! 按客户端 Key 的下游计费策略
//!
//! 原始 usage（上游 `metadataEvent.tokenUsage` 真值，或 CacheMeter 模拟拆分）始终
//! 保留，用于管理端「原始」口径统计；本模块只负责由原始 usage 生成发给下游、
//! 计入「计费后」口径的 usage 与 credit。
//!
//! 三种模式：
//! - [`BillingMode::Raw`]：完全不改；
//! - [`BillingMode::Redistribute`]：输入总量不变，只改读取 / 创建 / input 的构成；
//! - [`BillingMode::Inflate`]：输入总量乘以倍率 k（output 可单独放大），再按同样的
//!   规则决定构成。
//!
//! 构成规则见 [`Composition`]。所有模式都只依赖原始 usage，结果是确定性的。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::kiro::model::events::TokenUsage;
use crate::model::config::BillingPrice;

/// 加量模式倍率的合理上限，防止误填一个巨大值把下游账单放大到离谱。
pub const MAX_SCALE: f64 = 10.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingMode {
    /// 原样透传
    #[default]
    Raw,
    /// 输入总量不变，重新分配读取 / 创建 / input
    Redistribute,
    /// 输入总量按倍率放大后再分配
    Inflate,
    /// 旧版三类各自乘倍率（只为兼容已有配置，新配置不可选）
    LegacyMultipliers,
}

/// 输入侧的构成规则。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Composition {
    /// 保持原始比例
    #[default]
    Keep,
    /// 缓存读取占输入总量的比例固定为该值（0~1）
    TargetHitRate(f64),
    /// 缓存读取与创建都按原始比例乘以该系数（0~1），被砍掉的部分算 input。
    /// 与旧版 `cacheRatio` 等价。
    ScaleCache(f64),
}

/// 旧版分类倍率。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegacyMultipliers {
    pub input: f64,
    pub creation: f64,
    pub read: f64,
}

impl Default for LegacyMultipliers {
    fn default() -> Self {
        Self {
            input: 1.0,
            creation: 1.0,
            read: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BillingPolicy {
    pub mode: BillingMode,
    pub composition: Composition,
    /// 非命中部分里缓存创建的占比（0~1）；None 沿用原始比例。
    /// 只对 `Keep` / `TargetHitRate` 生效。
    pub creation_share: Option<f64>,
    /// 加量模式的输入总量倍率
    pub input_scale: f64,
    /// 加量模式的 output 倍率
    pub output_scale: f64,
    pub legacy: LegacyMultipliers,
}

impl Default for BillingPolicy {
    fn default() -> Self {
        Self {
            mode: BillingMode::Raw,
            composition: Composition::Keep,
            creation_share: None,
            input_scale: 1.0,
            output_scale: 1.0,
            legacy: LegacyMultipliers::default(),
        }
    }
}

/// 一次请求的结算结果：原始与计费后各一份，响应、用量统计、trace 共用。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BillingOutcome {
    pub raw: TokenUsage,
    pub raw_credits: f64,
    pub billed: TokenUsage,
    pub billed_credits: f64,
}

fn unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn to_tokens(value: f64) -> i32 {
    value.round().clamp(0.0, i32::MAX as f64) as i32
}

fn non_negative_credits(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

impl BillingPolicy {
    /// 校验通过管理接口设置的策略。旧版分类倍率只为兼容已有配置，不允许新设。
    pub fn validate(&self) -> Result<(), String> {
        let fraction = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
        match self.composition {
            Composition::TargetHitRate(rate) if !fraction(rate) => {
                return Err("目标命中率必须在 0~1 之间".to_string());
            }
            Composition::ScaleCache(ratio) if !fraction(ratio) => {
                return Err("缓存缩放系数必须在 0~1 之间".to_string());
            }
            _ => {}
        }
        if self.creation_share.is_some_and(|share| !fraction(share)) {
            return Err("缓存创建占比必须在 0~1 之间".to_string());
        }
        match self.mode {
            BillingMode::LegacyMultipliers => Err(
                "旧版分类倍率已弃用，请改选 原始 / 重新分配 / 加量".to_string(),
            ),
            BillingMode::Inflate => {
                let scale_ok = |v: f64| v.is_finite() && (1.0..=MAX_SCALE).contains(&v);
                if scale_ok(self.input_scale) && scale_ok(self.output_scale) {
                    Ok(())
                } else {
                    Err(format!("加量倍率必须在 1~{MAX_SCALE} 之间"))
                }
            }
            BillingMode::Raw | BillingMode::Redistribute => Ok(()),
        }
    }

    /// 由原始 usage 生成计费后 usage。
    pub fn apply(&self, raw: TokenUsage) -> TokenUsage {
        let raw = raw.sanitized();
        match self.mode {
            BillingMode::Raw => raw,
            BillingMode::LegacyMultipliers => self.apply_legacy(raw),
            BillingMode::Redistribute => self.compose(raw, raw.total_input_tokens(), raw.output_tokens),
            BillingMode::Inflate => {
                let total = to_tokens(raw.total_input_tokens() as f64 * self.input_scale.max(0.0));
                let output = to_tokens(raw.output_tokens as f64 * self.output_scale.max(0.0));
                self.compose(raw, total, output)
            }
        }
    }

    /// 把 `total` 个输入 token 按构成规则分到读取 / 创建 / input。
    fn compose(&self, raw: TokenUsage, total: i32, output_tokens: i32) -> TokenUsage {
        let raw_total = raw.total_input_tokens();
        let share_of_raw = |part: i32| -> f64 {
            if raw_total > 0 {
                total as f64 * part as f64 / raw_total as f64
            } else {
                0.0
            }
        };
        // 非命中部分里创建所占比例：显式配置优先，否则沿用原始比例。
        let creation_share = || -> f64 {
            if let Some(share) = self.creation_share {
                return unit(share);
            }
            let non_read = raw.uncached_input_tokens + raw.cache_write_input_tokens;
            if non_read > 0 {
                raw.cache_write_input_tokens as f64 / non_read as f64
            } else {
                0.0
            }
        };

        let (read, creation) = match self.composition {
            Composition::Keep => {
                let read = to_tokens(share_of_raw(raw.cache_read_input_tokens));
                let creation = if self.creation_share.is_some() {
                    to_tokens((total - read).max(0) as f64 * creation_share())
                } else {
                    to_tokens(share_of_raw(raw.cache_write_input_tokens))
                };
                (read, creation)
            }
            Composition::TargetHitRate(rate) => {
                let read = to_tokens(total as f64 * unit(rate));
                let creation = to_tokens((total - read).max(0) as f64 * creation_share());
                (read, creation)
            }
            Composition::ScaleCache(ratio) => {
                let ratio = unit(ratio);
                (
                    to_tokens(share_of_raw(raw.cache_read_input_tokens) * ratio),
                    to_tokens(share_of_raw(raw.cache_write_input_tokens) * ratio),
                )
            }
        };

        // 四舍五入后保证 read + creation 不超过总量，剩余全部是 input。
        let read = read.min(total);
        let creation = creation.min(total - read);
        TokenUsage {
            uncached_input_tokens: total - read - creation,
            output_tokens,
            cache_read_input_tokens: read,
            cache_write_input_tokens: creation,
        }
    }

    fn apply_legacy(&self, raw: TokenUsage) -> TokenUsage {
        let scale = |value: i32, factor: f64| to_tokens(value as f64 * factor.max(0.0));
        TokenUsage {
            uncached_input_tokens: scale(raw.uncached_input_tokens, self.legacy.input),
            cache_write_input_tokens: scale(raw.cache_write_input_tokens, self.legacy.creation),
            cache_read_input_tokens: scale(raw.cache_read_input_tokens, self.legacy.read),
            output_tokens: raw.output_tokens,
        }
    }
}

/// 结算一次请求。
pub fn settle(
    policy: &BillingPolicy,
    prices: &HashMap<String, BillingPrice>,
    model: &str,
    raw: TokenUsage,
    raw_credits: f64,
) -> BillingOutcome {
    let raw = raw.sanitized();
    let billed = policy.apply(raw);
    outcome(prices, model, raw, raw_credits, billed)
}

/// 由已经算好的原始 / 计费后 usage 组装结算结果（多轮请求按轮应用策略后累计时使用）。
///
/// 计费后 credit：配置了该模型的价格表时按计费后 token 计价；否则把原始 credit
/// 按「计费后总 token / 原始总 token」缩放——重新分配模式总量不变，credit 也不变。
pub fn outcome(
    prices: &HashMap<String, BillingPrice>,
    model: &str,
    raw: TokenUsage,
    raw_credits: f64,
    billed: TokenUsage,
) -> BillingOutcome {
    let raw = raw.sanitized();
    let billed = billed.sanitized();
    let raw_credits = non_negative_credits(raw_credits);
    let billed_credits = configured_token_cost(model, billed, prices)
        .unwrap_or_else(|| scale_credits(raw_credits, raw, billed));
    BillingOutcome {
        raw,
        raw_credits,
        billed,
        billed_credits: non_negative_credits(billed_credits),
    }
}

fn scale_credits(raw_credits: f64, raw: TokenUsage, billed: TokenUsage) -> f64 {
    let raw_total = raw.total_input_tokens() as f64 + raw.output_tokens as f64;
    if raw_total <= 0.0 {
        return raw_credits;
    }
    let billed_total = billed.total_input_tokens() as f64 + billed.output_tokens as f64;
    raw_credits * billed_total / raw_total
}

/// 按价格表计算 usage 的价格；该模型（及通配 `*`）都没配置价格时返回 None，
/// 用来区分「未配置」和「显式配置为 0」。
pub fn configured_token_cost(
    model: &str,
    usage: TokenUsage,
    prices: &HashMap<String, BillingPrice>,
) -> Option<f64> {
    let key = model.to_ascii_lowercase();
    let price = prices
        .get(&key)
        .or_else(|| {
            prices
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&key))
                .map(|(_, price)| price)
        })
        .or_else(|| prices.get("*"))?;
    let usage = usage.sanitized();
    Some(
        usage.uncached_input_tokens as f64 * price.input
            + usage.cache_write_input_tokens as f64 * price.cache_creation
            + usage.cache_read_input_tokens as f64 * price.cache_read
            + usage.output_tokens as f64 * price.output,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: i32, creation: i32, read: i32, output: i32) -> TokenUsage {
        TokenUsage {
            uncached_input_tokens: input,
            output_tokens: output,
            cache_write_input_tokens: creation,
            cache_read_input_tokens: read,
        }
    }

    fn policy(mode: BillingMode, composition: Composition) -> BillingPolicy {
        BillingPolicy {
            mode,
            composition,
            ..Default::default()
        }
    }

    fn hit_rate(u: TokenUsage) -> f64 {
        u.cache_read_input_tokens as f64 / u.total_input_tokens() as f64
    }

    #[test]
    fn raw_mode_passes_usage_through() {
        let raw = usage(1000, 2000, 7000, 500);
        assert_eq!(BillingPolicy::default().apply(raw), raw);
        // 即便误配了构成，Raw 模式也不改
        assert_eq!(
            policy(BillingMode::Raw, Composition::TargetHitRate(0.9)).apply(raw),
            raw
        );
    }

    #[test]
    fn redistribute_keeps_total_and_hits_target_exactly() {
        let raw = usage(1000, 2000, 7000, 500);
        let billed = policy(BillingMode::Redistribute, Composition::TargetHitRate(0.8)).apply(raw);
        assert_eq!(billed.total_input_tokens(), 10_000);
        assert_eq!(billed.cache_read_input_tokens, 8000);
        // 剩余 2000 按原始 创建:input = 2:1 分
        assert_eq!(billed.cache_write_input_tokens, 1333);
        assert_eq!(billed.uncached_input_tokens, 667);
        assert_eq!(billed.output_tokens, 500);
        assert!((hit_rate(billed) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn redistribute_with_input_only_raw_puts_rest_into_input() {
        // Kiro 只给 input/output 时没有创建比例可沿用，剩余全部是 input
        let raw = usage(1000, 0, 0, 500);
        let billed = policy(BillingMode::Redistribute, Composition::TargetHitRate(0.9)).apply(raw);
        assert_eq!(billed, usage(100, 0, 900, 500));
    }

    #[test]
    fn creation_share_overrides_raw_split() {
        let raw = usage(1000, 0, 0, 0);
        let p = BillingPolicy {
            creation_share: Some(0.25),
            ..policy(BillingMode::Redistribute, Composition::TargetHitRate(0.6))
        };
        assert_eq!(p.apply(raw), usage(300, 100, 600, 0));
    }

    #[test]
    fn scale_cache_matches_legacy_cache_ratio() {
        // 旧 cache_ratio=0.5：读取与写入各砍半，砍掉的转回 input
        let raw = usage(2400, 6000, 3600, 500);
        let billed = policy(BillingMode::Redistribute, Composition::ScaleCache(0.5)).apply(raw);
        assert_eq!(billed, usage(7200, 3000, 1800, 500));
        // 系数 0：缓存归零
        let none = policy(BillingMode::Redistribute, Composition::ScaleCache(0.0)).apply(raw);
        assert_eq!(none, usage(12_000, 0, 0, 500));
    }

    #[test]
    fn inflate_scales_total_then_applies_target() {
        // 用户确认方案中的示例
        let raw = usage(1000, 2000, 7000, 500);
        let p = BillingPolicy {
            input_scale: 1.2,
            ..policy(BillingMode::Inflate, Composition::TargetHitRate(0.8))
        };
        assert_eq!(p.apply(raw), usage(800, 1600, 9600, 500));
    }

    #[test]
    fn inflate_keep_composition_scales_every_category() {
        let raw = usage(1000, 2000, 7000, 500);
        let p = BillingPolicy {
            input_scale: 1.2,
            output_scale: 1.5,
            ..policy(BillingMode::Inflate, Composition::Keep)
        };
        let billed = p.apply(raw);
        assert_eq!(billed, usage(1200, 2400, 8400, 750));
        assert!((hit_rate(billed) - hit_rate(raw)).abs() < 1e-9);
    }

    #[test]
    fn inflate_increase_is_independent_of_cache_composition() {
        // 旧版分类倍率在热缓存轮和首轮之间加价幅度会跳；新模型固定 +20%
        let p = BillingPolicy {
            input_scale: 1.2,
            ..policy(BillingMode::Inflate, Composition::TargetHitRate(0.5))
        };
        for raw in [usage(1000, 2000, 7000, 0), usage(1000, 9000, 0, 0)] {
            assert_eq!(p.apply(raw).total_input_tokens(), 12_000);
        }
    }

    #[test]
    fn empty_input_stays_empty() {
        let raw = usage(0, 0, 0, 40);
        let p = BillingPolicy {
            input_scale: 2.0,
            output_scale: 2.0,
            ..policy(BillingMode::Inflate, Composition::TargetHitRate(0.8))
        };
        assert_eq!(p.apply(raw), usage(0, 0, 0, 80));
    }

    #[test]
    fn legacy_multipliers_keep_old_behavior() {
        let p = BillingPolicy {
            mode: BillingMode::LegacyMultipliers,
            legacy: LegacyMultipliers {
                input: 1.2,
                creation: 0.8,
                read: 2.0,
            },
            ..Default::default()
        };
        assert_eq!(p.apply(usage(250, 500, 500, 100)), usage(300, 400, 1000, 100));
    }

    #[test]
    fn settle_scales_credits_by_token_ratio_without_price_table() {
        let raw = usage(1000, 2000, 7000, 500);
        let prices = HashMap::new();

        // 重新分配：总量不变，credit 不变
        let redistribute = policy(BillingMode::Redistribute, Composition::TargetHitRate(0.8));
        let outcome = settle(&redistribute, &prices, "m", raw, 0.30);
        assert!((outcome.billed_credits - 0.30).abs() < 1e-12);
        assert_eq!(outcome.raw, raw);
        assert!((outcome.raw_credits - 0.30).abs() < 1e-12);

        // 加量：credit 按总 token 比例放大
        let inflate = BillingPolicy {
            input_scale: 1.2,
            ..policy(BillingMode::Inflate, Composition::Keep)
        };
        let outcome = settle(&inflate, &prices, "m", raw, 0.30);
        let expected = 0.30 * (12_000.0 + 500.0) / 10_500.0;
        assert!((outcome.billed_credits - expected).abs() < 1e-12);
    }

    #[test]
    fn settle_prices_billed_usage_when_configured() {
        let prices = HashMap::from([(
            "model".to_string(),
            BillingPrice {
                input: 1.0,
                cache_creation: 2.0,
                cache_read: 0.1,
                output: 3.0,
            },
        )]);
        let outcome = settle(
            &BillingPolicy::default(),
            &prices,
            "MODEL",
            usage(10, 5, 20, 4),
            0.5,
        );
        assert!((outcome.billed_credits - 34.0).abs() < 1e-12);
        // 原始 credit 保持上游值
        assert!((outcome.raw_credits - 0.5).abs() < 1e-12);
    }

    #[test]
    fn settle_keeps_credits_when_no_tokens_were_reported() {
        let outcome = settle(
            &BillingPolicy {
                input_scale: 2.0,
                ..policy(BillingMode::Inflate, Composition::Keep)
            },
            &HashMap::new(),
            "m",
            TokenUsage::default(),
            0.2,
        );
        assert!((outcome.billed_credits - 0.2).abs() < 1e-12);
    }
}
