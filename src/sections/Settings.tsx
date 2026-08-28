import { useState, useEffect } from "react";
import { getVersion, getTauriVersion } from "@tauri-apps/api/app";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { ACCENTS, applyTheme, ACCENT_KEYS } from "../lib/theme";
import type { AccentKey, ThemeMode } from "../lib/theme";
import type { RecoveryCodeInfo, SessionUser } from "../lib/types";
import PasswordChecklist from "../components/PasswordChecklist";
import { api } from "../lib/api";

type Section = "theme" | "credential" | "profile" | "about" | "recovery" | "storage";

export default function Settings({
  user,
  onThemeChanged,
}: {
  user: SessionUser;
  onThemeChanged?: (themeMode: string, themeAccent: string) => void;
}) {
  const [theme, setTheme] = useState<ThemeMode>((user.theme_mode as ThemeMode) ?? "light");
  const [accent, setAccent] = useState<string>(user.theme_accent ?? "blue");
  const [customAccent, setCustomAccent] = useState<string>(
    /^#[0-9a-fA-F]{6}$/.test(user.theme_accent ?? "") ? (user.theme_accent as string) : "#0f7de0",
  );
  const [themeDirty, setThemeDirty] = useState(false);
  const [savingTheme, setSavingTheme] = useState(false);
  const [section, setSection] = useState<Section>("theme");
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [recovery, setRecovery] = useState<RecoveryCodeInfo | null>(null);
  const [recoveryError, setRecoveryError] = useState<string | null>(null);
  const [recoveryVisible, setRecoveryVisible] = useState(false);

  const isAdmin = user.permissions.some((p) => p.key === "manage_users");

  // Draft edits preview immediately; only Save persists, so the choice survives
  // every session until it is changed and saved again.
  useEffect(() => {
    applyTheme(theme, accent);
  }, [theme, accent]);

  const isCustomAccent = !(ACCENT_KEYS as string[]).includes(accent);

  const handlePresetClick = (a: Exclude<AccentKey, "custom">) => {
    setAccent(a);
    setThemeDirty(true);
  };

  const handleCustomColorChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    setCustomAccent(e.target.value);
    setAccent(e.target.value);
    setThemeDirty(true);
  };

  const saveTheme = async () => {
    setError(null);
    setOk(null);
    setSavingTheme(true);
    try {
      await api.setUserTheme(user.id, theme, accent);
      setThemeDirty(false);
      onThemeChanged?.(theme, accent);
      setOk("Appearance saved — it will apply on every sign-in.");
      setTimeout(() => setOk(null), 3500);
    } catch (e) {
      setError(String(e));
    } finally {
      setSavingTheme(false);
    }
  };

  const resetTheme = () => {
    setTheme("light");
    setAccent("blue");
    setThemeDirty(true);
  };

  const switchSection = (s: Section) => {
    setSection(s);
    setError(null);
    setOk(null);
  };

  const changeCredential = async () => {
    setError(null);
    setOk(null);
    if (next !== confirm) {
      setError("New password does not match.");
      return;
    }
    setBusy(true);
    try {
      await api.changeOwnCredential(user.id, current, next);
      setOk("Password updated. Use it the next time you sign in.");
      setTimeout(() => setOk(null), 3500);
      setCurrent("");
      setNext("");
      setConfirm("");
    } catch (e) {
      setError(String(e));
      setTimeout(() => setError(null), 6000);
    } finally {
      setBusy(false);
    }
  };

  const loadRecovery = async () => {
    setRecoveryError(null);
    setOk(null);
    try {
      setRecovery(await api.getRecoveryCode(user.id));
    } catch (e) {
      setRecoveryError(String(e));
    }
  };

  const regenerate = async () => {
    if (!window.confirm("Generate a new recovery code? The old code stops working immediately.")) return;
    setRecoveryError(null);
    try {
      setRecovery(await api.regenerateRecoveryCode(user.id));
      setRecoveryVisible(true);
      setOk("New recovery code created and saved to the file.");
      setTimeout(() => setOk(null), 3500);
    } catch (e) {
      setRecoveryError(String(e));
    }
  };

  return (
    <div>
      <h2 className="section-title">Settings</h2>
      <p className="section-sub">Preferences that belong to you — admin controls live in the Admin tab.</p>

      <div className="seg" style={{ marginBottom: "18px", flexWrap: "wrap" }}>
        <button className={section === "theme" ? "active" : ""} onClick={() => switchSection("theme")}>
          Appearance
        </button>
        <button className={section === "credential" ? "active" : ""} onClick={() => switchSection("credential")}>
          Sign-in credential
        </button>
        <button className={section === "profile" ? "active" : ""} onClick={() => switchSection("profile")}>
          Profile
        </button>
        {isAdmin && (
          <button className={section === "recovery" ? "active" : ""} onClick={() => switchSection("recovery")}>
            Recovery code
          </button>
        )}
        <button className={section === "storage" ? "active" : ""} onClick={() => switchSection("storage")}>
          Storage
        </button>
        <button className={section === "about" ? "active" : ""} onClick={() => switchSection("about")}>
          About
        </button>
      </div>

      {section === "theme" && (
        <div className="card stack">
          {error && <div className="error-banner">{error}</div>}
          {ok && <div className="success-banner">{ok}</div>}
          <div className="field">
            <label>Theme</label>
            <div className="seg">
              {(["light", "dark", "system"] as ThemeMode[]).map((m) => (
                <button
                  key={m}
                  className={theme === m ? "active" : ""}
                  onClick={() => {
                    setTheme(m);
                    setThemeDirty(true);
                  }}
                >
                  {m[0].toUpperCase() + m.slice(1)}
                </button>
              ))}
            </div>
          </div>
          <div className="field">
            <label>Accent color</label>
            <div className="row" style={{ gap: 8, flexWrap: "wrap", alignItems: "center" }}>
              {ACCENT_KEYS.map((a) => (
                <button
                  key={a}
                  aria-label={a}
                  onClick={() => handlePresetClick(a)}
                  style={{
                    width: 30,
                    height: 30,
                    borderRadius: "50%",
                    border: accent === a ? "3px solid var(--text)" : "1px solid var(--border)",
                    background: ACCENTS[a],
                  }}
                />
              ))}
              <input
                type="color"
                value={customAccent}
                onChange={handleCustomColorChange}
                style={{ width: 36, height: 36, border: isCustomAccent ? "3px solid var(--text)" : "1px solid var(--border)", borderRadius: "6px", cursor: "pointer" }}
                title="Custom color"
              />
              <span className="muted small">Custom</span>
            </div>
          </div>
          <div className="row" style={{ gap: 10 }}>
            <button className="primary" onClick={saveTheme} disabled={savingTheme || !themeDirty}>
              {savingTheme ? "Saving…" : "Save changes"}
            </button>
            <button className="ghost" onClick={resetTheme}>
              Reset to defaults
            </button>
            {!themeDirty && <span className="muted small">All changes are saved.</span>}
          </div>
          <p className="muted small" style={{ marginBottom: 0 }}>
            Defaults are the light theme with a blue accent. Saving overrides the default for this account on every
            sign-in until you change it again.
          </p>
        </div>
      )}

      {section === "credential" && (
        <div className="card stack">
          <p className="muted small">
            Change your password. Enter your current password to verify your identity, then choose a new one.
          </p>
          {error && <div className="error-banner">{error}</div>}
          {ok && <div className="success-banner">{ok}</div>}
          <div className="row">
            <div className="field grow">
              <label>Current password</label>
              <input type="password" value={current} onChange={(e) => setCurrent(e.target.value)} />
            </div>
          </div>
          <div className="row">
            <div className="field grow">
              <label>New password</label>
              <input type="password" value={next} onChange={(e) => setNext(e.target.value)} />
              <PasswordChecklist password={next} />
            </div>
            <div className="field grow">
              <label>Confirm new password</label>
              <input type="password" value={confirm} onChange={(e) => setConfirm(e.target.value)} />
            </div>
          </div>
          <div>
            <button className="primary" onClick={changeCredential} disabled={busy || !current || !next || next !== confirm}>
              {busy ? "Updating…" : "Update password"}
            </button>
          </div>
        </div>
      )}

      {section === "profile" && (
        <div className="stack">
          {error && <div className="error-banner">{error}</div>}
          {ok && <div className="success-banner">{ok}</div>}
          <ProfileSettings user={user} onOk={setOk} onError={setError} />
        </div>
      )}

      {section === "storage" && <StorageSettings />}

      {section === "about" && <AboutPanel />}

      {section === "recovery" && isAdmin && (
        <div className="card stack">
          <p className="muted small">
            The recovery code is the escape hatch when no other admin can reset you. It is saved in a file on this
            computer — open it and copy the code when you need it. Only admins can see or change it.
          </p>
          {recoveryError && <div className="error-banner">{recoveryError}</div>}
          {ok && <div className="success-banner">{ok}</div>}
          {!recovery ? (
            <div>
              <button className="primary" onClick={loadRecovery}>
                Show recovery code
              </button>
            </div>
          ) : (
            <>
              <div className="field">
                <label>Recovery code</label>
                <div className="row" style={{ gap: 10, alignItems: "center" }}>
                  <code style={{ fontSize: 18, letterSpacing: 2 }}>
                    {recoveryVisible ? recovery.code : "•••••-•••••"}
                  </code>
                  <button className="ghost small" onClick={() => setRecoveryVisible((v) => !v)}>
                    {recoveryVisible ? "Hide" : "Reveal"}
                  </button>
                  <button
                    className="ghost small"
                    onClick={() => navigator.clipboard.writeText(recovery.code).catch(() => undefined)}
                  >
                    Copy
                  </button>
                </div>
              </div>
              <div className="field">
                <label>Saved in this file</label>
                <code className="small" style={{ wordBreak: "break-all" }}>
                  {recovery.file_path}
                </code>
              </div>
              <div className="row">
                <button className="danger" onClick={regenerate}>
                  Regenerate code
                </button>
              </div>
              <p className="muted small" style={{ marginBottom: 0 }}>
                Anyone with this code can reset an admin password — keep the file private. Regenerating makes the old
                code useless.
              </p>
            </>
          )}
        </div>
      )}
    </div>
  );
}

function ProfileSettings({ user, onOk, onError }: { user: SessionUser; onOk: (m: string) => void; onError: (m: string) => void }) {
  const [phone, setPhone] = useState(user.phone_number ?? "");
  const [lang, setLang] = useState(user.language_preference ?? "en");
  const [sound, setSound] = useState(user.notification_sound ?? true);
  const [photo, setPhoto] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api
      .getProfilePhoto(user.id)
      .then(setPhoto)
      .catch(() => undefined);
  }, [user.id]);

  const saveProfile = async () => {
    setBusy(true);
    try {
      await api.updateOwnProfile(user.id, phone.trim() || null, lang, sound);
      onOk("Profile saved.");
      setTimeout(() => onOk(""), 3500);
      setDirty(false);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const pickPhoto = (file: File | undefined) => {
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      const url = String(reader.result ?? "");
      const approxKb = Math.round(url.length * 0.75 / 1024);
      if (approxKb > 1500) {
        onError("Image is too large — keep it under ~1.5 MB.");
        return;
      }
      setPhoto(url);
      setDirty(true);
    };
    reader.readAsDataURL(file);
  };

  const savePhoto = async () => {
    setBusy(true);
    try {
      await api.setProfilePhoto(user.id, photo);
      onOk("Profile photo updated.");
      setTimeout(() => onOk(""), 3500);
      setDirty(false);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const removePhoto = async () => {
    setBusy(true);
    try {
      await api.setProfilePhoto(user.id, null);
      setPhoto(null);
      setDirty(false);
      onOk("Profile photo removed.");
      setTimeout(() => onOk(""), 3500);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="card stack">
      <div className="field">
        <label>Profile photo</label>
        <div className="row" style={{ gap: 14, alignItems: "center" }}>
          <div
            style={{
              width: 64,
              height: 64,
              borderRadius: "50%",
              background: "var(--surface-2)",
              border: "1px solid var(--border)",
              overflow: "hidden",
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              color: "var(--text-dim)",
              fontWeight: 700,
              fontSize: 22,
            }}
          >
            {photo ? (
              <img src={photo} alt="profile" style={{ width: "100%", height: "100%", objectFit: "cover" }} />
            ) : (
              initials(user.name)
            )}
          </div>
          <div className="stack" style={{ gap: 6 }}>
            <input
              type="file"
              accept="image/png,image/jpeg,image/webp"
              onChange={(e) => pickPhoto(e.target.files?.[0])}
              style={{ maxWidth: 260 }}
            />
            <div className="row" style={{ gap: 8 }}>
              <button className="primary" onClick={savePhoto} disabled={busy || !dirty || photo === null}>
                Save photo
              </button>
              <button className="ghost" onClick={removePhoto} disabled={busy || photo === null}>
                Remove
              </button>
            </div>
            <span className="muted small">Stored on this device only. Kept under 1.5 MB.</span>
          </div>
        </div>
      </div>

      <div className="row">
        <div className="field grow">
          <label>Contact phone</label>
          <input
            value={phone}
            onChange={(e) => {
              setPhone(e.target.value);
              setDirty(true);
            }}
            placeholder="+254 7XX XXX XXX"
          />
        </div>
        <div className="field">
          <label>Language</label>
          <select
            value={lang}
            onChange={(e) => {
              setLang(e.target.value);
              setDirty(true);
            }}
            style={{ minWidth: 180 }}
          >
            <option value="en">English</option>
            <option value="sw" disabled>
              Kiswahili — coming soon
            </option>
          </select>
        </div>
      </div>

      <div className="field">
        <label>Notifications</label>
        <label className="row" style={{ gap: 8, alignItems: "center", cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={sound}
            onChange={(e) => {
              setSound(e.target.checked);
              setDirty(true);
            }}
          />
          <span>Play a sound for new captures and queue items</span>
        </label>
      </div>

      <div>
        <button className="primary" onClick={saveProfile} disabled={busy || !dirty}>
          {busy ? "Saving…" : "Save profile"}
        </button>
      </div>
    </div>
  );
}

function AboutPanel() {
  const [version, setVersion] = useState<string>("…");
  const [tauriVersion, setTauriVersion] = useState<string>("…");
  const [checking, setChecking] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [updateMsg, setUpdateMsg] = useState<string | null>(null);
  const [updateErr, setUpdateErr] = useState<string | null>(null);

  useEffect(() => {
    getVersion().then(setVersion).catch(() => setVersion("—"));
    getTauriVersion().then(setTauriVersion).catch(() => setTauriVersion("—"));
  }, []);

  const runCheck = async () => {
    setChecking(true);
    setUpdateErr(null);
    setUpdateMsg(null);
    try {
      const update = await check();
      if (update?.available) {
        setUpdateMsg(`Version ${update.version} is available.`);
      } else {
        setUpdateMsg("You're on the latest version.");
      }
    } catch (e) {
      setUpdateErr(
        "No update server reachable right now. Run scripts/update-server.ps1 to test the updater against a local build."
      );
    } finally {
      setChecking(false);
    }
  };

  const runInstall = async () => {
    setInstalling(true);
    setUpdateErr(null);
    try {
      const update = await check();
      if (!update?.available) {
        setInstalling(false);
        return;
      }
      await update.downloadAndInstall();
      await relaunch();
    } catch (e) {
      setInstalling(false);
      setUpdateErr("Update failed to install: " + String(e));
    }
  };

  return (
    <div className="card stack">
      <div className="row" style={{ gap: 12, alignItems: "center" }}>
        <div className="brand-mark" style={{ width: 44, height: 44, fontSize: 18 }}>
          TF
        </div>
        <div>
          <div className="brand-name">TruckFlow</div>
          <div className="muted small">Gate trip management</div>
        </div>
      </div>
      <div className="entry-grid" style={{ marginTop: 10 }}>
        <div>
          <div className="muted small">App version</div>
          <div>{version}</div>
        </div>
        <div>
          <div className="muted small">Tauri runtime</div>
          <div>{tauriVersion}</div>
        </div>
        <div>
          <div className="muted small">Database</div>
          <div>SQLite (local)</div>
        </div>
        <div>
          <div className="muted small">Platform</div>
          <div>{navigator.platform || "Windows"}</div>
        </div>
      </div>
      <div>
        <div className="row" style={{ gap: 10, alignItems: "center" }}>
          <button className="primary" onClick={runCheck} disabled={checking || installing}>
            {checking ? "Checking…" : "Check for updates"}
          </button>
          {updateMsg && updateMsg.includes("available") && (
            <button className="primary" onClick={runInstall} disabled={installing}>
              {installing ? "Installing…" : "Download & install"}
            </button>
          )}
        </div>
        {updateMsg && <p className="muted small">{updateMsg}</p>}
        {updateErr && <p className="error-banner" style={{ margin: "8px 0 0" }}>{updateErr}</p>}
      </div>
      <p className="muted small" style={{ marginBottom: 0 }}>
        Updates are verified against a signing key and distributed through the in-app updater when one is published.
      </p>
    </div>
  );
}

function initials(name: string): string {
  return name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => w[0].toUpperCase())
    .join("");
}

function StorageSettings() {
  const [framesDir, setFramesDir] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    api.getFramesDir().then(setFramesDir).catch(() => {}).finally(() => setLoading(false));
  }, []);

  const handlePickFolder = async () => {
    try {
      const folder = await api.pickFolder();
      if (folder) {
        setFramesDir(folder);
        setMsg(null);
        setErr(null);
      }
    } catch (e) {
      setErr(String(e));
    }
  };

  const handleSave = async () => {
    if (!framesDir.trim()) {
      setErr("Path cannot be empty.");
      return;
    }
    setSaving(true);
    setErr(null);
    setMsg(null);
    try {
      await api.setFramesDir(framesDir.trim());
      setMsg("Storage path saved. Restart the app for the change to take full effect.");
      setTimeout(() => setMsg(null), 4000);
    } catch (e) {
      setErr(String(e));
      setTimeout(() => setErr(null), 6000);
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return <div className="card"><div className="center-fill"><div className="spinner" /></div></div>;
  }

  return (
    <div className="card stack">
      <div style={{ fontWeight: 600, fontSize: 15 }}>Data Storage</div>
      <p className="muted small">
        Choose where captured frames, evidence photos, and training data are saved on this computer.
      </p>

      {err && <div className="error-banner">{err}</div>}
      {msg && <div className="success-banner">{msg}</div>}

      <div className="field">
        <label>Frames & Evidence Directory</label>
        <div className="row" style={{ gap: 8, alignItems: "flex-end" }}>
          <input
            style={{ flex: 1, fontFamily: "monospace", fontSize: 13 }}
            value={framesDir}
            onChange={(e) => { setFramesDir(e.target.value); setMsg(null); setErr(null); }}
            placeholder="C:\Users\...\frames"
          />
          <button className="ghost" onClick={handlePickFolder}>Browse...</button>
        </div>
      </div>

      <p className="muted small" style={{ marginBottom: 0 }}>
        All trip photos, plate crops, and training candidate images are stored under this directory. Changing the path moves future saves; existing files stay at the old location.
      </p>

      <button className="primary" onClick={handleSave} disabled={saving}>
        {saving ? "Saving..." : "Save"}
      </button>
    </div>
  );
}
