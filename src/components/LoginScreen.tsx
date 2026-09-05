import { useEffect, useState } from "react";
import { api } from "../lib/api";
import type { SessionUser } from "../lib/types";
import PasswordChecklist from "./PasswordChecklist";

export const SAVED_KEY = "tf.saved-login";
const LOGGED_OUT_KEY = "tf.logged-out";

interface SavedLogin {
  username: string;
  password: string;
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

function saveLogin(username: string, password: string) {
  try {
    localStorage.setItem(SAVED_KEY, JSON.stringify({ username, password }));
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
  const [showSignup, setShowSignup] = useState(false);
  const [codeMode, setCodeMode] = useState(false);
  const [reqMode, setReqMode] = useState(false);
  const [codeUser, setCodeUser] = useState("");
  const [code, setCode] = useState("");
  const [codeOk, setCodeOk] = useState(false);
  const [codePass, setCodePass] = useState("");
  const [codeConfirm, setCodeConfirm] = useState("");
  const [reqUser, setReqUser] = useState("");
  const [reqSent, setReqSent] = useState(false);

  // Prefill from the saved sign-in. Auto sign-in only when the user did not
  // manually log out (a manual log out must stick until they sign in again).
  useEffect(() => {
    const saved = loadSaved();
    if (!saved) return;
    setUsername(saved.username);
    setPassword(saved.password);
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
      .loginPassword(saved.username, saved.password)
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
      const res = await api.loginPassword(username, password);
      if (remember) {
        saveLogin(username.trim(), password);
      } else {
        clearSaved();
      }
      clearLoggedOut(); // a manual sign-in resumes auto sign-in on future launches
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

        <div className="auth-title">{auto ? "Signing you in…" : "Sign in"}</div>
        <div className="auth-hint">
          {auto ? (
            <>Using your saved sign-in for <b>{username}</b>. No need to type anything.</>
          ) : (
            "Every account signs in with a username and password."
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

          <label className="row" style={{ gap: 8, alignItems: "center", cursor: "pointer", marginBottom: 14 }}>
            <input
              type="checkbox"
              style={{ width: "auto" }}
              checked={remember}
              onChange={(e) => setRemember(e.target.checked)}
            />
            <span className="small">Keep me signed in — no typing next time</span>
          </label>

          <button className="primary" style={{ width: "100%", padding: "11px" }} type="submit" disabled={busy || !username || !password}>
            {busy ? "Signing in…" : "Sign in"}
          </button>
        </form>

        <div style={{ display: "flex", justifyContent: "space-between", marginTop: 16 }}>
          <button
            className="ghost small"
            onClick={() => setShowSignup(true)}
            disabled={busy}
          >
            Sign up
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

        {showSignup && (
          <div
            className="stack"
            style={{ marginTop: 12, border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}
          >
            <div className="section-title" style={{ fontSize: 14 }}>
              Create an account
            </div>
            <p className="muted small">
              Contact your administrator to create your account. Once created, you'll receive your login credentials.
            </p>
            <div className="row">
              <button className="ghost" onClick={() => setShowSignup(false)}>
                Back to sign in
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
