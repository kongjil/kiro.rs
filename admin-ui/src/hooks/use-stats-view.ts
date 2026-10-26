import { useCallback, useState } from 'react'
import type { StatsView } from '@/types/api'

const STORAGE_KEY = 'statsView'

function readStoredView(): StatsView {
  if (typeof localStorage === 'undefined') return 'billed'
  return localStorage.getItem(STORAGE_KEY) === 'raw' ? 'raw' : 'billed'
}

/**
 * 统计口径（原始 / 计费后），仪表概览与请求日志共用，记在 localStorage 里。
 */
export function useStatsView(): [StatsView, (view: StatsView) => void] {
  const [view, setView] = useState<StatsView>(readStoredView)
  const update = useCallback((next: StatsView) => {
    setView(next)
    if (typeof localStorage !== 'undefined') localStorage.setItem(STORAGE_KEY, next)
  }, [])
  return [view, update]
}
