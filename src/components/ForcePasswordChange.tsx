import { useState } from "react";
import { api } from "../lib/api";
import type { SessionUser } from "../lib/types";
import PasswordChecklist from "./PasswordChecklist";
import { updateSavedLogin } from "./LoginScreen";

/**
 * Shown instead of the app when an admin reset this account's password: the
 * user must choose their own new password before they can use the app.
 */
export default function ForcePasswordChange({
  user,
  onDone,
}: {
  user: SessionUser;
  onDone: () => void;
}) {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setError(null);
    if (next !== confirm) {
      setError("Passwords do not match.");
      return;
    }
    setBusy(true);
    try {
      await api.changeOwnCredential(user.id, current, next);
      // Keep "Keep me signed in" working with the new password.
      updateSavedLogin(user.name, next);
      onDone();
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
        <div className="auth-title">Set a new password</div>
        <div className="auth-hint">
          An admin reset your password. Choose a new one now — you'll use it for every future sign-in.
        </div>

        {error && <div className="error-banner">{error}</div>}

        <div className="field">
          <label>Current password (the one the admin gave you)</label>
          <input type="password" value={current} onChange={(e) => setCurrent(e.target.value)} autoFocus />
        </div>

        <div className="field">
          <label>New password</label>
          <input type="password" value={next} onChange={(e) => setNext(e.target.value)} />
          <PasswordChecklist password={next} />
        </div>

        <div className="field">
          <label>Confirm new password</label>
          <input type="password" value={confirm} onChange={(e) => setConfirm(e.target.value)} />
        </div>

        <button
          className="primary"
          style={{ width: "100%", padding: "11px" }}
          onClick={submit}
          disabled={busy || !current || !next || next !== confirm}
        >
          {busy ? "Updating…" : "Set new password"}
        </button>
      </div>
    </div>
  );
}
