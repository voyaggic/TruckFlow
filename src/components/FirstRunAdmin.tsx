import { useState } from "react";
import { api } from "../lib/api";
import type { SessionUser } from "../lib/types";
import PasswordChecklist from "./PasswordChecklist";

type View = "choice" | "signup" | "signin";

export default function FirstRunAdmin({ onDone }: { onDone: (user: SessionUser) => void }) {
  const [view, setView] = useState<View>("choice");

  // Sign Up state
  const [companyName, setCompanyName] = useState("");
  const [adminName, setAdminName] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [created, setCreated] = useState<{ user: SessionUser; recoveryCode: string | null; filePath: string | null } | null>(null);
  const [copied, setCopied] = useState(false);

  // Sign In state
  const [signInUser, setSignInUser] = useState("");
  const [signInPass, setSignInPass] = useState("");
  const [signInError, setSignInError] = useState<string | null>(null);
  const [signInBusy, setSignInBusy] = useState(false);

  const submitSignUp = async () => {
    setError(null);
    if (password !== confirm) {
      setError("Passwords do not match.");
      return;
    }
    setBusy(true);
    try {
      const res = await api.createCompanyAndAdmin(companyName, adminName, password);
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

  const submitSignIn = async () => {
    setSignInError(null);
    setSignInBusy(true);
    try {
      const res = await api.loginPassword(signInUser, signInPass);
      onDone(res.user);
    } catch (e) {
      setSignInError(String(e));
    } finally {
      setSignInBusy(false);
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

  // --- Choice screen ---
  if (view === "choice") {
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

          <div className="auth-title">Welcome to TruckFlow</div>
          <div className="auth-hint">
            This is a fresh installation. Choose how to get started:
          </div>

          <button
            className="primary"
            style={{ width: "100%", padding: "11px", marginBottom: 10 }}
            onClick={() => setView("signup")}
          >
            Sign Up — Create a new account
          </button>

          <button
            className="ghost"
            style={{ width: "100%", padding: "11px" }}
            onClick={() => setView("signin")}
          >
            Sign In — I already have an account
          </button>
        </div>
      </div>
    );
  }

  // --- Sign In screen ---
  if (view === "signin") {
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

          <div className="auth-title">Sign in</div>
          <div className="auth-hint">Enter your existing account credentials.</div>

          {signInError && <div className="error-banner">{signInError}</div>}

          <form
            onSubmit={(e) => {
              e.preventDefault();
              submitSignIn();
            }}
          >
            <div className="field">
              <label>Username</label>
              <input
                value={signInUser}
                onChange={(e) => setSignInUser(e.target.value)}
                autoFocus
                placeholder="e.g. andreah"
                autoComplete="username"
              />
            </div>
            <div className="field">
              <label>Password</label>
              <input
                type="password"
                value={signInPass}
                onChange={(e) => setSignInPass(e.target.value)}
                autoComplete="current-password"
                placeholder="••••••••"
              />
            </div>

            <button
              className="primary"
              style={{ width: "100%", padding: "11px" }}
              type="submit"
              disabled={signInBusy || !signInUser || !signInPass}
            >
              {signInBusy ? "Signing in…" : "Sign in"}
            </button>
          </form>

          <div style={{ textAlign: "center", marginTop: 16 }}>
            <button
              className="ghost small"
              onClick={() => { setView("choice"); setSignInError(null); }}
              disabled={signInBusy}
            >
              Back
            </button>
          </div>
        </div>
      </div>
    );
  }

  // --- Sign Up screen ---
  if (created) {
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

          <div className="auth-title">Admin account created!</div>

          {created.recoveryCode && (
            <div className="auth-hint">
              Save this recovery code somewhere safe. You'll need it if no other admin can reset your password.
            </div>
          )}

          {created.recoveryCode && (
            <div
              className="stack"
              style={{
                background: "var(--surface-2)",
                borderRadius: "var(--radius)",
                padding: 14,
                margin: "16px 0",
                fontFamily: "monospace",
                fontSize: 16,
                textAlign: "center",
                letterSpacing: "0.05em",
                userSelect: "all",
              }}
            >
              {created.recoveryCode}
            </div>
          )}

          {created.recoveryCode && (
            <button
              className="ghost"
              style={{ width: "100%", marginBottom: 10 }}
              onClick={copyCode}
            >
              {copied ? "Copied!" : "Copy to clipboard"}
            </button>
          )}

          {created.filePath && (
            <p className="muted small" style={{ margin: "0 0 16px" }}>
              Also saved to: <br />
              <span style={{ fontFamily: "monospace", fontSize: 11 }}>{created.filePath}</span>
            </p>
          )}

          <button
            className="primary"
            style={{ width: "100%", padding: "11px" }}
            onClick={() => onDone(created.user)}
          >
            Continue to TruckFlow
          </button>
        </div>
      </div>
    );
  }

  // --- Sign Up form ---
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

          <div className="auth-title">Set up your account</div>
          <div className="auth-hint">
            This creates your account and configures the system.
          </div>

        {error && <div className="error-banner">{error}</div>}

        <form
          onSubmit={(e) => {
            e.preventDefault();
            submitSignUp();
          }}
        >
          <div className="field">
            <label>Company name</label>
            <input
              value={companyName}
              onChange={(e) => setCompanyName(e.target.value)}
              placeholder="e.g. Acme Exhauster Services"
              autoFocus
            />
          </div>
          <div className="field">
            <label>Username</label>
            <input
              value={adminName}
              onChange={(e) => setAdminName(e.target.value)}
              placeholder="e.g. andreah"
              autoComplete="username"
            />
          </div>
          <div className="field">
            <label>Password</label>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="new-password"
              placeholder="••••••••"
            />
            <PasswordChecklist password={password} />
          </div>
          <div className="field">
            <label>Confirm password</label>
            <input
              type="password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              autoComplete="new-password"
              placeholder="••••••••"
            />
          </div>

          <button
            className="primary"
            style={{ width: "100%", padding: "11px" }}
            type="submit"
            disabled={
              busy ||
              !companyName.trim() ||
              !adminName.trim() ||
              password.length < 8 ||
              password !== confirm
            }
          >
            {busy ? "Setting up…" : "Create account"}
          </button>
        </form>

        <div style={{ textAlign: "center", marginTop: 16 }}>
          <button
            className="ghost small"
            onClick={() => setView("choice")}
            disabled={busy}
          >
            Back
          </button>
        </div>
      </div>
    </div>
  );
}
