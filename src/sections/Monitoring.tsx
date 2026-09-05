import { useEffect, useState } from "react";
import { api } from "../lib/api";
import type { MachineStatusView, MonitoringDashboard, UserStatusView } from "../lib/types";
import type { SessionUser } from "../lib/types";

function formatTimeAgo(dateStr: string): string {
  const date = new Date(dateStr);
  const now = new Date();
  const diff = Math.floor((now.getTime() - date.getTime()) / 1000);
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

function formatDate(dateStr: string): string {
  return new Date(dateStr).toLocaleString();
}

function OnlineIndicator({ online }: { online: boolean }) {
  return (
    <span
      style={{
        display: "inline-block",
        width: 8,
        height: 8,
        borderRadius: "50%",
        backgroundColor: online ? "#22c55e" : "#9ca3af",
        boxShadow: online ? "0 0 4px #22c55e" : "none",
      }}
    />
  );
}

function StatusBadge({ status }: { status: string }) {
  const colors: Record<string, string> = {
    active: "#22c55e",
    pending: "#f59e0b",
    inactive: "#9ca3af",
    suspended: "#ef4444",
  };
  return (
    <span
      className="badge"
      style={{
        backgroundColor: `${colors[status] || "#9ca3af"}20`,
        color: colors[status] || "#9ca3af",
        border: `1px solid ${colors[status] || "#9ca3af"}40`,
      }}
    >
      {status}
    </span>
  );
}

function AuthBadge({ type }: { type: string }) {
  return (
    <span
      style={{
        fontSize: 11,
        padding: "2px 8px",
        borderRadius: 4,
        backgroundColor: "var(--surface-2)",
        color: "var(--muted)",
      }}
    >
      {type}
    </span>
  );
}

export function MonitoringDashboard({ user }: { user: SessionUser }) {
  const [data, setData] = useState<MonitoringDashboard | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await api.monitoringDashboard(user.id);
      setData(result);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    load();
    const interval = setInterval(load, 30000);
    return () => clearInterval(interval);
  }, [user.id]);

  if (loading && !data) {
    return <div className="muted">Loading...</div>;
  }

  if (error) {
    return <div className="error-banner">{error}</div>;
  }

  if (!data) return null;

  return (
    <div className="stack" style={{ gap: 20 }}>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
        <h2 style={{ margin: 0 }}>System Overview</h2>
        <button className="ghost small" onClick={load} disabled={loading}>
          {loading ? "Refreshing..." : "Refresh"}
        </button>
      </div>

      <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))", gap: 12 }}>
        <div className="card">
          <div style={{ fontSize: 28, fontWeight: 700, color: "#22c55e" }}>{data.online_machines_count}</div>
          <div className="muted small">Online Machines</div>
        </div>
        <div className="card">
          <div style={{ fontSize: 28, fontWeight: 700, color: data.pending_users_count > 0 ? "#f59e0b" : "#22c55e" }}>
            {data.pending_users_count}
          </div>
          <div className="muted small">Pending Users</div>
        </div>
        <div className="card">
          <div style={{ fontSize: 28, fontWeight: 700 }}>{data.machines.length}</div>
          <div className="muted small">Total Machines</div>
        </div>
        <div className="card">
          <div style={{ fontSize: 28, fontWeight: 700 }}>{data.users.length}</div>
          <div className="muted small">Total Users</div>
        </div>
      </div>

      <div className="card">
        <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 12 }}>
          <h3 style={{ margin: 0 }}>Your Session</h3>
        </div>
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(200px, 1fr))", gap: 12 }}>
          <div>
            <div className="muted small" style={{ marginBottom: 2 }}>Logged in as</div>
            <div style={{ fontWeight: 500 }}>{user.name}</div>
          </div>
          <div>
            <div className="muted small" style={{ marginBottom: 2 }}>Role</div>
            <div style={{ fontWeight: 500 }}>{user.permissions.includes("manage_users") ? "Administrator" : "User"}</div>
          </div>
          <div>
            <div className="muted small" style={{ marginBottom: 2 }}>Session started</div>
            <div style={{ fontWeight: 500 }}>{formatDate(user.created_at)}</div>
          </div>
          <div>
            <div className="muted small" style={{ marginBottom: 2 }}>Account ID</div>
            <div style={{ fontFamily: "monospace", fontSize: 12 }}>{user.id.slice(0, 8)}...</div>
          </div>
        </div>
      </div>

      <div>
        <h3 style={{ margin: "0 0 12px 0" }}>Connected Machines</h3>
        {data.machines.length === 0 ? (
          <div className="muted" style={{ padding: 20, textAlign: "center", border: "1px dashed var(--border)", borderRadius: "var(--radius)" }}>
            No machines connected
          </div>
        ) : (
          <div className="table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Machine</th>
                  <th>Status</th>
                  <th>User</th>
                  <th>IP Address</th>
                  <th>Last Seen</th>
                </tr>
              </thead>
              <tbody>
                {data.machines.map((m) => (
                  <tr key={m.machine_id}>
                    <td>
                      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                        <OnlineIndicator online={m.is_online} />
                        <span style={{ fontWeight: 500 }}>{m.pc_name || m.machine_id.slice(0, 8)}</span>
                      </div>
                    </td>
                    <td>
                      <StatusBadge status={m.is_online ? "online" : "offline"} />
                    </td>
                    <td>
                      {m.user_name ? (
                        <span>{m.user_name}</span>
                      ) : (
                        <span className="muted">—</span>
                      )}
                    </td>
                    <td>
                      <span className="muted">{m.ip_address || "—"}</span>
                    </td>
                    <td>
                      <span className="muted small">{formatTimeAgo(m.last_seen_at)}</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div>
        <h3 style={{ margin: "0 0 12px 0" }}>Users</h3>
        {data.users.length === 0 ? (
          <div className="muted" style={{ padding: 20, textAlign: "center", border: "1px dashed var(--border)", borderRadius: "var(--radius)" }}>
            No users found
          </div>
        ) : (
          <div className="table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Status</th>
                  <th>Auth</th>
                  <th>Last Login</th>
                  <th>Created</th>
                </tr>
              </thead>
              <tbody>
                {data.users.map((u) => (
                  <tr key={u.id}>
                    <td>
                      <span style={{ fontWeight: 500 }}>{u.name}</span>
                      {u.id === user.id && <span className="muted small"> (you)</span>}
                    </td>
                    <td>
                      <StatusBadge status={u.status} />
                    </td>
                    <td>
                      <AuthBadge type={u.auth_type} />
                    </td>
                    <td>
                      <span className="muted small">{u.last_login ? formatTimeAgo(u.last_login) : "Never"}</span>
                    </td>
                    <td>
                      <span className="muted small">{formatTimeAgo(u.created_at)}</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
