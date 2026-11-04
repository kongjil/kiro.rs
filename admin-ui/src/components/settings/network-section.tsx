import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import {
  Activity,
  Send,
  Plus,
  Trash2,
  RotateCcw,
  Save,
  Loader2,
  Info,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { SettingGroup, SettingRow } from '@/components/console/setting-row'
import {
  batchAddProxies,
  assignProxiesRoundRobin,
  getCredentials,
  getGlobalProxy,
  getProxyPool,
  getProxyQualityConfig,
  runProxyQualityCheck,
  setProxyQualityConfig,
  setGlobalProxy,
} from '@/api/credentials'
import { useCustomHeaders, useSetCustomHeaders } from '@/hooks/use-credentials'
import { extractErrorMessage, maskProxyUrl } from '@/lib/utils'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import type { CustomHeaderItem } from '@/types/api'

/**
 * 自定义响应头支持的运行时插值变量
 */
const AVAILABLE_VARIABLES: { tag: string; desc: string }[] = [
  { tag: '{trace_id}', desc: '链路追踪 ID' },
  { tag: '{request_id}', desc: '同 trace_id' },
  { tag: '{model}', desc: '请求模型名' },
  { tag: '{credential_id}', desc: '命中凭据 ID' },
  { tag: '{key_id}', desc: '客户端 Key ID' },
  { tag: '{group}', desc: '分组名' },
  { tag: '{client_ip}', desc: '客户端真实 IP' },
]

/**
 * 默认推荐的自定义响应头（开箱即用支持 NewAPI / OneAPI）
 */
const DEFAULT_HEADERS: CustomHeaderItem[] = [
  { key: 'X-Oneapi-Request-Id', value: '{trace_id}', enabled: true },
  { key: 'X-Request-Id', value: '{trace_id}', enabled: true },
]

/**
 * 账号专属代理：每个账号单独绑定一条代理，不提供共享全局代理编辑器，
 * 只保留清除旧全局兜底的入口。
 */
function AccountProxySetting() {
  const [batchText, setBatchText] = useState('')
  const [precheckMinIps, setPrecheckMinIps] = useState<number | undefined>()
  const queryClient = useQueryClient()

  const { data: pool } = useQuery({ queryKey: ['proxy-pool'], queryFn: getProxyPool })
  const { data: credentials } = useQuery({ queryKey: ['credentials'], queryFn: () => getCredentials() })
  const { data: globalProxy } = useQuery({ queryKey: ['global-proxy'], queryFn: getGlobalProxy })
  const { data: qualityConfig } = useQuery({ queryKey: ['proxy-quality-config'], queryFn: getProxyQualityConfig })

  const batchMutation = useMutation({
    mutationFn: () => batchAddProxies({
      urls: batchText.split(/\r?\n/).map((line) => line.trim()).filter(Boolean),
    }),
    onSuccess: (result) => {
      setBatchText('')
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
      toast.success(`代理池导入完成：成功 ${result.added} 条，跳过 ${result.errors} 条`)
    },
    onError: (error) => toast.error(`代理导入失败：${extractErrorMessage(error)}`),
  })

  const assignMutation = useMutation({
    mutationFn: () => assignProxiesRoundRobin(),
    onSuccess: (result) => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
      toast.success(`一对一分配完成：${result.assigned} 个账号，使用 ${result.proxyCount} 条代理`)
    },
    onError: (error) => toast.error(`分配失败：${extractErrorMessage(error)}`),
  })

  const precheckMutation = useMutation({
    mutationFn: async () => {
      await setProxyQualityConfig({
        enabled: true,
        autoAssign: true,
        precheckMinIps: precheckMinIps ?? qualityConfig?.precheckMinIps ?? 0,
      })
      return runProxyQualityCheck()
    },
    onSuccess: (result) => {
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      queryClient.invalidateQueries({ queryKey: ['proxy-quality-config'] })
      toast.success(
        `预检完成：已检查 ${result.checked} 条，干净 ${result.clean} 条，新增 ${result.generated} 条；账号 ${result.requiredCredentials} 个，干净 IP ${result.availableClean} 条，分配/更新 ${result.assigned} 个，删除旧链接 ${result.removed} 条。`
      )
    },
    onError: (error) => toast.error(`预检失败：${extractErrorMessage(error)}`),
  })

  const clearLegacyGlobalMutation = useMutation({
    mutationFn: () => setGlobalProxy({ proxyUrl: null }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['global-proxy'] })
      toast.success('旧的全局代理兜底已清除；后续只走账号专属代理')
    },
    onError: (error) => toast.error(`清除失败：${extractErrorMessage(error)}`),
  })

  const handleClearLegacyGlobal = () => {
    if (window.confirm('清除旧的全局代理兜底？只有已绑定账号专属代理的账号还能通过代理访问上游。')) {
      clearLegacyGlobalMutation.mutate()
    }
  }

  const enabledProxies = pool?.proxies.filter((proxy) => proxy.enabled) ?? []

  return (
    <SettingGroup
      title="账号专属代理"
      description="每个 Kiro 账号单独绑定一条代理；预检通过后按出口 IP 去重分配，不把代理设为全局共享。"
    >
      {globalProxy?.proxyUrl && (
        <SettingRow
          label="旧的全局代理兜底"
          hint={`仍有全局代理配置：${maskProxyUrl(globalProxy.proxyUrl)}。未绑定专属代理的账号会回退使用它。`}
        >
          <Button
            size="sm"
            variant="destructive"
            onClick={handleClearLegacyGlobal}
            disabled={clearLegacyGlobalMutation.isPending}
          >
            <Trash2 className="mr-1 h-3.5 w-3.5" />
            {clearLegacyGlobalMutation.isPending ? '清除中…' : '清除全局兜底'}
          </Button>
        </SettingRow>
      )}

      <SettingRow
        label="导入代理池"
        hint="每行一条完整代理 URL；支持 socks5://用户名:密码@主机:端口，认证信息会保留在该代理链接内。"
      >
        <div className="w-full space-y-2">
          <textarea
            value={batchText}
            onChange={(event) => setBatchText(event.target.value)}
            placeholder={'socks5://用户名:密码@主机:端口\nsocks5://用户名:密码@主机:端口'}
            className="min-h-[96px] w-full rounded-md border border-input bg-background px-2 py-1.5 font-mono text-[12px] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/30"
            spellCheck={false}
          />
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              onClick={() => batchMutation.mutate()}
              disabled={batchMutation.isPending || !batchText.trim()}
            >
              {batchMutation.isPending ? '导入中…' : '批量导入'}
            </Button>
            <span className="text-xs text-muted-foreground">代理池 {pool?.total ?? 0} 条</span>
          </div>
        </div>
      </SettingRow>

      <SettingRow
        label="一账号一代理预检"
        hint={`当前账号 ${credentials?.total ?? 0} 个；启用前检查出口 IP、去重，不足时先检查备用链接并按 sid 模板补充。`}
      >
        <div className="flex flex-wrap items-center gap-2">
          <label className="text-xs text-muted-foreground">
            最少保留干净 IP
            <Input
              type="number"
              min={0}
              value={precheckMinIps ?? qualityConfig?.precheckMinIps ?? 0}
              onChange={(event) => setPrecheckMinIps(Math.max(0, Number(event.target.value)))}
              className="mt-1 h-8 w-36"
            />
          </label>
          <Button
            size="sm"
            onClick={() => precheckMutation.mutate()}
            disabled={precheckMutation.isPending}
          >
            <Activity className="mr-1 h-3.5 w-3.5" />
            {precheckMutation.isPending ? '预检中…' : '预检并一对一分配'}
          </Button>
          <Button
            size="sm"
            variant="outline"
            onClick={() => assignMutation.mutate()}
            disabled={assignMutation.isPending}
          >
            {assignMutation.isPending ? '分配中…' : '一对一分配现有代理'}
          </Button>
          <span className="text-xs text-muted-foreground">默认每 100 分钟自动检查，并发 10</span>
        </div>
      </SettingRow>

      <SettingRow label="代理池状态" hint="同一条代理不会被手动分配给多个账号；质量检查通过的出口 IP 会显示在这里。">
        <div className="max-h-56 w-full space-y-1 overflow-y-auto rounded-md border p-2">
          {enabledProxies.length === 0 && (
            <div className="py-3 text-center text-xs text-muted-foreground">代理池暂无启用代理</div>
          )}
          {enabledProxies.map((proxy) => (
            <div key={proxy.id} className="flex items-center justify-between gap-2 rounded px-1 py-1 text-xs">
              <div className="min-w-0">
                <div className="truncate font-mono" title={proxy.url}>{maskProxyUrl(proxy.url)}</div>
                <div className="mt-0.5 flex flex-wrap gap-2 text-muted-foreground">
                  {proxy.observedIp && <span>出口 IP {proxy.observedIp}</span>}
                  {proxy.credentialCount > 0 && <span>绑定 {proxy.credentialCount} 个账号</span>}
                </div>
              </div>
              <Badge variant={proxy.qualityStatus === 'clean' ? 'secondary' : 'outline'}>
                {proxy.qualityStatus === 'clean' ? '纯净' : proxy.qualityStatus === 'dirty' ? '脏 IP' : proxy.qualityStatus === 'error' ? '检查错误' : '待检查'}
              </Badge>
            </div>
          ))}
        </div>
      </SettingRow>
    </SettingGroup>
  )
}

function CustomHeadersSetting() {
  const { data, isLoading } = useCustomHeaders()
  const { mutateAsync: saveHeaders, isPending: isSaving } = useSetCustomHeaders()

  const [drafts, setDrafts] = useState<CustomHeaderItem[]>([])
  const [activeInputIndex, setActiveInputIndex] = useState<number | null>(null)

  useEffect(() => {
    if (data?.headers) {
      setDrafts(data.headers.map((h) => ({ ...h })))
    }
  }, [data])

  const isDirty = useMemo(() => {
    if (!data?.headers) return drafts.length > 0
    if (drafts.length !== data.headers.length) return true
    return drafts.some((d, i) => {
      const orig = data.headers[i]
      return (
        d.key !== orig.key ||
        d.value !== orig.value ||
        d.enabled !== orig.enabled
      )
    })
  }, [drafts, data])

  const addRow = () => {
    if (drafts.length >= 50) {
      toast.error('最多支持配置 50 条自定义响应头')
      return
    }
    setDrafts((prev) => [
      ...prev,
      { key: '', value: '{trace_id}', enabled: true },
    ])
    setActiveInputIndex(drafts.length)
  }

  const removeRow = (index: number) => {
    setDrafts((prev) => prev.filter((_, i) => i !== index))
  }

  const updateRow = <K extends keyof CustomHeaderItem>(
    index: number,
    field: K,
    value: CustomHeaderItem[K],
  ) => {
    setDrafts((prev) => {
      const next = [...prev]
      next[index] = { ...next[index], [field]: value }
      return next
    })
  }

  const insertVariable = (tag: string) => {
    if (drafts.length === 0) {
      setDrafts([{ key: 'X-Oneapi-Request-Id', value: tag, enabled: true }])
      return
    }
    const targetIdx =
      activeInputIndex !== null && activeInputIndex < drafts.length
        ? activeInputIndex
        : drafts.length - 1
    const currentVal = drafts[targetIdx].value
    updateRow(targetIdx, 'value', currentVal ? `${currentVal}${tag}` : tag)
    toast.success(`已向第 ${targetIdx + 1} 行追加变量 ${tag}`)
  }

  const resetToDefault = () => {
    setDrafts(DEFAULT_HEADERS.map((h) => ({ ...h })))
    toast.info('已载入推荐默认响应头，点击「保存更改」生效')
  }

  const handleSave = async () => {
    const seen = new Set<string>()
    for (let i = 0; i < drafts.length; i++) {
      const row = drafts[i]
      const key = row.key.trim()
      if (!key) {
        toast.error(`第 ${i + 1} 条响应头的 Header Name 不能为空`)
        return
      }
      // HTTP Header Name 标准校验
      if (!/^[a-zA-Z0-9!#$%&'*+-.^_`|~]+$/.test(key)) {
        toast.error(`第 ${i + 1} 条响应头名称 "${key}" 格式不合法（不能含空格或特殊符号）`)
        return
      }
      if (row.value.includes('\r') || row.value.includes('\n')) {
        toast.error(`第 ${i + 1} 条响应头的值不能包含换行符`)
        return
      }
      const lower = key.toLowerCase()
      if (seen.has(lower)) {
        toast.error(`响应头名称 "${key}" 存在重复（不区分大小写）`)
        return
      }
      seen.add(lower)
    }

    try {
      await saveHeaders({
        headers: drafts.map((d) => ({
          key: d.key.trim(),
          value: d.value.trim(),
          enabled: d.enabled,
        })),
      })
      toast.success('自定义响应头配置已保存并即时生效')
    } catch (err) {
      toast.error(`保存失败: ${extractErrorMessage(err)}`)
    }
  }

  const enabledCount = drafts.filter((d) => d.enabled).length

  return (
    <SettingGroup
      title="自定义 HTTP 响应头"
      description="向所有下游 HTTP 响应（包含流式 SSE、非流式 JSON 及异常错误）注入自定义 Header。支持多响应头、启用停用与动态变量插值，修改后即时生效。"
    >
      <div className="space-y-4 py-1">
        {/* 概览与快捷操作 */}
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="flex items-center gap-2 text-[12px] text-muted-foreground">
            <Send className="h-4 w-4 text-primary" />
            <Badge variant="outline" className="console-num text-[11px]">
              {enabledCount} / {drafts.length} 启用
            </Badge>
          </div>
          <Button
            size="sm"
            variant="outline"
            onClick={resetToDefault}
            disabled={isLoading || isSaving}
            title="恢复推荐的 NewAPI / OneAPI 默认响应头配置"
            className="h-8 text-xs"
          >
            <RotateCcw className="mr-1.5 h-3.5 w-3.5" />
            恢复默认
          </Button>
        </div>

        {/* 表格视图（桌面端） */}
        {drafts.length > 0 ? (
          <div className="rounded-lg border border-border/70 overflow-hidden bg-card/50">
            <div className="hidden md:grid grid-cols-[60px_1fr_1.5fr_44px] items-center gap-3 border-b border-border/60 bg-muted/40 px-4 py-2 text-[11.5px] font-semibold text-muted-foreground uppercase tracking-wider">
              <div>启用</div>
              <div>Header Name（响应头键名）</div>
              <div>Header Value（取值 / 支持变量插值）</div>
              <div className="text-right">操作</div>
            </div>

            <div className="divide-y divide-border/40">
              {drafts.map((row, i) => (
                <div
                  key={i}
                  className="flex flex-col gap-2 p-3 md:grid md:grid-cols-[60px_1fr_1.5fr_44px] md:items-center md:gap-3 md:px-4 md:py-2.5 hover:bg-muted/20 transition-colors"
                >
                  <div className="flex items-center justify-between md:justify-start">
                    <span className="text-xs text-muted-foreground md:hidden font-medium">启用状态</span>
                    <Switch
                      checked={row.enabled}
                      onCheckedChange={(val) => updateRow(i, 'enabled', val)}
                      disabled={isSaving}
                      aria-label={`启用第 ${i + 1} 条响应头`}
                    />
                  </div>

                  <div className="space-y-1 md:space-y-0">
                    <span className="text-[11px] text-muted-foreground md:hidden font-medium">Header Name</span>
                    <Input
                      value={row.key}
                      onChange={(e) => updateRow(i, 'key', e.target.value)}
                      placeholder="如 X-Oneapi-Request-Id"
                      disabled={isSaving}
                      spellCheck={false}
                      className="console-num h-8 text-[12.5px]"
                    />
                  </div>

                  <div className="space-y-1 md:space-y-0">
                    <span className="text-[11px] text-muted-foreground md:hidden font-medium">Header Value</span>
                    <Input
                      value={row.value}
                      onChange={(e) => updateRow(i, 'value', e.target.value)}
                      onFocus={() => setActiveInputIndex(i)}
                      placeholder="如 {trace_id}"
                      disabled={isSaving}
                      spellCheck={false}
                      className="console-num h-8 text-[12.5px]"
                    />
                  </div>

                  <div className="flex justify-end">
                    <Button
                      size="icon"
                      variant="ghost"
                      onClick={() => removeRow(i)}
                      disabled={isSaving}
                      title="删除此响应头"
                      className="h-8 w-8 text-muted-foreground hover:text-destructive"
                    >
                      <Trash2 className="h-4 w-4" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          </div>
        ) : (
          <div className="rounded-lg border border-dashed border-border/80 p-8 text-center">
            <p className="text-sm text-muted-foreground">当前暂无自定义响应头规则</p>
            <p className="mt-1 text-xs text-muted-foreground/80">
              默认会下发 <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">X-Oneapi-Request-Id</code> 与 <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">X-Request-Id</code>。点击下方按钮添加。
            </p>
          </div>
        )}

        {/* 变量插值快捷选择 */}
        <div className="rounded-lg border border-border/50 bg-muted/20 p-3 space-y-2">
          <div className="text-[12px] font-medium text-foreground flex items-center gap-1.5">
            <span>常用变量插值（点击追加到正在编辑的行）：</span>
          </div>
          <div className="flex flex-wrap gap-1.5">
            {AVAILABLE_VARIABLES.map((item) => (
              <button
                key={item.tag}
                type="button"
                onClick={() => insertVariable(item.tag)}
                title={`点击插入 ${item.tag} (${item.desc})`}
                className="inline-flex items-center gap-1 rounded border border-border/70 bg-card px-2 py-1 font-mono text-[11.5px] transition-colors hover:border-primary/50 hover:bg-primary/5 cursor-pointer"
              >
                <span className="font-semibold text-primary">{item.tag}</span>
                <span className="text-[11px] text-muted-foreground">· {item.desc}</span>
              </button>
            ))}
          </div>
        </div>

        {/* NewAPI / OneAPI 规范提示 */}
        <div className="rounded-lg border border-primary/20 bg-primary/5 p-3 text-[12px] leading-relaxed text-muted-foreground">
          <div className="flex items-center gap-1.5 font-medium text-foreground">
            <Info className="h-4 w-4 text-primary shrink-0" />
            <span>NewAPI / OneAPI 链路关联机制</span>
          </div>
          <div className="mt-1">
            NewAPI 与 OneAPI 会自动在上游响应头中提取 <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px] text-foreground">X-Oneapi-Request-Id</code> 作为其内部的渠道请求 ID（upstream_request_id）。保留该响应头并将值设为 <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px] text-foreground">&#123;trace_id&#125;</code>，可在 NewAPI 的消费日志中精准关联与追踪 kiro-rs 侧的详细诊断日志。
          </div>
        </div>

        {/* 底部操作栏 */}
        <div className="flex flex-wrap items-center justify-between gap-2 pt-2 border-t border-border/40">
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={addRow}
            disabled={isSaving || drafts.length >= 50}
            className="h-8 text-xs"
          >
            <Plus className="mr-1.5 h-3.5 w-3.5" />
            添加响应头
          </Button>

          <div className="flex items-center gap-2">
            {isDirty && (
              <span className="text-xs text-amber-500 font-medium">
                有未保存的更改
              </span>
            )}
            <Button
              type="button"
              size="sm"
              onClick={handleSave}
              disabled={isSaving || !isDirty}
              className="h-8 text-xs min-w-[80px]"
            >
              {isSaving ? (
                <>
                  <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" />
                  保存中
                </>
              ) : (
                <>
                  <Save className="mr-1.5 h-3.5 w-3.5" />
                  保存更改
                </>
              )}
            </Button>
          </div>
        </div>
      </div>
    </SettingGroup>
  )
}

export function NetworkSection() {
  return (
    <div className="space-y-6">
      <AccountProxySetting />
      <CustomHeadersSetting />
    </div>
  )
}
