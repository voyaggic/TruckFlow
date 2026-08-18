import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type { SheetColumnEntry, SessionUser, SyncStatusView } from "../lib/types";

export default function SyncPanel({ user }: { user: SessionUser }) {
  const [status, setStatus] = useState<SyncStatusView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    api
      .syncStatus()
      .then(setStatus)
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const run = async (fn: () => Promise<unknown>, okMsg: string) => {
    setError(null);
    setNotice(null);
    setBusy(true);
    try {
      await fn();
      setNotice(okMsg);
      refresh();
      setTimeout(() => setNotice(null), 4000);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const totalPending = (status?.pg.tables ?? []).reduce((sum, t) => sum + t.pending, 0);

  return (
    <div>
      <h2 className="section-title">Sync & Integrations</h2>
      <p className="section-sub">
        One-way export to the central database and Google Sheets. Both are best-effort and independent — capture never
        waits on connectivity, one target failing never blocks the other, and a background loop retries automatically
        every 10 seconds so nothing waits for a manual "send".
      </p>

      {error && <div className="error-banner">{error}</div>}
      {notice && <div className="success-banner">{notice}</div>}

      <PostgresPanel status={status} totalPending={totalPending} busy={busy} actor={user} onRun={run} />
      <SheetsPanel status={status} busy={busy} actor={user} onRun={run} />
      {status?.sheets?.configured && <ColumnMappingPanel actor={user} onRun={run} />}
    </div>
  );
}

function AdapterError({ message }: { message: string | null | undefined }) {
  if (!message) return null;
  return (
    <p className="small" style={{ margin: "6px 0 0", color: "var(--danger, #d32f2f)" }}>
      {message}
    </p>
  );
}

// ---------------------------------------------------------------------------
// 6e — PostgreSQL sync health + configuration
// ---------------------------------------------------------------------------

function PostgresPanel({
  status,
  totalPending,
  busy,
  actor,
  onRun,
}: {
  status: SyncStatusView | null;
  totalPending: number;
  busy: boolean;
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
}) {
  const [connString, setConnString] = useState("");
  const [tripRetention, setTripRetention] = useState("");
  const pg = status?.pg;

  const saveTripRetention = () => {
    const v = tripRetention.trim();
    const days = v === "" ? null : Number(v);
    if (days !== null && (!Number.isFinite(days) || days < 1)) {
      window.alert("Enter a number of days (at least 1), or leave blank to keep entries forever.");
      return;
    }
    onRun(
      () => api.setTripRetention(actor.id, days),
      days
        ? `Daily entries older than ${days} day${days === 1 ? "" : "s"} will be deleted from local + PostgreSQL automatically.`
        : "Retention disabled — daily entries are kept forever.",
    );
  };

  const connect = () => {
    onRun(() => api.configurePostgres(actor.id, connString.trim()), "PostgreSQL connected — central database ready.");
  };

  const disconnect = () => {
    if (window.confirm("Disconnect PostgreSQL? Pending records stop syncing; local capture is unaffected.")) {
      onRun(() => api.disconnectPostgres(actor.id), "PostgreSQL disconnected.");
    }
  };

  return (
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="row between">
        <div className="section-title" style={{ fontSize: 15 }}>
          Central database (PostgreSQL)
        </div>
        <span className={`badge ${pg?.connected ? "active" : "disabled"}`}>
          {pg?.connected ? "Online" : pg?.configured ? "Offline" : "Not configured"}
        </span>
      </div>

      <div className="row" style={{ gap: 12 }}>
        <p className="muted small grow">
          Adapter <b>{pg?.adapter ?? "…"}</b>. Rows are pushed one-way to the central database and marked synced only
          on confirmed receipt — reconnect is safe, nothing is ever duplicated.
        </p>
        {pg?.configured && (
          <button className="ghost small" disabled={busy} onClick={() => onRun(() => api.syncNowPg(actor.id), "Postgres sync run complete.")}>
            Sync now
          </button>
        )}
      </div>

      {!pg?.configured ? (
        <div className="stack">
          <p className="muted small">
            Connect to a PostgreSQL server by pasting its connection string. The first connect <b>creates the database
            and its tables automatically</b>, so setup is paste-and-go. On this machine your local server accepts:
          </p>
          <div className="row">
            <div className="field grow">
              <label>Connection string</label>
              <input
                value={connString}
                onChange={(e) => setConnString(e.target.value)}
                placeholder="postgresql://postgres@127.0.0.1:5432/truckflow_central"
                spellCheck={false}
              />
            </div>
            <div className="field">
              <label>&nbsp;</label>
              <button className="primary" onClick={connect} disabled={busy || !connString.trim()}>
                {busy ? "Connecting…" : "Connect"}
              </button>
            </div>
          </div>
          <AdapterError message={pg?.last_error} />
        </div>
      ) : (
        <div className="stack">
          {status && (
            <table className="table">
              <thead>
                <tr>
                  <th>Table</th>
                  <th>Pending</th>
                  <th>Status</th>
                </tr>
              </thead>
              <tbody>
                {status.pg.tables.map((t) => (
                  <tr key={t.table}>
                    <td>{t.display}</td>
                    <td>{t.pending}</td>
                    <td>
                      <span className={`badge ${t.pending === 0 ? "active" : "pin"}`}>{t.pending === 0 ? "synced" : "pending"}</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}

          <p className="muted small">
            {totalPending === 0
              ? "All data synced."
              : `${totalPending} record${totalPending === 1 ? "" : "s"} waiting for connectivity.`}{" "}
            {status?.pg.last_synced_at ? `Last sync: ${new Date(status.pg.last_synced_at).toLocaleString()}` : "No sync yet."}
          </p>

          <div className="field" style={{ maxWidth: 260 }}>
            <label>Keep daily entries (days)</label>
            <div className="row" style={{ gap: 8 }}>
              <input
                type="number"
                min={1}
                value={tripRetention}
                onChange={(e) => setTripRetention(e.target.value)}
                placeholder="Blank = keep forever"
              />
              <button className="ghost small" disabled={busy} onClick={saveTripRetention}>
                Save
              </button>
            </div>
            <p className="muted small" style={{ marginTop: 6 }}>
              Daily trip entries older than this window are deleted in bulk from local + PostgreSQL (the reference
              registry is never touched). {pg?.trip_retention_days
                ? `Currently set to ${pg.trip_retention_days} day${pg.trip_retention_days === 1 ? "" : "s"}.`
                : "Not set — entries are kept forever."}
            </p>
          </div>

          <AdapterError message={pg?.last_error} />
          <div className="row">
            <button className="danger small" onClick={disconnect} disabled={busy}>
              Disconnect
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 6g — Sheet column mapping
// ---------------------------------------------------------------------------

function ColumnMappingPanel({
  actor,
  onRun,
}: {
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
}) {
  const [mapping, setMapping] = useState<SheetColumnEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [editHeader, setEditHeader] = useState<Record<string, string>>({});

  useEffect(() => {
    api.getSheetColumnMapping().then((m) => {
      setMapping(m);
      const h: Record<string, string> = {};
      m.forEach((e) => (h[e.field_key] = e.header));
      setEditHeader(h);
      setLoading(false);
    });
  }, []);

  const save = () => {
    const updated = mapping.map((e) => ({ ...e, header: editHeader[e.field_key] || e.header }));
    onRun(
      () => api.setSheetColumnMapping(actor.id, updated),
      "Column mapping saved — next sync will use the new layout.",
    );
    setMapping(updated);
  };

  const toggle = (key: string) => {
    setMapping((prev) => prev.map((e) => (e.field_key === key ? { ...e, enabled: !e.enabled } : e)));
  };

  const moveUp = (idx: number) => {
    if (idx <= 0) return;
    setMapping((prev) => {
      const arr = [...prev];
      [arr[idx - 1], arr[idx]] = [arr[idx], arr[idx - 1]];
      return arr;
    });
  };

  const moveDown = (idx: number) => {
    setMapping((prev) => {
      if (idx >= prev.length - 1) return prev;
      const arr = [...prev];
      [arr[idx], arr[idx + 1]] = [arr[idx + 1], arr[idx]];
      return arr;
    });
  };

  if (loading) return <p className="muted small">Loading column mapping…</p>;

  const enabledCount = mapping.filter((e) => e.enabled).length;

  return (
    <div className="card stack" style={{ marginTop: 8 }}>
      <div className="row between" style={{ alignItems: "center" }}>
        <div className="section-title" style={{ fontSize: 15 }}>
          Sheet columns
        </div>
        <span className="badge" style={{ fontSize: 12 }}>
          {enabledCount} of {mapping.length} columns enabled
        </span>
      </div>
      <p className="muted small">
        Choose which fields appear in the Google Sheet, edit their column headers, and reorder them.
        Disabled fields won't be exported. Changes take effect on the next sync.
      </p>
      <div className="stack" style={{ gap: 4 }}>
        {mapping.map((entry, idx) => (
          <div
            key={entry.field_key}
            className="row"
            style={{
              gap: 8,
              alignItems: "center",
              padding: "6px 8px",
              borderRadius: 6,
              background: entry.enabled ? "var(--card)" : "var(--card-muted)",
              border: "1px solid var(--border)",
              opacity: entry.enabled ? 1 : 0.6,
            }}
          >
            <button
              className="ghost small"
              style={{ padding: "2px 6px", fontSize: 11 }}
              onClick={() => moveUp(idx)}
              disabled={idx === 0}
              title="Move up"
            >
              ▲
            </button>
            <button
              className="ghost small"
              style={{ padding: "2px 6px", fontSize: 11 }}
              onClick={() => moveDown(idx)}
              disabled={idx === mapping.length - 1}
              title="Move down"
            >
              ▼
            </button>
            <input
              type="checkbox"
              checked={entry.enabled}
              onChange={() => toggle(entry.field_key)}
              style={{ cursor: "pointer" }}
            />
            <span style={{ fontSize: 12, minWidth: 100, color: "var(--text-muted)", fontFamily: "monospace" }}>
              {entry.field_key}
            </span>
            <input
              type="text"
              value={editHeader[entry.field_key] || ""}
              onChange={(e) => setEditHeader((h) => ({ ...h, [entry.field_key]: e.target.value }))}
              placeholder={entry.header}
              style={{ flex: 1, fontSize: 13, padding: "3px 8px" }}
            />
          </div>
        ))}
      </div>
      <div className="row" style={{ gap: 8 }}>
        <button className="primary small" onClick={save}>
          Save mapping
        </button>
        <button
          className="ghost small"
          onClick={() => {
            const h: Record<string, string> = {};
            mapping.forEach((e) => (h[e.field_key] = e.header));
            setEditHeader(h);
          }}
        >
          Reset edits
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 6f — Google Sheets integration + configuration
// ---------------------------------------------------------------------------

function SheetsPanel({
  status,
  busy,
  actor,
  onRun,
}: {
  status: SyncStatusView | null;
  busy: boolean;
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
}) {
  const sheets = status?.sheets;
  const [saJson, setSaJson] = useState("");
  const [sheetId, setSheetId] = useState("");
  const [sharedGroup, setSharedGroup] = useState("");
  const [frequency, setFrequency] = useState<string>("realtime");
  const [retention, setRetention] = useState<string>("");

  const saveRetention = () => {
    const v = retention.trim();
    const days = v === "" ? null : Number(v);
    if (days !== null && (!Number.isFinite(days) || days < 1)) {
      window.alert("Enter a number of days (at least 1), or leave blank to disable pruning.");
      return;
    }
    onRun(
      () => api.setSheetsRetention(actor.id, days),
      days ? `Sheet will keep ${days} day${days === 1 ? "" : "s"} of trips — older rows prune automatically.` : "Retention disabled — the sheet keeps everything until you clear it.",
    );
  };

  const connect = () => {
    onRun(
      () =>
        api.configureGoogleSheets(actor.id, saJson.trim(), sheetId.trim(), sharedGroup.trim() || null, frequency),
      "Google Sheets connected — logged trips will now export.",
    );
  };

  const changeFrequency = (f: string) => {
    setFrequency(f);
    if (sheets?.connected) {
      onRun(() => api.setGoogleSheetsFrequency(actor.id, f), "Sync frequency updated.");
    }
  };

  return (
    <div className="card stack">
      <div className="row between">
        <div className="section-title" style={{ fontSize: 15 }}>
          Google Sheets export
        </div>
        {sheets && (
          <span className={`badge ${sheets.connected ? "active" : "disabled"}`}>
            {sheets.connected ? "Connected" : sheets.status === "disconnected" ? "Disconnected" : sheets.status}
          </span>
        )}
      </div>

      {!sheets?.configured ? (
        <div className="stack">
          <p className="muted small">
            Logged trips are appended to a spreadsheet so anyone you share it with can view them without app access.
            This connects with a <b>Google service account</b> (no browser popup): create one in Google Cloud, enable
            the Sheets API, download the key JSON, paste it here, then share your spreadsheet with the service account's
            email. This panel is granted only with the <b>manage_integrations</b> permission.
          </p>
          <div className="field">
            <label>Service account key (JSON)</label>
            <textarea
              rows={5}
              value={saJson}
              onChange={(e) => setSaJson(e.target.value)}
              placeholder='{"type":"service_account","project_id":"…","client_email":"…","private_key":"-----BEGIN PRIVATE KEY-----…"}'
              spellCheck={false}
              style={{ fontFamily: "monospace", fontSize: 12 }}
            />
          </div>
          <div className="row">
            <div className="field grow">
              <label>Spreadsheet ID or URL</label>
              <input
                value={sheetId}
                onChange={(e) => setSheetId(e.target.value)}
                placeholder="1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms"
                spellCheck={false}
              />
            </div>
            <div className="field grow">
              <label>Shared Google Group (optional)</label>
              <input value={sharedGroup} onChange={(e) => setSharedGroup(e.target.value)} placeholder="ops@acme.com" />
            </div>
            <div className="field">
              <label>Sync frequency</label>
              <select value={frequency} onChange={(e) => changeFrequency(e.target.value)}>
                <option value="realtime">Real-time</option>
                <option value="every_15_min">Every 15 min</option>
              </select>
            </div>
          </div>
          <div className="row">
            <button className="primary" onClick={connect} disabled={busy || !saJson.trim() || !sheetId.trim()}>
              {busy ? "Connecting…" : "Connect Google Sheets"}
            </button>
          </div>
          <AdapterError message={sheets?.last_error} />
        </div>
      ) : (
        <div className="stack">
          <div className="row" style={{ gap: 12, flexWrap: "wrap" }}>
            <p className="muted small grow">
              Target sheet: <b>{sheets.target_sheet_id || "—"}</b>. Service account:{" "}
              <b>{sheets.service_account_email || "—"}</b>. Shared with: <b>{sheets.shared_group || "—"}</b>.
            </p>
            <button className="ghost small" disabled={busy} onClick={() => onRun(() => api.syncNowSheets(actor.id), "Sheets sync run complete.")}>
              Sync now
            </button>
            <button
              className="danger small"
              disabled={busy}
              onClick={() => {
                if (window.confirm("Disconnect Google Sheets? Export stops immediately; Postgres sync and local capture are unaffected.")) {
                  onRun(() => api.disconnectGoogleSheets(actor.id), "Google Sheets disconnected.");
                }
              }}
            >
              Disconnect
            </button>
          </div>

          <div className="field">
            <label>Sync frequency</label>
            <div className="seg">
              <button className={sheets.frequency === "realtime" ? "active" : ""} onClick={() => changeFrequency("realtime")}>
                Real-time
              </button>
              <button className={sheets.frequency === "every_15_min" ? "active" : ""} onClick={() => changeFrequency("every_15_min")}>
                Every 15 min
              </button>
            </div>
          </div>

          <div className="row" style={{ gap: 12, flexWrap: "wrap", alignItems: "flex-end" }}>
            <div className="field" style={{ maxWidth: 260 }}>
              <label>Keep trips in the sheet (days)</label>
              <div className="row" style={{ gap: 8 }}>
                <input
                  type="number"
                  min={1}
                  value={retention}
                  onChange={(e) => setRetention(e.target.value)}
                  placeholder="Blank = no pruning"
                />
                <button className="ghost small" disabled={busy} onClick={saveRetention}>
                  Save
                </button>
              </div>
            </div>
            <button
              className="danger small"
              disabled={busy}
              onClick={() => {
                if (window.confirm("Remove every exported trip from the sheet now? The header stays; only new trips will be appended. PostgreSQL and local data are not affected.")) {
                  onRun(() => api.clearExportedTrips(actor.id), "Sheet cleared — only new trips will append.");
                }
              }}
            >
              Clear exported trips
            </button>
          </div>

          <p className="muted small">
            {sheets.pending === 0
              ? "All logged trips exported."
              : `${sheets.pending} logged trip${sheets.pending === 1 ? "" : "s"} awaiting export.`}{" "}
            {sheets.last_synced_at ? `Last synced: ${new Date(sheets.last_synced_at).toLocaleString()}.` : "No export yet."}{" "}
            {sheets.retention_days
              ? `Older than ${sheets.retention_days} day${sheets.retention_days === 1 ? "" : "s"} is pruned automatically.`
              : "No retention set — the sheet keeps everything until you set days or clear it."}
          </p>
          <AdapterError message={sheets?.last_error} />
        </div>
      )}
    </div>
  );
}
