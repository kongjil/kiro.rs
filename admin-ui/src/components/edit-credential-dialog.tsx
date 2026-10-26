import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { useQuery } from '@tanstack/react-query'
import { Tags, Settings2, Gauge } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import {
  Select,
  SelectGroup,
  SelectLabel,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from '@/components/ui/select'
import { Input } from '@/components/ui/input'
import {
  useUpdateCredential,
  useAccountConcurrencyConfig,
  useAccountRpmLimitConfig,
} from '@/hooks/use-credentials'
import { useGroupOptions } from '@/hooks/use-groups'
import { getProxyPool } from '@/api/credentials'
import { extractErrorMessage, maskProxyUrl, cn } from '@/lib/utils'
import { GroupMultiSelect } from '@/components/group-select'
import { CredentialMetadataEditor, metadataDefaults } from '@/components/credential-metadata-field'
import type { CredentialMetadata, CredentialMetadataSchema, CredentialStatusItem } from '@/types/api'

type TabKey = 'general' | 'limits' | 'metadata'

const TABS: { key: TabKey; label: string; icon: React.ReactNode }[] = [
  { key: 'general', label: '基本', icon: <Settings2 className="h-3.5 w-3.5" /> },
  { key: 'limits', label: '限流', icon: <Gauge className="h-3.5 w-3.5" /> },
  { key: 'metadata', label: 'Metadata', icon: <Tags className="h-3.5 w-3.5" /> },
]

// 与后端 MAX_ACCOUNT_CONCURRENCY / MAX_ACCOUNT_RPM 保持一致
const MAX_CONCURRENCY = 1000
const MAX_RPM = 100000

/** 限流输入框 → 请求值：留空 = 跟随全局（null），否则须为 0..=max 的整数（0 = 不限） */
function parseLimitInput(text: string, max: number): number | null | undefined {
  const trimmed = text.trim()
  if (trimmed === '') return null
  const n = Number(trimmed)
  return Number.isInteger(n) && n >= 0 && n <= max ? n : undefined
}

/** 「已用/上限」读数；上限缺省表示不限 */
function usageText(used: number | undefined, limit: number | undefined): string {
  return `${used ?? 0} / ${limit ?? '不限'}`
}

interface EditCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credential: CredentialStatusItem
  metadataSchema?: CredentialMetadataSchema
}

/** 状态接口的 metadata 带展示信息；编辑时只回填其中的实际值。 */
function metadataValues(metadata: CredentialStatusItem['metadata']): Partial<CredentialMetadata> {
  return Object.fromEntries(
    Object.entries(metadata ?? {}).map(([key, detail]) => [key, detail.value]),
  ) as Partial<CredentialMetadata>
}

export function EditCredentialDialog({
  open,
  onOpenChange,
  credential,
  metadataSchema,
}: EditCredentialDialogProps) {
  const [activeTab, setActiveTab] = useState<TabKey>('general')
  const [email, setEmail] = useState(credential.email ?? '')
  const [proxyUrl, setProxyUrl] = useState(credential.proxyUrl ?? '')
  const [proxyUsername, setProxyUsername] = useState('')
  const [proxyPassword, setProxyPassword] = useState('')
  const [groups, setGroups] = useState<string[]>(credential.groups?.length ? credential.groups : ['default'])
  const [sourceChannel, setSourceChannel] = useState(credential.sourceChannel ?? '')
  const [metadata, setMetadata] = useState<CredentialMetadata>(
    { ...metadataDefaults(metadataSchema), ...metadataValues(credential.metadata) },
  )
  const [manualMode, setManualMode] = useState(false)
  const [maxConcurrency, setMaxConcurrency] = useState(credential.maxConcurrency?.toString() ?? '')
  const [rpmLimit, setRpmLimit] = useState(credential.rpmLimit?.toString() ?? '')

  const groupOptions = useGroupOptions()
  const { data: concurrencyConfig } = useAccountConcurrencyConfig()
  const { data: rpmConfig } = useAccountRpmLimitConfig()

  const { data: proxyPool } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
    enabled: open,
  })

  // 每次打开时重置表单为当前凭据值
  useEffect(() => {
    if (open) {
      setActiveTab('general')
      setEmail(credential.email ?? '')
      setProxyUrl(credential.proxyUrl ?? '')
      setProxyUsername('')
      setProxyPassword('')
      setGroups(credential.groups?.length ? credential.groups : ['default'])
      setSourceChannel(credential.sourceChannel ?? '')
      setMetadata({
        ...metadataDefaults(metadataSchema),
        ...metadataValues(credential.metadata),
      })
      setManualMode(false)
      setMaxConcurrency(credential.maxConcurrency?.toString() ?? '')
      setRpmLimit(credential.rpmLimit?.toString() ?? '')
    }
  }, [open, credential, metadataSchema])

  const { mutate, isPending } = useUpdateCredential()

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()

    const nextConcurrency = parseLimitInput(maxConcurrency, MAX_CONCURRENCY)
    const nextRpm = parseLimitInput(rpmLimit, MAX_RPM)
    if (nextConcurrency === undefined || nextRpm === undefined) {
      setActiveTab('limits')
      toast.error(
        nextConcurrency === undefined
          ? `并发上限需为 0~${MAX_CONCURRENCY} 的整数，留空跟随全局`
          : `RPM 上限需为 0~${MAX_RPM} 的整数，留空跟随全局`,
      )
      return
    }

    mutate(
      {
        id: credential.id,
        req: {
          email: email,
          proxyUrl: proxyUrl,
          proxyUsername: proxyUsername || undefined,
          proxyPassword: proxyPassword || undefined,
          groups: groups,
          sourceChannel: sourceChannel,
          metadata,
          maxConcurrency: nextConcurrency,
          rpmLimit: nextRpm,
        },
      },
      {
        onSuccess: (data) => {
          toast.success(data.message)
          onOpenChange(false)
        },
        onError: (error: unknown) => {
          toast.error(`更新失败: ${extractErrorMessage(error)}`)
        },
      }
    )
  }

  const enabledProxies = proxyPool?.proxies.filter(p => p.enabled) ?? []

  // 当前 proxyUrl 是否是自定义值（不匹配任何标准选项）
  const isCustomUrl = proxyUrl !== '' && proxyUrl !== 'direct' &&
    !enabledProxies.some(p => p.url === proxyUrl)

  // 显示手动输入框：明确进入手动模式，或当前值就是自定义值
  const showManualInput = manualMode || isCustomUrl

  const selectValue = showManualInput ? '__custom__' : proxyUrl

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            编辑凭据 #{credential.id}
          </DialogTitle>
          <DialogDescription>
            修改凭据标识、分组、代理、限流与 Metadata。
          </DialogDescription>
        </DialogHeader>

        {/* 标签导航 */}
        <nav className="-mx-1 flex gap-0.5" aria-label="编辑分区">
          {TABS.map((tab) => (
            <button
              key={tab.key}
              type="button"
              onClick={() => setActiveTab(tab.key)}
              className={cn(
                'inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-[13px] transition-colors',
                'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40',
                activeTab === tab.key
                  ? 'bg-primary/12 font-medium text-foreground'
                  : 'text-muted-foreground hover:bg-accent/60 hover:text-foreground',
              )}
            >
              {tab.icon}
              {tab.label}
            </button>
          ))}
        </nav>

        <form onSubmit={handleSubmit}>
          <div className="min-h-[280px] space-y-4 py-2">
            {activeTab === 'general' && (
              <>
                {/* 邮箱 */}
                <div className="space-y-2">
                  <label htmlFor="email" className="text-sm font-medium">
                    邮箱（用于显示标识）
                  </label>
                  <Input
                    id="email"
                    type="email"
                    placeholder="例: user@example.com"
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    留空则显示凭据 ID，清除请提交空值
                  </p>
                </div>

                {/* 账号分组 */}
                <div className="space-y-2">
                  <label className="text-sm font-medium">账号分组</label>
                  <GroupMultiSelect
                    value={groups}
                    options={groupOptions}
                    onChange={setGroups}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    绑定了某分组的客户端 Key 只会调度到含该分组的账号。不选表示不属于任何分组。
                  </p>
                </div>

                {/* 账号来源渠道 */}
                <div className="space-y-2">
                  <label htmlFor="sourceChannel" className="text-sm font-medium">
                    账号来源渠道（备注）
                  </label>
                  <Input
                    id="sourceChannel"
                    placeholder="例: 官方, 转售商A, 采购平台X"
                    value={sourceChannel}
                    onChange={(e) => setSourceChannel(e.target.value)}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    纯备注，标记此账号的购买来源/渠道，便于追踪。留空表示清除。
                  </p>
                </div>

                {/* 代理配置 */}
                <div className="space-y-2">
                  <label className="text-sm font-medium">代理配置</label>

                  {/* 下拉选择代理 */}
                  <Select
                    value={selectValue === '' ? '__global__' : selectValue}
                    onValueChange={(val) => {
                      if (val === '__custom__') {
                        setManualMode(true)
                      } else {
                        setManualMode(false)
                        setProxyUrl(val === '__global__' ? '' : val)
                      }
                    }}
                    disabled={isPending}
                  >
                    <SelectTrigger className="h-10 rounded-xl px-3.5">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="__global__">使用全局代理配置</SelectItem>
                      <SelectItem value="direct">直连（不使用代理）</SelectItem>
                      {enabledProxies.length > 0 && (
                        <SelectGroup>
                          <SelectLabel>代理池</SelectLabel>
                          {enabledProxies.map((p) => (
                            <SelectItem key={p.id} value={p.url}>
                              {p.label ? `${p.label} | ${maskProxyUrl(p.url)}` : maskProxyUrl(p.url)}
                            </SelectItem>
                          ))}
                        </SelectGroup>
                      )}
                      <SelectItem value="__custom__">手动输入...</SelectItem>
                    </SelectContent>
                  </Select>

                  {/* 自定义 URL 手动输入框 */}
                  {showManualInput && (
                    <Input
                      placeholder="自定义代理 URL（如 socks5://user:pass@host:port）"
                      value={proxyUrl}
                      onChange={(e) => setProxyUrl(e.target.value)}
                      disabled={isPending}
                      className="font-mono text-sm"
                    />
                  )}

                  {/* 代理认证（仅在需要时显示） */}
                  <div className="grid grid-cols-2 gap-2">
                    <Input
                      id="proxyUsername"
                      placeholder="代理用户名（留空不修改）"
                      value={proxyUsername}
                      onChange={(e) => setProxyUsername(e.target.value)}
                      disabled={isPending}
                    />
                    <Input
                      id="proxyPassword"
                      type="password"
                      placeholder="代理密码（留空不修改）"
                      value={proxyPassword}
                      onChange={(e) => setProxyPassword(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <p className="text-xs text-muted-foreground">
                    用户名/密码留空表示不修改；代理 URL 已包含凭据时无需填写
                  </p>
                </div>
              </>
            )}

            {activeTab === 'limits' && (
              <>
                <div className="space-y-2">
                  <label htmlFor="maxConcurrency" className="text-sm font-medium">
                    并发上限
                  </label>
                  <Input
                    id="maxConcurrency"
                    type="number"
                    min={0}
                    max={MAX_CONCURRENCY}
                    step={1}
                    placeholder={
                      concurrencyConfig ? `跟随全局（${concurrencyConfig.limit}）` : '跟随全局'
                    }
                    value={maxConcurrency}
                    onChange={(e) => setMaxConcurrency(e.target.value)}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    本账号同时处理的请求数上限。留空跟随全局默认，0 表示本账号不限。
                    {concurrencyConfig && !concurrencyConfig.enabled && (
                      <span className="text-amber-600 dark:text-amber-400">
                        {' '}全局并发控制已关闭，此设置暂不生效。
                      </span>
                    )}
                  </p>
                </div>

                <div className="space-y-2">
                  <label htmlFor="rpmLimit" className="text-sm font-medium">
                    RPM 上限（次/分钟）
                  </label>
                  <Input
                    id="rpmLimit"
                    type="number"
                    min={0}
                    max={MAX_RPM}
                    step={1}
                    placeholder={rpmConfig ? `跟随全局（${rpmConfig.limit}）` : '跟随全局'}
                    value={rpmLimit}
                    onChange={(e) => setRpmLimit(e.target.value)}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    本账号 60 秒内最多发起的请求数。留空跟随全局默认，0 表示本账号不限。
                    {rpmConfig && !rpmConfig.enabled && (
                      <span className="text-amber-600 dark:text-amber-400">
                        {' '}全局 RPM 限流已关闭，此设置暂不生效。
                      </span>
                    )}
                  </p>
                </div>

                <div className="rounded-md border border-border/60 bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
                  当前负载：并发{' '}
                  <span className="font-mono text-foreground">
                    {usageText(credential.inFlight, credential.effectiveMaxConcurrency)}
                  </span>
                  {' · '}RPM{' '}
                  <span className="font-mono text-foreground">
                    {usageText(credential.rpmUsed, credential.effectiveRpmLimit)}
                  </span>
                </div>
              </>
            )}

            {activeTab === 'metadata' && (
              <CredentialMetadataEditor
                schema={metadataSchema}
                value={metadata}
                onChange={setMetadata}
                disabled={isPending}
              />
            )}
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={isPending}
            >
              取消
            </Button>
            <Button type="submit" disabled={isPending}>
              {isPending ? '保存中...' : '保存'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
