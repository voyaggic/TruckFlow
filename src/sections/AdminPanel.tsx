import { useCallback, useEffect, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { api } from "../lib/api";
import type {
  AuditEntry,
  AuditFilters,
  ColumnInfo,
  CombinedImportSummary,
  CompanyView,
  ConfirmedColumn,
  ConfirmedSheet,
  DriverView,
  FieldDefinition,
  ListPermissionItem,
  OfficerActivityView,
  PasswordResetRequestView,
  ReferenceEntityType,
  ReferenceImportPreview,
  ReferenceImportRequest,
  RolePresetView,
  SessionUser,
  SheetPreview,
  TripView,
  UserView,
  VehicleView,
} from "../lib/types";
import PasswordChecklist from "../components/PasswordChecklist";
import SyncPanel from "./SyncPanel";

type AdminTabId = "users" | "reference" | "trips" | "sync" | "oversight";

interface AdminTab {
  id: AdminTabId;
  label: string;
}

export default function AdminPanel({ user }: { user: SessionUser }) {
  const [users, setUsers] = useState<UserView[] | null>(null);
  const [perms, setPerms] = useState<ListPermissionItem[]>([]);
  const [presets, setPresets] = useState<RolePresetView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<AdminTabId>(() => {
    if (user.permissions.some((p) => p.key === "manage_users")) return "users";
    if (user.permissions.some((p) => p.key === "manage_reference_database")) return "reference";
    if (user.permissions.some((p) => p.key === "manage_integrations")) return "sync";
    if (user.permissions.some((p) => p.key === "view_audit_log")) return "oversight";
    return "trips";
  });

  const refresh = useCallback(() => {
    api.listUsers().then(setUsers).catch((e) => setError(String(e)));
    api.listPermissions().then(setPerms).catch((e) => setError(String(e)));
    api.listRolePresets().then(setPresets).catch(() => undefined);
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const canManageUsers = user.permissions.some((p) => p.key === "manage_users");
  const canManageReference = user.permissions.some((p) => p.key === "manage_reference_database");
  const canEditVehicles = user.permissions.some((p) => p.key === "edit_existing_vehicles");
  const canAccessReference = canManageReference || canEditVehicles;
  const canManageIntegrations = user.permissions.some((p) => p.key === "manage_integrations");
  const canViewAudit = user.permissions.some((p) => p.key === "view_audit_log");

  const tabs: AdminTab[] = [];
  if (canManageUsers) tabs.push({ id: "users", label: "Users" });
  if (canAccessReference) tabs.push({ id: "reference", label: "Reference Database" });
  if (canManageUsers) tabs.push({ id: "trips", label: "Trip Archive" });
  if (canManageIntegrations) tabs.push({ id: "sync", label: "Sync & Integrations" });
  if (canViewAudit) tabs.push({ id: "oversight", label: "Oversight & Audit" });

  return (
    <div>
      <h2 className="section-title">Admin</h2>
      <p className="section-sub">Select a section below — only sections matching your permissions are shown.</p>

      {error && <div className="error-banner">{error}</div>}
      {notice && <div className="success-banner">{notice}</div>}

      {tabs.length > 1 && (
        <div className="tabbar" style={{ marginBottom: 16 }}>
          {tabs.map((t) => (
            <button key={t.id} className={activeTab === t.id ? "active" : ""} onClick={() => setActiveTab(t.id)}>
              {t.label}
            </button>
          ))}
        </div>
      )}

      {activeTab === "users" && canManageUsers && (
        <UserManagement
          users={users}
          perms={perms}
          presets={presets}
          actor={user}
          onChanged={() => {
            refresh();
            setNotice("User updated.");
            setTimeout(() => setNotice(null), 4000);
          }}
        />
      )}

      {activeTab === "trips" && canManageUsers && <TripArchive actor={user} />}

      {activeTab === "reference" && canAccessReference && <ReferenceDatabase actor={user} onNotice={setNotice} canRegister={canManageReference} />}

      {activeTab === "sync" && canManageIntegrations && <SyncPanel user={user} />}

      {activeTab === "oversight" && canViewAudit && <OversightSection actor={user} />}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Trip archive — soft/hard delete, restore, local purge (admin, password-gated)
// ---------------------------------------------------------------------------

function TripArchive({ actor }: { actor: SessionUser }) {
  const [view, setView] = useState<"active" | "archived">("active");
  const [trips, setTrips] = useState<TripView[] | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    const fn =
      view === "active"
        ? api.listRecentTrips(actor.id, 200)
        : api.listArchivedTrips(actor.id, { from: null, to: null, company_id: null });
    fn.then(setTrips).catch((e) => setError(String(e)));
    setSelected([]);
  }, [view, actor.id]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const askPassword = (purpose: string): string | null => {
    const v = window.prompt(`Enter your password to ${purpose}:`)?.trim();
    return v ? v : null;
  };

  const run = async (fn: () => Promise<unknown>, okMsg: string) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await fn();
      setNotice(okMsg);
      refresh();
      setTimeout(() => setNotice(null), 6000);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const toggle = (id: string) =>
    setSelected((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]));
  const toggleAll = () =>
    setSelected((prev) => (trips && prev.length === trips.length ? [] : (trips ?? []).map((t) => t.id)));

  const softDelete = () => {
    if (!selected.length) return window.alert("Select at least one trip.");
    const pw = askPassword("soft-delete the selected trips");
    if (!pw) return;
    run(() => api.softDeleteTrips(actor.id, selected, pw), `${selected.length} trip(s) hidden from the app and sheet — still kept in PostgreSQL.`);
  };

  const hardDelete = () => {
    if (!selected.length) return window.alert("Select at least one trip.");
    if (
      !window.confirm(
        "PERMANENTLY delete the selected trips? They will be removed from local data, PostgreSQL, and the sheet. This cannot be undone.",
      )
    )
      return;
    const pw = askPassword("permanently delete the selected trips");
    if (!pw) return;
    run(() => api.hardDeleteTrips(actor.id, selected, pw), `${selected.length} trip(s) permanently deleted.`);
  };

  const restore = () => {
    if (!selected.length) return window.alert("Select at least one trip.");
    run(() => api.restoreTrips(actor.id, selected), `${selected.length} trip(s) restored to normal views.`);
  };

  const purgeLocal = () => {
    if (
      !window.confirm(
        "Delete the local copies of all logged trips already confirmed in PostgreSQL?\n\nThe permanent archive stays in PostgreSQL. Note: reports currently read local data, so after this purge they will only show trips captured afterwards (pointing reporting at the central database is the next phase).",
      )
    )
      return;
    const pw = askPassword("purge local trip data");
    if (!pw) return;
    run(() => api.purgeLocalTrips(actor.id, pw), "Local copies removed — PostgreSQL keeps the full archive.");
  };

  return (
    <div className="card stack" style={{ marginTop: 16 }}>
      <div className="row between">
        <div className="section-title" style={{ fontSize: 15 }}>
          Trip archive
        </div>
        <div className="seg">
          <button className={view === "active" ? "active" : ""} onClick={() => setView("active")}>
            Recent trips
          </button>
          <button className={view === "archived" ? "active" : ""} onClick={() => setView("archived")}>
            Archived
          </button>
        </div>
      </div>

      <p className="muted small">
        {view === "active"
          ? "Select trips to hide (soft delete — hidden from the app and sheet, kept in PostgreSQL) or to delete permanently (removed everywhere). Both need your password."
          : "Soft-deleted trips still live in PostgreSQL. Restore to bring them back, or delete permanently."}
      </p>

      {error && <div className="error-banner">{error}</div>}
      {notice && <div className="success-banner">{notice}</div>}

      <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
        <button className="ghost small" disabled={busy} onClick={toggleAll}>
          {trips && selected.length === trips.length && trips.length > 0 ? "Clear selection" : "Select all"}
        </button>
        {view === "active" ? (
          <>
            <button className="ghost small" disabled={busy || !selected.length} onClick={softDelete}>
              Soft delete
            </button>
            <button className="danger small" disabled={busy || !selected.length} onClick={hardDelete}>
              Delete permanently
            </button>
          </>
        ) : (
          <>
            <button className="ghost small" disabled={busy || !selected.length} onClick={restore}>
              Restore
            </button>
            <button className="danger small" disabled={busy || !selected.length} onClick={hardDelete}>
              Delete permanently
            </button>
          </>
        )}
        <span style={{ flex: 1 }} />
        <button className="danger small" disabled={busy} onClick={purgeLocal}>
          Free up local space…
        </button>
      </div>

      {trips === null ? (
        <p className="muted small">Loading…</p>
      ) : trips.length === 0 ? (
        <p className="muted small">{view === "active" ? "No recent trips to manage." : "Nothing is archived right now."}</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th></th>
              <th>Plate</th>
              <th>Time in</th>
              <th>Company</th>
              <th>Receipt</th>
              <th>Status</th>
            </tr>
          </thead>
          <tbody>
            {trips.map((t) => (
              <tr key={t.id}>
                <td>
                  <input type="checkbox" checked={selected.includes(t.id)} onChange={() => toggle(t.id)} />
                </td>
                <td>{t.plate_number}</td>
                <td>{fmtDate(t.time_in)}</td>
                <td>{t.company_name ?? ""}</td>
                <td>{t.receipt_no ?? ""}</td>
                <td>
                  <span className={`badge ${view === "archived" ? "pin" : "active"}`}>
                    {view === "archived" ? "archived" : t.status}
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 6c / 6g. Oversight — officer activity aggregate + audit log trail
// ---------------------------------------------------------------------------

function OversightSection({ actor }: { actor: SessionUser }) {
  const [activity, setActivity] = useState<OfficerActivityView[]>([]);
  const [audit, setAudit] = useState<AuditEntry[]>([]);
  const [actions, setActions] = useState<string[]>([]);
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [action, setAction] = useState<string>("");
  const [selected, setSelected] = useState<string[]>([]);
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canDelete = actor.permissions.some((p) => p.key === "manage_users");

  const filters = (): AuditFilters => ({ from: from || null, to: to || null, actor_id: null, action: action || null });

  const refresh = useCallback(async () => {
    try {
      const [act, log, acts] = await Promise.all([
        api.officerActivity(actor.id, from || null, to || null),
        api.listAuditLog(actor.id, filters()),
        api.listAuditActions(actor.id),
      ]);
      setActivity(act);
      setAudit(log);
      setActions(acts);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [actor.id, from, to, action]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const toggleSelect = (id: string) => {
    setSelected((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]));
  };

  const toggleSelectAll = () => {
    setSelected((prev) => (prev.length === audit.length ? [] : audit.map((e) => e.id)));
  };

  const removeEntries = async (ids: string[]) => {
    if (!ids.length) return;
    const ok = window.confirm(
      `Delete ${ids.length} audit entr${ids.length === 1 ? "y" : "ies"}? This cannot be undone.`,
    );
    if (!ok) return;
    setDeleting(true);
    try {
      await api.deleteAuditEntries(actor.id, ids);
      setSelected([]);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setDeleting(false);
    }
  };

  return (
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="section-title" style={{ fontSize: 15 }}>
        Oversight
      </div>
      <p className="muted small">Officer activity and the full audit trail. Read-only — nothing here can be modified.</p>

      {error && <div className="error-banner">{error}</div>}

      <h4 style={{ margin: "6px 0 0", fontSize: 14 }}>Officer activity</h4>
      {activity.length === 0 ? (
        <p className="muted small">No officer activity in the selected range.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Officer</th>
              <th>Trips logged</th>
              <th>Queue resolved</th>
              <th>Last active</th>
            </tr>
          </thead>
          <tbody>
            {activity.map((o) => (
              <tr key={o.officer_id}>
                <td>{o.officer_name}</td>
                <td>{o.trips_logged}</td>
                <td>{o.queue_resolved}</td>
                <td>{o.last_active_at ? fmtDate(o.last_active_at) : "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h4 style={{ margin: "16px 0 0", fontSize: 14 }}>Audit log</h4>
      <div className="row" style={{ gap: 8, flexWrap: "wrap", marginTop: 6 }}>
        <label className="muted small">
          From{" "}
          <input type="date" value={from} onChange={(e) => setFrom(e.target.value)} />
        </label>
        <label className="muted small">
          To{" "}
          <input type="date" value={to} onChange={(e) => setTo(e.target.value)} />
        </label>
        <label className="muted small">
          Action{" "}
          <select value={action} onChange={(e) => setAction(e.target.value)}>
            <option value="">All actions</option>
            {actions.map((a) => (
              <option key={a} value={a}>
                {a}
              </option>
            ))}
          </select>
        </label>
        {canDelete && (
          <div className="row" style={{ gap: 8, marginLeft: "auto" }}>
            <button className="ghost small" onClick={toggleSelectAll}>
              {selected.length === audit.length && audit.length > 0 ? "Clear selection" : "Select all visible"}
            </button>
            <button
              className="danger small"
              disabled={deleting || !selected.length}
              onClick={() => removeEntries(selected)}
            >
              {deleting ? "Deleting…" : `Delete selected (${selected.length})`}
            </button>
            <button
              className="danger small"
              disabled={deleting || !audit.length}
              onClick={() => removeEntries(audit.map((e) => e.id))}
            >
              Delete all matching filters
            </button>
          </div>
        )}
      </div>

      {audit.length === 0 ? (
        <p className="muted small">No audit entries match the current filter.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              {canDelete && <th />}
              <th>Time</th>
              <th>Officer</th>
              <th>Action</th>
              <th>Target</th>
              <th>Details</th>
            </tr>
          </thead>
          <tbody>
            {audit.map((e) => (
              <tr key={e.id}>
                {canDelete && (
                  <td>
                    <input
                      type="checkbox"
                      style={{ width: "auto" }}
                      checked={selected.includes(e.id)}
                      onChange={() => toggleSelect(e.id)}
                    />
                  </td>
                )}
                <td>{fmtDate(e.timestamp)}</td>
                <td>{e.actor_name ?? "—"}</td>
                <td>
                  <span className="badge">{e.action}</span>
                </td>
                <td className="small">{e.target_id ?? "—"}</td>
                <td className="small">{e.details ? JSON.stringify(e.details) : "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function fmtDate(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, { day: "2-digit", month: "short" }) + " " + d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

interface MgmtProps {
  users: UserView[] | null;
  perms: ListPermissionItem[];
  presets: RolePresetView[];
  actor: SessionUser;
  onChanged: () => void;
}

function UserManagement({ users, perms, presets, actor, onChanged }: MgmtProps) {
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [showDeleted, setShowDeleted] = useState(false);

  if (!users) {
    return (
      <div className="card">
        <div className="section-title" style={{ fontSize: 15 }}>
          Users
        </div>
        <div className="center-fill">
          <div className="spinner" />
        </div>
      </div>
    );
  }

  const editing = users.find((u) => u.id === editingId) ?? null;
  const visible = showDeleted ? users : users.filter((u) => u.status !== "deleted");

  return (
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="row between">
        <div className="section-title" style={{ fontSize: 15 }}>
          Users
        </div>
        <div className="row" style={{ gap: 8 }}>
          <button className="ghost small" onClick={() => setShowDeleted((v) => !v)} disabled={adding}>
            {showDeleted ? "Hide deleted" : "Show deleted"}
          </button>
          <button className="primary" onClick={() => setAdding((v) => !v)}>
            {adding ? "Cancel" : "+ Add User"}
          </button>
        </div>
      </div>

      {adding && (
        <AddUserForm
          perms={perms}
          presets={presets}
          actor={actor}
          onDone={() => {
            setAdding(false);
            onChanged();
          }}
        />
      )}

      <table className="table">
        <thead>
          <tr>
            <th>Name</th>
            <th>Credential</th>
            <th>Status</th>
            <th>Permissions</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {visible.map((u) => (
            <tr key={u.id}>
              <td>
                <b>{u.name}</b>
                {u.id === actor.id && <span className="muted small"> (you)</span>}
              </td>
              <td>
                <span className="badge password">{u.permissions.includes("manage_users") ? "Admin" : "User"}</span>
              </td>
              <td>
                <span className={`badge ${u.status}`}>{u.status}</span>
              </td>
              <td className="small">{u.permissions.length ? u.permissions.join(", ") : "—"}</td>
              <td>
                <button className="ghost small" onClick={() => setEditingId(editingId === u.id ? null : u.id)}>
                  Edit
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {editing && (
        <EditUserForm
          key={editing.id}
          target={editing}
          perms={perms}
          actor={actor}
          onDone={() => {
            setEditingId(null);
            onChanged();
          }}
        />
      )}

      <ResetRequests users={users} actor={actor} />
    </div>
  );
}

function ResetRequests({ users, actor }: { users: UserView[]; actor: SessionUser }) {
  const [requests, setRequests] = useState<PasswordResetRequestView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [resettingFor, setResettingFor] = useState<string | null>(null);
  const [tempPass, setTempPass] = useState("");
  const [adminPass, setAdminPass] = useState("");

  const refresh = useCallback(() => {
    api
      .listPasswordResetRequests(actor.id)
      .then(setRequests)
      .catch((e) => setError(String(e)));
  }, [actor.id]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const doReset = async (req: PasswordResetRequestView) => {
    setError(null);
    const user = users.find((u) => u.name === req.username);
    if (!user) {
      setError(`No active account named "${req.username}" — use Ignore to clear the request.`);
      return;
    }
    setBusy(true);
    try {
      await api.resetUserPassword(actor.id, user.id, tempPass, adminPass);
      setResettingFor(null);
      setTempPass("");
      setAdminPass("");
      refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doIgnore = async (req: PasswordResetRequestView) => {
    setError(null);
    try {
      await api.dismissPasswordResetRequest(actor.id, req.id);
      refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, marginTop: 16 }}>
      <div className="section-title" style={{ fontSize: 14 }}>
        Password reset requests
        {requests.length > 0 && <span className="badge" style={{ marginLeft: 8 }}>{requests.length}</span>}
      </div>
      <p className="muted small">
        Users who forgot their password. Reset it for them — they'll set a new one at their next sign-in.
      </p>
      {error && <div className="error-banner">{error}</div>}
      {requests.length === 0 ? (
        <p className="muted small" style={{ marginBottom: 0 }}>
          No pending requests.
        </p>
      ) : (
        requests.map((req) => {
          const user = users.find((u) => u.name === req.username);
          return (
            <div
              key={req.id}
              className="stack"
              style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 12, marginTop: 8 }}
            >
              <div className="row between">
                <div>
                  <b>{req.username}</b>
                  <div className="muted small">Requested {fmtDate(req.requested_at)}</div>
                  {!user && <div className="muted small">No active account with this name.</div>}
                </div>
                <div className="row" style={{ gap: 8 }}>
                  <button
                    className="primary small"
                    disabled={!user}
                    onClick={() => setResettingFor(resettingFor === req.id ? null : req.id)}
                  >
                    {resettingFor === req.id ? "Cancel" : "Reset password"}
                  </button>
                  <button className="ghost small" onClick={() => doIgnore(req)} disabled={busy}>
                    Ignore
                  </button>
                </div>
              </div>
              {resettingFor === req.id && user && (
                <div className="stack" style={{ marginTop: 10, gap: 10 }}>
                  <div className="muted small">
                    Set a temporary password for {req.username}. They'll choose their own at next sign-in.
                  </div>
                  <div className="row">
                    <div className="field grow">
                      <label>Temporary password</label>
                      <input type="password" value={tempPass} onChange={(e) => setTempPass(e.target.value)} />
                    </div>
                    <div className="field grow">
                      <label>Your password (confirm)</label>
                      <input type="password" value={adminPass} onChange={(e) => setAdminPass(e.target.value)} />
                    </div>
                  </div>
                  <div className="row">
                    <button className="primary" onClick={() => doReset(req)} disabled={busy || !tempPass || !adminPass}>
                      {busy ? "Resetting…" : "Reset password"}
                    </button>
                  </div>
                </div>
              )}
            </div>
          );
        })
      )}
    </div>
  );
}

function AddUserForm({
  perms,
  presets,
  actor,
  onDone,
}: {
  perms: ListPermissionItem[];
  presets: RolePresetView[];
  actor: SessionUser;
  onDone: () => void;
}) {
  const [name, setName] = useState("");
  const [presetId, setPresetId] = useState<string>("preset-gate-officer");
  const [selected, setSelected] = useState<string[]>([]);
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const preset = presets.find((p) => p.id === presetId);
    setSelected(preset ? preset.permission_keys : []);
  }, [presetId, presets]);

  const toggleKey = (key: string) => {
    setPresetId("");
    setSelected((prev) => (prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key]));
  };

  const submit = async () => {
    setError(null);
    if (!name.trim()) {
      setError("Name is required.");
      return;
    }
    setBusy(true);
    try {
      await api.createUser(actor.id, name.trim(), selected, password);
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}>
      <div className="section-title" style={{ fontSize: 14 }}>
        New user
      </div>
      {error && <div className="error-banner">{error}</div>}

      <div className="row">
        <div className="field grow">
          <label>Full name</label>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. Peter Otieno" />
        </div>
        <div className="field">
          <label>Starting permission bundle</label>
          <select value={presetId} onChange={(e) => setPresetId(e.target.value)}>
            <option value="">Custom…</option>
            {presets.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </div>
      </div>

      <div className="field">
        <label>Permissions (composable — pick any combination)</label>
        <div className="row">
          {perms.map((p) => (
            <label key={p.key} className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
              <input
                type="checkbox"
                style={{ width: "auto" }}
                checked={selected.includes(p.key)}
                onChange={() => toggleKey(p.key)}
              />
              <span>{p.key}</span>
            </label>
          ))}
        </div>
      </div>

      <div className="field">
        <label>Initial password</label>
        <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} />
        <PasswordChecklist password={password} />
      </div>

      <div className="row">
        <button className="primary" onClick={submit} disabled={busy || !name.trim() || !selected.length || !password}>
          {busy ? "Creating…" : "Create user"}
        </button>
      </div>
    </div>
  );
}

function EditUserForm({
  target,
  perms,
  actor,
  onDone,
}: {
  target: UserView;
  perms: ListPermissionItem[];
  actor: SessionUser;
  onDone: () => void;
}) {
  const [selected, setSelected] = useState<string[]>(target.permissions);
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [resetOpen, setResetOpen] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [purgeOpen, setPurgeOpen] = useState(false);
  const [tempPass, setTempPass] = useState("");
  const [confirmPass, setConfirmPass] = useState("");

  const toggleKey = (key: string) => {
    setSelected((prev) => (prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key]));
  };

  const applyPermissions = async () => {
    setError(null);
    setBusy(true);
    try {
      await api.setUserPermissions(actor.id, target.id, selected, password);
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const toggleStatus = async () => {
    setError(null);
    const next = target.status === "active" ? "disabled" : "active";
    const confirmed = window.confirm(
      next === "disabled"
        ? `Disable "${target.name}"? They will be blocked at their next sign-in. Their history stays intact.`
        : `Re-enable "${target.name}"?`,
    );
    if (!confirmed) return;
    try {
      await api.setUserStatus(actor.id, target.id, next);
      onDone();
    } catch (e) {
      setError(String(e));
    }
  };

  const doReset = async () => {
    setError(null);
    setBusy(true);
    try {
      await api.resetUserPassword(actor.id, target.id, tempPass, confirmPass);
      setResetOpen(false);
      setTempPass("");
      setConfirmPass("");
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async () => {
    setError(null);
    const ok = window.confirm(
      `Delete "${target.name}"? They can never sign in again and disappear from the user list. Every trip and audit entry keeps their name.`,
    );
    if (!ok) return;
    setBusy(true);
    try {
      await api.deleteUser(actor.id, target.id, confirmPass);
      setDeleteOpen(false);
      setConfirmPass("");
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doRestore = async () => {
    setError(null);
    const ok = window.confirm(`Restore "${target.name}"? They will be able to sign in again.`);
    if (!ok) return;
    setBusy(true);
    try {
      await api.restoreUser(actor.id, target.id);
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doPurge = async () => {
    setError(null);
    const ok = window.confirm(
      `Permanently erase "${target.name}"? This deletes the account, its permissions and its entire audit trail forever. Trips they logged are kept but no longer attributed to them. This cannot be undone.`,
    );
    if (!ok) return;
    setBusy(true);
    try {
      await api.purgeUser(actor.id, target.id, confirmPass);
      setPurgeOpen(false);
      setConfirmPass("");
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const isDeleted = target.status === "deleted";

  return (
    <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}>
      <div className="row between">
        <div className="section-title" style={{ fontSize: 14 }}>
          Edit — {target.name}
        </div>
        {!isDeleted && (
          <button className={target.status === "active" ? "danger" : "primary"} onClick={toggleStatus} disabled={target.id === actor.id}>
            {target.status === "active" ? "Disable account" : "Re-enable account"}
          </button>
        )}
      </div>
      {error && <div className="error-banner">{error}</div>}

      {!isDeleted && (
        <>
          <div className="field">
            <label>Permissions</label>
            <div className="row">
              {perms.map((p) => (
                <label key={p.key} className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
                  <input
                    type="checkbox"
                    style={{ width: "auto" }}
                    checked={selected.includes(p.key)}
                    onChange={() => toggleKey(p.key)}
                  />
                  <span>{p.key}</span>
                </label>
              ))}
            </div>
          </div>

          <div className="field">
            <label>Your password (confirm to save changes)</label>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              style={{ maxWidth: 320 }}
              placeholder="Confirm your identity as admin"
            />
          </div>

          <div className="row">
            <button className="primary" onClick={applyPermissions} disabled={busy || !password}>
              {busy ? "Saving…" : "Save permissions"}
            </button>
          </div>
        </>
      )}

      {!isDeleted && target.id !== actor.id && (
        <>
          <div className="row" style={{ marginTop: 14, gap: 10 }}>
            <button className="ghost" onClick={() => { setResetOpen((v) => !v); setDeleteOpen(false); setPurgeOpen(false); }}>
              Reset password
            </button>
            <button className="danger" onClick={() => { setDeleteOpen((v) => !v); setResetOpen(false); setPurgeOpen(false); }}>
              Delete account
            </button>
          </div>
          {resetOpen && (
            <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 12, marginTop: 10 }}>
              <div className="muted small">
                Set a temporary password. {target.name} must choose their own password at their next sign-in.
              </div>
              <div className="field">
                <label>Temporary password</label>
                <input type="password" value={tempPass} onChange={(e) => setTempPass(e.target.value)} />
                <PasswordChecklist password={tempPass} />
              </div>
              <div className="field">
                <label>Your password (confirm)</label>
                <input type="password" value={confirmPass} onChange={(e) => setConfirmPass(e.target.value)} />
              </div>
              <div className="row">
                <button className="primary" onClick={doReset} disabled={busy || !tempPass || !confirmPass}>
                  {busy ? "Resetting…" : "Reset password"}
                </button>
              </div>
            </div>
          )}
          {deleteOpen && (
            <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 12, marginTop: 10 }}>
              <div className="muted small">
                <b>{target.name}</b> will be deleted: they can never sign in again and disappear from the user list.
                Their trips and audit history are kept. Type your password to confirm.
              </div>
              <div className="field">
                <label>Your password (confirm)</label>
                <input type="password" value={confirmPass} onChange={(e) => setConfirmPass(e.target.value)} />
              </div>
              <div className="row">
                <button className="danger" onClick={doDelete} disabled={busy || !confirmPass}>
                  {busy ? "Deleting…" : "Delete account"}
                </button>
              </div>
            </div>
          )}
        </>
      )}

      {isDeleted && (
        <>
          <div className="row" style={{ marginTop: 8, gap: 10 }}>
            <button className="primary" onClick={doRestore} disabled={busy}>
              Restore account
            </button>
            <button className="danger" onClick={() => setPurgeOpen((v) => !v)} disabled={busy}>
              Purge permanently
            </button>
          </div>
          {purgeOpen && (
            <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 12, marginTop: 10 }}>
              <div className="muted small">
                This <b>permanently erases</b> "{target.name}" — account, permissions and audit trail. Trips they
                logged are kept but no longer attributed to them. This cannot be undone.
              </div>
              <div className="field">
                <label>Your password (confirm)</label>
                <input type="password" value={confirmPass} onChange={(e) => setConfirmPass(e.target.value)} />
              </div>
              <div className="row">
                <button className="danger" onClick={doPurge} disabled={busy || !confirmPass}>
                  {busy ? "Purging…" : "Purge permanently"}
                </button>
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 6b. Reference Database Management — companies / drivers / vehicles
// ---------------------------------------------------------------------------

function ReferenceDatabase({ actor, onNotice, canRegister }: { actor: SessionUser; onNotice: (msg: string) => void; canRegister: boolean }) {
  const [companies, setCompanies] = useState<CompanyView[]>([]);
  const [drivers, setDrivers] = useState<DriverView[]>([]);
  const [vehicles, setVehicles] = useState<VehicleView[]>([]);
  const [vehicleFields, setVehicleFields] = useState<FieldDefinition[]>([]);
  const [companyFields, setCompanyFields] = useState<FieldDefinition[]>([]);
  const [driverFields, setDriverFields] = useState<FieldDefinition[]>([]);
  const [search, setSearch] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<"vehicles" | "companies" | "drivers" | "fields">("vehicles");
  const [importPreview, setImportPreview] = useState<ReferenceImportPreview | null>(null);
  const [importBusy, setImportBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [c, d, v, vf, cf, df] = await Promise.all([
        api.listCompanies(),
        api.listDrivers(),
        api.listVehicles(),
        api.listFieldDefinitions("vehicle"),
        api.listFieldDefinitions("company"),
        api.listFieldDefinitions("driver"),
      ]);
      setCompanies(c);
      setDrivers(d);
      setVehicles(v);
      setVehicleFields(vf);
      setCompanyFields(cf);
      setDriverFields(df);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const run = async (fn: () => Promise<unknown>, okMsg: string) => {
    setError(null);
    try {
      await fn();
      onNotice(okMsg);
      setTimeout(() => onNotice(""), 4000);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const exportAll = async () => {
    setError(null);
    try {
      const filePath = await save({
        defaultPath: `truckflow-reference-${new Date().toISOString().slice(0, 10)}.xlsx`,
        filters: [{ name: "Excel Workbook", extensions: ["xlsx"] }],
      });
      if (!filePath) return; // cancelled
      const path = await api.referenceExportCombined(actor.id, filePath);
      onNotice(`Reference database exported to ${path}`);
      setTimeout(() => onNotice(""), 5000);
    } catch (e) {
      setError(String(e));
    }
  };

  const startImport = async () => {
    setError(null);
    try {
      const picked = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "CSV or Excel", extensions: ["csv", "xlsx"] }],
      });
      if (typeof picked !== "string") return; // cancelled
      setImportBusy(true);
      const preview = await api.referenceImportPreview(actor.id, picked);
      setImportPreview(preview);
    } catch (e) {
      setError(String(e));
    } finally {
      setImportBusy(false);
    }
  };

  const q = search.trim().toLowerCase();
  const vq = vehicles.filter(
    (v) => !q || v.plate_number.toLowerCase().includes(q) || (v.company_name ?? "").toLowerCase().includes(q),
  );
  const cq = companies.filter((c) => !q || c.name.toLowerCase().includes(q));
  const dq = drivers.filter((d) => !q || d.name.toLowerCase().includes(q));

  return (
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="row between">
        <div className="section-title" style={{ fontSize: 15 }}>
          Reference Database
        </div>
        <div className="seg" style={{ flexWrap: "wrap" }}>
          <button className={tab === "vehicles" ? "active" : ""} onClick={() => setTab("vehicles")}>
            Vehicles ({vehicles.length})
          </button>
          <button className={tab === "companies" ? "active" : ""} onClick={() => setTab("companies")}>
            Companies ({companies.length})
          </button>
          <button className={tab === "drivers" ? "active" : ""} onClick={() => setTab("drivers")}>
            Drivers ({drivers.length})
          </button>
          <button className={tab === "fields" ? "active" : ""} onClick={() => setTab("fields")}>
            Fields
          </button>
        </div>
      </div>

      {error && <div className="error-banner">{error}</div>}

      <div className="row" style={{ flexWrap: "wrap", gap: 8 }}>
        <button className="primary" onClick={exportAll}>
          ⬇ Export all (one Excel file)
        </button>
        <button className="primary" onClick={startImport} disabled={importBusy}>
          ⬆ Import spreadsheet…
        </button>
      </div>

      {tab !== "fields" && (
        <div className="row">
          <input
            style={{ maxWidth: 300 }}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search this tab…"
          />
        </div>
      )}

      {tab === "vehicles" && (
        <VehicleTable
          vehicles={vq}
          companies={companies}
          drivers={drivers}
          fieldDefs={vehicleFields}
          actor={actor}
          canRegister={canRegister}
          onSave={(v, id) =>
            run(
              () =>
                id
                  ? api.updateVehicle(actor.id, id, v.plate, v.companyId, v.capacity, v.capacityUnit, v.driverId, v.extraFields)
                  : api.createVehicle(actor.id, v.plate, v.companyId, v.capacity, v.capacityUnit, v.driverId, v.extraFields).then(() => undefined),
              id ? "Vehicle updated." : "Vehicle registered.",
            )
          }
          onStatus={(id, status) =>
            run(() => api.setVehicleStatus(actor.id, id, status), status === "inactive" ? "Vehicle deactivated." : "Vehicle reactivated.")
          }
          onDelete={(id) => run(() => api.deleteVehicle(actor.id, id), "Vehicle deleted.")}
        />
      )}
      {tab === "companies" && (
        <CompanyTable
          companies={cq}
          fieldDefs={companyFields}
          canRegister={canRegister}
          onSave={(name, extraFields, id) =>
            run(
              () => id ? api.updateCompany(actor.id, id, name, extraFields) : api.createCompany(actor.id, name, extraFields).then(() => undefined),
              "Saved.",
            )
          }
          onStatus={(id, status) =>
            run(() => api.setCompanyStatus(actor.id, id, status), status === "inactive" ? "Company deactivated." : "Company reactivated.")
          }
          onDelete={(id) => run(() => api.deleteCompany(actor.id, id), "Company deleted.")}
        />
      )}
      {tab === "drivers" && (
        <DriverTable
          drivers={dq}
          fieldDefs={driverFields}
          canRegister={canRegister}
          onSave={(name, extraFields, id) =>
            run(
              () => id ? api.updateDriver(actor.id, id, name, extraFields) : api.createDriver(actor.id, name, extraFields).then(() => undefined),
              "Saved.",
            )
          }
          onStatus={(id, status) =>
            run(() => api.setDriverStatus(actor.id, id, status), status === "inactive" ? "Driver deactivated." : "Driver reactivated.")
          }
          onDelete={(id) => run(() => api.deleteDriver(actor.id, id), "Driver deleted.")}
        />
      )}
      {tab === "fields" && (
        <FieldManager actor={actor} onNotice={onNotice} onChanged={refresh} />
      )}

      {importPreview && (
        <ImportWizard
          actor={actor}
          preview={importPreview}
          fields={{ vehicle: vehicleFields, company: companyFields, driver: driverFields }}
          onClose={() => setImportPreview(null)}
          onApplied={(summary) => {
            const parts: string[] = [];
            for (const [label, s] of [
              ["Companies", summary.companies],
              ["Drivers", summary.drivers],
              ["Vehicles", summary.vehicles],
            ] as const) {
              if (s.created || s.updated || s.skipped) parts.push(`${label}: ${s.created} created, ${s.updated} updated, ${s.skipped} skipped`);
            }
            const errs = [...summary.companies.errors, ...summary.drivers.errors, ...summary.vehicles.errors];
            if (errs.length) {
              setError(`${parts.join(" · ")}. Errors:\n${errs.slice(0, 10).join("\n")}`);
            } else {
              onNotice(`Import complete — ${parts.join(" · ")}`);
              setTimeout(() => onNotice(""), 8000);
            }
            refresh();
          }}
        />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Import wizard — map the columns of the admin's own spreadsheet, then apply
// ---------------------------------------------------------------------------

interface ColConfig {
  mapping: string; // field key | "new" | "ignore"
  newKey: string;
  newType: "text" | "number" | "boolean" | "mixed";
  newRequired: boolean;
}

interface SheetConfig {
  entity: ReferenceEntityType;
  columns: Record<string, ColConfig>;
}

const FIELD_TYPE_LABELS: Record<string, string> = {
  text: "Text",
  number: "Number",
  boolean: "Yes / No",
  mixed: "Mixed",
};

/** The mapping value for a field: its fixed binding (standard) or its key (custom). */
function fieldMappingValue(f: FieldDefinition): string {
  return f.is_standard ? f.binding ?? f.field_key : f.field_key;
}

function defaultMappingFor(col: ColumnInfo, fields: FieldDefinition[]): ColConfig {
  const valid = new Set(fields.filter((f) => !f.is_hidden).map(fieldMappingValue));
  let mapping: string;
  if ((col.kind === "standard" || col.kind === "existing_custom") && valid.has(col.field_key)) {
    mapping = col.field_key;
  } else {
    mapping = "new";
  }
  return {
    mapping,
    newKey: col.kind === "new_custom" ? col.field_key : "",
    newType: col.kind === "new_custom" ? col.field_type : "text",
    newRequired: col.kind === "new_custom" ? col.is_required : false,
  };
}

function ImportWizard({
  actor,
  preview,
  fields,
  onClose,
  onApplied,
}: {
  actor: SessionUser;
  preview: ReferenceImportPreview;
  fields: Record<ReferenceEntityType, FieldDefinition[]>;
  onClose: () => void;
  onApplied: (s: CombinedImportSummary) => void;
}) {
  const resolveEntity = (e: string): ReferenceEntityType =>
    e === "vehicle" || e === "company" || e === "driver" ? e : "company";

  const [configs, setConfigs] = useState<SheetConfig[]>(() =>
    preview.sheets.map((s) => {
      const entity = resolveEntity(s.entity_type);
      return {
        entity,
        columns: Object.fromEntries(s.columns.map((c) => [c.header, defaultMappingFor(c, fields[entity])])),
      };
    }),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const setCol = (sheetIdx: number, header: string, patch: Partial<ColConfig>) => {
    setConfigs((prev) =>
      prev.map((c, i) =>
        i === sheetIdx ? { ...c, columns: { ...c.columns, [header]: { ...c.columns[header], ...patch } } } : c,
      ),
    );
  };

  const changeEntity = (sheetIdx: number, entity: ReferenceEntityType, sheet: SheetPreview) => {
    setConfigs((prev) =>
      prev.map((c, i) =>
        i === sheetIdx
          ? {
              entity,
              columns: Object.fromEntries(
                sheet.columns.map((col) => [col.header, defaultMappingFor(col, fields[entity])]),
              ),
            }
          : c,
      ),
    );
  };

  const apply = async () => {
    setError(null);
    setBusy(true);
    try {
      const sheets: ConfirmedSheet[] = preview.sheets.map((sheet, si) => ({
        sheet_name: sheet.sheet_name,
        entity_type: configs[si].entity,
        columns: sheet.columns.map((col): ConfirmedColumn => {
          const cfg = configs[si].columns[col.header];
          if (cfg.mapping === "ignore") return { header: col.header, mapping: "ignore" };
          if (cfg.mapping === "new") {
            return {
              header: col.header,
              mapping: "new",
              new_field_key: cfg.newKey || col.header.toLowerCase().replace(/[^a-z0-9_]/g, "_"),
              new_field_type: cfg.newType,
              new_is_required: cfg.newRequired,
            };
          }
          return { header: col.header, mapping: cfg.mapping };
        }),
      }));
      const request: ReferenceImportRequest = { file_path: preview.file_path, sheets };
      const summary = await api.referenceImportCombined(actor.id, request);
      onApplied(summary);
      onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, marginTop: 10 }}>
      <div className="row between">
        <div className="section-title" style={{ fontSize: 14 }}>
          Import — review &amp; map columns
        </div>
        <button className="ghost small" onClick={onClose}>
          ✕
        </button>
      </div>
      <p className="muted small">
        For each sheet, choose what each column maps to. Columns you don't want can be ignored. "Create new field"
        adds a new field to the database — it will appear in the forms next to the standard fields after importing.
      </p>
      {error && <div className="error-banner">{error}</div>}

      {preview.sheets.map((sheet, si) => {
        const cfg = configs[si];
        const options = [
          ...fields[cfg.entity]
            .filter((f) => !f.is_hidden)
            .map((f) => ({
              value: fieldMappingValue(f),
              label: `${f.field_label}${f.is_standard ? " (standard)" : " (existing field)"}`,
            })),
          { value: "new", label: "＋ Create new field…" },
          { value: "ignore", label: "✕ Ignore (don't import)" },
        ];
        return (
          <div key={sheet.sheet_name} className="stack" style={{ borderTop: "1px solid var(--border)", paddingTop: 10, marginTop: 10 }}>
            <div className="row between" style={{ flexWrap: "wrap", gap: 8 }}>
              <div className="section-title" style={{ fontSize: 13 }}>
                Sheet: <b>{sheet.sheet_name}</b> — {sheet.row_count} data rows
              </div>
              <div className="field" style={{ margin: 0 }}>
                <label>Import as</label>
                <select value={cfg.entity} onChange={(e) => changeEntity(si, e.target.value as ReferenceEntityType, sheet)}>
                  <option value="vehicle">Vehicles</option>
                  <option value="company">Companies</option>
                  <option value="driver">Drivers</option>
                </select>
              </div>
            </div>
            <table className="table">
              <thead>
                <tr>
                  <th>Spreadsheet column</th>
                  <th>Example values</th>
                  <th>Maps to</th>
                </tr>
              </thead>
              <tbody>
                {sheet.columns.map((col) => {
                  const colCfg = cfg.columns[col.header];
                  return (
                    <tr key={col.header}>
                      <td>
                        <b>{col.header}</b>
                        {col.kind !== "new_custom" && (
                          <span className="badge" style={{ marginLeft: 6 }}>
                            {col.kind === "standard" ? "Standard" : "Existing field"}
                          </span>
                        )}
                      </td>
                      <td className="muted small">{(col.sample_values ?? []).slice(0, 3).join(", ") || "—"}</td>
                      <td>
                        <div className="stack" style={{ gap: 6 }}>
                          <select
                            value={colCfg.mapping}
                            onChange={(e) => setCol(si, col.header, { mapping: e.target.value })}
                          >
                            {options.map((o) => (
                              <option key={o.value} value={o.value}>
                                {o.label}
                              </option>
                            ))}
                          </select>
                          {colCfg.mapping === "new" && (
                            <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
                              <input
                                style={{ maxWidth: 160 }}
                                value={colCfg.newKey}
                                onChange={(e) => setCol(si, col.header, { newKey: e.target.value.toLowerCase().replace(/[^a-z0-9_]/g, "_") })}
                                placeholder="field key"
                              />
                              <select
                                style={{ width: 110 }}
                                value={colCfg.newType}
                                onChange={(e) => setCol(si, col.header, { newType: e.target.value as ColConfig["newType"] })}
                              >
                                {Object.entries(FIELD_TYPE_LABELS).map(([v, l]) => (
                                  <option key={v} value={v}>
                                    {l}
                                  </option>
                                ))}
                              </select>
                              <label style={{ display: "flex", alignItems: "center", gap: 4, fontSize: 12 }}>
                                <input
                                  type="checkbox"
                                  checked={colCfg.newRequired}
                                  onChange={(e) => setCol(si, col.header, { newRequired: e.target.checked })}
                                  style={{ width: "auto" }}
                                />
                                required
                              </label>
                            </div>
                          )}
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        );
      })}

      <div className="row">
        <button className="primary" onClick={apply} disabled={busy}>
          {busy ? "Importing…" : "Apply import"}
        </button>
        <button className="ghost" onClick={onClose}>
          Cancel
        </button>
      </div>
    </div>
  );
}

interface VehicleDraft {
  plate: string;
  companyId: string | null;
  capacity: number | null;
  capacityUnit: string;
  driverId: string | null;
  extraFields: Record<string, unknown>;
}

function VehicleTable({
  vehicles,
  companies,
  drivers,
  fieldDefs,
  actor,
  canRegister,
  onSave,
  onStatus,
  onDelete,
}: {
  vehicles: VehicleView[];
  companies: CompanyView[];
  drivers: DriverView[];
  fieldDefs: FieldDefinition[];
  actor: SessionUser;
  canRegister: boolean;
  onSave: (v: VehicleDraft, id: string | null) => void;
  onStatus: (id: string, status: "active" | "inactive") => void;
  onDelete: (id: string) => void;
}) {
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [plate, setPlate] = useState("");
  const [companyId, setCompanyId] = useState<string>("");
  const [capacity, setCapacity] = useState("");
  const [capacityUnit, setCapacityUnit] = useState<string>("litres");
  const [driverId, setDriverId] = useState<string>("");
  const [extraFields, setExtraFields] = useState<Record<string, unknown>>({});

  const startAdd = () => {
    setAdding(true);
    setEditingId(null);
    setPlate("");
    setCompanyId(companies.find((c) => c.status === "active")?.id ?? "");
    setCapacity("");
    setCapacityUnit("litres");
    setDriverId("");
    setExtraFields({});
  };

  const startEdit = (v: VehicleView) => {
    setAdding(false);
    setEditingId(v.id);
    setPlate(v.plate_number);
    setCompanyId(v.company_id ?? "");
    setCapacity(v.registered_capacity != null ? String(v.registered_capacity) : "");
    setCapacityUnit(v.capacity_unit ?? "litres");
    setDriverId(v.default_driver_id ?? "");
    setExtraFields(v.extra_fields ?? {});
  };

  const submit = () => {
    if (!plate.trim()) return;
    const capacityOk = capacity === "" || !isNaN(parseFloat(capacity));
    if (!capacityOk) return;
    if (editingId && capacity !== "" && parseFloat(capacity) !== undefined) {
      // Confirmation before saving capacity changes (05-ui-screens.md §6b).
      const changed = vehicles.find((v) => v.id === editingId);
      if (changed && changed.registered_capacity !== parseFloat(capacity)) {
        const ok = window.confirm(
          `Changing registered capacity affects future trip records. Continue with ${parseFloat(capacity)} ${capacityUnit}?`,
        );
        if (!ok) return;
      }
    }
    onSave(
      {
        plate: plate.trim().toUpperCase(),
        companyId: companyId || null,
        capacity: capacity === "" ? null : parseFloat(capacity),
        capacityUnit,
        driverId: driverId || null,
        extraFields,
      },
      editingId,
    );
    setAdding(false);
    setEditingId(null);
  };

  const visibleDefs = fieldDefs.filter((fd) => !fd.is_hidden);
  const stdDefs = visibleDefs.filter((fd) => fd.is_standard);
  const customDefs = visibleDefs.filter((fd) => !fd.is_standard);
  const showUnit = stdDefs.some((fd) => fd.binding === "capacity_unit");

  return (
    <div className="stack">
      {(adding || editingId) && (
        <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}>
          <div className="section-title" style={{ fontSize: 14 }}>
            {editingId ? "Edit vehicle" : "Register vehicle"}
          </div>
          <div className="row" style={{ flexWrap: "wrap", gap: 10 }}>
            {stdDefs.map((fd) => {
              const req = fd.is_required ? <span style={{ color: "var(--danger, #d32f2f)" }}> *</span> : null;
              if (fd.binding === "plate_number") {
                return (
                  <div key={fd.id} className="field" style={{ margin: 0 }}>
                    <label>{fd.field_label}{req}</label>
                    <input value={plate} onChange={(e) => setPlate(e.target.value.toUpperCase())} placeholder="A123AB" />
                  </div>
                );
              }
              if (fd.binding === "company") {
                return (
                  <div key={fd.id} className="field" style={{ margin: 0 }}>
                    <label>{fd.field_label}{req}</label>
                    <select value={companyId} onChange={(e) => setCompanyId(e.target.value)}>
                      <option value="">—</option>
                      {companies
                        .filter((c) => c.status === "active")
                        .map((c) => (
                          <option key={c.id} value={c.id}>
                            {c.name}
                          </option>
                        ))}
                    </select>
                  </div>
                );
              }
              if (fd.binding === "driver") {
                return (
                  <div key={fd.id} className="field" style={{ margin: 0 }}>
                    <label>{fd.field_label}{req}</label>
                    <select value={driverId} onChange={(e) => setDriverId(e.target.value)}>
                      <option value="">—</option>
                      {drivers
                        .filter((d) => d.status === "active")
                        .map((d) => (
                          <option key={d.id} value={d.id}>
                            {d.name}
                          </option>
                        ))}
                    </select>
                  </div>
                );
              }
              if (fd.binding === "registered_capacity") {
                return (
                  <div key={fd.id} className="field" style={{ margin: 0 }}>
                    <label>{fd.field_label}{req}</label>
                    <div className="row" style={{ gap: 8, alignItems: "flex-end" }}>
                      <input
                        style={{ flex: 1 }}
                        value={capacity}
                        onChange={(e) => setCapacity(e.target.value)}
                        placeholder="25"
                      />
                      {showUnit && (
                        <select
                          style={{ width: 120 }}
                          value={capacityUnit}
                          onChange={(e) => setCapacityUnit(e.target.value)}
                        >
                          <option value="litres">Litres</option>
                          <option value="cubic_meters">m³</option>
                          <option value="gallons">Gallons</option>
                          <option value="tonnes">Tonnes</option>
                          <option value="kg">kg</option>
                        </select>
                      )}
                    </div>
                  </div>
                );
              }
              if (fd.binding === "capacity_unit") return null; // rendered inside the capacity row
              return null;
            })}
            {customDefs.map((fd) => (
              <DynamicFieldInput
                key={fd.id}
                fd={fd}
                value={extraFields[fd.field_key]}
                onChange={(v) => setExtraFields({ ...extraFields, [fd.field_key]: v })}
              />
            ))}
          </div>
          <div className="row">
            <button className="primary" onClick={submit} disabled={!plate.trim()}>
              {editingId ? "Save changes" : "Register vehicle"}
            </button>
            <button
              className="ghost"
              onClick={() => {
                setAdding(false);
                setEditingId(null);
              }}
            >
              Cancel
            </button>
          </div>
        </div>
      )}

      {canRegister && (
        <button className="primary" style={{ alignSelf: "flex-start" }} onClick={startAdd}>
          + Register vehicle
        </button>
      )}

      {vehicles.length === 0 ? (
        <p className="muted small">No vehicles yet — register the first one above.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Plate</th>
              <th>Company</th>
              <th>Capacity</th>
              <th>Default driver</th>
              <th>Status</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {vehicles.map((v) => (
              <tr key={v.id}>
                <td className="plate-font">{v.plate_number}</td>
                <td>{v.company_name ?? "—"}</td>
                <td>{v.registered_capacity != null ? `${v.registered_capacity} t` : "—"}</td>
                <td>{v.default_driver_name ?? "—"}</td>
                <td>
                  <span className={`badge ${v.status}`}>{v.status}</span>
                </td>
                <td>
                  <div className="row" style={{ gap: 6 }}>
                    <button className="ghost small" onClick={() => startEdit(v)}>
                      Edit
                    </button>
                    <button
                      className="ghost small"
                      onClick={() => {
                        const ok = window.confirm(
                          v.status === "active"
                            ? `Deactivate ${v.plate_number}? Its history stays intact.`
                            : `Reactivate ${v.plate_number}?`,
                        );
                        if (ok) onStatus(v.id, v.status === "active" ? "inactive" : "active");
                      }}
                    >
                      {v.status === "active" ? "Deactivate" : "Reactivate"}
                    </button>
                    <button
                      className="danger small"
                      onClick={() => {
                        const ok = window.confirm(
                          `Delete vehicle ${v.plate_number} permanently?\n\nThis removes the vehicle from the reference database. Past trips are kept but no longer linked to it. This cannot be undone.`,
                        );
                        if (ok) onDelete(v.id);
                      }}
                    >
                      Delete
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <p className="muted small">
        Registered once per vehicle — the system cross-references every capture against this list. Editing runs under{" "}
        {actor.name}.
      </p>
    </div>
  );
}

function CompanyTable({
  companies,
  fieldDefs,
  canRegister,
  onSave,
  onStatus,
  onDelete,
}: {
  companies: CompanyView[];
  fieldDefs: FieldDefinition[];
  canRegister: boolean;
  onSave: (name: string, extraFields: Record<string, unknown>, id: string | null) => void;
  onStatus: (id: string, status: "active" | "inactive") => void;
  onDelete: (id: string) => void;
}) {
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [extraFields, setExtraFields] = useState<Record<string, unknown>>({});

  const startAdd = () => {
    setAdding(true);
    setEditingId(null);
    setName("");
    setExtraFields({});
  };

  const startEdit = (c: CompanyView) => {
    setAdding(false);
    setEditingId(c.id);
    setName(c.name);
    setExtraFields(c.extra_fields ?? {});
  };

  const submit = () => {
    if (!name.trim()) return;
    onSave(name.trim(), extraFields, editingId);
    setAdding(false);
    setEditingId(null);
    setName("");
  };

  const customDefs = fieldDefs.filter((fd) => !fd.is_hidden && !fd.is_standard);
  const nameDef = fieldDefs.find((fd) => fd.is_standard && fd.binding === "name" && !fd.is_hidden);

  return (
    <div className="stack">
      {canRegister && (
        <button className="primary" style={{ alignSelf: "flex-start" }} onClick={startAdd}>
          + Add company
        </button>
      )}

      {(adding || editingId) && (
        <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}>
          <div className="row" style={{ flexWrap: "wrap", gap: 10 }}>
            <div className="field" style={{ margin: 0, maxWidth: 300 }}>
              {nameDef && (
                <label>
                  {nameDef.field_label}
                  {nameDef.is_required && <span style={{ color: "var(--danger, #d32f2f)" }}> *</span>}
                </label>
              )}
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder={nameDef?.field_label ?? "Company name"} />
            </div>
            {customDefs.map((fd) => (
              <DynamicFieldInput key={fd.id} fd={fd} value={extraFields[fd.field_key]} onChange={(v) => setExtraFields({ ...extraFields, [fd.field_key]: v })} />
            ))}
          </div>
          <div className="row">
            <button className="primary" onClick={submit} disabled={!name.trim()}>
              {editingId ? "Save" : "Add"}
            </button>
            <button className="ghost" onClick={() => { setAdding(false); setEditingId(null); }}>
              Cancel
            </button>
          </div>
        </div>
      )}

      {companies.length === 0 ? (
        <p className="muted small">No companies yet.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Status</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {companies.map((c) => (
              <tr key={c.id}>
                <td>{c.name}</td>
                <td>
                  <span className={`badge ${c.status}`}>{c.status}</span>
                </td>
                <td>
                  <div className="row" style={{ gap: 6 }}>
                    <button
                      className="ghost small"
                      onClick={() => startEdit(c)}
                    >
                      Edit
                    </button>
                    <button
                      className="ghost small"
                      onClick={() => {
                        const ok = window.confirm(
                          c.status === "active"
                            ? `Deactivate "${c.name}"? Its history stays intact.`
                            : `Reactivate "${c.name}"?`,
                        );
                        if (ok) onStatus(c.id, c.status === "active" ? "inactive" : "active");
                      }}
                    >
                      {c.status === "active" ? "Deactivate" : "Reactivate"}
                    </button>
                    <button
                      className="danger small"
                      onClick={() => {
                        const ok = window.confirm(
                          `Delete company "${c.name}" permanently?\n\nVehicles linked to it will keep their history but be unlinked. This cannot be undone.`,
                        );
                        if (ok) onDelete(c.id);
                      }}
                    >
                      Delete
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function DriverTable({
  drivers,
  fieldDefs,
  canRegister,
  onSave,
  onStatus,
  onDelete,
}: {
  drivers: DriverView[];
  fieldDefs: FieldDefinition[];
  canRegister: boolean;
  onSave: (name: string, extraFields: Record<string, unknown>, id: string | null) => void;
  onStatus: (id: string, status: "active" | "inactive") => void;
  onDelete: (id: string) => void;
}) {
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [extraFields, setExtraFields] = useState<Record<string, unknown>>({});

  const startAdd = () => {
    setAdding(true);
    setEditingId(null);
    setName("");
    setExtraFields({});
  };

  const startEdit = (d: DriverView) => {
    setAdding(false);
    setEditingId(d.id);
    setName(d.name);
    setExtraFields(d.extra_fields ?? {});
  };

  const submit = () => {
    if (!name.trim()) return;
    onSave(name.trim(), extraFields, editingId);
    setAdding(false);
    setEditingId(null);
    setName("");
  };

  const customDefs = fieldDefs.filter((fd) => !fd.is_hidden && !fd.is_standard);
  const nameDef = fieldDefs.find((fd) => fd.is_standard && fd.binding === "name" && !fd.is_hidden);

  return (
    <div className="stack">
      {canRegister && (
        <button className="primary" style={{ alignSelf: "flex-start" }} onClick={startAdd}>
          + Add driver
        </button>
      )}

      {(adding || editingId) && (
        <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}>
          <div className="row" style={{ flexWrap: "wrap", gap: 10 }}>
            <div className="field" style={{ margin: 0, maxWidth: 300 }}>
              {nameDef && (
                <label>
                  {nameDef.field_label}
                  {nameDef.is_required && <span style={{ color: "var(--danger, #d32f2f)" }}> *</span>}
                </label>
              )}
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder={nameDef?.field_label ?? "Driver name"} />
            </div>
            {customDefs.map((fd) => (
              <DynamicFieldInput key={fd.id} fd={fd} value={extraFields[fd.field_key]} onChange={(v) => setExtraFields({ ...extraFields, [fd.field_key]: v })} />
            ))}
          </div>
          <div className="row">
            <button className="primary" onClick={submit} disabled={!name.trim()}>
              {editingId ? "Save" : "Add"}
            </button>
            <button className="ghost" onClick={() => { setAdding(false); setEditingId(null); }}>
              Cancel
            </button>
          </div>
        </div>
      )}

      {drivers.length === 0 ? (
        <p className="muted small">No drivers yet.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Status</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {drivers.map((d) => (
              <tr key={d.id}>
                <td>{d.name}</td>
                <td>
                  <span className={`badge ${d.status}`}>{d.status}</span>
                </td>
                <td>
                  <div className="row" style={{ gap: 6 }}>
                    <button
                      className="ghost small"
                      onClick={() => startEdit(d)}
                    >
                      Edit
                    </button>
                    <button
                      className="ghost small"
                      onClick={() => {
                        const ok = window.confirm(
                          d.status === "active"
                            ? `Deactivate "${d.name}"? Their history stays intact.`
                            : `Reactivate "${d.name}"?`,
                        );
                        if (ok) onStatus(d.id, d.status === "active" ? "inactive" : "active");
                      }}
                    >
                      {d.status === "active" ? "Deactivate" : "Reactivate"}
                    </button>
                    <button
                      className="danger small"
                      onClick={() => {
                        const ok = window.confirm(
                          `Delete driver "${d.name}" permanently?\n\nVehicles using them as default driver will be unlinked. This cannot be undone.`,
                        );
                        if (ok) onDelete(d.id);
                      }}
                    >
                      Delete
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Field Manager — dynamic custom fields for the reference database
// ---------------------------------------------------------------------------

const ENTITY_LABELS: Record<string, string> = {
  vehicle: "Vehicles",
  company: "Companies",
  driver: "Drivers",
};

/** Reusable input that renders based on a field definition's type. */
function DynamicFieldInput({
  fd,
  value,
  onChange,
}: {
  fd: FieldDefinition;
  value: unknown;
  onChange: (v: unknown) => void;
}) {
  const strVal = value != null ? String(value) : "";
  if (fd.field_type === "boolean") {
    return (
      <div className="field" style={{ margin: 0, minWidth: 140 }}>
        <label>
          {fd.field_label}
          {fd.is_required && <span style={{ color: "var(--danger, #d32f2f)" }}> *</span>}
        </label>
        <select value={strVal} onChange={(e) => onChange(e.target.value === "" ? null : e.target.value === "true")}>
          <option value="">—</option>
          <option value="true">Yes</option>
          <option value="false">No</option>
        </select>
      </div>
    );
  }
  if (fd.field_type === "number") {
    return (
      <div className="field" style={{ margin: 0, minWidth: 140 }}>
        <label>
          {fd.field_label}
          {fd.is_required && <span style={{ color: "var(--danger, #d32f2f)" }}> *</span>}
        </label>
        <input
          type="number"
          value={strVal}
          onChange={(e) => onChange(e.target.value === "" ? null : Number(e.target.value))}
          placeholder={fd.field_label}
        />
      </div>
    );
  }
  return (
    <div className="field" style={{ margin: 0, minWidth: 160 }}>
      <label>
        {fd.field_label}
        {fd.is_required && <span style={{ color: "var(--danger, #d32f2f)" }}> *</span>}
      </label>
      <input
        value={strVal}
        onChange={(e) => onChange(e.target.value || null)}
        placeholder={fd.field_label}
      />
    </div>
  );
}

const FIELD_TYPE_OPTIONS: { value: string; label: string; desc: string }[] = [
  { value: "text", label: "Text", desc: "Letters and words (e.g. Notes, License #)" },
  { value: "number", label: "Number", desc: "Numeric values only (e.g. Year, Weight)" },
  { value: "boolean", label: "Yes / No", desc: "True or false toggle (e.g. Insured, Active)" },
  { value: "mixed", label: "Mixed", desc: "Letters, numbers, and symbols (e.g. Plate, Code)" },
];

function FieldManager({
  actor,
  onNotice,
  onChanged,
}: {
  actor: SessionUser;
  onNotice: (msg: string) => void;
  onChanged: () => void;
}) {
  const [entityTab, setEntityTab] = useState<string>("vehicle");
  const [fields, setFields] = useState<FieldDefinition[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [newKey, setNewKey] = useState("");
  const [newLabel, setNewLabel] = useState("");
  const [newType, setNewType] = useState("text");
  const [newRequired, setNewRequired] = useState(false);

  const refreshFields = useCallback(async () => {
    try {
      const f = await api.listFieldDefinitions(entityTab);
      setFields(f);
    } catch (e) {
      setError(String(e));
    }
  }, [entityTab]);

  useEffect(() => {
    refreshFields();
    setAdding(false);
    setEditingId(null);
  }, [refreshFields]);

  const startAdd = () => {
    setAdding(true);
    setEditingId(null);
    setNewKey("");
    setNewLabel("");
    setNewType("text");
    setNewRequired(false);
  };

  const startEdit = (fd: FieldDefinition) => {
    setAdding(false);
    setEditingId(fd.id);
    setNewKey(fd.field_key);
    setNewLabel(fd.field_label);
    setNewType(fd.field_type);
    setNewRequired(fd.is_required);
  };

  const submitNew = async () => {
    setError(null);
    if (!newKey.trim() || !newLabel.trim()) {
      setError("Both key and label are required.");
      return;
    }
    try {
      await api.createFieldDefinition(
        actor.id,
        entityTab,
        newKey.trim(),
        newLabel.trim(),
        newType,
        newRequired,
        fields.length,
      );
      onNotice(`Field "${newLabel.trim()}" added to ${ENTITY_LABELS[entityTab]}.`);
      setTimeout(() => onNotice(""), 4000);
      setAdding(false);
      await refreshFields();
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  const submitEdit = async () => {
    setError(null);
    if (!editingId || !newLabel.trim()) {
      setError("Label is required.");
      return;
    }
    try {
      await api.updateFieldDefinition(actor.id, editingId, {
        field_key: newKey.trim(),
        field_label: newLabel.trim(),
        field_type: newType as FieldDefinition["field_type"],
        is_required: newRequired,
      });
      onNotice(`Field "${newLabel.trim()}" updated.`);
      setTimeout(() => onNotice(""), 4000);
      setEditingId(null);
      await refreshFields();
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  const deleteField = async (fd: FieldDefinition) => {
    const ok = window.confirm(
      `Delete the "${fd.field_label}" field from ${ENTITY_LABELS[entityTab]} permanently?\n\nIt disappears from the registration forms and from import/export. Your existing records keep their data — only the field definition is removed. This cannot be undone.`,
    );
    if (!ok) return;
    setError(null);
    try {
      await api.deleteFieldDefinition(actor.id, fd.id);
      onNotice(`Field "${fd.field_label}" deleted.`);
      setTimeout(() => onNotice(""), 4000);
      await refreshFields();
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="stack">
      <p className="muted small">
        These fields drive the registration forms, exports, and imports. The standard fields (Plate, Company, Driver,
        Capacity…) are here too — rename them, change their type, or remove them so the system matches how your
        operation actually records vehicles. Add as many custom fields as you need.
      </p>

      <div className="seg" style={{ alignSelf: "flex-start" }}>
        {Object.entries(ENTITY_LABELS).map(([key, label]) => (
          <button key={key} className={entityTab === key ? "active" : ""} onClick={() => setEntityTab(key)}>
            {label}
          </button>
        ))}
      </div>

      {error && <div className="error-banner">{error}</div>}

      {(adding || editingId) && (
        <div className="stack" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}>
          <div className="section-title" style={{ fontSize: 14 }}>
            {editingId ? "Edit field" : "Add new field"}
          </div>
          <div className="row" style={{ flexWrap: "wrap", gap: 10 }}>
            <div className="field" style={{ margin: 0, minWidth: 160 }}>
              <label>Field key</label>
              <input
                value={newKey}
                onChange={(e) => setNewKey(e.target.value.toLowerCase().replace(/[^a-z0-9_]/g, "_"))}
                placeholder="e.g. insurance_expiry"
              />
              <p className="muted small" style={{ marginTop: 2 }}>
                Lowercase letters, numbers, and underscores only. Renaming a field key renames its stored data —
                custom field values follow automatically.
              </p>
            </div>
            <div className="field" style={{ margin: 0, minWidth: 200 }}>
              <label>Display label</label>
              <input
                value={newLabel}
                onChange={(e) => setNewLabel(e.target.value)}
                placeholder="e.g. Insurance Expiry Date"
              />
            </div>
            <div className="field" style={{ margin: 0, minWidth: 180 }}>
              <label>Field type</label>
              <select value={newType} onChange={(e) => setNewType(e.target.value)}>
                {FIELD_TYPE_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label} — {opt.desc}
                  </option>
                ))}
              </select>
            </div>
            <div className="field" style={{ margin: 0 }}>
              <label>&nbsp;</label>
              <label style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer" }}>
                <input
                  type="checkbox"
                  checked={newRequired}
                  onChange={(e) => setNewRequired(e.target.checked)}
                  style={{ width: "auto" }}
                />
                Required field
              </label>
            </div>
          </div>
          <div className="row">
            <button
              className="primary"
              onClick={editingId ? submitEdit : submitNew}
              disabled={editingId ? !newLabel.trim() : !newKey.trim() || !newLabel.trim()}
            >
              {editingId ? "Save changes" : "Add field"}
            </button>
            <button
              className="ghost"
              onClick={() => {
                setAdding(false);
                setEditingId(null);
              }}
            >
              Cancel
            </button>
          </div>
        </div>
      )}

      <button className="primary" style={{ alignSelf: "flex-start" }} onClick={startAdd}>
        + Add custom field
      </button>

      {fields.length === 0 ? (
        <p className="muted small">
          No fields defined yet for {ENTITY_LABELS[entityTab]}. Add fields above.
        </p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Label</th>
              <th>Key</th>
              <th>Type</th>
              <th>Required</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {fields.map((fd) => (
              <tr key={fd.id}>
                <td>
                  <b>{fd.field_label}</b>{" "}
                  {fd.is_standard && <span className="badge">Standard</span>}
                </td>
                <td className="muted small" style={{ fontFamily: "monospace" }}>{fd.field_key}</td>
                <td>
                  <span className="badge">
                    {FIELD_TYPE_OPTIONS.find((o) => o.value === fd.field_type)?.label ?? fd.field_type}
                  </span>
                </td>
                <td>{fd.is_required ? <span className="badge active">Yes</span> : "No"}</td>
                <td>
                  <div className="row" style={{ gap: 6 }}>
                    <button className="ghost small" onClick={() => startEdit(fd)}>
                      Edit
                    </button>
                    <button className="danger small" onClick={() => deleteField(fd)}>
                      Delete
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
