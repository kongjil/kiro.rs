import { useEffect, useState } from 'react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  SettingGroup,
  SettingRow,
  useFieldSaver,
} from '@/components/console/setting-row'
import { useGlobalProxy, useSetGlobalProxy } from '@/hooks/use-credentials'
import { batchAddProxies, getProxyPool, setGlobalProxy } from '@/api/credentials'
import { maskProxyUrl, extractErrorMessage } from '@/lib/utils'
import { reportSaveError } from '@/components/settings/report-error'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

/**
 * 网络分区：全局出站代理。
 *
 * 代理地址填错会让**所有**上游请求立刻失败，所以要求显式「应用」按钮，
 * 并对 URL 做基本协议校验。
 */
const PROXY_SCHEMES = [
  'http://',
  'https://',
  'socks4://',
  'socks4a://',
  'socks5://',
  'socks5h://',
]

export function NetworkSection() {
  const { data, isLoading } = useGlobalProxy()
  const { mutate } = useSetGlobalProxy()
  const saver = useFieldSaver(mutate, reportSaveError)
  const isPending = saver.isSaving('proxy')
  const saved = saver.isSaved('proxy')
  const queryClient = useQueryClient()
  const { data: proxyPool } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
  })

  const currentUrl = data?.proxyUrl ?? null
  const currentUsername = data?.proxyUsername ?? null
  const currentPasswordSet = data?.proxyPasswordSet ?? false

  const [draftUrl, setDraftUrl] = useState('')
  const [draftUsername, setDraftUsername] = useState('')
  const [draftPassword, setDraftPassword] = useState('')
  const [batchText, setBatchText] = useState('')

  const batchMutation = useMutation({
    mutationFn: () => batchAddProxies({
      urls: batchText.split(/\r?\n/).map((line) => line.trim()).filter(Boolean),
    }),
    onSuccess: (result) => {
      setBatchText('')
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
      toast.success(`代理池导入完成：成功 ${result.added} 条，跳过 ${result.errors} 条`)
    },
    onError: (error) => toast.error(`代理池导入失败：${extractErrorMessage(error)}`),
  })

  const usePoolProxyMutation = useMutation({
    mutationFn: (url: string) => setGlobalProxy({ proxyUrl: url }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['global-proxy'] }),
    onError: (error) => toast.error(`设置全局代理失败：${extractErrorMessage(error)}`),
  })

  useEffect(() => {
    setDraftUrl(currentUrl ?? '')
    setDraftUsername(currentUsername ?? '')
    setDraftPassword('')
  }, [currentUrl, currentUsername, currentPasswordSet])

  const buildPayload = () => {
    const url = draftUrl.trim() || null
    if (!url) return null
    return {
      proxyUrl: url,
      proxyUsername: draftUsername.trim() || null,
      ...(draftPassword ? { proxyPassword: draftPassword } : {}),
    }
  }

  const apply = () => {
    const url = draftUrl.trim()
    if (!url) {
      toast.error('代理地址不能为空。要停用请点「清除」。')
      return
    }
    if (!PROXY_SCHEMES.some((s) => url.toLowerCase().startsWith(s))) {
      toast.error(`代理地址需以 ${PROXY_SCHEMES.join(' / ')} 开头`)
      return
    }
    const payload = buildPayload()
    if (!payload) return
    saver.save('proxy', payload)
  }

  const clear = () => {
    setDraftUrl('')
    setDraftUsername('')
    setDraftPassword('')
    saver.save('proxy', { proxyUrl: null })
  }

  const hint = currentUrl
    ? `当前生效：${maskProxyUrl(currentUrl)}${currentUsername ? ` (${currentUsername})` : ''}`
    : '未配置，直连上游。支持 HTTP / HTTPS / SOCKS，可填写认证凭据'

  return (
    <SettingGroup
      title="全局出站代理"
      description="所有上游请求默认走这个代理；未绑定专属代理的凭据都受它影响"
    >
      <SettingRow label="代理地址" hint={hint} pending={isPending} saved={saved}>
        <div className="flex flex-wrap items-center gap-1.5">
          <Input
            value={draftUrl}
            onChange={(e) => setDraftUrl(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') apply()
              if (e.key === 'Escape') {
                setDraftUrl(currentUrl ?? '')
                setDraftUsername(currentUsername ?? '')
                setDraftPassword('')
              }
            }}
            placeholder="socks5://host:1080"
            disabled={isLoading || isPending}
            spellCheck={false}
            autoComplete="off"
            className="console-num h-8 w-[min(20rem,60vw)] text-[12.5px]"
          />
          <Button
            size="sm"
            variant="outline"
            onClick={apply}
            disabled={isLoading || isPending || !draftUrl.trim()}
          >
            应用
          </Button>
          {currentUrl && (
            <Button
              size="sm"
              variant="ghost"
              onClick={clear}
              disabled={isPending}
              title="停用全局代理，恢复直连"
            >
              清除
            </Button>
          )}
        </div>
      </SettingRow>

      {(currentUrl || draftUrl.trim()) && (
        <>
          <SettingRow
            label="认证用户名"
            hint="选填，代理要求 Basic Auth 时填写"
            pending={isPending}
            saved={saved}
          >
            <Input
              value={draftUsername}
              onChange={(e) => setDraftUsername(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Escape') setDraftUsername(currentUsername ?? '')
              }}
              placeholder="留空则不认证"
              disabled={isLoading || isPending}
              spellCheck={false}
              autoComplete="off"
              className="console-num h-8 w-[min(16rem,50vw)] text-[12.5px]"
            />
          </SettingRow>
          <SettingRow
            label="认证密码"
            hint={currentPasswordSet ? '已配置密码；留空保存时保留现有密码' : '选填，与用户名配合使用'}
            pending={isPending}
            saved={saved}
          >
            <Input
              type="password"
              value={draftPassword}
              onChange={(e) => setDraftPassword(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Escape') setDraftPassword('')
              }}
              placeholder={currentPasswordSet ? '已配置，留空则保留' : '留空则不认证'}
              disabled={isLoading || isPending}
              spellCheck={false}
              autoComplete="new-password"
              className="console-num h-8 w-[min(16rem,50vw)] text-[12.5px]"
            />
            {currentPasswordSet && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  setDraftPassword('')
                  saver.save('proxy', {
                    proxyUrl: currentUrl,
                    proxyUsername: currentUsername,
                    proxyPassword: null,
                  })
                }}
                disabled={isLoading || isPending}
                title="清除已保存的代理密码"
              >
                清除密码
              </Button>
            )}
          </SettingRow>
        </>
      )}

      <SettingRow
        label="同步代理池"
        hint="与凭据管理中的代理池共用数据；每行一个完整 URL，可包含用户名和密码"
        pending={batchMutation.isPending || usePoolProxyMutation.isPending}
      >
        <div className="w-full space-y-2">
          <textarea
            value={batchText}
            onChange={(event) => setBatchText(event.target.value)}
            placeholder={'socks5://用户名:密码@主机:端口\nsocks5://用户名:密码@主机:端口'}
            className="min-h-[76px] w-full rounded-md border border-input bg-background px-2 py-1.5 font-mono text-[12px] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/30"
            spellCheck={false}
          />
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              onClick={() => batchMutation.mutate()}
              disabled={batchMutation.isPending || !batchText.trim()}
            >
              批量导入到代理池
            </Button>
            <span className="text-xs text-muted-foreground">
              已有 {proxyPool?.total ?? 0} 条
            </span>
          </div>
          {proxyPool && proxyPool.proxies.length > 0 && (
            <div className="max-h-36 space-y-1 overflow-y-auto rounded-md border p-1.5">
              {proxyPool.proxies.filter((proxy) => proxy.enabled).map((proxy) => (
                <div key={proxy.id} className="flex items-center justify-between gap-2 px-1 text-xs">
                  <span className="min-w-0 truncate font-mono" title={proxy.url}>{maskProxyUrl(proxy.url)}</span>
                  <Button
                    size="sm"
                    variant={proxy.url === currentUrl ? 'secondary' : 'ghost'}
                    className="h-7 shrink-0 px-2 text-xs"
                    onClick={() => usePoolProxyMutation.mutate(proxy.url)}
                    disabled={usePoolProxyMutation.isPending || proxy.url === currentUrl}
                  >
                    {proxy.url === currentUrl ? '当前全局' : '设为全局'}
                  </Button>
                </div>
              ))}
            </div>
          )}
        </div>
      </SettingRow>
    </SettingGroup>
  )
}
