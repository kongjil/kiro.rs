import { useEffect, useState } from 'react'
import { toast } from 'sonner'
import { Plus, Save, Trash2 } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { SettingGroup } from '@/components/console/setting-row'
import { getBillingPrices, setBillingPrices } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

const PER_MILLION = 1_000_000

interface PriceRow {
  model: string
  input: string
  cacheCreation: string
  cacheRead: string
  output: string
}

const emptyRow = (): PriceRow => ({ model: '', input: '0', cacheCreation: '0', cacheRead: '0', output: '0' })

function toRate(text: string): number | null {
  const value = Number(text)
  return Number.isFinite(value) && value >= 0 ? value / PER_MILLION : null
}

export function BillingPricesSection() {
  const queryClient = useQueryClient()
  const { data, isLoading } = useQuery({
    queryKey: ['billing-prices'],
    queryFn: getBillingPrices,
  })
  const [rows, setRows] = useState<PriceRow[]>([])

  useEffect(() => {
    if (!data) return
    setRows(Object.entries(data.prices).map(([model, price]) => ({
      model,
      input: String(price.input * PER_MILLION),
      cacheCreation: String(price.cacheCreation * PER_MILLION),
      cacheRead: String(price.cacheRead * PER_MILLION),
      output: String(price.output * PER_MILLION),
    })))
  }, [data])

  const saveMutation = useMutation({
    mutationFn: (nextRows: PriceRow[]) => {
      const prices: Record<string, { input: number; cacheCreation: number; cacheRead: number; output: number }> = {}
      for (const row of nextRows) {
        const model = row.model.trim().toLowerCase()
        if (!model) throw new Error('请填写每条价格的模型名称；通配价格请用 *')
        if (model.length > 256 || /[\u0000-\u001f\u007f]/.test(model)) throw new Error(`模型名无效：${model}`)
        if (Object.prototype.hasOwnProperty.call(prices, model)) throw new Error(`模型名重复：${model}`)
        const input = toRate(row.input)
        const cacheCreation = toRate(row.cacheCreation)
        const cacheRead = toRate(row.cacheRead)
        const output = toRate(row.output)
        if ([input, cacheCreation, cacheRead, output].some((value) => value === null)) {
          throw new Error(`模型 ${model} 的价格必须是大于等于 0 的数字`)
        }
        prices[model] = { input: input!, cacheCreation: cacheCreation!, cacheRead: cacheRead!, output: output! }
      }
      return setBillingPrices(prices)
    },
    onSuccess: (result) => {
      queryClient.setQueryData(['billing-prices'], result)
      toast.success('模型价格表已保存并立即生效')
    },
    onError: (error) => toast.error(`保存价格表失败：${extractErrorMessage(error)}`),
  })

  const update = (index: number, key: keyof PriceRow, value: string) => {
    setRows((current) => current.map((row, i) => i === index ? { ...row, [key]: value } : row))
  }

  return (
    <SettingGroup
      title="模型 Token 价格"
      description="价格按每百万 Token 输入；服务端保存为每 Token 单价。匹配不到模型时使用 * 通配项；无匹配项时沿用上游 credits 比例。保存后即时生效，无需重启。"
    >
      <div className="overflow-x-auto">
      <div className="min-w-[760px] space-y-3">
        <div className="grid grid-cols-[minmax(8rem,1.4fr)_repeat(4,minmax(6rem,1fr))_2rem] gap-2 px-1 text-xs font-medium text-muted-foreground">
          <span>模型名</span><span>输入 / 1M</span><span>缓存创建 / 1M</span><span>缓存读取 / 1M</span><span>输出 / 1M</span><span />
        </div>
        {rows.map((row, index) => (
          <div key={`${row.model}-${index}`} className="grid grid-cols-[minmax(8rem,1.4fr)_repeat(4,minmax(6rem,1fr))_2rem] items-center gap-2">
            <Input value={row.model} onChange={(event) => update(index, 'model', event.target.value)} placeholder="claude-opus-5-5 或 *" disabled={isLoading || saveMutation.isPending} className="h-9 font-mono text-xs" />
            <Input type="number" min="0" step="any" value={row.input} onChange={(event) => update(index, 'input', event.target.value)} disabled={isLoading || saveMutation.isPending} className="h-9 text-right font-mono text-xs" />
            <Input type="number" min="0" step="any" value={row.cacheCreation} onChange={(event) => update(index, 'cacheCreation', event.target.value)} disabled={isLoading || saveMutation.isPending} className="h-9 text-right font-mono text-xs" />
            <Input type="number" min="0" step="any" value={row.cacheRead} onChange={(event) => update(index, 'cacheRead', event.target.value)} disabled={isLoading || saveMutation.isPending} className="h-9 text-right font-mono text-xs" />
            <Input type="number" min="0" step="any" value={row.output} onChange={(event) => update(index, 'output', event.target.value)} disabled={isLoading || saveMutation.isPending} className="h-9 text-right font-mono text-xs" />
            <Button type="button" variant="ghost" size="icon" className="h-8 w-8" onClick={() => setRows((current) => current.filter((_, i) => i !== index))} disabled={saveMutation.isPending} aria-label="删除模型价格">
              <Trash2 className="h-4 w-4" />
            </Button>
          </div>
        ))}
        <div className="flex flex-wrap gap-2">
          <Button type="button" size="sm" variant="outline" onClick={() => setRows((current) => [...current, emptyRow()])} disabled={saveMutation.isPending}>
            <Plus className="mr-1 h-3.5 w-3.5" />添加模型
          </Button>
          <Button type="button" size="sm" onClick={() => saveMutation.mutate(rows)} disabled={isLoading || saveMutation.isPending}>
            <Save className="mr-1 h-3.5 w-3.5" />{saveMutation.isPending ? '保存中…' : '保存并应用'}
          </Button>
          {rows.length === 0 && <span className="self-center text-xs text-muted-foreground">价格表为空；建议添加具体模型或 * 通配价格。</span>}
        </div>
        <p className="text-xs text-muted-foreground">例如输入价格 $4 / 1M 时填 4；若价格是 $4 / 1M，输出 $20 / 1M，则分别填 4 和 20。</p>
      </div>
      </div>
    </SettingGroup>
  )
}
