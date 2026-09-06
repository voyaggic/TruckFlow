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

function clearSaved() {
  try {
    localStorage.removeItem(SAVED_KEY);
  } catch {
    /* ignore */
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
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [remember, setRemember] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [auto, setAuto] = useState(false);
  const [showForgot, setShowForgot] = useState(false);
  const [codeMode, setCodeMode] = useState(false);
  const [reqMode, setReqMode] = useState(false);
  const [codeUser, setCodeUser] = useState("");
  const [code, setCode] = useState("");
  const [codeOk, setCodeOk] = useState(false);
  const [codePass, setCodePass] = useState("");
  const [codeConfirm, setCodeConfirm] = useState("");
  const [reqUser, setReqUser] = useState("");
  const [reqSent, setReqSent] = useState(false);
  
  // Cloud auth state
  const [activeTab, setActiveTab] = useState<"login" | "setup">("login");
  const [supabaseUrl, setSupabaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [companyName, setCompanyName] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [needsCredentials, setNeedsCredentials] = useState(false);

  // Prefill from the saved sign-in. Auto sign-in only when the user did not
  // manually log out (a manual log out must stick until they sign in again).
  useEffect(() => {
    const saved = loadSaved();
    if (!saved) return;
    setUsername(saved.username);
    setPassword(saved.password);
    if (saved.supabaseUrl) setSupabaseUrl(saved.supabaseUrl);
    if (saved.apiKey) setApiKey(saved.apiKey);
    
    const loggedOut = (() => {
      try {
        return localStorage.getItem("tf.logged-out") === "1";
      } catch {
        return false;
      }
    })();
    if (loggedOut) return; // stay on the sign-in screen, just prefilled
    setAuto(true);
    setBusy(true);
    api
      .loginPassword(saved.username, saved.password, saved.supabaseUrl || "", saved.apiKey || "")
      .then((res) => onLogin(res.user))
      .catch((e) => {
        setError(String(e));
        setAuto(false);
        setBusy(false);
        // If auth failed due to connection issue, show credentials fields
        if (String(e).includes("connection") || String(e).includes("Supabase")) {
          setNeedsCredentials(true);
        }
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const submit = async () => {
    setError(null);
    setBusy(true);
    try {
      const res = await api.loginPassword(username.trim(), password, supabaseUrl.trim(), apiKey.trim());
      if (remember) {
        saveLogin(username.trim(), password, supabaseUrl.trim(), apiKey.trim());
      } else {
        clearSaved();
      }
      clearLoggedOut(); // a manual sign-in resumes auto sign-in on future launches
      onLogin(res.user);
    } catch (e) {
      setError(String(e));
      // If auth failed due to connection issue, prompt for credentials
      if (String(e).includes("connection") || String(e).includes("Supabase") || String(e).includes("no account")) {
        setNeedsCredentials(true);
      }
    } finally {
      setBusy(false);
    }
  };

  const submitSetup = async () => {
    setError(null);
    
    if (!companyName.trim()) {
      setError("Company name is required.");
      return;
    }
    if (!username.trim()) {
      setError("Admin username is required.");
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
    if (!supabaseUrl.trim()) {
      setError("Supabase URL is required.");
      return;
    }
    if (!apiKey.trim()) {
      setError("API Key is required.");
      return;
    }
    
    setBusy(true);
    try {
      const res = await api.createCompanyAndAdminCloud(
        companyName.trim(),
        username.trim(),
        password,
        supabaseUrl.trim(),
        apiKey.trim()
      );
      if (remember) {
        saveLogin(username.trim(), password, supabaseUrl.trim(), apiKey.trim());
      } else {
        clearSaved();
      }
      clearLoggedOut();
      onLogin(res.user);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const checkCode = async () => {
    setError(null);
    setBusy(true);
    try {
      await api.checkRecoveryCode(codeUser, code);
      setCodeOk(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const submitCode = async () => {
    setError(null);
    if (codePass !== codeConfirm) {
      setError("Passwords do not match.");
      return;
    }
    setBusy(true);
    try {
      const res = await api.recoverAdminPassword(codeUser, code, codePass);
      saveLogin(codeUser.trim(), codePass);
      clearLoggedOut();
      onLogin(res.user);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const submitRequest = async () => {
    setError(null);
    setBusy(true);
    try {
      await api.createPasswordResetRequest(reqUser);
      setReqSent(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
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

        {/* Tab switcher */}
        <div style={{ display: "flex", marginBottom: 16, gap: 4, background: "var(--bg-secondary)", borderRadius: "var(--radius)", padding: 4 }}>
          <button
            style={{
              flex: 1,
              padding: "8px 12px",
              border: "none",
              borderRadius: "calc(var(--radius) - 2px)",
              cursor: "pointer",
              fontWeight: activeTab === "login" ? 600 : 400,
              background: activeTab === "login" ? "var(--bg-primary)" : "transparent",
              color: activeTab === "login" ? "var(--text)" : "var(--muted)",
              transition: "all 0.15s",
            }}
            onClick={() => setActiveTab("login")}
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
              fontWeight: activeTab === "setup" ? 600 : 400,
              background: activeTab === "setup" ? "var(--bg-primary)" : "transparent",
              color: activeTab === "setup" ? "var(--text)" : "var(--muted)",
              transition: "all 0.15s",
            }}
            onClick={() => setActiveTab("setup")}
            disabled={busy}
          >
            First Time Setup
          </button>
        </div>

        {activeTab === "login" ? (
          /* LOGIN TAB */
          <>
            <div className="auth-title">{auto ? "Signing you in…" : "Sign in"}</div>
            <div className="auth-hint">
              {auto ? (
                <>Using your saved sign-in for <b>{username}</b>. No need to type anything.</>
              ) : (
                "Enter your credentials to sign in."
              )}
            </div>

            {error && <div className="error-banner">{error}</div>}

            <form
              onSubmit={(e) => {
                e.preventDefault();
                submit();
              }}
            >
              <div className="field">
                <label>Username</label>
                <input
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  autoFocus={!auto}
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
                  autoComplete="current-password"
                  placeholder="••••••••"
                />
              </div>

              {/* Toggle to show Supabase credentials */}
              {!needsCredentials && !supabaseUrl && !apiKey && (
                <button
                  type="button"
                  className="small muted"
                  style={{ background: "none", border: "none", cursor: "pointer", textDecoration: "underline", marginTop: 8 }}
                  onClick={() => setNeedsCredentials(true)}
                >
                  + Connect to Supabase (first time setup)
                </button>
              )}

              {/* Supabase credentials - shown when needed */}
              {(needsCredentials || supabaseUrl || apiKey) && (
                <div style={{ marginTop: 12, padding: 12, background: "var(--bg-secondary)", borderRadius: "var(--radius)" }}>
                  <div className="small muted" style={{ marginBottom: 8 }}>
                    Supabase Connection (first time or new device)
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
                      placeholder="Service role key"
                      autoComplete="off"
                      type="password"
                    />
                  </div>
                </div>
              )}

              <label className="row" style={{ gap: 8, alignItems: "center", cursor: "pointer", marginBottom: 14, marginTop: 14 }}>
                <input
                  type="checkbox"
                  style={{ width: "auto" }}
                  checked={remember}
                  onChange={(e) => setRemember(e.target.checked)}
                />
                <span className="small">Keep me signed in</span>
              </label>

              <button className="primary" style={{ width: "100%", padding: "11px" }} type="submit" disabled={busy || !username || !password}>
                {busy ? "Signing in…" : "Sign in"}
              </button>
            </form>

            <div style={{ display: "flex", justifyContent: "space-between", marginTop: 16 }}>
              <button
                className="ghost small"
                onClick={() => setNeedsCredentials(true)}
                disabled={busy}
              >
                Change connection
              </button>
              <button
                className="ghost small"
                onClick={() => {
                  setShowForgot((f) => !f);
                  setCodeMode(false);
                  setReqMode(false);
                  setReqSent(false);
                  setCodeOk(false);
                }}
                disabled={busy}
              >
                {showForgot ? "Back to sign in" : "Forgot password?"}
              </button>
            </div>

            {showForgot && (
              <div
                className="stack"
                style={{ marginTop: 12, border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}
              >
                {!codeMode && !reqMode && (
                  <>
                    <div className="muted small" style={{ marginBottom: 10 }}>
                      Recover your account:
                    </div>
                    <div className="row" style={{ gap: 10 }}>
                      <button className="ghost" style={{ flex: 1 }} onClick={() => setCodeMode(true)}>
                        I have a recovery code
                      </button>
                      <button className="ghost" style={{ flex: 1 }} onClick={() => setReqMode(true)}>
                        Request password reset
                      </button>
                    </div>
                  </>
                )}

                {codeMode && (
                  <>
                    <div className="section-title" style={{ fontSize: 14 }}>
                      Enter recovery code
                    </div>
                    {!codeOk ? (
                      <>
                        <p className="muted small">
                          Enter your username and the recovery code you saved.
                        </p>
                        <div className="field">
                          <label>Username</label>
                          <input value={codeUser} onChange={(e) => setCodeUser(e.target.value)} placeholder="e.g. andreah" />
                        </div>
                        <div className="field">
                          <label>Recovery code</label>
                          <input value={code} onChange={(e) => setCode(e.target.value)} placeholder="XXXXX-XXXXX" />
                        </div>
                        <div className="row">
                          <button className="primary" onClick={checkCode} disabled={busy || !codeUser || !code}>
                            {busy ? "Checking…" : "Check code"}
                          </button>
                          <button className="ghost" onClick={() => setCodeMode(false)}>
                            Back
                          </button>
                        </div>
                      </>
                    ) : (
                      <>
                        <p className="muted small">Code accepted. Set your new password.</p>
                        <div className="field">
                          <label>New password</label>
                          <input type="password" value={codePass} onChange={(e) => setCodePass(e.target.value)} />
                          <PasswordChecklist password={codePass} />
                        </div>
                        <div className="field">
                          <label>Confirm new password</label>
                          <input type="password" value={codeConfirm} onChange={(e) => setCodeConfirm(e.target.value)} />
                        </div>
                        <div className="row">
                          <button className="primary" onClick={submitCode} disabled={busy || !codePass || codePass !== codeConfirm}>
                            {busy ? "Saving…" : "Set password"}
                          </button>
                        </div>
                      </>
                    )}
                  </>
                )}

                {reqMode && (
                  <>
                    <div className="section-title" style={{ fontSize: 14 }}>
                      Request password reset
                    </div>
                    {reqSent ? (
                      <p className="muted small">
                        Your request has been sent. An administrator will review it and reset your password. You'll be able to sign in once it's been approved.
                      </p>
                    ) : (
                      <>
                        <p className="muted small">
                          Enter your username to request a password reset.
                        </p>
                        <div className="field">
                          <label>Username</label>
                          <input value={reqUser} onChange={(e) => setReqUser(e.target.value)} placeholder="e.g. peter" />
                        </div>
                        <div className="row">
                          <button className="primary" onClick={submitRequest} disabled={busy || !reqUser}>
                            {busy ? "Sending…" : "Send request"}
                          </button>
                          <button className="ghost" onClick={() => setReqMode(false)}>
                            Back
                          </button>
                        </div>
                      </>
                    )}
                  </>
                )}
              </div>
            )}
          </>
        ) : (
          /* FIRST TIME SETUP TAB */
          <>
            <div className="auth-title">Create Company & Admin</div>
            <div className="auth-hint">
              Set up your company and admin account. This connects your app to Supabase.
            </div>

            {error && <div className="error-banner">{error}</div>}

            <form
              onSubmit={(e) => {
                e.preventDefault();
                submitSetup();
              }}
            >
              <div className="field">
                <label>Company Name</label>
                <input
                  value={companyName}
                  onChange={(e) => setCompanyName(e.target.value)}
                  placeholder="My Transport Company"
                  autoComplete="organization"
                />
              </div>
              <div className="field">
                <label>Admin Username</label>
                <input
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  placeholder="admin"
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

              <div style={{ marginTop: 16, marginBottom: 8 }}>
                <div className="small muted" style={{ marginBottom: 8 }}>Supabase Connection</div>
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
                  <label>Service Role Key</label>
                  <input
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                    placeholder="eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
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

              <button className="primary" style={{ width: "100%", padding: "11px" }} type="submit" disabled={busy || !companyName || !username || !password || !supabaseUrl || !apiKey}>
                {busy ? "Setting up…" : "Create Company & Admin"}
              </button>
            </form>
          </>
        )}
      </div>
    </div>
  );
}
