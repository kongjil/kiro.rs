import { Button } from '@/components/ui/button'
import type { StatsView } from '@/types/api'

const OPTIONS: { value: StatsView; label: string; hint: string }[] = [
  { value: 'billed', label: '计费后', hint: '下游实际看到的用量（已应用 Key 计费策略）' },
  { value: 'raw', label: '原始', hint: '上游真实用量（未应用 Key 计费策略）' },
]

/** 统计口径切换：原始 / 计费后 */
export function StatsViewToggle({
  value,
  onChange,
}: {
  value: StatsView
  onChange: (value: StatsView) => void
}) {
  return (
    <div
      role="radiogroup"
      aria-label="统计口径"
      className="grid grid-cols-2 gap-1 rounded-md border border-border/60 p-0.5 sm:flex sm:items-center"
    >
      {OPTIONS.map((option) => (
        <Button
          key={option.value}
          role="radio"
          aria-checked={value === option.value}
          title={option.hint}
          size="sm"
          variant={value === option.value ? 'default' : 'ghost'}
          className="h-7 rounded-md px-3 text-xs"
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </Button>
      ))}
    </div>
  )
}
