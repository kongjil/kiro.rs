import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import type { BillingMode, BillingPolicy } from '@/types/api'

/** 表单里可编辑的计费模式（旧版分类倍率只读，不能新选） */
type EditableMode = Exclude<BillingMode, 'legacy_multipliers'>
type CompositionKind = 'keep' | 'target' | 'scale'

/** 计费策略表单草稿：数值保留输入框原文，提交时再解析 */
export interface BillingDraft {
  mode: BillingMode
  composition: CompositionKind
  targetHitRate: string
  cacheScale: string
  creationShare: string
  inputScale: string
  outputScale: string
  legacy?: [number, number, number]
}

const MAX_SCALE = 10

const MODES: { value: EditableMode; label: string; hint: string }[] = [
  { value: 'raw', label: '原始', hint: '完全不修改 token 与缓存命中率' },
  { value: 'redistribute', label: '重新分配', hint: '输入总量不变，只改读取 / 创建 / input 的比例' },
  { value: 'inflate', label: '加量', hint: '输入总量乘以倍率，再按构成规则分配' },
]

const COMPOSITIONS: { value: CompositionKind; label: string }[] = [
  { value: 'keep', label: '保持原始比例' },
  { value: 'target', label: '目标命中率' },
  { value: 'scale', label: '缓存缩放系数' },
]

export function emptyBillingDraft(): BillingDraft {
  return {
    mode: 'raw',
    composition: 'target',
    targetHitRate: '0.8',
    cacheScale: '0.5',
    creationShare: '',
    inputScale: '1.2',
    outputScale: '1',
  }
}

export function draftFromPolicy(policy?: BillingPolicy): BillingDraft {
  const draft = emptyBillingDraft()
  if (!policy) return draft
  return {
    ...draft,
    mode: policy.mode,
    composition:
      policy.targetHitRate != null ? 'target' : policy.cacheScale != null ? 'scale' : 'keep',
    targetHitRate: policy.targetHitRate != null ? String(policy.targetHitRate) : draft.targetHitRate,
    cacheScale: policy.cacheScale != null ? String(policy.cacheScale) : draft.cacheScale,
    creationShare: policy.creationShare != null ? String(policy.creationShare) : '',
    inputScale: policy.inputScale != null ? String(policy.inputScale) : draft.inputScale,
    outputScale: policy.outputScale != null ? String(policy.outputScale) : draft.outputScale,
    legacy: policy.legacyMultipliers,
  }
}

function parseFraction(raw: string, label: string): number {
  const n = Number(raw.trim())
  if (raw.trim() === '' || !Number.isFinite(n) || n < 0 || n > 1) {
    throw new Error(`${label}必须在 0~1 之间`)
  }
  return n
}

function parseScale(raw: string, label: string): number {
  const n = Number(raw.trim())
  if (raw.trim() === '' || !Number.isFinite(n) || n < 1 || n > MAX_SCALE) {
    throw new Error(`${label}必须在 1~${MAX_SCALE} 之间`)
  }
  return n
}

/** 草稿 → 提交给后端的策略。旧版分类倍率不能保存，必须先改选新模式。 */
export function policyFromDraft(draft: BillingDraft): BillingPolicy {
  if (draft.mode === 'legacy_multipliers') {
    throw new Error('旧版分类倍率已弃用，请先选择 原始 / 重新分配 / 加量')
  }
  if (draft.mode === 'raw') return { mode: 'raw' }
  const policy: BillingPolicy = { mode: draft.mode }
  if (draft.composition === 'target') {
    policy.targetHitRate = parseFraction(draft.targetHitRate, '目标命中率')
  } else if (draft.composition === 'scale') {
    policy.cacheScale = parseFraction(draft.cacheScale, '缓存缩放系数')
  }
  if (draft.composition !== 'scale' && draft.creationShare.trim() !== '') {
    policy.creationShare = parseFraction(draft.creationShare, '缓存创建占比')
  }
  if (draft.mode === 'inflate') {
    policy.inputScale = parseScale(draft.inputScale, '输入总量倍率')
    policy.outputScale = parseScale(draft.outputScale || '1', 'output 倍率')
  }
  return policy
}

interface Usage {
  input: number
  creation: number
  read: number
  output: number
}

/** 与后端 `BillingPolicy::apply` 相同的算法，只用于表单预览 */
function applyPolicy(policy: BillingPolicy, raw: Usage): Usage {
  const rawTotal = raw.input + raw.creation + raw.read
  if (policy.mode === 'raw') return raw
  const total =
    policy.mode === 'inflate' ? Math.round(rawTotal * (policy.inputScale ?? 1)) : rawTotal
  const output =
    policy.mode === 'inflate' ? Math.round(raw.output * (policy.outputScale ?? 1)) : raw.output
  const shareOfRaw = (part: number) => (rawTotal > 0 ? (total * part) / rawTotal : 0)
  const creationShare = () => {
    if (policy.creationShare != null) return policy.creationShare
    const nonRead = raw.input + raw.creation
    return nonRead > 0 ? raw.creation / nonRead : 0
  }
  let read: number
  let creation: number
  if (policy.targetHitRate != null) {
    read = Math.round(total * policy.targetHitRate)
    creation = Math.round(Math.max(total - read, 0) * creationShare())
  } else if (policy.cacheScale != null) {
    read = Math.round(shareOfRaw(raw.read) * policy.cacheScale)
    creation = Math.round(shareOfRaw(raw.creation) * policy.cacheScale)
  } else {
    read = Math.round(shareOfRaw(raw.read))
    creation =
      policy.creationShare != null
        ? Math.round(Math.max(total - read, 0) * policy.creationShare)
        : Math.round(shareOfRaw(raw.creation))
  }
  read = Math.min(read, total)
  creation = Math.min(creation, total - read)
  return { input: total - read - creation, creation, read, output }
}

const SAMPLE: Usage = { input: 1000, creation: 2000, read: 7000, output: 500 }

function describe(u: Usage): string {
  const prompt = u.input + u.creation + u.read
  const hit = prompt > 0 ? ((u.read / prompt) * 100).toFixed(1) : '0'
  return `input ${u.input} · 创建 ${u.creation} · 读取 ${u.read}（命中 ${hit}%）· 输出 ${u.output}`
}

/** 列表里的一行摘要 */
export function billingSummary(policy: BillingPolicy): string {
  const composition =
    policy.targetHitRate != null
      ? `命中 ${Math.round(policy.targetHitRate * 100)}%`
      : policy.cacheScale != null
        ? `缓存 ×${policy.cacheScale}`
        : '原比例'
  switch (policy.mode) {
    case 'raw':
      return '原始'
    case 'redistribute':
      return `分配 · ${composition}`
    case 'inflate':
      return `加量 ×${policy.inputScale ?? 1} · ${composition}`
    case 'legacy_multipliers':
      return '旧版倍率'
  }
}

export function BillingPolicyFields({
  draft,
  onChange,
  disabled,
}: {
  draft: BillingDraft
  onChange: (draft: BillingDraft) => void
  disabled?: boolean
}) {
  const set = (patch: Partial<BillingDraft>) => onChange({ ...draft, ...patch })
  let preview: string | null = null
  let previewError: string | null = null
  if (draft.mode !== 'legacy_multipliers') {
    try {
      preview = describe(applyPolicy(policyFromDraft(draft), SAMPLE))
    } catch (err) {
      previewError = err instanceof Error ? err.message : String(err)
    }
  }

  return (
    <div className="space-y-2.5 rounded-md border border-border/60 p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <label className="text-[12px] font-medium">下游计费策略</label>
        <div className="flex gap-1 rounded-md border border-border/60 p-0.5">
          {MODES.map((m) => (
            <Button
              key={m.value}
              type="button"
              size="sm"
              title={m.hint}
              variant={draft.mode === m.value ? 'default' : 'ghost'}
              className="h-7 px-2.5 text-xs"
              disabled={disabled}
              onClick={() => set({ mode: m.value })}
            >
              {m.label}
            </Button>
          ))}
        </div>
      </div>

      {draft.mode === 'legacy_multipliers' && (
        <p className="rounded-md bg-amber-500/10 px-2.5 py-1.5 text-[11px] text-amber-700 dark:text-amber-400">
          当前为旧版分类倍率（input ×{draft.legacy?.[0] ?? 1}、创建 ×{draft.legacy?.[1] ?? 1}、读取 ×
          {draft.legacy?.[2] ?? 1}），仍按原规则生效。该模式已弃用；如需修改，请改选上面的新模式。
        </p>
      )}

      {draft.mode === 'raw' && (
        <p className="text-[11px] text-muted-foreground">
          token 与缓存命中率原样透传给下游。
        </p>
      )}

      {(draft.mode === 'redistribute' || draft.mode === 'inflate') && (
        <>
          {draft.mode === 'inflate' && (
            <div className="grid grid-cols-2 gap-3">
              <div>
                <label className="text-[12px] text-muted-foreground">输入总量倍率（1~{MAX_SCALE}）</label>
                <Input
                  type="number"
                  min="1"
                  max={MAX_SCALE}
                  step="0.05"
                  value={draft.inputScale}
                  onChange={(e) => set({ inputScale: e.target.value })}
                  disabled={disabled}
                />
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">output 倍率（1~{MAX_SCALE}）</label>
                <Input
                  type="number"
                  min="1"
                  max={MAX_SCALE}
                  step="0.05"
                  value={draft.outputScale}
                  onChange={(e) => set({ outputScale: e.target.value })}
                  disabled={disabled}
                />
              </div>
            </div>
          )}

          <div>
            <label className="text-[12px] text-muted-foreground">输入侧构成</label>
            <div className="mt-1 flex flex-wrap gap-1">
              {COMPOSITIONS.map((c) => (
                <Button
                  key={c.value}
                  type="button"
                  size="sm"
                  variant={draft.composition === c.value ? 'secondary' : 'ghost'}
                  className="h-7 px-2.5 text-xs"
                  disabled={disabled}
                  onClick={() => set({ composition: c.value })}
                >
                  {c.label}
                </Button>
              ))}
            </div>
          </div>

          <div className="grid grid-cols-2 gap-3">
            {draft.composition === 'target' && (
              <div>
                <label className="text-[12px] text-muted-foreground">目标命中率（0~1）</label>
                <Input
                  type="number"
                  min="0"
                  max="1"
                  step="0.05"
                  value={draft.targetHitRate}
                  onChange={(e) => set({ targetHitRate: e.target.value })}
                  disabled={disabled}
                />
              </div>
            )}
            {draft.composition === 'scale' && (
              <div>
                <label className="text-[12px] text-muted-foreground">缓存缩放系数（0~1）</label>
                <Input
                  type="number"
                  min="0"
                  max="1"
                  step="0.05"
                  value={draft.cacheScale}
                  onChange={(e) => set({ cacheScale: e.target.value })}
                  disabled={disabled}
                />
              </div>
            )}
            {draft.composition !== 'scale' && (
              <div>
                <label className="text-[12px] text-muted-foreground">缓存创建占比（可选，0~1）</label>
                <Input
                  type="number"
                  min="0"
                  max="1"
                  step="0.05"
                  placeholder="留空 = 沿用原始比例"
                  value={draft.creationShare}
                  onChange={(e) => set({ creationShare: e.target.value })}
                  disabled={disabled}
                />
              </div>
            )}
          </div>
          <p className="text-[11px] text-muted-foreground">
            {draft.composition === 'target' &&
              '读取占输入总量的比例固定为目标值，剩余部分按「缓存创建占比」分给创建与 input。'}
            {draft.composition === 'scale' &&
              '读取与创建按原始比例乘以系数，被砍掉的部分算 input（等同旧版缓存系数）。'}
            {draft.composition === 'keep' && '读取 / 创建 / input 保持原始比例。'}
          </p>
        </>
      )}

      {draft.mode !== 'legacy_multipliers' && draft.mode !== 'raw' && (
        <div className="rounded bg-secondary/40 px-2.5 py-1.5 font-mono text-[11px] leading-relaxed">
          <div className="text-muted-foreground">原始：{describe(SAMPLE)}</div>
          {preview ? (
            <div>下游：{preview}</div>
          ) : (
            <div className="text-destructive">{previewError}</div>
          )}
        </div>
      )}
      {draft.mode === 'inflate' && (
        <p className="text-[11px] text-amber-700 dark:text-amber-400">
          加量会改变客户端看到的 token 数；Claude Code 等客户端据此计算上下文占用，倍率过大会让它提前压缩上下文。
        </p>
      )}
    </div>
  )
}
