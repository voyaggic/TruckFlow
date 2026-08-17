import { useCallback, useEffect, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { api } from "../lib/api";
import { useReferenceFields } from "../lib/referenceFields";
import type {
  CompanyView,
  FrameEvidence,
  ReportDashboard,
  ReportFilters,
  SessionUser,
  SyncStatusView,
  TripView,
} from "../lib/types";

type PresetKey = "today" | "7d" | "30d" | "mtd" | "custom";

const PRESETS: { key: PresetKey; label: string }[] = [
  { key: "today", label: "Today" },
  { key: "7d", label: "Last 7 days" },
  { key: "30d", label: "Last 30 days" },
  { key: "mtd", label: "This month" },
  { key: "custom", label: "Custom…" },
];

function presetRange(p: Exclude<PresetKey, "custom">): { from: string; to: string } {
  const to = new Date();
  const from = new Date();
  if (p === "today") {
    from.setHours(0, 0, 0, 0);
  } else if (p === "7d") {
    from.setDate(from.getDate() - 6);
    from.setHours(0, 0, 0, 0);
  } else if (p === "30d") {
    from.setDate(from.getDate() - 29);
    from.setHours(0, 0, 0, 0);
  } else {
    from.setDate(1);
    from.setHours(0, 0, 0, 0);
  }
  return { from: toLocalIso(from), to: toEndOfDay(to) };
}

function toLocalIso(d: Date): string {
  const off = d.getTimezoneOffset();
  const shifted = new Date(d.getTime() - off * 60000);
  return shifted.toISOString().slice(0, 10);
}

/** Include end-of-day time so SQL string comparison covers the full day. */
function toEndOfDay(d: Date): string {
  const end = new Date(d);
  end.setHours(23, 59, 59, 999);
  return toLocalIso(end);
}

export default function Reporting({ user }: { user: SessionUser }) {
  const { label, entityLabel } = useReferenceFields();
  const [preset, setPreset] = useState<PresetKey>("7d");
  const [customFrom, setCustomFrom] = useState("");
  const [customTo, setCustomTo] = useState("");
  const [companies, setCompanies] = useState<CompanyView[]>([]);
  const [companyId, setCompanyId] = useState<string>("");
  const [dashboard, setDashboard] = useState<ReportDashboard | null>(null);
  const [sync, setSync] = useState<SyncStatusView | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [flash, setFlash] = useState<string | null>(null);
  const [drill, setDrill] = useState<{ rows: TripView[]; frames: Record<string, FrameEvidence[]> } | null>(null);
  const [drillOpenFor, setDrillOpenFor] = useState<string | null>(null);

  const filters = useCallback((): ReportFilters => {
    if (preset === "custom") {
      return { from: customFrom || null, to: customTo || null, company_id: companyId || null };
    }
    const range = presetRange(preset);
    return { from: range.from, to: range.to, company_id: companyId || null };
  }, [preset, customFrom, customTo, companyId]);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [dash, sy, cos] = await Promise.all([
        api.reportDashboard(user.id, filters()),
        api.syncStatus(),
        api.listCompanies(),
      ]);
      setDashboard(dash);
      setSync(sy);
      setCompanies(cos);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [user.id, filters]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const pickPreset = (p: PresetKey) => {
    setPreset(p);
    if (p !== "custom") {
      const r = presetRange(p);
      setCustomFrom(r.from);
      setCustomTo(r.to);
    }
  };

  const openDrill = async () => {
    setError(null);
    try {
      const rows = await api.reportTripsDrill(user.id, filters(), 200);
      setDrill({ rows, frames: {} });
    } catch (e) {
      setError(String(e));
    }
  };

  const loadFrames = async (trip: TripView) => {
    if (!drill) return;
    setDrillOpenFor(trip.id);
    try {
      const frames = await api.tripFrames(trip.id);
      setDrill({ ...drill, frames: { ...drill.frames, [trip.id]: frames } });
    } catch {
      setDrill({ ...drill, frames: { ...drill.frames, [trip.id]: [] } });
    } finally {
      setDrillOpenFor(null);
    }
  };

  const exportCsv = async () => {
    setError(null);
    setFlash(null);
    try {
      const filePath = await save({
        defaultPath: `truckflow-report-${new Date().toISOString().slice(0, 10)}.csv`,
        filters: [{ name: "CSV", extensions: ["csv"] }],
      });
      if (!filePath) return; // user cancelled
      const path = await api.reportExportCsv(user.id, filters(), filePath);
      setFlash(`Report exported to ${path}`);
      setTimeout(() => setFlash(null), 5000);
    } catch (e) {
      setError(String(e));
    }
  };

  const exportXlsx = async () => {
    setError(null);
    setFlash(null);
    try {
      const filePath = await save({
        defaultPath: `truckflow-report-${new Date().toISOString().slice(0, 10)}.xlsx`,
        filters: [{ name: "Excel", extensions: ["xlsx"] }],
      });
      if (!filePath) return; // user cancelled
      const path = await api.reportExportXlsx(user.id, filters(), filePath);
      setFlash(`Excel workbook exported to ${path}`);
      setTimeout(() => setFlash(null), 5000);
    } catch (e) {
      setError(String(e));
    }
  };

  const sheetsLink = sync?.sheets.connected ? sync.sheets.target_sheet_id ?? "connected" : null;

  return (
    <div className="stack">
      <div className="row between">
        <div>
          <h2 className="section-title">Reporting</h2>
          <p className="section-sub">Read-only analytics — no trip data can be edited from this screen.</p>
        </div>
        {sync && (
          <div className="row">
            {dashboard && (
              <span className={`badge ${dashboard.data_source === "postgres" ? "active" : "pin"}`}>
                {dashboard.data_source === "postgres" ? "Archive · PostgreSQL" : "Local data · central offline"}
              </span>
            )}
            <span className={`badge ${sheetsLink ? "active" : "disabled"}`}>
              Sheets {sheetsLink ? `synced · ${sheetsLink}` : "not connected"}
            </span>
          </div>
        )}
      </div>

      {error && <div className="error-banner">{error}</div>}
      {flash && <div className="success-banner">{flash}</div>}

      <div className="card">
        <div className="row between" style={{ flexWrap: "wrap", gap: 10 }}>
          <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
            {PRESETS.map((p) => (
              <button key={p.key} className={preset === p.key ? "primary" : ""} onClick={() => pickPreset(p.key)}>
                {p.label}
              </button>
            ))}
          </div>
          <div className="row" style={{ gap: 8 }}>
            <label className="muted small">
              From{" "}
              <input type="date" value={customFrom} onChange={(e) => setCustomFrom(e.target.value)} disabled={preset !== "custom"} />
            </label>
            <label className="muted small">
              To{" "}
              <input type="date" value={customTo} onChange={(e) => setCustomTo(e.target.value)} disabled={preset !== "custom"} />
            </label>
            <label className="muted small">
              {entityLabel("company")}{" "}
              <select value={companyId} onChange={(e) => setCompanyId(e.target.value)}>
                <option value="">All {entityLabel("company").toLowerCase()}</option>
                {companies.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
              </select>
            </label>
          </div>
        </div>
      </div>

      {loading && !dashboard ? (
        <div className="center-fill">
          <div className="spinner" />
        </div>
      ) : dashboard ? (
        <>
          <div className="stat-grid">
            <StatCard label="Total trips" value={dashboard.summary.total_trips} onClick={openDrill} />
            <StatCard label={`Active ${entityLabel("company").toLowerCase()}`} value={dashboard.summary.active_companies} />
            <StatCard label="Avg trips / day" value={dashboard.summary.avg_trips_per_day.toFixed(1)} />
            <div className="stat-card">
              <div className="muted small">vs prior period</div>
              <div className="stat-value">
                {dashboard.summary.prior_period.delta_trips > 0 ? "+" : ""}
                {dashboard.summary.prior_period.delta_trips}
              </div>
              <div className="muted small">
                {dashboard.summary.prior_period.delta_percent == null
                  ? "no prior data"
                  : `${dashboard.summary.prior_period.delta_percent > 0 ? "+" : ""}${dashboard.summary.prior_period.delta_percent.toFixed(1)}% vs ${dashboard.summary.prior_period.prior_trips} prior`}
              </div>
            </div>
          </div>

          <div className="chart-grid">
            <div className="card">
              <h3 style={{ margin: "0 0 8px", fontSize: 15 }}>Trips over time</h3>
              {dashboard.trips_over_time.length === 0 ? (
                <p className="muted small">No trips in the selected range.</p>
              ) : (
                <LineChart data={dashboard.trips_over_time.map((d) => ({ label: d.date, value: d.count }))} />
              )}
            </div>
            <div className="card">
              <h3 style={{ margin: "0 0 8px", fontSize: 15 }}>Top {entityLabel("company").toLowerCase()}</h3>
              {dashboard.top_companies.length === 0 ? (
                <p className="muted small">
                  No {entityLabel("company").toLowerCase()} activity in the selected range.
                </p>
              ) : (
                <BarChart data={dashboard.top_companies.map((c) => ({ label: c.company_name, value: c.count }))} />
              )}
            </div>
          </div>

          <div className="card">
            <div className="row between">
              <h3 style={{ margin: 0, fontSize: 15 }}>Trips by vehicle</h3>
              <div className="row" style={{ gap: 8 }}>
                <button onClick={openDrill}>View records</button>
                <button onClick={exportCsv}>Export CSV</button>
                <button onClick={exportXlsx}>Export Excel (.xlsx)</button>
              </div>
            </div>
            {dashboard.trips_by_vehicle.length === 0 ? (
              <p className="muted small" style={{ marginTop: 10 }}>
                No vehicle activity in the selected range.
              </p>
            ) : (
              <table className="table" style={{ marginTop: 10 }}>
                <thead>
                  <tr>
                    <th>{label("vehicle", "plate_number")}</th>
                    <th>{label("vehicle", "company")}</th>
                    <th>Trips</th>
                    <th>Total capacity</th>
                  </tr>
                </thead>
                <tbody>
                  {dashboard.trips_by_vehicle.map((v) => (
                    <tr key={v.plate_number}>
                      <td className="plate-font">{v.plate_number}</td>
                      <td>{v.company_name ?? "—"}</td>
                      <td>{v.trip_count}</td>
                      <td>{v.total_capacity != null ? `${v.total_capacity}` : "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </>
      ) : null}

      {drill && (
        <div className="overlay" onClick={() => setDrill(null)}>
          <div className="modal modal-wide" onClick={(e) => e.stopPropagation()}>
            <div className="row between">
              <h3 style={{ margin: 0 }}>
                Trip records <span className="badge">{drill.rows.length}</span>
              </h3>
              <button className="ghost" onClick={() => setDrill(null)} aria-label="Close">
                ✕
              </button>
            </div>
            <p className="muted small">
              Underlying records for the current filter (limited to 200). Select a trip to view captured photo evidence.
            </p>
            {drill.rows.length === 0 ? (
              <p className="muted small">No trips match the current filter.</p>
            ) : (
              <table className="table" style={{ marginTop: 10 }}>
                <thead>
                  <tr>
                    <th>{label("vehicle", "plate_number")}</th>
                    <th>{label("vehicle", "company")}</th>
                    <th>{label("vehicle", "driver")}</th>
                    <th>Time in</th>
                    <th>Source</th>
                    <th>Confidence</th>
                    <th>Evidence</th>
                  </tr>
                </thead>
                <tbody>
                  {drill.rows.map((t) => (
                    <tr key={t.id}>
                      <td className="plate-font">{t.plate_number}</td>
                      <td>{t.company_name ?? "—"}</td>
                      <td>{t.driver_name ?? "—"}</td>
                      <td>{fmtDateTime(t.time_in)}</td>
                      <td>{t.capture_method === "auto" ? "Auto" : "Manual"}</td>
                      <td>{t.confidence_score != null ? `${Math.round(t.confidence_score * 100)}%` : "—"}</td>
                      <td>
                        <button disabled={drillOpenFor === t.id} onClick={() => loadFrames(t)}>
                          {drillOpenFor === t.id ? "Loading…" : drill.frames[t.id] ? `${drill.frames[t.id].length} photo(s)` : "Photos"}
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
            {drill.rows.some((t) => drill.frames[t.id]?.length) && (
              <div className="frame-strip" style={{ marginTop: 12 }}>
                {drill.rows.flatMap((t) =>
                  (drill.frames[t.id] ?? []).map((f) => (
                    <div key={`${t.id}-${f.index}`} className="frame-card">
                      {f.data_base64 ? (
                        <img src={`data:image/png;base64,${f.data_base64}`} alt={`${t.plate_number} frame ${f.index}`} />
                      ) : (
                        <div className="frame-missing">frame {f.index}</div>
                      )}
                      <div className="muted small">{t.plate_number}</div>
                    </div>
                  )),
                )}
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

function StatCard({ label, value, onClick }: { label: string; value: number | string; onClick?: () => void }) {
  return (
    <div className={`stat-card ${onClick ? "clickable" : ""}`} onClick={onClick} title={onClick ? "Click to view underlying records" : undefined}>
      <div className="muted small">{label}</div>
      <div className="stat-value">{value}</div>
    </div>
  );
}

function LineChart({ data }: { data: { label: string; value: number }[] }) {
  const w = 640;
  const h = 150;
  const pad = 24;
  const max = Math.max(1, ...data.map((d) => d.value));
  const stepX = data.length > 1 ? (w - pad * 2) / (data.length - 1) : 0;
  const coords = data.map((d, i) => {
    const x = pad + (data.length > 1 ? i * stepX : w / 2);
    const y = h - pad - (d.value / max) * (h - pad * 2);
    return [x, y] as const;
  });
  const labelStep = Math.max(1, Math.ceil(data.length / 8));
  return (
    <svg viewBox={`0 0 ${w} ${h}`} className="chart-line" role="img" aria-label="Trips over time">
      <polyline points={coords.map(([x, y]) => `${x},${y}`).join(" ")} fill="none" stroke="var(--accent)" strokeWidth="2.5" />
      {coords.map(([x, y], i) => (
        <g key={i}>
          <circle cx={x} cy={y} r="3.5" fill="var(--accent)" />
          {i % labelStep === 0 && (
            <text x={x} y={h - 5} textAnchor="middle" className="chart-label">
              {data[i].label}
            </text>
          )}
        </g>
      ))}
    </svg>
  );
}

function BarChart({ data }: { data: { label: string; value: number }[] }) {
  const max = Math.max(1, ...data.map((d) => d.value));
  return (
    <div className="bar-chart">
      {data.map((d, i) => (
        <div key={i} className="bar-row">
          <div className="bar-label" title={d.label}>
            {d.label}
          </div>
          <div className="bar-track">
            <div className="bar-fill" style={{ width: `${(d.value / max) * 100}%` }} />
          </div>
          <div className="bar-value">{d.value}</div>
        </div>
      ))}
    </div>
  );
}

function fmtDateTime(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, { day: "2-digit", month: "short" }) + " " + d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
