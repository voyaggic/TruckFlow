import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import { hasPerm } from "../components/Shell";
import type { ConfidenceTrendPoint, HealthDashboard, SessionUser } from "../lib/types";

/** Plain-English component labels — non-technical users must understand these. */
const COMPONENT_LABELS: Record<string, string> = {
  camera: "Gate Camera",
  anpr_service: "Plate Recognition",
  sync: "Data Sync",
  database: "Local Database",
};

/** Human-readable descriptions for each component (shown in the health cards). */
const COMPONENT_DESCRIPTIONS: Record<string, string> = {
  camera: "The camera feed watching the gate. If this goes down, manual entry still works.",
  anpr_service:
    "The system that reads plate numbers from the camera. If this stops, you can still log trips by typing the plate manually.",
  sync:
    "Sending trip records to the central server and spreadsheet. If this stops, trips are saved locally and will sync automatically when the connection returns.",
  database: "The local storage where all trip records are kept. This should always be working.",
};

const STATUS_LABELS: Record<string, string> = {
  ok: "Working",
  degraded: "Slowing down",
  offline: "Down",
  resolved: "Resolved",
};

type SortKey = "date-desc" | "date-asc" | "component" | "status";

const SORT_OPTIONS: { key: SortKey; label: string }[] = [
  { key: "date-desc", label: "Newest first" },
  { key: "date-asc", label: "Oldest first" },
  { key: "component", label: "By component" },
  { key: "status", label: "By status" },
];

export default function SystemMonitor({ user }: { user: SessionUser }) {
  const [dash, setDash] = useState<HealthDashboard | null>(null);
  const [trend, setTrend] = useState<ConfidenceTrendPoint[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [flash, setFlash] = useState<string | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [sortKey, setSortKey] = useState<SortKey>("date-desc");
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [deleting, setDeleting] = useState(false);
  const canAck = hasPerm(user, "acknowledge_health_alerts");
  const canDelete = canAck; // only System Monitor role can delete incidents

  const toggleSelect = (id: string) => {
    setSelected((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]));
  };

  const toggleExpand = (id: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id); else next.add(id);
      return next;
    });
  };

  const sorted = [...(dash?.recent_history ?? [])].sort((a, b) => {
    if (sortKey === "date-desc") return b.detected_at.localeCompare(a.detected_at);
    if (sortKey === "date-asc") return a.detected_at.localeCompare(b.detected_at);
    if (sortKey === "component") return a.component.localeCompare(b.component);
    if (sortKey === "status") return a.status.localeCompare(b.status);
    return 0;
  });

  const bulkDelete = async () => {
    if (!selected.length) return;
    const ok = window.confirm(
      `Delete ${selected.length} incident record${selected.length === 1 ? "" : "s"}? This removes them from the history permanently.`,
    );
    if (!ok) return;
    setDeleting(true);
    try {
      await api.deleteHealthEvents(user.id, selected);
      setSelected([]);
      setFlash(`${selected.length} record${selected.length === 1 ? "" : "s"} deleted.`);
      setTimeout(() => setFlash(null), 3000);
      refreshBg();
    } catch (e) {
      setError(String(e));
      setTimeout(() => setError(null), 6000);
    } finally {
      setDeleting(false);
    }
  };

  const refresh = useCallback(async () => {
    try {
      const [d, t] = await Promise.all([
        api.healthDashboard(user.id),
        api.anprConfidenceTrend(user.id, null, null),
      ]);
      setDash(d);
      setTrend(t);
      setError(null);
    } catch (e) {
      setError(String(e));
      setTimeout(() => setError(null), 6000);
    }
  }, [user.id]);

  const refreshBg = useCallback(() => { refresh().catch(() => {}); }, [refresh]);

  useEffect(() => {
    refreshBg();
    const t = setInterval(refreshBg, 15000);
    return () => clearInterval(t);
  }, [refreshBg]);

  const acknowledge = async (id: string) => {
    setError(null);
    try {
      await api.acknowledgeHealthEvent(user.id, id);
      setFlash("Alert acknowledged — it will not appear in the open alert list.");
      setTimeout(() => setFlash(null), 3000);
      refreshBg();
    } catch (e) {
      setError(String(e));
      setTimeout(() => setError(null), 6000);
    }
  };

  return (
    <div className="stack">
      <div className="row between">
        <div>
          <h2 className="section-title">System Monitor</h2>
          <p className="section-sub">Live component health, open incidents and recent history. Auto-refreshes every 15s.</p>
        </div>
        <button onClick={refresh}>Refresh</button>
      </div>

      {error && <div className="error-banner">{error}</div>}
      {flash && <div className="success-banner">{flash}</div>}

      {dash && (
        <>
          <div className="health-grid">
            {dash.components.map((c) => (
              <div key={c.component} className={`health-card ${c.status}`}>
                <div className="row between">
                  <div className="muted small">{COMPONENT_LABELS[c.component] ?? c.component}</div>
                  <span className={`badge ${c.status === "ok" ? "active" : c.status === "degraded" ? "pin" : "disabled"}`}>
                    {STATUS_LABELS[c.status] ?? c.status}
                  </span>
                </div>
                <div className="health-value">{c.open_events > 0 ? `${c.open_events} open` : "No issues"}</div>
                <div className="muted small" style={{ marginTop: 4 }}>
                  {COMPONENT_DESCRIPTIONS[c.component] ?? c.detail ?? ""}
                </div>
                {c.detail && c.component !== "anpr_service" && (
                  <div className="muted small" style={{ marginTop: 4, fontStyle: "italic" }}>{friendlyDetail(c.detail)}</div>
                )}
                {c.last_detected_at && (
                  <div className="muted small" style={{ marginTop: 2 }}>
                    Last issue: {fmtDateTime(c.last_detected_at)}
                  </div>
                )}
              </div>
            ))}
          </div>

          <div className="card">
            <div className="row between">
              <h3 style={{ margin: 0, fontSize: 15 }}>ANPR confidence trend</h3>
              {trend.length > 0 && (
                <span className="muted small">
                  {trend.reduce((n, p) => n + p.reads, 0)} reads · avg{" "}
                  {Math.round((trend.reduce((s, p) => s + (p.avg_confidence ?? 0), 0) / trend.length) * 100)}%
                </span>
              )}
            </div>
            {trend.length === 0 ? (
              <p className="muted small">
                No reads recorded yet. The trend builds as the pipeline processes captures — including simulator
                reads during development.
              </p>
            ) : (
              <TrendChart data={trend} />
            )}
          </div>

          <div className="card">
            <h3 style={{ margin: 0, fontSize: 15 }}>
              Open alerts <span className="badge">{dash.open_alerts.length}</span>
            </h3>
            {dash.open_alerts.length === 0 ? (
              <p className="muted small" style={{ margin: "10px 0 0" }}>
                All clear — no open issues.
              </p>
            ) : (
              <div className="stack" style={{ marginTop: 10 }}>
                {dash.open_alerts.map((e) => (
                  <div key={e.id} className="row between" style={{ padding: "8px 0", borderBottom: "1px solid var(--border)" }}>
                    <div className="grow">
                      <div className="row" style={{ gap: 8, alignItems: "center" }}>
                        <b>{COMPONENT_LABELS[e.component] ?? e.component}</b>
                        <span className={`badge ${e.status === "degraded" ? "pin" : "disabled"}`}>{STATUS_LABELS[e.status] ?? e.status}</span>
                      </div>
                      <div className="muted small" style={{ marginTop: 2 }}>
                        {friendlyDetail(e.detail ?? "No details available.")}
                      </div>
                      <div className="muted small">Detected: {fmtDateTime(e.detected_at)}</div>
                    </div>
                    {canAck && (
                      <button className="ghost small" onClick={() => acknowledge(e.id)}>
                        Mark as handled
                      </button>
                    )}
                  </div>
                ))}
              </div>
            )}
          </div>

          <div className="card">
            <div className="row between">
              <h3 style={{ margin: 0, fontSize: 15 }}>
                Recent activity <span className="badge">{dash.recent_history.length}</span>
              </h3>
              {dash.recent_history.length > 0 && (
                <div className="row" style={{ gap: 8 }}>
                  <select value={sortKey} onChange={(e) => setSortKey(e.target.value as SortKey)} style={{ fontSize: 12 }}>
                    {SORT_OPTIONS.map((o) => (
                      <option key={o.key} value={o.key}>{o.label}</option>
                    ))}
                  </select>
                  {canDelete && (
                    <button className="ghost small" onClick={() => {
                      const allIds = sorted.map((e) => e.id);
                      setSelected((prev) => prev.length === allIds.length ? [] : allIds);
                    }}>
                      {selected.length === sorted.length && sorted.length > 0 ? "Clear" : "Select all"}
                    </button>
                  )}
                  {canDelete && selected.length > 0 && (
                    <button className="danger small" disabled={deleting} onClick={bulkDelete}>
                      {deleting ? "Deleting…" : `Delete (${selected.length})`}
                    </button>
                  )}
                  {canDelete && dash.recent_history.length > 0 && (
                    <button className="danger small" disabled={deleting} onClick={() => {
                      const allIds = dash.recent_history.map((e) => e.id);
                      setSelected(allIds);
                      // Trigger delete immediately
                      setDeleting(true);
                      api.deleteHealthEvents(user.id, allIds).then(() => {
                        refresh();
                        setDeleting(false);
                        setSelected([]);
                      }).catch(() => setDeleting(false));
                    }}>
                      {deleting ? "Clearing…" : "Clear All"}
                    </button>
                  )}
                </div>
              )}
            </div>
            {dash.recent_history.length === 0 ? (
              <p className="muted small" style={{ margin: "10px 0 0" }}>
                No recorded issues — everything has been running smoothly.
              </p>
            ) : (
              <div className="stack" style={{ marginTop: 10 }}>
                {sorted.map((e) => {
                  const isExpanded = expanded.has(e.id);
                  return (
                    <div key={e.id} style={{ borderBottom: "1px solid var(--border)", paddingBottom: 6 }}>
                      <div className="row" style={{ gap: 8, padding: "6px 0", cursor: "pointer" }} onClick={() => toggleExpand(e.id)}>
                        {canDelete && (
                          <input
                            type="checkbox"
                            checked={selected.includes(e.id)}
                            onChange={(ev) => { ev.stopPropagation(); toggleSelect(e.id); }}
                            style={{ width: "auto", margin: 0 }}
                          />
                        )}
                        <span style={{ fontWeight: 500, minWidth: 120 }}>{COMPONENT_LABELS[e.component] ?? e.component}</span>
                        <span className={`badge ${e.status === "ok" ? "active" : e.status === "degraded" ? "pin" : "disabled"}`} style={{ minWidth: 80, textAlign: "center" }}>
                          {STATUS_LABELS[e.status] ?? e.status}
                        </span>
                        <span className="muted small grow">{friendlyDetail(e.detail ?? "Issue recorded.")}</span>
                        <span className="muted small" style={{ whiteSpace: "nowrap" }}>{fmtDateTime(e.detected_at)}</span>
                        {e.resolved_at && <span className="badge active" style={{ whiteSpace: "nowrap" }}>Fixed</span>}
                        <span className="muted small" style={{ fontSize: 10 }}>{isExpanded ? "▾" : "▸"}</span>
                      </div>
                      {isExpanded && (
                        <div className="stack" style={{ padding: "4px 0 4px 24px", fontSize: 12 }}>
                          <div className="muted">Component: {COMPONENT_LABELS[e.component] ?? e.component} ({e.component})</div>
                          <div className="muted">Status: {STATUS_LABELS[e.status] ?? e.status}</div>
                          <div className="muted">Detail: {e.detail ?? "No additional details."}</div>
                          <div className="muted">Detected: {fmtDateTime(e.detected_at)}</div>
                          {e.acknowledged_at && <div className="muted">Acknowledged: {fmtDateTime(e.acknowledged_at)} by {e.acknowledged_by ?? "—"}</div>}
                          {e.resolved_at && <div className="muted">Resolved: {fmtDateTime(e.resolved_at)}</div>}
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}

function TrendChart({ data }: { data: ConfidenceTrendPoint[] }) {
  const w = 640;
  const h = 150;
  const pad = 24;
  const max = Math.max(1, ...data.map((d) => d.avg_confidence ?? 0));
  const min = Math.min(...data.map((d) => d.avg_confidence ?? 0));
  const range = Math.max(0.1, max - min);
  const stepX = data.length > 1 ? (w - pad * 2) / (data.length - 1) : 0;
  const coords = data.map((d, i) => {
    const x = pad + (data.length > 1 ? i * stepX : w / 2);
    const y = h - pad - ((d.avg_confidence ?? 0) - min) / range * (h - pad * 2);
    return [x, y] as const;
  });
  const labelStep = Math.max(1, Math.ceil(data.length / 8));
  return (
    <svg viewBox={`0 0 ${w} ${h}`} className="chart-line" role="img" aria-label="ANPR confidence over time">
      <polyline points={coords.map(([x, y]) => `${x},${y}`).join(" ")} fill="none" stroke="var(--accent)" strokeWidth="2.5" />
      {coords.map(([x, y], i) => (
        <g key={i}>
          <circle cx={x} cy={y} r="3.5" fill="var(--accent)" />
          {i % labelStep === 0 && (
            <text x={x} y={h - 5} textAnchor="middle" className="chart-label">
              {data[i].date.slice(5)}
            </text>
          )}
        </g>
      ))}
    </svg>
  );
}

/**
 * Translate technical detail strings into plain English.
 * The backend stores raw error messages; this function wraps them in
 * user-friendly context so non-technical staff can understand what happened.
 */
function friendlyDetail(raw: string): string {
  const lower = raw.toLowerCase();
  if (lower.includes("anpr service unreachable") || lower.includes("anpr_service"))
    return "The plate recognition service is not responding. Manual entry is still available.";
  if (lower.includes("database ping") || lower.includes("database"))
    return "The local database had a read error. Trip logging may be temporarily affected.";
  if (lower.includes("sync") && lower.includes("fail"))
    return "Sync to the central server failed. Trip data is safe locally and will retry automatically.";
  if (lower.includes("camera") && (lower.includes("offline") || lower.includes("lost")))
    return "The gate camera feed has been lost. Check camera power and cables.";
  return raw; // fall back to raw if no pattern matches
}

function fmtDateTime(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, { day: "2-digit", month: "short" }) + " " + d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
