import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "../lib/api";
import type { PendingUpgradeInfo, SessionUser } from "../lib/types";

interface AppliedDiff {
  added: string[];
  removed: string[];
  by: string;
}

export default function PendingUpgradeBanner({
  user,
  onApplied,
}: {
  user: SessionUser;
  onApplied?: () => void;
}) {
  const [pending, setPending] = useState<PendingUpgradeInfo | null | undefined>(undefined);
  const [labels, setLabels] = useState<Record<string, string>>({});
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [applied, setApplied] = useState<AppliedDiff | null>(null);

  const refresh = useCallback(() => {
    api
      .getPendingUpgrade(user.id)
      .then(setPending)
      .catch(() => setPending(null));
  }, [user.id]);

  useEffect(() => {
    refresh();
    // Human-readable labels for the permission keys.
    api
      .listPermissions()
      .then((items) => {
        const m: Record<string, string> = {};
        for (const it of items) m[it.key] = it.description ?? it.key;
        setLabels(m);
      })
      .catch(() => undefined);
    // Poll so a change staged while the app is open still shows up.
    const t = setInterval(refresh, 45000);
    return () => clearInterval(t);
  }, [refresh]);

  const diff = useMemo(() => {
    if (!pending) return null;
    const oldSet = new Set(pending.previous_permission_keys);
    const newSet = new Set(pending.permission_keys);
    return {
      added: pending.permission_keys.filter((k) => !oldSet.has(k)),
      removed: pending.previous_permission_keys.filter((k) => !newSet.has(k)),
    };
  }, [pending]);

  if (pending === undefined) return null;
  if (pending === null && !applied) return null;

  const label = (k: string) => labels[k] ?? k;

  const submit = async () => {
    setError(null);
    setBusy(true);
    try {
      await api.completeAuthUpgrade(user.id, password);
      setApplied({
        added: diff?.added ?? [],
        removed: diff?.removed ?? [],
        by: pending?.requester_name || "an admin",
      });
      setPending(null);
      setPassword("");
      // Refresh the session user so tabs / sections update immediately.
      onApplied?.();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  if (applied) {
    return (
      <div className="section" style={{ paddingBottom: 0 }}>
        <div
          className="card"
          style={{ borderColor: "color-mix(in srgb, var(--success) 45%, transparent)" }}
        >
          <div className="section-title" style={{ fontSize: 15 }}>
            ✓ Your permissions were updated
          </div>
          <p className="muted small">
            Your role was changed by {applied.by}. The changes are already in effect — the tabs and
            sections you can see now reflect your new permissions.
          </p>
          {(applied.added.length > 0 || applied.removed.length > 0) && (
            <ul style={{ margin: "8px 0 0", paddingLeft: 20 }}>
              {applied.added.map((k) => (
                <li key={k} className="small" style={{ color: "var(--success)" }}>
                  + {label(k)}
                </li>
              ))}
              {applied.removed.map((k) => (
                <li key={k} className="small" style={{ color: "var(--danger)" }}>
                  − {label(k)}
                </li>
              ))}
            </ul>
          )}
          <div className="row" style={{ marginTop: 12 }}>
            <button className="primary" onClick={() => setApplied(null)}>
              Done
            </button>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="section" style={{ paddingBottom: 0 }}>
      <div
        className="card"
        style={{ borderColor: "color-mix(in srgb, var(--warning) 40%, transparent)" }}
      >
        <div className="section-title" style={{ fontSize: 15 }}>
          Action required: confirm your password
        </div>
        <p className="muted small">
          Your role was just changed by {pending?.requester_name || "an admin"}. Confirm your current
          password and the changes take effect immediately — nothing else is needed. Your password
          stays exactly as it is.
        </p>
        {diff && (diff.added.length > 0 || diff.removed.length > 0) && (
          <ul style={{ margin: "8px 0 0", paddingLeft: 20 }}>
            {diff.added.map((k) => (
              <li key={k} className="small" style={{ color: "var(--success)" }}>
                You'll gain: {label(k)}
              </li>
            ))}
            {diff.removed.map((k) => (
              <li key={k} className="small" style={{ color: "var(--danger)" }}>
                You'll lose: {label(k)}
              </li>
            ))}
          </ul>
        )}
        {error && <div className="error-banner">{error}</div>}
        <div className="row">
          <div className="field grow" style={{ maxWidth: 340 }}>
            <label>Your password</label>
            <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoFocus />
          </div>
        </div>
        <div className="row">
          <button className="primary" onClick={submit} disabled={busy || !password}>
            {busy ? "Confirming…" : "Confirm and apply"}
          </button>
        </div>
      </div>
    </div>
  );
}
