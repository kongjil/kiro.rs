import { useState } from 'react'
import { toast } from 'sonner'
import { Activity, Trash2 } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
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
import { extractErrorMessage, maskProxyUrl } from '@/lib/utils'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

export function NetworkSection() {
  const [batchText, setBatchText] = useState('')
  const [precheckMinIps, setPrecheckMinIps] = useState<number | undefined>()
  const queryClient = useQueryClient()

  const { data: pool } = useQuery({ queryKey: ['proxy-pool'], queryFn: getProxyPool })
  const { data: credentials } = useQuery({ queryKey: ['credentials'], queryFn: getCredentials })
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
        `预检完成：已检查 ${result.checked} 条，干净 ${result.clean} 条，新增 ${result.generated} 条；账号 ${result.requiredCredentials} 个，干净 IP ${result.availableClean} 条，分配/更新 ${result.assigned} 个。`
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
