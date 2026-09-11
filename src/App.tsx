import { useEffect, useState } from "react";
import { api } from "./lib/api";
import type { AppStatus, SessionUser } from "./lib/types";
import type { ThemeMode } from "./lib/theme";
import { applyTheme } from "./lib/theme";
import FirstRunAdmin from "./components/FirstRunAdmin";
import ForcePasswordChange from "./components/ForcePasswordChange";
import LoginScreen, { markLoggedOut } from "./components/LoginScreen";
import Shell from "./components/Shell";

export default function App() {
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    // Default appearance (light theme, blue accent) applies from the very first
    // paint — including the login / first-run screens — before any user
    // preferences are known. Saved preferences override it once signed in.
    applyTheme("light", "blue");
    api
      .appStatus()
      .then(setStatus)
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    if (status?.current_user) {
      const { theme_mode, theme_accent } = status.current_user;
      applyTheme((theme_mode as ThemeMode) ?? "light", theme_accent ?? "blue");
    }
  }, [status?.current_user]);

  const handleLogin = (user: SessionUser) => {
    setStatus({ needs_first_run: false, current_user: user });
  };

  // Keep the session user in sync when the user saves appearance settings, so
  // every tab/section (and the next sign-in) sees the saved theme.
  const handleThemeChanged = (themeMode: string, themeAccent: string) => {
    setStatus((s) =>
      s?.current_user
        ? { ...s, current_user: { ...s.current_user, theme_mode: themeMode, theme_accent: themeAccent } }
        : s,
    );
  };

  // After the user confirms a role change, reload the session user from the
  // backend so tabs / sections reflect the new permissions immediately.
  const handlePermissionsApplied = () => {
    api
      .getCurrentUser()
      .then((u) => {
        if (u) setStatus((s) => (s ? { ...s, current_user: u } : s));
      })
      .catch(() => undefined);
  };

  const handleLogout = () => {
    api.logout().catch(() => undefined);
    markLoggedOut(); // a manual log out must stick — no auto sign-in on the next launch
    setStatus({ needs_first_run: false, current_user: null });
  };

  if (error) {
    return (
      <div className="auth-wrap">
        <div className="auth-card">
          <div className="error-banner">{error}</div>
          <p className="muted small">The app could not start. Check that the local database is available.</p>
        </div>
      </div>
    );
  }

  if (!status) {
    return (
      <div className="center-fill">
        <div className="spinner" />
      </div>
    );
  }

  if (status.needs_first_run) {
    return <FirstRunAdmin onDone={handleLogin} />;
  }

  if (!status.current_user) {
    return <LoginScreen onLogin={handleLogin} />;
  }

  // An admin reset this account's password — the user must choose a new one
  // before the app unlocks.
  if (status.current_user.must_change_password) {
    return (
      <ForcePasswordChange
        user={status.current_user}
        onDone={() => handlePermissionsApplied() /* reloads the user, flag now clear */}
      />
    );
  }

  return (
    <Shell
      user={status.current_user}
      onLogout={handleLogout}
      onThemeChanged={handleThemeChanged}
      onPermissionsApplied={handlePermissionsApplied}
    />
  );
}