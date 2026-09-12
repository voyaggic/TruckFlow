import { useEffect, useState } from "react";
import { api } from "../lib/api";
import type { SessionUser } from "../lib/types";
import PasswordChecklist from "./PasswordChecklist";

export const SAVED_KEY = "tf.saved-login";
const LOGGED_OUT_KEY = "tf.logged-out";

interface SavedLogin {
  username: string;
  password: string;
  supabaseUrl?: string;
  apiKey?: string;
}

function loadSaved(): SavedLogin | null {
  try {
    const raw = localStorage.getItem(SAVED_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as SavedLogin;
    if (parsed && typeof parsed.username === "string" && typeof parsed.password === "string") {
      return parsed;
    }
    return null;
  } catch {
    return null;
  }
}

function saveLogin(username: string, password: string, supabaseUrl?: string, apiKey?: string) {
  try {
    localStorage.setItem(SAVED_KEY, JSON.stringify({ username, password, supabaseUrl, apiKey }));
  } catch {
    /* storage unavailable — ignore */
  }
}

// Refresh the stored sign-in after a forced password change, so "Keep me
// signed in" keeps working with the new password.
export function updateSavedLogin(username: string, password: string) {
  saveLogin(username, password);
}

// Auto sign-in only applies when the user did NOT log out last. A manual log
// out sets this flag so the next launch stays on the sign-in screen; signing in
// again clears it, so future launches resume auto sign-in.
export function markLoggedOut() {
  try {
    localStorage.setItem(LOGGED_OUT_KEY, "1");
  } catch {
    /* ignore */
  }
}

function clearLoggedOut() {
  try {
    localStorage.removeItem(LOGGED_OUT_KEY);
  } catch {
    /* ignore */
  }
}

export default function LoginScreen({
  onLogin,
}: {
  onLogin: (user: SessionUser) => void;
}) {
  const [mode, setMode] = useState<"login" | "signup">("login");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [remember, setRemember] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [auto, setAuto] = useState(false);

  // Supabase connection fields (shared between login and signup)
  const [supabaseUrl, setSupabaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");

  // Signup fields
  const [companyName, setCompanyName] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");

  // Prefill from the saved sign-in. Auto sign-in only when the user did not
  // manually log out (a manual log out must stick until they sign in again).
  useEffect(() => {
    const saved = loadSaved();
    if (!saved) return;

    const loggedOut = (() => {
      try {
        return localStorage.getItem("tf.logged-out") === "1";
      } catch {
        return false;
      }
    })();

    // Always prefill fields regardless of loggedOut status
    setUsername(saved.username);
    setPassword(saved.password);
    if (saved.supabaseUrl) setSupabaseUrl(saved.supabaseUrl);
    if (saved.apiKey) setApiKey(saved.apiKey);

    if (loggedOut) return;
    setAuto(true);
    setBusy(true);
    api
      .loginPassword(saved.username, saved.password, saved.supabaseUrl || "", saved.apiKey || "")
      .then((res) => onLogin(res.user))
      .catch((e) => {
        setError(String(e));
        setAuto(false);
        setBusy(false);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const submit = async () => {
    setError(null);
    setBusy(true);
    try {
      if (!username.trim()) {
        setError("Username is required.");
        setBusy(false);
        return;
      }
      if (!password) {
        setError("Password is required.");
        setBusy(false);
        return;
      }
      if (!supabaseUrl.trim()) {
        setError("Supabase URL is required.");
        setBusy(false);
        return;
      }
      if (!apiKey.trim()) {
        setError("API Key is required.");
        setBusy(false);
        return;
      }

      const res = await api.loginPassword(username.trim(), password, supabaseUrl.trim(), apiKey.trim());
      // Always save supabase credentials - needed for cloud auth
      // Only save password if "remember me" is checked
      if (remember) {
        saveLogin(username.trim(), password, supabaseUrl.trim(), apiKey.trim());
      } else {
        saveLogin(username.trim(), "", supabaseUrl.trim(), apiKey.trim());
      }
      clearLoggedOut();
      onLogin(res.user);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const submitSignup = async () => {
    setError(null);
    if (!companyName.trim()) {
      setError("Company name is required.");
      return;
    }
    if (!username.trim()) {
      setError("Username is required.");
      return;
    }
    if (password.length < 8) {
      setError("Password must be at least 8 characters.");
      return;
    }
    if (password !== confirmPassword) {
      setError("Passwords do not match.");
      return;
    }

    setBusy(true);
    try {
      // Local-only signup — cloud connection is configured later via the Sync panel
      const res = await api.createCompanyAndAdmin(companyName.trim(), username.trim(), password);
      if (remember) {
        saveLogin(username.trim(), password);
      }
      clearLoggedOut();
      onLogin(res.user);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{
      position: "fixed",
      inset: 0,
      display: "flex",
      alignItems: "center",
      justifyContent: "center",
      background: "var(--bg)",
    }}>
      <div className="auth-card" style={{ width: 360 }}>
        <div className="brand">
          <div className="brand-mark">TF</div>
          <div>
            <div className="brand-name">TruckFlow</div>
            <div className="brand-sub">Gate trip management</div>
          </div>
        </div>

        {error && <div className="error-banner">{error}</div>}

        {/* Mode switcher */}
        <div style={{ display: "flex", marginBottom: 16, gap: 4, background: "var(--bg-secondary)", borderRadius: "var(--radius)", padding: 4 }}>
          <button
            style={{
              flex: 1,
              padding: "8px 12px",
              border: "none",
              borderRadius: "calc(var(--radius) - 2px)",
              cursor: "pointer",
              fontWeight: mode === "login" ? 600 : 400,
              background: mode === "login" ? "var(--bg-primary)" : "transparent",
              color: mode === "login" ? "var(--text)" : "var(--muted)",
            }}
            onClick={() => { setMode("login"); setError(null); }}
            disabled={busy}
          >
            Login
          </button>
          <button
            style={{
              flex: 1,
              padding: "8px 12px",
              border: "none",
              borderRadius: "calc(var(--radius) - 2px)",
              cursor: "pointer",
              fontWeight: mode === "signup" ? 600 : 400,
              background: mode === "signup" ? "var(--bg-primary)" : "transparent",
              color: mode === "signup" ? "var(--text)" : "var(--muted)",
            }}
            onClick={() => { setMode("signup"); setError(null); }}
            disabled={busy}
          >
            Sign Up
          </button>
        </div>

        {mode === "login" ? (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              submit();
            }}
          >
            <div className="auth-title">{auto ? "Signing you in…" : "Login"}</div>

            <div className="field">
              <label>Username</label>
              <input
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                autoFocus={!auto}
                placeholder="Enter username"
                autoComplete="username"
              />
            </div>
            <div className="field">
              <label>Password</label>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoComplete="current-password"
                placeholder="Enter password"
              />
            </div>

            {/* Supabase fields - always visible on login */}
            <div style={{ marginTop: 12, padding: 12, background: "var(--bg-secondary)", borderRadius: "var(--radius)" }}>
              <div className="small muted" style={{ marginBottom: 8, fontWeight: 600 }}>
                Supabase Connection
              </div>
              <div className="field" style={{ marginBottom: 8 }}>
                <label>Supabase URL</label>
                <input
                  value={supabaseUrl}
                  onChange={(e) => setSupabaseUrl(e.target.value)}
                  placeholder="https://xxx.supabase.co"
                  autoComplete="off"
                />
              </div>
              <div className="field" style={{ marginBottom: 0 }}>
                <label>API Key</label>
                <input
                  value={apiKey}
                  onChange={(e) => setApiKey(e.target.value)}
                  placeholder="eyJhbGciOiJIUzI1NiIs..."
                  autoComplete="off"
                  type="password"
                />
              </div>
            </div>

            <label className="row" style={{ gap: 8, alignItems: "center", cursor: "pointer", marginBottom: 14, marginTop: 14 }}>
              <input
                type="checkbox"
                style={{ width: "auto" }}
                checked={remember}
                onChange={(e) => setRemember(e.target.checked)}
              />
              <span className="small">Keep me signed in</span>
            </label>

            <button className="primary" style={{ width: "100%", padding: "11px" }} type="submit" disabled={busy}>
              {busy ? "Signing in…" : "Sign in"}
            </button>
          </form>
        ) : (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              submitSignup();
            }}
          >
            <div className="auth-title">Create Account</div>

            <div className="field">
              <label>Company Name</label>
              <input
                value={companyName}
                onChange={(e) => setCompanyName(e.target.value)}
                placeholder="Enter company name"
                autoComplete="off"
              />
            </div>
            <div className="field">
              <label>Username</label>
              <input
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                placeholder="Choose username"
                autoComplete="username"
              />
            </div>
            <div className="field">
              <label>Password</label>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="Min 8 characters"
                autoComplete="new-password"
              />
              <PasswordChecklist password={password} />
            </div>
            <div className="field">
              <label>Confirm Password</label>
              <input
                type="password"
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
                placeholder="Re-enter password"
                autoComplete="new-password"
              />
            </div>
            <p className="small muted" style={{ marginTop: 12 }}>
              Cloud connection is configured later in the Sync panel.
            </p>

            <label className="row" style={{ gap: 8, alignItems: "center", cursor: "pointer", marginBottom: 14, marginTop: 14 }}>
              <input
                type="checkbox"
                style={{ width: "auto" }}
                checked={remember}
                onChange={(e) => setRemember(e.target.checked)}
              />
              <span className="small">Keep me signed in</span>
            </label>

            <button
              className="primary"
              style={{ width: "100%", padding: "11px" }}
              type="submit"
              disabled={busy || !companyName || !username || !password}
            >
              {busy ? "Creating account…" : "Create Account"}
            </button>
          </form>
        )}
      </div>
    </div>
  );
}
