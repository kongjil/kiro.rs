import { useState, useMemo } from 'react'
import { toast } from 'sonner'
import {
  Plus, KeyRound, Trash2, Copy, Eye, EyeOff, Power, RotateCcw, Pencil, RefreshCw, Loader2,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuItem,
} from '@/components/ui/dropdown-menu'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  useClientKeys, useCreateClientKey, useDeleteClientKey,
  useSetClientKeyDisabled, useResetClientKeyStats, useUpdateClientKey,
  useRotateClientKey, useSetClientKeyMaxCredits,
} from '@/hooks/use-client-keys'
import { useGroupOptions } from '@/hooks/use-groups'
import { GroupMultiSelect } from '@/components/group-select'
import type { ClientKeyItem, CreateClientKeyResponse } from '@/types/api'
import { extractErrorMessage, formatCredits } from '@/lib/utils'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { ConsoleTable, type ConsoleColumn } from '@/components/console/data-table'
import { BulkBar } from '@/components/console/bulk-bar'
import { PageHeader } from '@/components/console/page-header'

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + 'M'
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K'
  return n.toString()
}

/**
 * 解析积分上限输入框：
 * - 空字符串 → null（不限制）
 * - 合法非负数 → number
 * - 其它 → 'invalid'
 */
function parseMaxCreditsInput(raw: string): number | null | 'invalid' {
  const t = raw.trim()
  if (t === '') return null
  const n = Number(t)
  if (!Number.isFinite(n) || n < 0) return 'invalid'
  return n
}

/** 缓存计费系数展示：1 → 满，0 → 无，其余显示百分比 */
function formatCacheRatio(ratio?: number): string {
  const v = ratio ?? 1
  if (v >= 1) return '满'
  if (v <= 0) return '无'
  return `${Math.round(v * 100)}%`
}

/** 解析缓存系数输入框：非法 / 空值回落为 1（满缓存），其余夹到 0~1 */
function parseCacheRatio(raw: string): number {
  const n = parseFloat(raw)
  if (!Number.isFinite(n)) return 1
  return Math.min(1, Math.max(0, n))
}

function parseOptionalMultiplier(raw: string): number | undefined {
  if (!raw.trim()) return undefined
  const n = Number(raw)
  return Number.isFinite(n) && n >= 0 ? n : undefined
}

function parseOptionalMultiplierForUpdate(raw: string): number | null {
  if (!raw.trim()) return null
  const n = Number(raw)
  return Number.isFinite(n) && n >= 0 ? n : null
}

/** 渲染「已用积分 / 上限」列。无上限时仅显示已用量。 */
function CreditsUsage({ used, max }: { used: number; max?: number }) {
  if (max == null) {
    return (
      <span className="text-[12px] tabular-nums text-muted-foreground">
        {formatCredits(used)} <span className="text-[11px]">/ 无限制</span>
      </span>
    )
  }
  const ratio = max > 0 ? used / max : 1
  const over = used >= max
  const color = over ? 'text-destructive' : ratio >= 0.8 ? 'text-amber-500' : 'text-foreground'
  return (
    <span className={`text-[12px] tabular-nums ${color}`} title={`已用 ${used} / 上限 ${max}`}>
      {formatCredits(used)} <span className="text-[11px] text-muted-foreground">/ {formatCredits(max)}</span>
    </span>
  )
}

function formatRelative(ts?: string): string {
  if (!ts) return '从未使用'
  const t = new Date(ts).getTime()
  const diff = Date.now() - t
  if (diff < 60_000) return '刚刚'
  if (diff < 3600_000) return `${Math.floor(diff / 60_000)} 分钟前`
  if (diff < 86400_000) return `${Math.floor(diff / 3600_000)} 小时前`
  return `${Math.floor(diff / 86400_000)} 天前`
}

export function ClientKeysPage() {
  const { data, isLoading, isFetching, refetch } = useClientKeys()
  // 已注册分组列表（来自 groups.json 注册表，与凭据的 groups 字段解耦）
  const groupOptions = Array.from(new Set(['default', ...useGroupOptions()]))
  const createKey = useCreateClientKey()
  const deleteKey = useDeleteClientKey()
  const setDisabled = useSetClientKeyDisabled()
  const resetStats = useResetClientKeyStats()
  const updateKey = useUpdateClientKey()
  const rotateKey = useRotateClientKey()
  const setMaxCredits = useSetClientKeyMaxCredits()
  const confirm = useConfirm()

  const [createOpen, setCreateOpen] = useState(false)
  const [createName, setCreateName] = useState('')
  const [createDesc, setCreateDesc] = useState('')
  const [createGroups, setCreateGroups] = useState<string[]>(['default'])
  const [createMaxCredits, setCreateMaxCredits] = useState('')
  const [createCacheRatio, setCreateCacheRatio] = useState('1')
  const [createInputMultiplier, setCreateInputMultiplier] = useState('')
  const [createCreationMultiplier, setCreateCreationMultiplier] = useState('')
  const [createReadMultiplier, setCreateReadMultiplier] = useState('')
  const [createTargetRate, setCreateTargetRate] = useState('')
  const [createSellMultiplier, setCreateSellMultiplier] = useState('1')
  const [createdKey, setCreatedKey] = useState<CreateClientKeyResponse | null>(null)
  const [showCreatedPlain, setShowCreatedPlain] = useState(true)

  const [editOpen, setEditOpen] = useState(false)
  const [editTarget, setEditTarget] = useState<ClientKeyItem | null>(null)
  const [editName, setEditName] = useState('')
  const [editDesc, setEditDesc] = useState('')
  const [editGroups, setEditGroups] = useState<string[]>(['default'])
  const [editMaxCredits, setEditMaxCredits] = useState('')
  const [editCacheRatio, setEditCacheRatio] = useState('1')
  const [editInputMultiplier, setEditInputMultiplier] = useState('')
  const [editCreationMultiplier, setEditCreationMultiplier] = useState('')
  const [editReadMultiplier, setEditReadMultiplier] = useState('')
  const [editTargetRate, setEditTargetRate] = useState('')
  const [editSellMultiplier, setEditSellMultiplier] = useState('1')

  const handleCreate = async (e: React.FormEvent) => {
    e.preventDefault()
    const name = createName.trim()
    if (!name) {
      toast.error('请填写名称')
      return
    }
    const maxCredits = parseMaxCreditsInput(createMaxCredits)
    if (maxCredits === 'invalid') {
      toast.error('积分上限必须是非负数')
      return
    }
    try {
      const res = await createKey.mutateAsync({
        name,
        description: createDesc.trim() || undefined,
        groups: createGroups,
        maxCredits: maxCredits ?? undefined,
        cacheRatio: parseCacheRatio(createCacheRatio),
        inputMultiplier: parseOptionalMultiplier(createInputMultiplier),
        cacheCreationMultiplier: parseOptionalMultiplier(createCreationMultiplier),
        cacheReadMultiplier: parseOptionalMultiplier(createReadMultiplier),
        targetEffectiveHitRate: parseOptionalMultiplier(createTargetRate),
        sellMultiplier: parseOptionalMultiplier(createSellMultiplier) ?? 1,
      })
      setCreatedKey(res)
      setCreateOpen(false)
      setCreateName('')
      setCreateDesc('')
      setCreateGroups(['default'])
      setCreateMaxCredits('')
      setCreateCacheRatio('1')
      setCreateInputMultiplier('')
      setCreateCreationMultiplier('')
      setCreateReadMultiplier('')
      setCreateTargetRate('')
      setCreateSellMultiplier('1')
      setShowCreatedPlain(true)
    } catch (err) {
      toast.error('创建失败：' + extractErrorMessage(err))
    }
  }

  const handleDelete = async (item: ClientKeyItem) => {
    if (
      !(await confirm({
        title: '确认删除 Key',
        description: `确认删除 Key "${item.name}"？此操作无法撤销。`,
        confirmText: '确认删除',
        destructive: true,
      }))
    )
      return
    try {
      await deleteKey.mutateAsync(item.id)
      toast.success(`已删除 Key #${item.id}`)
    } catch (err) {
      toast.error('删除失败：' + extractErrorMessage(err))
    }
  }

  const handleToggleDisabled = async (item: ClientKeyItem) => {
    try {
      await setDisabled.mutateAsync({ id: item.id, disabled: !item.disabled })
      toast.success(item.disabled ? '已启用' : '已禁用')
    } catch (err) {
      toast.error('操作失败：' + extractErrorMessage(err))
    }
  }

  const handleReset = async (item: ClientKeyItem) => {
    if (
      !(await confirm({
        title: '重置统计',
        description: `重置 Key "${item.name}" 的累计统计？`,
        confirmText: '重置',
      }))
    )
      return
    try {
      await resetStats.mutateAsync(item.id)
      toast.success('统计已重置')
    } catch (err) {
      toast.error('重置失败：' + extractErrorMessage(err))
    }
  }

  const handleRotate = async (item: ClientKeyItem) => {
    const systemHint = item.isSystem
      ? '这是系统密钥（config.json apiKey），重新生成后会同步更新 config.json 的 apiKey。'
      : ''
    if (
      !(await confirm({
        title: '重新生成 Key',
        description: `重新生成 Key "${item.name}"？旧明文将立即失效，使用旧明文的下游需要换上新明文才能继续调用。Key 的名称、描述、绑定分组与累计统计保留不变。${systemHint ? ' ' + systemHint : ''}`,
        confirmText: '重新生成',
        destructive: true,
      }))
    )
      return
    try {
      const res = await rotateKey.mutateAsync(item.id)
      setCreatedKey(res)
      setShowCreatedPlain(true)
      // 系统密钥轮换后本地存储的 apiKey 已失效，提示用户用新明文重新登录
      if (item.isSystem) {
        toast.info('系统密钥已更新，若你正用该密钥登录管理面板，请用新明文重新登录')
      }
    } catch (err) {
      toast.error('重新生成失败：' + extractErrorMessage(err))
    }
  }

  const startEdit = (item: ClientKeyItem) => {
    setEditTarget(item)
    setEditName(item.name)
    setEditDesc(item.description ?? '')
    setEditGroups(item.groups?.length ? item.groups : ['default'])
    setEditMaxCredits(item.maxCredits != null ? String(item.maxCredits) : '')
    setEditCacheRatio(String(item.cacheRatio ?? 1))
    setEditInputMultiplier(item.inputMultiplier != null ? String(item.inputMultiplier) : '')
    setEditCreationMultiplier(item.cacheCreationMultiplier != null ? String(item.cacheCreationMultiplier) : '')
    setEditReadMultiplier(item.cacheReadMultiplier != null ? String(item.cacheReadMultiplier) : '')
    setEditTargetRate(item.targetEffectiveHitRate != null ? String(item.targetEffectiveHitRate) : '')
    setEditSellMultiplier(String(item.sellMultiplier ?? 1))
    setEditOpen(true)
  }

  const handleEditSave = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!editTarget) return
    const maxCredits = parseMaxCreditsInput(editMaxCredits)
    if (maxCredits === 'invalid') {
      toast.error('积分上限必须是非负数')
      return
    }
    try {
      await updateKey.mutateAsync({
        id: editTarget.id,
        req: {
          name: editName.trim(),
          description: editDesc.trim(),
          groups: editGroups,
          cacheRatio: parseCacheRatio(editCacheRatio),
          inputMultiplier: parseOptionalMultiplierForUpdate(editInputMultiplier),
          cacheCreationMultiplier: parseOptionalMultiplierForUpdate(editCreationMultiplier),
          cacheReadMultiplier: parseOptionalMultiplierForUpdate(editReadMultiplier),
          targetEffectiveHitRate: parseOptionalMultiplierForUpdate(editTargetRate),
          sellMultiplier: parseOptionalMultiplierForUpdate(editSellMultiplier),
        },
      })
      // 仅在上限发生变化时才调用额度接口，避免无谓写入
      const prev = editTarget.maxCredits ?? null
      if (maxCredits !== prev) {
        await setMaxCredits.mutateAsync({ id: editTarget.id, maxCredits })
      }
      toast.success('已更新')
      setEditOpen(false)
    } catch (err) {
      toast.error('更新失败：' + extractErrorMessage(err))
    }
  }

  const copyText = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text)
      toast.success('已复制')
    } catch {
      toast.error('复制失败')
    }
  }

  const [selectedIds, setSelectedIds] = useState<Set<number | string>>(new Set())
  const [batchActionPending, setBatchActionPending] = useState(false)
  const [batchProgress, setBatchProgress] = useState<{ current: number; total: number; action: 'delete' | 'toggle' } | null>(null)

  const keys: ClientKeyItem[] = useMemo(() => data?.keys ?? [], [data?.keys])

  const handleBatchDelete = async () => {
    if (selectedIds.size === 0) return
    const selectedItems = keys.filter((k) => selectedIds.has(k.id))
    const deletableItems = selectedItems.filter((k) => !k.isSystem)
    const systemCount = selectedItems.length - deletableItems.length

    if (deletableItems.length === 0) {
      toast.error('所选项均为系统密钥，不可删除')
      return
    }

    const systemHint = systemCount > 0 ? `（已自动跳过 ${systemCount} 个系统密钥）` : ''
    const ok = await confirm({
      title: `批量删除 ${deletableItems.length} 把 Key？`,
      description: `确定要删除选中的 ${deletableItems.length} 把客户端 Key 吗？${systemHint}此操作无法撤销。`,
      confirmText: '确认删除',
      destructive: true,
    })
    if (!ok) return

    setBatchActionPending(true)
    setBatchProgress({ current: 0, total: deletableItems.length, action: 'delete' })
    let s = 0
    let f = 0
    try {
      for (let i = 0; i < deletableItems.length; i++) {
        const item = deletableItems[i]
        try {
          await deleteKey.mutateAsync(item.id)
          s++
        } catch {
          f++
        }
        setBatchProgress({ current: i + 1, total: deletableItems.length, action: 'delete' })
      }
      if (f === 0) {
        toast.success(`已批量删除 ${s} 把 Key`)
      } else {
        toast.warning(`批量删除完成：成功 ${s} 个，失败 ${f} 个`)
      }
      setSelectedIds(new Set())
    } finally {
      setBatchActionPending(false)
      setBatchProgress(null)
    }
  }

  const handleBatchSetDisabled = async (disabled: boolean) => {
    if (selectedIds.size === 0) return
    const ids = Array.from(selectedIds).map(Number)
    setBatchActionPending(true)
    setBatchProgress({ current: 0, total: ids.length, action: 'toggle' })
    let s = 0
    let f = 0
    try {
      for (let i = 0; i < ids.length; i++) {
        const id = ids[i]
        try {
          await setDisabled.mutateAsync({ id, disabled })
          s++
        } catch {
          f++
        }
        setBatchProgress({ current: i + 1, total: ids.length, action: 'toggle' })
      }
      toast.success(`已批量${disabled ? '禁用' : '启用'} ${s} 把 Key${f > 0 ? `，失败 ${f} 个` : ''}`)
      setSelectedIds(new Set())
    } finally {
      setBatchActionPending(false)
      setBatchProgress(null)
    }
  }

  const columns: ConsoleColumn<ClientKeyItem>[] = useMemo(
    () => [
      {
        id: 'id',
        header: 'ID',
        cell: (k) => (
          <span className="console-num text-[12px] text-muted-foreground">
            #{k.id}
          </span>
        ),
      },
      {
        id: 'name',
        header: '名称',
        cell: (k) => (
          <div className="min-w-0 max-w-[240px]">
            <div className="flex items-center gap-1.5">
              <span className="truncate font-medium text-foreground">{k.name}</span>
              {k.isSystem && (
                <Badge variant="secondary" title="由 config.json apiKey 同步，不可删除、可轮换">
                  系统
                </Badge>
              )}
            </div>
            {k.description && (
              <div className="truncate text-[11px] text-muted-foreground">
                {k.description}
              </div>
            )}
          </div>
        ),
      },
      {
        id: 'key',
        header: 'Key',
        cell: (k) => (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                className="rounded px-1 py-0.5 font-mono text-[12px] text-muted-foreground hover:bg-accent/60 focus:outline-none focus-visible:ring-1 focus-visible:ring-ring"
                title="点击展开 Key 操作"
              >
                {k.maskedKey}
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start">
              <DropdownMenuItem onSelect={() => handleRotate(k)}>
                <RefreshCw className="h-3.5 w-3.5" />
                重新生成 Key（旧 Key 立即失效）
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        ),
      },
      {
        id: 'group',
        header: '分组',
        cell: (k) =>
          k.groups?.length ? (
            <Badge variant="outline">{k.groups.join(' + ')}</Badge>
          ) : (
            <span className="text-[12px] text-muted-foreground">全部账号</span>
          ),
      },
      {
        id: 'cacheRatio',
        header: '缓存',
        cell: (k) => {
          const v = k.cacheRatio ?? 1
          return (
            <Badge
              variant={v >= 1 ? 'success' : v <= 0 ? 'destructive' : 'outline'}
              title={`缓存计费系数 ${v}`}
            >
              {formatCacheRatio(v)}
            </Badge>
          )
        },
      },
      {
        id: 'status',
        header: '状态',
        cell: (k) =>
          k.disabled ? (
            <Badge variant="destructive">已禁用</Badge>
          ) : (
            <Badge variant="success">启用</Badge>
          ),
      },
      {
        id: 'totalCalls',
        header: '总调用',
        align: 'right',
        cell: (k) => <span className="console-num">{k.totalCalls}</span>,
      },
      {
        id: 'inputTokens',
        header: '输入',
        align: 'right',
        cell: (k) => <span className="console-num">{formatTokens(k.totalInputTokens)}</span>,
      },
      {
        id: 'outputTokens',
        header: '输出',
        align: 'right',
        cell: (k) => <span className="console-num">{formatTokens(k.totalOutputTokens)}</span>,
      },
      {
        id: 'credits',
        header: '积分 / 上限',
        align: 'right',
        cell: (k) => <CreditsUsage used={k.totalCredits} max={k.maxCredits} />,
      },
      {
        id: 'lastUsed',
        header: '最后使用',
        cell: (k) => (
          <span className="text-[12px] text-muted-foreground">
            {formatRelative(k.lastUsedAt)}
          </span>
        ),
      },
    ],
    [],
  )

  const rowActions = (k: ClientKeyItem) => (
    <div className="flex items-center justify-end gap-1">
      <Button
        size="icon"
        variant="ghost"
        onClick={(e) => {
          e.stopPropagation()
          startEdit(k)
        }}
        title="编辑"
      >
        <Pencil className="h-3.5 w-3.5" />
      </Button>
      <Button
        size="icon"
        variant="ghost"
        onClick={(e) => {
          e.stopPropagation()
          handleToggleDisabled(k)
        }}
        title={k.disabled ? '启用' : '禁用'}
      >
        <Power className={`h-3.5 w-3.5 ${k.disabled ? 'text-emerald-500' : 'text-amber-500'}`} />
      </Button>
      <Button
        size="icon"
        variant="ghost"
        onClick={(e) => {
          e.stopPropagation()
          handleReset(k)
        }}
        title="重置统计"
      >
        <RotateCcw className="h-3.5 w-3.5" />
      </Button>
      {!k.isSystem && (
        <Button
          size="icon"
          variant="ghost"
          className="text-destructive hover:text-destructive"
          onClick={(e) => {
            e.stopPropagation()
            handleDelete(k)
          }}
          title="删除"
        >
          <Trash2 className="h-3.5 w-3.5" />
        </Button>
      )}
    </div>
  )

  return (
    <div className="console-scope space-y-4">
      <PageHeader
        breadcrumbs={[{ label: '控制台' }, { label: '客户端 Key', active: true }]}
        icon={<KeyRound className="h-4 w-4" />}
        title="客户端 Key"
        description="分发给下游用户/项目的访问密钥。每把 Key 独立计数与禁用，泄露后只需替换一把。"
        actions={
          <>
            <Button
              variant="outline"
              size="sm"
              onClick={() => refetch()}
              disabled={isFetching}
            >
              <RefreshCw className={`h-3.5 w-3.5 ${isFetching ? 'animate-spin' : ''}`} />
              刷新
            </Button>
            <Button onClick={() => setCreateOpen(true)} size="sm">
              <Plus className="h-3.5 w-3.5" />
              新建 Key
            </Button>
          </>
        }
      />

      <ConsoleTable
        rows={keys}
        columns={columns}
        rowKey={(k) => k.id}
        selectable
        selected={selectedIds}
        onSelectedChange={setSelectedIds}
        rowActions={rowActions}
        loading={isLoading}
        empty="还没有客户端 Key，点击右上角「新建 Key」开始。"
      />

      {/* 吸底批量操作栏 */}
      <BulkBar
        count={selectedIds.size}
        onClear={() => setSelectedIds(new Set())}
        noun="把 Key"
      >
        <Button
          onClick={() => handleBatchSetDisabled(false)}
          size="sm"
          variant="ghost"
          className="h-7 px-2.5 text-xs gap-1 rounded hover:bg-accent"
          disabled={batchActionPending}
        >
          <Power className="h-3.5 w-3.5 text-emerald-500" />
          批量启用
        </Button>
        <Button
          onClick={() => handleBatchSetDisabled(true)}
          size="sm"
          variant="ghost"
          className="h-7 px-2.5 text-xs gap-1 rounded hover:bg-accent"
          disabled={batchActionPending}
        >
          {batchActionPending && batchProgress?.action === 'toggle' ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin text-amber-500" />
              处理中 {batchProgress.current}/{batchProgress.total}
            </>
          ) : (
            <>
              <Power className="h-3.5 w-3.5 text-amber-500" />
              批量禁用
            </>
          )}
        </Button>
        <Button
          onClick={handleBatchDelete}
          size="sm"
          variant="destructive"
          className="h-7 px-2.5 text-xs gap-1 rounded"
          disabled={batchActionPending}
        >
          {batchActionPending && batchProgress?.action === 'delete' ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              删除中 {batchProgress.current}/{batchProgress.total}
            </>
          ) : (
            <>
              <Trash2 className="h-3.5 w-3.5" />
              批量删除
            </>
          )}
        </Button>
      </BulkBar>

      {/* 新建对话框 */}
      {createOpen && (
        <Dialog open={createOpen} onOpenChange={(o) => !createKey.isPending && setCreateOpen(o)}>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>新建客户端 Key</DialogTitle>
              <DialogDescription>
                创建后明文 Key 仅显示一次，请立即复制保存到安全位置。
              </DialogDescription>
            </DialogHeader>
            <form onSubmit={handleCreate} className="space-y-3 py-2">
              <div>
                <label className="text-[12px] text-muted-foreground">名称 *</label>
                <Input
                  placeholder="VS Code 本机 / 团队 A 等"
                  value={createName}
                  onChange={(e) => setCreateName(e.target.value)}
                  disabled={createKey.isPending}
                  autoFocus
                />
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">描述（可选）</label>
                <Input
                  placeholder="可选备注，如绑定的项目、负责人等"
                  value={createDesc}
                  onChange={(e) => setCreateDesc(e.target.value)}
                  disabled={createKey.isPending}
                />
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">绑定分组（可选）</label>
                <GroupMultiSelect
                  value={createGroups}
                  options={groupOptions}
                  onChange={setCreateGroups}
                  disabled={createKey.isPending}
                />
                <p className="mt-1 text-[11px] text-muted-foreground">
                  default 始终生效；额外分组会扩大可调用账号范围。未分组账号按 default 处理。
                </p>
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">积分上限（可选）</label>
                <Input
                  type="number"
                  min="0"
                  step="any"
                  inputMode="decimal"
                  placeholder="留空表示不限制"
                  value={createMaxCredits}
                  onChange={(e) => setCreateMaxCredits(e.target.value)}
                  disabled={createKey.isPending}
                />
                <p className="mt-1 text-[11px] text-muted-foreground">
                  累计使用的 credit 达到上限后，该 Key 的请求会被拒绝（HTTP 429）。重置统计后重新计费。
                </p>
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">缓存系数（0~1）</label>
                <Input
                  type="number"
                  min="0"
                  max="1"
                  step="0.05"
                  inputMode="decimal"
                  placeholder="1 = 满缓存，0 = 无缓存，0.5 = 缓存红利打五折"
                  value={createCacheRatio}
                  onChange={(e) => setCreateCacheRatio(e.target.value)}
                  disabled={createKey.isPending}
                />
                <p className="mt-1 text-[11px] text-muted-foreground">
                  控制本 Key 请求上报给下游的缓存红利比例：1=满缓存（享 cache_read 折扣），0=无缓存（全按 input 计），0.5=缓存覆盖砍半。不影响发往上游的真实请求。
                </p>
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div><label className="text-[12px] text-muted-foreground">Input 倍率（无目标时）</label><Input type="number" min="0" step="0.05" placeholder="留空=原始" value={createInputMultiplier} onChange={(e) => setCreateInputMultiplier(e.target.value)} /></div>
                <div><label className="text-[12px] text-muted-foreground">Cache 创建倍率（无目标时）</label><Input type="number" min="0" step="0.05" placeholder="留空=原始" value={createCreationMultiplier} onChange={(e) => setCreateCreationMultiplier(e.target.value)} /></div>
                <div><label className="text-[12px] text-muted-foreground">Cache 读取倍率（无目标时）</label><Input type="number" min="0" step="0.05" placeholder="留空=原始" value={createReadMultiplier} onChange={(e) => setCreateReadMultiplier(e.target.value)} /></div>
                <div><label className="text-[12px] text-muted-foreground">目标命中率</label><Input type="number" min="0" max="1" step="0.05" placeholder="留空=倍率模式" value={createTargetRate} onChange={(e) => setCreateTargetRate(e.target.value)} /></div>
                <div><label className="text-[12px] text-muted-foreground">卖出倍率</label><Input type="number" min="0" step="0.05" value={createSellMultiplier} onChange={(e) => setCreateSellMultiplier(e.target.value)} /></div>
              </div>
              <DialogFooter>
                <Button type="button" variant="outline" onClick={() => setCreateOpen(false)} disabled={createKey.isPending}>
                  取消
                </Button>
                <Button type="submit" disabled={createKey.isPending || !createName.trim()}>
                  {createKey.isPending ? '创建中…' : '创建'}
                </Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      )}

      {/* 创建后明文展示对话框 */}
      {createdKey && (
        <Dialog open={!!createdKey} onOpenChange={(o) => { if (!o) setCreatedKey(null) }}>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2">
                <KeyRound className="h-4 w-4 text-emerald-500" />
                Key 已生成
              </DialogTitle>
              <DialogDescription>
                这是 Key "{createdKey?.name}" 的明文。<strong>关闭对话框后将无法再查看</strong>，请立即复制。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="relative">
                <Input
                  readOnly
                  type={showCreatedPlain ? 'text' : 'password'}
                  value={createdKey?.key ?? ''}
                  className="pr-20 font-mono text-[13px]"
                />
                <div className="absolute inset-y-0 right-0 flex items-center pr-1">
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7"
                    onClick={() => setShowCreatedPlain((v) => !v)}
                    title={showCreatedPlain ? '隐藏' : '显示'}
                  >
                    {showCreatedPlain ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                  </Button>
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7"
                    onClick={() => createdKey && copyText(createdKey.key)}
                    title="复制"
                  >
                    <Copy className="h-3.5 w-3.5" />
                  </Button>
                </div>
              </div>
              <p className="text-[11px] text-muted-foreground">
                客户端调用 <code>/v1/messages</code> 时，把它放在 <code>x-api-key</code> 或 <code>Authorization: Bearer</code> 头中。
              </p>
            </div>
            <DialogFooter>
              <Button onClick={() => setCreatedKey(null)}>我已保存好</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}

      {/* 编辑对话框 */}
      {editOpen && (
        <Dialog open={editOpen} onOpenChange={(o) => !updateKey.isPending && setEditOpen(o)}>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>编辑 Key</DialogTitle>
              <DialogDescription>修改名称与描述（不影响 Key 值与统计）</DialogDescription>
            </DialogHeader>
            <form onSubmit={handleEditSave} className="space-y-3 py-2">
              <div>
                <label className="text-[12px] text-muted-foreground">名称</label>
                <Input value={editName} onChange={(e) => setEditName(e.target.value)} />
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">描述</label>
                <Input value={editDesc} onChange={(e) => setEditDesc(e.target.value)} />
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">绑定分组</label>
                <GroupMultiSelect
                  value={editGroups}
                  options={groupOptions}
                  onChange={setEditGroups}
                  disabled={updateKey.isPending}
                />
                <p className="mt-1 text-[11px] text-muted-foreground">
                  default 始终生效；选择 A 后可调用 default 或 A 账号。
                </p>
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">积分上限</label>
                <Input
                  type="number"
                  min="0"
                  step="any"
                  inputMode="decimal"
                  placeholder="留空表示不限制"
                  value={editMaxCredits}
                  onChange={(e) => setEditMaxCredits(e.target.value)}
                  disabled={updateKey.isPending || setMaxCredits.isPending}
                />
                <p className="mt-1 text-[11px] text-muted-foreground">
                  累计 credit 达到上限后该 Key 请求会被拒绝（HTTP 429）。清空则取消限制；重置统计可清零已用量。
                </p>
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">缓存系数（0~1）</label>
                <Input
                  type="number"
                  min="0"
                  max="1"
                  step="0.05"
                  inputMode="decimal"
                  placeholder="1 = 满缓存，0 = 无缓存，0.5 = 缓存红利打五折"
                  value={editCacheRatio}
                  onChange={(e) => setEditCacheRatio(e.target.value)}
                  disabled={updateKey.isPending}
                />
                <p className="mt-1 text-[11px] text-muted-foreground">
                  1=满缓存，0=无缓存（缓存 token 归零，全按 input 计），0.5=缓存红利打五折。
                </p>
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div><label className="text-[12px] text-muted-foreground">Input 倍率（无目标时）</label><Input type="number" min="0" step="0.05" placeholder="留空=原始" value={editInputMultiplier} onChange={(e) => setEditInputMultiplier(e.target.value)} /></div>
                <div><label className="text-[12px] text-muted-foreground">Cache 创建倍率（无目标时）</label><Input type="number" min="0" step="0.05" placeholder="留空=原始" value={editCreationMultiplier} onChange={(e) => setEditCreationMultiplier(e.target.value)} /></div>
                <div><label className="text-[12px] text-muted-foreground">Cache 读取倍率（无目标时）</label><Input type="number" min="0" step="0.05" placeholder="留空=原始" value={editReadMultiplier} onChange={(e) => setEditReadMultiplier(e.target.value)} /></div>
                <div><label className="text-[12px] text-muted-foreground">目标命中率</label><Input type="number" min="0" max="1" step="0.05" placeholder="留空=倍率模式" value={editTargetRate} onChange={(e) => setEditTargetRate(e.target.value)} /></div>
                <div><label className="text-[12px] text-muted-foreground">卖出倍率</label><Input type="number" min="0" step="0.05" value={editSellMultiplier} onChange={(e) => setEditSellMultiplier(e.target.value)} /></div>
              </div>
              <DialogFooter>
                <Button type="button" variant="outline" onClick={() => setEditOpen(false)}>取消</Button>
                <Button type="submit" disabled={updateKey.isPending || setMaxCredits.isPending}>
                  {updateKey.isPending || setMaxCredits.isPending ? '保存中…' : '保存'}
                </Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      )}
    </div>
  )
}
