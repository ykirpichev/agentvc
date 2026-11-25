import { useEffect, useState } from 'react'
import { ApiError } from './client'
import { useLive } from './live'

export interface Resource<T> {
  data: T | undefined
  error: ApiError | undefined
  loading: boolean
}

/**
 * Fetch `load()` when `key` changes and again whenever the space changes.
 * Keeps showing the previous data while a refetch is in flight, so live
 * updates never flash a spinner.
 */
export function useResource<T>(key: string | null, load: () => Promise<T>): Resource<T> {
  const { revision } = useLive()
  const [state, setState] = useState<{ key: string | null; data?: T; error?: ApiError }>({
    key: null,
  })

  useEffect(() => {
    if (key === null) return
    let cancelled = false
    load().then(
      (data) => !cancelled && setState({ key, data }),
      (e: unknown) => {
        if (cancelled) return
        const error = e instanceof ApiError ? e : new ApiError(0, 'client', String(e))
        setState((s) => ({ key, data: s.key === key ? s.data : undefined, error }))
      },
    )
    return () => {
      cancelled = true
    }
    // `load` is expected to be an inline closure over `key`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, revision])

  const fresh = state.key === key
  return {
    data: fresh ? state.data : undefined,
    error: fresh ? state.error : undefined,
    loading: key !== null && (!fresh || (state.data === undefined && state.error === undefined)),
  }
}
