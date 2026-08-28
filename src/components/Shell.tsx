import { memo, useEffect, useState } from "react";
import type { SessionUser } from "../lib/types";
import { api } from "../lib/api";
import GateOfficer from "../sections/GateOfficer";
import Reporting from "../sections/Reporting";
import AdminPanel from "../sections/AdminPanel";
import SystemMonitor from "../sections/SystemMonitor";
import Settings from "../sections/Settings";
import AnprConfig from "../sections/AnprConfig";
import PendingUpgradeBanner from "./PendingUpgradeBanner";

const PERM = {
  gateEntries: "view_gate_entries",
  queue: "resolve_queue",
  reporting: "view_reporting_dashboard",
  systemHealth: "view_system_health",
  manageUsers: "manage_users",
  manageReference: "manage_reference_database",
  manageIntegrations: "manage_integrations",
  manageAnprConfig: "manage_anpr_config",
  viewAudit: "view_audit_log",
  registerNewVehicle: "register_new_vehicle",
  exportReporting: "export_reporting",
  editTrip: "edit_trip",
};

export function hasPerm(user: SessionUser, key: string): boolean {
  return user.permissions.some((p) => p.key === key);
}

export type TabId = "gate" | "reporting" | "admin" | "monitor" | "anpr" | "settings";

interface TabDef {
  id: TabId;
  label: string;
}

export default function Shell({
  user,
  onLogout,
  onThemeChanged,
  onPermissionsApplied,
}: {
  user: SessionUser;
  onLogout: () => void;
  onThemeChanged?: (themeMode: string, themeAccent: string) => void;
  onPermissionsApplied?: () => void;
}) {
  const [photo, setPhoto] = useState<string | null>(null);
  const [tab, setTab] = useState<TabId>(() => {
    if (hasPerm(user, PERM.gateEntries)) return "gate";
    if (hasPerm(user, PERM.manageUsers)) return "admin";
    if (hasPerm(user, PERM.reporting)) return "reporting";
    if (hasPerm(user, PERM.systemHealth)) return "monitor";
    if (hasPerm(user, PERM.manageAnprConfig)) return "anpr";
    return "settings";
  });

  // Track which tabs have been visited so we can lazy-mount them.
  // On first visit: mount + fire API calls.
  // On subsequent visits: already mounted, instant switch via display toggle.
  const [visited, setVisited] = useState<Set<TabId>>(() => new Set([tab]));

  useEffect(() => {
    api
      .getProfilePhoto(user.id)
      .then((p) => setPhoto(p ?? null))
      .catch(() => undefined);
  }, [user.id]);

  // Build stable tab list — permissions don't change mid-session.
  const tabs: TabDef[] = [];
  if (hasPerm(user, PERM.gateEntries)) {
    tabs.push({ id: "gate", label: "Gate" });
  }
  if (hasPerm(user, PERM.reporting)) {
    tabs.push({ id: "reporting", label: "Reporting" });
  }
  if (
    hasPerm(user, PERM.manageUsers) ||
    hasPerm(user, PERM.manageReference) ||
    hasPerm(user, PERM.manageIntegrations) ||
    hasPerm(user, PERM.viewAudit)
  ) {
    tabs.push({ id: "admin", label: "Admin" });
  }
  if (hasPerm(user, PERM.systemHealth)) {
    tabs.push({ id: "monitor", label: "System Monitor" });
  }
  if (hasPerm(user, PERM.manageAnprConfig)) {
    tabs.push({ id: "anpr", label: "ANPR" });
  }
  tabs.push({ id: "settings", label: "Settings" });

  const active = tabs.find((t) => t.id === tab) ?? tabs[tabs.length - 1];

  const switchTab = (id: TabId) => {
    setTab(id);
    // Mark as visited so the component mounts on next render (or immediately).
    setVisited((prev) => {
      if (prev.has(id)) return prev;
      const next = new Set(prev);
      next.add(id);
      return next;
    });
  };

  return (
    <div className="app-root">
      <div className="topbar">
        <div className="brand-mark" style={{ width: 28, height: 28, fontSize: 13 }}>
          TF
        </div>
        <div className="brand-name">TruckFlow</div>
        <div className="grow" />
        <div className="user-chip">
          <div className="avatar">
            {photo ? <img src={photo} alt="" style={{ width: "100%", height: "100%", objectFit: "cover" }} /> : initials(user.name)}
          </div>
          <div className="meta">
            <div className="name">{user.name}</div>
            <div className="role">{hasPerm(user, PERM.manageUsers) ? "Admin" : "User"}</div>
          </div>
        </div>
        <button className="ghost" onClick={onLogout}>
          Log out
        </button>
      </div>

      <div className="tabbar">
        {tabs.map((t) => (
          <button key={t.id} className={active.id === t.id ? "active" : ""} onClick={() => switchTab(t.id)}>
            {t.label}
          </button>
        ))}
      </div>

      <PendingUpgradeBanner user={user} onApplied={onPermissionsApplied} />

      <div className="section">
        {/*
          Lazy-mount: only render a tab's content once it has been visited.
          First click: mounts + fires API calls (~10ms).
          Subsequent clicks: already mounted, display toggle only (~0ms).
        */}
        {tabs.map((t) =>
          visited.has(t.id) ? (
            <div key={t.id} style={{ display: t.id === active.id ? "block" : "none" }}>
              <TabContent
                tabId={t.id}
                user={user}
                canResolve={hasPerm(user, PERM.queue)}
                canRegisterVehicle={hasPerm(user, PERM.registerNewVehicle)}
                canEditTrip={hasPerm(user, PERM.editTrip)}
                onThemeChanged={onThemeChanged}
                onPermissionsApplied={onPermissionsApplied}
              />
            </div>
          ) : null
        )}
      </div>
    </div>
  );
}

const TabContent = memo(function TabContent({
  tabId,
  user,
  canResolve,
  canRegisterVehicle,
  canEditTrip,
  onThemeChanged,
}: {
  tabId: TabId;
  user: SessionUser;
  canResolve: boolean;
  canRegisterVehicle: boolean;
  canEditTrip: boolean;
  onThemeChanged?: (themeMode: string, themeAccent: string) => void;
  onPermissionsApplied?: () => void;
}) {
  switch (tabId) {
    case "gate":
      return <GateOfficer user={user} canResolve={canResolve} canRegisterVehicle={canRegisterVehicle} canEditTrip={canEditTrip} />;
    case "reporting":
      return <Reporting user={user} />;
    case "admin":
      return <AdminPanel user={user} />;
    case "monitor":
      return <SystemMonitor user={user} />;
    case "anpr":
      return <AnprConfig user={user} />;
    case "settings":
      return <Settings user={user} onThemeChanged={onThemeChanged} />;
  }
});

function initials(name: string) {
  return name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => w[0].toUpperCase())
    .join("");
}
