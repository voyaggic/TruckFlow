import { useCallback, useRef, useState } from "react";

/**
 * Banner state hook — success messages auto-dismiss after `successMs`,
 * errors auto-dismiss after `errorMs`.  Returns `[msg, set, err, setErr]`.
 *
 * Usage:
 *   const [ok, setOk, error, setError] = useBanner();
 *   <div className="success-banner">{ok}</div>
 *   {error && <div className="error-banner">{error}</div>}
 */
export function useBanner(successMs = 3500, errorMs = 6000) {
  const [ok, setOk] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const okTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const errTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const showOk = useCallback(
    (msg: string | null) => {
      if (okTimer.current) clearTimeout(okTimer.current);
      setOk(msg);
      if (msg) {
        okTimer.current = setTimeout(() => setOk(null), successMs);
      }
    },
    [successMs],
  );

  const showErr = useCallback(
    (msg: string | null) => {
      if (errTimer.current) clearTimeout(errTimer.current);
      setError(msg);
      if (msg) {
        errTimer.current = setTimeout(() => setError(null), errorMs);
      }
    },
    [errorMs],
  );

  return [ok, showOk, error, showErr] as const;
}
