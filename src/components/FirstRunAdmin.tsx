import { useState } from "react";
import { api } from "../lib/api";
import type { SessionUser } from "../lib/types";
import PasswordChecklist from "./PasswordChecklist";

export default function FirstRunAdmin({ onDone }: { onDone: (user: SessionUser) => void }) {
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [created, setCreated] = useState<{ user: SessionUser; recoveryCode: string | null; filePath: string | null } | null>(null);
  const [copied, setCopied] = useState(false);

  const submit = async () => {
    setError(null);
    if (password !== confirm) {
      setError("Passwords do not match.");
      return;
    }
    setBusy(true);
    try {
      const res = await api.createFirstAdmin(name, password);
      let filePath: string | null = null;
      try {
        const info = await api.getRecoveryCode(res.user.id);
        filePath = info.file_path;
      } catch {
        /* path is informational */
      }
      setCreated({ user: res.user, recoveryCode: res.recovery_code, filePath });
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const copyCode = async () => {
    if (!created?.recoveryCode) return;
    try {
      await navigator.clipboard.writeText(created.recoveryCode);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };

  return (
    <div className="auth-wrap">
      <div className="auth-card">
        <div className="brand">
          <div className="brand-mark">TF</div>
          <div>
            <div className="brand-name">TruckFlow</div>
            <div className="brand-sub">Gate trip management</div>
          </div>
        </div>

        {created ? (
          <>
            <div className="auth-title">Admin account created</div>
            <p className="muted small">
              Your account <b>{created.user.name}</b> is ready. The recovery code below is also saved in a file on this
              computer — open it anytime to copy the code:
            </p>

            <div
              className="card"
              style={{
                margin: "14px 0",
                textAlign: "center",
                borderColor: "color-mix(in srgb, var(--warning) 45%, transparent)",
              }}
            >
              <div className="section-title" style={{ fontSize: 20, letterSpacing: 3, fontFamily: "monospace" }}>
                {created.recoveryCode ?? "—"}
              </div>
              <button className="ghost small" onClick={copyCode} style={{ marginTop: 8 }}>
                {copied ? "Copied ✓" : "Copy code"}
              </button>
              {created.filePath && (
                <div className="muted small" style={{ marginTop: 8, wordBreak: "break-all" }}>
                  Saved in: <code>{created.filePath}</code>
                </div>
              )}
            </div>

            <div className="error-banner" style={{ background: "color-mix(in srgb, var(--warning) 12%, transparent)" }}>
              <b>This code is only for admins who are locked out.</b> If you ever forget your password and no other
              admin can reset you, this code (or the file) is the only way back in. Anyone with it can reset an admin
              password — keep the file private. You can regenerate it anytime in Settings → Recovery code.
            </div>

            <button className="primary" style={{ width: "100%", padding: "11px", marginTop: 14 }} onClick={() => onDone(created.user)}>
              Continue
            </button>
          </>
        ) : (
          <>
            <div className="auth-title">Create your first Admin account</div>
            <div className="auth-hint">
              This happens once on a fresh installation. This account manages all other users.
            </div>

            {error && <div className="error-banner">{error}</div>}

            <div className="field">
              <label>Full name</label>
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. Jane Mwangi" autoFocus />
            </div>

            <div className="field">
              <label>Password</label>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="At least 8 characters"
              />
              <PasswordChecklist password={password} />
            </div>

            <div className="field">
              <label>Confirm password</label>
              <input type="password" value={confirm} onChange={(e) => setConfirm(e.target.value)} />
            </div>

            <button
              className="primary"
              style={{ width: "100%", padding: "11px" }}
              onClick={submit}
              disabled={busy || !name || !password || password !== confirm}
            >
              {busy ? "Creating account…" : "Create account"}
            </button>
          </>
        )}
      </div>
    </div>
  );
}
