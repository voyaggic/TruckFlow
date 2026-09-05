import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import AnprSetupWizard from "../components/AnprSetupWizard";
import type {
  AnprConfigView,
  AnprCredentialView,
  AnprDiagnosticsView,
  CameraSourceView,
  ConfidenceTrendPoint,
  DetectedCamera,
  MachineInfo,
  ModelVersionView,
  OcrEngine,
  SessionUser,
  TrainingCandidateView,
} from "../lib/types";

/** Live status from the ANPR service /status endpoint. */

const SOURCE_TYPES = ["rtsp", "http", "nvr_export", "usb", "video_file", "live_test"] as const;

type AnprTabId = "live" | "settings" | "engine" | "credentials" | "models" | "training" | "diagnostics";

const TABS: { id: AnprTabId; label: string }[] = [
  { id: "live", label: "Live Preview" },
  { id: "settings", label: "Settings" },
  { id: "engine", label: "Engine & Threshold" },
  { id: "credentials", label: "Credentials" },
  { id: "models", label: "Models" },
  { id: "training", label: "Training" },
  { id: "diagnostics", label: "Diagnostics" },
];

export default function AnprConfig({ user }: { user: SessionUser }) {
  const [config, setConfig] = useState<AnprConfigView | null>(null);
  const [cameras, setCameras] = useState<CameraSourceView[]>([]);
  const [versions, setVersions] = useState<ModelVersionView[]>([]);
  const [candidates, setCandidates] = useState<TrainingCandidateView[]>([]);
  const [credentials, setCredentials] = useState<AnprCredentialView[]>([]);
  const [diagnostics, setDiagnostics] = useState<AnprDiagnosticsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<AnprTabId>("live");
  const anprUrl = useAnprServiceUrl();

  // Batch-load everything every tab needs on mount (config + cameras + diagnostics)
  const refreshCore = useCallback(() => {
    Promise.allSettled([
      api.getAnprConfig(),
      api.listCameraSources(),
      api.anprDiagnostics(),
    ]).then(([configRes, camerasRes, diagRes]) => {
      if (configRes.status === "fulfilled") setConfig(configRes.value);
      else setError(String(configRes.reason));
      if (camerasRes.status === "fulfilled") setCameras(camerasRes.value);
      else setError(String(camerasRes.reason));
      if (diagRes.status === "fulfilled") setDiagnostics(diagRes.value);
    });
  }, []);

  // Lazy-load tab-specific data only when the tab is activated
  useEffect(() => {
    if (activeTab === "engine" || activeTab === "settings" || activeTab === "live") {
      // Already loaded via refreshCore
    }
    if (activeTab === "credentials") {
      api.listAnprCredentials().then(setCredentials).catch(() => undefined);
    }
    if (activeTab === "models") {
      api.listModelVersions().then(setVersions).catch((e) => setError(String(e)));
    }
    if (activeTab === "training") {
      api.listTrainingCandidates().then(setCandidates).catch(() => undefined);
    }
    if (activeTab === "diagnostics") {
      api.anprDiagnostics().then(setDiagnostics).catch(() => undefined);
    }
  }, [activeTab]);

  useEffect(() => {
    refreshCore();
    // Fetch diagnostics immediately when live/diagnostics tab activates,
    // then poll every 10s. Without the immediate fetch, the UI shows
    // "ANPR Stopped" for ~10s after navigating back to the live tab.
    let t: ReturnType<typeof setInterval> | undefined;
    if (activeTab === "diagnostics" || activeTab === "live") {
      api.anprDiagnostics().then(setDiagnostics).catch(() => undefined);
      t = setInterval(() => {
        api.anprDiagnostics().then(setDiagnostics).catch(() => undefined);
      }, 10000);
    }
    return () => { if (t) clearInterval(t); };
  }, [refreshCore]);

  // Re-sync cameras after every ANPR (re)start. Pause/Resume triggers a
  // BACKGROUND service restart (5-8s) — an immediate refresh would keep the
  // stale pipeline mapping, so feeds appeared under the wrong camera name.
  useEffect(() => {
    const unlisten = listen("anpr-started", () => {
      // Poll /health until the service is actually ready
      let attempts = 0;
      const pollHealth = async () => {
        const maxAttempts = 30; // 60 seconds max
        const interval = setInterval(async () => {
          attempts++;
          try {
            const status = await api.anprDiagnostics();
            if (status?.service_running) {
              clearInterval(interval);
              refreshCore();
            }
          } catch {}
          if (attempts >= maxAttempts) {
            clearInterval(interval);
            refreshCore(); // refresh anyway after timeout
          }
        }, 2000);
      };
      pollHealth();
    });
    return () => { unlisten.then((f) => f()); };
  }, [refreshCore]);

  const run = async (fn: () => Promise<unknown>, okMsg: string) => {
    setError(null);
    setNotice(null);
    try {
      await fn();
      setNotice(okMsg);
      setTimeout(() => setNotice(null), 3500);
      refreshCore();
    } catch (e) {
      setError(String(e));
      setTimeout(() => setError(null), 6000);
    }
  };

  return (
    <div>
      <h2 className="section-title">ANPR — Live Recognition</h2>
      <p className="section-sub">
        Seven sections, one page: see the live feed, configure cameras and sensitivity, pick the OCR engine, manage
        API keys, deploy models, and watch the pipeline's health. Every change is audit-logged.
      </p>

      {error && <div className="error-banner">{error}</div>}
      {notice && <div className="success-banner">{notice}</div>}

      <div className="tabbar" style={{ marginBottom: 16 }}>
        {TABS.map((t) => (
          <button key={t.id} className={activeTab === t.id ? "active" : ""} onClick={() => setActiveTab(t.id)}>
            {t.label}
          </button>
        ))}
      </div>

      {activeTab === "live" && <LivePreviewTab cameras={cameras} actor={user} onRun={run} serviceRunning={diagnostics?.service_running ?? false} onStopped={() => api.anprDiagnostics().then(setDiagnostics).catch(() => undefined)} anprUrl={anprUrl} />}

      {activeTab === "settings" && (
        <SettingsTab config={config} cameras={cameras} actor={user} onRun={run} onConfigSave={(c) => run(() => api.updateAnprConfig(user.id, c), "Settings saved.")} />
      )}

      {activeTab === "engine" && (
        config ? (
          <EngineTab
            config={config}
            actor={user}
            onSave={(changes) => run(() => api.updateAnprConfig(user.id, changes), "Engine settings saved.")}
            onRun={run}
            serviceRunning={diagnostics?.service_running ?? false}
            onStopped={() => api.anprDiagnostics().then(setDiagnostics).catch(() => undefined)}
            anprUrl={anprUrl}
          />
        ) : (
          <div className="card"><div className="center-fill"><div className="spinner" /></div></div>
        )
      )}

      {activeTab === "credentials" && <CredentialsTab credentials={credentials} actor={user} onRun={run} />}

      {activeTab === "models" && <ModelPanel versions={versions} actor={user} onRun={run} />}

      {activeTab === "training" && (
        <CandidatePanel candidates={candidates} config={config} actor={user} onRun={(fn, okMsg) => {
          setError(null);
          setNotice(null);
          fn().then(() => {
            setNotice(okMsg);
            setTimeout(() => setNotice(null), 3500);
            // Only refresh training candidates, not config+cameras
            api.listTrainingCandidates().then(setCandidates).catch(() => undefined);
          }).catch((e) => { setError(String(e)); setTimeout(() => setError(null), 6000); });
          setTimeout(() => setError(null), 6000);
        }} />
      )}

      {activeTab === "diagnostics" && <DiagnosticsTab diagnostics={diagnostics} actor={user} />}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Live Preview — camera cards + ANPR service status
// ---------------------------------------------------------------------------

function LivePreviewTab({
  cameras,
  actor,
  onRun,
  serviceRunning,
  onStopped,
  anprUrl,
}: {
  cameras: CameraSourceView[];
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
  serviceRunning: boolean;
  onStopped?: () => void;
  anprUrl: string;
}) {
  const [expanded, setExpanded] = useState<CameraSourceView | null>(null);
  const [orientation, setOrientation] = useState<"landscape" | "portrait">("landscape");
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const [lastPlate, setLastPlate] = useState<{ plate: string; confidence: number; timestamp: string } | null>(null);

  useEffect(() => {
    const next: Record<string, string> = {};
    for (const c of cameras) {
      if (c.source_type === "video_file" || c.source_type === "nvr_export") {
        next[c.id] = convertFileSrc(c.connection_string);
      }
    }
    setPreviews(next);
  }, [cameras]);

  // Poll for latest plate detection to show green border
  useEffect(() => {
    const poll = async () => {
      try {
        const resp = await fetch(`${anprUrl}/latest`);
        if (resp.ok) {
          const data = await resp.json();
          if (data.plate && data.plate.trim()) {
            setLastPlate({ plate: data.plate, confidence: data.confidence, timestamp: data.timestamp });
          } else {
            setLastPlate(null);
          }
        }
      } catch {
        setLastPlate(null);
      }
    };
    poll();
    const t = setInterval(() => {
      if (document.visibilityState === "visible") poll();
    }, 500);
    return () => clearInterval(t);
  }, [anprUrl, cameras]);

  // NOTE: We intentionally do NOT auto-stop the ANPR service when cameras
  // is empty — cameras starts as [] on mount before the parent fetches them,
  // and this effect would kill a running service every time the tab mounts.
  // Users stop the service manually via the Engine tab.

  const isPortrait = orientation === "portrait";

  // Dynamic grid: 1 camera = full, 2 = side-by-side, 3+ = grid
  const cameraCount = cameras.length;
  const gridCols = cameraCount === 1
    ? "1fr"
    : cameraCount === 2
      ? "1fr 1fr"
      : `repeat(${Math.min(cameraCount, 4)}, 1fr)`;

  // Pipeline index = position among ACTIVE + TRACKED sources — must match the
  // config writer order (created_at ASC) so ?camera=N hits the right feed.
  const trackedActive = cameras.filter((c) => c.status === "active" && c.tracked);
  const pipelineIndexOf = (id: string): number | undefined => {
    const i = trackedActive.findIndex((c) => c.id === id);
    return i >= 0 ? i : undefined;
  };

  return (
    <div className="stack" style={{ height: "100%" }}>
      <ServiceStatusBar cameras={cameras} actor={actor} serviceRunning={serviceRunning} lastPlate={lastPlate} onStopped={onStopped} anprUrl={anprUrl} />

      {/* Orientation toggle */}
      <div className="row" style={{ gap: 8, alignItems: "center" }}>
        <span className="muted small">Preview:</span>
        <button
          className={`ghost small ${orientation === "landscape" ? "active" : ""}`}
          onClick={() => setOrientation("landscape")}
        >
          Landscape
        </button>
        <button
          className={`ghost small ${orientation === "portrait" ? "active" : ""}`}
          onClick={() => setOrientation("portrait")}
        >
          Portrait
        </button>
      </div>

      {cameras.length === 0 ? (
        <div className="placeholder">
          No camera sources yet. Add one in <b>Settings</b>, then come back here to watch the live feed.
        </div>
      ) : (
        <div
          style={{
            display: "grid",
            gridTemplateColumns: gridCols,
            gap: 12,
            flex: 1,
            minHeight: 0,
          }}
        >
          {cameras.map((c) => (
            <CameraCard
              key={c.id}
              camera={c}
              previewUrl={previews[c.id]}
              lastPlate={lastPlate}
              onExpand={() => setExpanded(c)}
              onRun={onRun}
              actor={actor}
              pipelineIndex={pipelineIndexOf(c.id)}
              serviceRunning={serviceRunning}
              anprUrl={anprUrl}
            />
          ))}
        </div>
      )}

      {/* Expanded fullscreen view */}
      {expanded && (
        <div className="overlay" onClick={() => setExpanded(null)}>
          <div
            className="modal modal-wide"
            onClick={(e) => e.stopPropagation()}
            style={{
              maxWidth: isPortrait ? "50vw" : "90vw",
              height: isPortrait ? "85vh" : "75vh",
              display: "flex",
              flexDirection: "column",
            }}
          >
            <div className="row between">
              <h3 style={{ marginTop: 0 }}>{expanded.label}</h3>
              <div className="row" style={{ gap: 6 }}>
                <button className="ghost small" onClick={() => setOrientation(isPortrait ? "landscape" : "portrait")}>
                  {isPortrait ? "Landscape" : "Portrait"}
                </button>
                <button className="ghost" onClick={() => setExpanded(null)}>Close</button>
              </div>
            </div>
            <div className="muted small" style={{ marginBottom: 8 }}>
              {expanded.source_type} — {expanded.connection_string}
            </div>
            {lastPlate && (
              <div style={{
                display: "flex", alignItems: "center", gap: 12, marginBottom: 8,
                padding: "6px 12px", border: "2px solid var(--success)", borderRadius: 8,
                background: "color-mix(in srgb, var(--success) 8%, transparent)",
              }}>
                <span style={{ fontSize: 18, fontWeight: 700, fontFamily: "monospace" }}>{lastPlate.plate}</span>
                <span className="badge active">{Math.round(lastPlate.confidence * 100)}%</span>
                <span className="muted small">{new Date(lastPlate.timestamp).toLocaleTimeString()}</span>
              </div>
            )}
            <div style={{
              flex: 1,
              minHeight: 0,
              borderRadius: 8,
              overflow: "hidden",
              background: "#000",
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              border: lastPlate ? "3px solid var(--success)" : "3px solid transparent",
            }}>
              <CameraThumb camera={expanded} previewUrl={previews[expanded.id]} large pipelineIndex={pipelineIndexOf(expanded.id)} serviceRunning={serviceRunning} anprUrl={anprUrl} />
            </div>
            {expanded.last_connection_check_result && (
              <div className="small muted" style={{ marginTop: 8 }}>
                Last check: {expanded.last_connection_check_result}
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/** Individual camera card with plate detection overlay. */
function CameraCard({
  camera,
  previewUrl,
  lastPlate,
  onExpand,
  onRun,
  actor,
  pipelineIndex,
  serviceRunning,
  anprUrl,
}: {
  camera: CameraSourceView;
  previewUrl?: string;
  lastPlate: { plate: string; confidence: number; timestamp: string } | null;
  onExpand: () => void;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
  actor: SessionUser;
  pipelineIndex?: number;
  serviceRunning?: boolean;
  anprUrl: string;
}) {
  return (
    <div
      className={`health-card ${camera.status === "active" ? "ok" : "offline"}`}
      style={{
        cursor: "pointer",
        display: "flex",
        flexDirection: "column",
        overflow: "hidden",
      }}
      onClick={onExpand}
    >
      <div className="row between" style={{ alignItems: "center", padding: "8px 10px 0" }}>
        <b style={{ fontSize: 13 }}>{camera.label}</b>
        <span className={`badge ${camera.status === "active" ? "active" : "disabled"}`} style={{ fontSize: 11 }}>
          {camera.status}
        </span>
      </div>
      <div className="muted small" style={{ padding: "2px 10px" }}>
        {camera.source_type} · {camera.connection_string}
      </div>

      {/* Plate details above the detection zone */}
      {lastPlate && (
        <div style={{
          display: "flex", alignItems: "center", gap: 8, padding: "4px 10px",
          border: "2px solid var(--success)", borderRadius: 6, margin: "4px 6px 0",
          background: "color-mix(in srgb, var(--success) 8%, transparent)",
        }}>
          <span style={{ fontSize: 14, fontWeight: 700, fontFamily: "monospace" }}>{lastPlate.plate}</span>
          <span className="badge active" style={{ fontSize: 10 }}>{Math.round(lastPlate.confidence * 100)}%</span>
          <span className="muted small" style={{ marginLeft: "auto" }}>{new Date(lastPlate.timestamp).toLocaleTimeString()}</span>
        </div>
      )}

      {/* Camera preview — fills available space with green border when plate detected */}
      <div
        style={{
          flex: 1,
          minHeight: 0,
          margin: "6px 6px",
          borderRadius: 6,
          overflow: "hidden",
          background: "#000",
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          border: lastPlate ? "2px solid var(--success)" : "2px solid transparent",
        }}
      >
        <CameraThumb camera={camera} previewUrl={previewUrl} pipelineIndex={pipelineIndex} serviceRunning={serviceRunning} anprUrl={anprUrl} />
      </div>

      <div className="row" style={{ gap: 4, padding: "0 8px 8px" }}>
        {camera.status === "active" ? (
          <button
            className="ghost small"
            onClick={(e) => {
              e.stopPropagation();
              onRun(() => api.setCameraSourceStatus(actor.id, camera.id, "inactive"), "Feed paused — it will stop being analyzed in a few seconds.");
            }}
          >
            Pause
          </button>
        ) : (
          <button
            className="ghost small"
            onClick={(e) => {
              e.stopPropagation();
              onRun(() => api.setCameraSourceStatus(actor.id, camera.id, "active"), "Feed resumed — it will restart in a few seconds.");
            }}
          >
            Resume
          </button>
        )}
        <button className="ghost small" onClick={(e) => { e.stopPropagation(); onExpand(); }}>
          Expand
        </button>
      </div>
    </div>
  );
}

function CameraThumb({ camera, previewUrl, large, pipelineIndex, serviceRunning, anprUrl }: { camera: CameraSourceView; previewUrl?: string; pipelineIndex?: number; serviceRunning?: boolean; large?: boolean; anprUrl: string }) {
  const h = large ? "65vh" : "100%";

  // Live-source polling tick. React RULES OF HOOKS: every hook must run on
  // EVERY render, before any early return. An early return above the hooks
  // changes the hook count when a camera's status/type flips (e.g. clicking
  // Pause) and crashes the whole app to a blank screen.
  const isLiveType = ["http", "rtsp", "usb", "live_test"].includes(camera.source_type);
  const shouldPoll = isLiveType && camera.status === "active" && pipelineIndex !== undefined && serviceRunning !== false;
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!shouldPoll) return;
    const t = setInterval(() => {
      if (document.visibilityState === "visible") setTick((v) => v + 1);
    }, 200);
    return () => clearInterval(t);
  }, [shouldPoll]);

  // Paused feeds are NOT shown and NOT analyzed — a frozen last frame would
  // wrongly suggest the camera is still being watched.
  if (camera.status !== "active") {
    return (
      <div style={{
        width: "100%", height: h, display: "flex", flexDirection: "column",
        alignItems: "center", justifyContent: "center", gap: 6,
        background: "repeating-linear-gradient(45deg, #1a1a1a, #1a1a1a 12px, #222 12px, #222 24px)",
        color: "#9ca3af", borderRadius: 6,
      }}>
        <span style={{ fontSize: large ? 40 : 24 }}>⏸</span>
        <span className="small" style={{ fontWeight: 600 }}>PAUSED</span>
        <span className="small" style={{ opacity: 0.7, textAlign: "center", padding: "0 12px" }}>
          This feed is not being analyzed. Click Resume to restore it.
        </span>
      </div>
    );
  }
  // For video files, use the converted file src
  if (camera.source_type === "video_file" || camera.source_type === "nvr_export") {
    // muted+autoPlay+loop: a paused <video> shows a black tile — always play.
    return <video src={previewUrl ?? convertFileSrc(camera.connection_string)} controls muted autoPlay loop playsInline style={{ width: "100%", height: h, objectFit: "contain" }} />;
  }
  const liveSrc = shouldPoll ? `${anprUrl}/preview_frame?camera=${pipelineIndex}&t=${tick}` : null;
  if (liveSrc) {
    return <img src={liveSrc} alt={`${camera.label} live feed`} style={{ width: "100%", height: h, objectFit: "contain" }} />;
  }
  // Fallback: single snapshot when the service isn't running / index unknown
  const proxySrc = `${anprUrl}/preview_frame?t=${Date.now()}`;
  return (
    <img
      src={proxySrc}
      alt={`${camera.label} live feed`}
      style={{ width: "100%", height: h, objectFit: "contain" }}
      onError={(e) => {
        // If ANPR service proxy isn't running, show a placeholder
        const target = e.currentTarget as HTMLImageElement;
        target.style.display = "none";
        // Fallback: try direct camera URL for HTTP sources
        const parent = target.parentElement;
        if (parent && (camera.connection_string.startsWith("http://") || camera.connection_string.startsWith("https://"))) {
          const fallback = document.createElement("img");
          fallback.src = camera.connection_string;
          fallback.alt = `${camera.label} direct feed`;
          fallback.style.cssText = `width:100%;height:${h};object-fit:contain`;
          parent.appendChild(fallback);
        }
      }}
    />
  );
}

/** Hook to get the ANPR service URL from the backend (defaults to http://127.0.0.1:9800). */
function useAnprServiceUrl() {
  const [url, setUrl] = useState("http://127.0.0.1:9800");
  useEffect(() => {
    api.getAnprServiceUrl().then(setUrl).catch(() => {});
  }, []);
  return url;
}

/** Auto-refreshing live preview from the ANPR service. ?camera=N selects the feed in multi-camera mode. */
function LivePreview({ cameraIdx, anprUrl }: { cameraIdx?: number; anprUrl: string }) {
  const camQ = cameraIdx !== undefined ? `camera=${cameraIdx}&` : "";
  const [src, setSrc] = useState(`${anprUrl}/preview_frame?${camQ}t=${Date.now()}`);

  useEffect(() => {
    setSrc(`${anprUrl}/preview_frame?${camQ}t=${Date.now()}`);
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") {
        setSrc(`${anprUrl}/preview_frame?${camQ}t=${Date.now()}`);
      }
    }, 1000);
    return () => clearInterval(timer);
  }, [anprUrl, camQ]);

  return (
    <img
      src={src}
      alt="Live camera feed"
      style={{ width: "100%", maxHeight: 300, objectFit: "contain", background: "#000", borderRadius: 6 }}
      onError={() => {
        // If preview_frame fails, try the /preview MJPEG endpoint
        setSrc(`${anprUrl}/preview?${camQ}t=${Date.now()}`);
      }}
    />
  );
}

function ServiceStatusBar({ cameras, actor, serviceRunning, lastPlate, onStopped, anprUrl }: { cameras: CameraSourceView[]; actor: SessionUser; serviceRunning: boolean; lastPlate: { plate: string; confidence: number; timestamp: string } | null; onStopped?: () => void; anprUrl: string }) {
  const [starting, setStarting] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [showSetup, setShowSetup] = useState(false);
  const [setupComplete, setSetupComplete] = useState(false);

  /** Sources the service will actually capture: active AND tracked */
  const trackedActive = cameras.filter(c => c.status === "active" && c.tracked);

  /** Actually start the ANPR service (called after setup is complete or skipped) */
  const doStart = useCallback(async () => {
    setStarting(true);
    setMessage(null);
    try {
      await api.startAnprService(actor.id);
      setMessage(`ANPR service starting — ${trackedActive.length} tracked camera(s)...`);
      let tries = 0;
      const check = async () => {
        try {
          const resp = await fetch(`${anprUrl}/health`);
          if (resp.ok) {
            setMessage(`ANPR service running — tracking ${trackedActive.length} camera(s)`);
            onStopped?.();
            return;
          }
        } catch {}
        if (++tries < 30) setTimeout(check, 1000);
        else setMessage("ANPR service started but not responding yet — check logs.");
      };
      setTimeout(check, 2000);
    } catch (e: any) {
      // If the backend says ANPR isn't ready, show the setup wizard
      if (e === "anpr_not_ready" || String(e).includes("anpr_not_ready")) {
        setShowSetup(true);
        setStarting(false);
        return;
      }
      setMessage(`Failed: ${e}`);
    } finally {
      setStarting(false);
    }
  }, [trackedActive, actor, anprUrl]);

  /** Start the ANPR service — checks readiness first */
  const handleStart = useCallback(async () => {
    if (trackedActive.length === 0) {
      setMessage("No active tracked camera source. In Camera Settings, set a camera Active + Tracked (☑).");
      return;
    }
    // If we already verified readiness (after setup wizard), skip the check
    if (setupComplete) {
      doStart();
      return;
    }
    // Check if ANPR is ready
    try {
      const status = await api.checkAnprReady();
      if (status.ready) {
        doStart();
      } else {
        setShowSetup(true);
      }
    } catch {
      // If check fails, try to start anyway (will show wizard on error)
      doStart();
    }
  }, [trackedActive, setupComplete, doStart]);

  /** Stop the ANPR service */
  const handleStop = useCallback(async () => {
    try {
      await api.stopAnprService(actor.id);
      setMessage("ANPR service stopped.");
      onStopped?.();
    } catch (e: any) {
      setMessage(`Failed: ${e}`);
    }
  }, [actor, onStopped]);

  return (
    <div className="card">
      <div className="row" style={{ gap: 12, flexWrap: "wrap", alignItems: "center" }}>
        <span className={`badge ${serviceRunning ? "active" : "disabled"}`}>
          {serviceRunning ? "ANPR Running" : "ANPR Stopped"}
        </span>
        {!serviceRunning ? (
          <button className="small" onClick={handleStart} disabled={starting} style={{ marginLeft: 4 }}>
            {starting ? "Starting..." : "Start ANPR"}
          </button>
        ) : (
          <button className="small danger" onClick={handleStop} style={{ marginLeft: 4 }}>
            Stop ANPR
          </button>
        )}
      </div>
      {message && (
        <div className="muted small" style={{ marginTop: 6, padding: "4px 8px", background: "var(--surface-2, #1a1a2e)", borderRadius: 4 }}>
          {message}
        </div>
      )}

      {/* Live per-camera previews — one tile per active+tracked source */}
      {serviceRunning && trackedActive.length > 0 && (
        <div style={{
          marginTop: 12,
          display: "grid",
          gridTemplateColumns: trackedActive.length === 1 ? "1fr" : "repeat(2, 1fr)",
          gap: 8,
        }}>
          {trackedActive.map((c, i) => (
            <div key={c.id}>
              <div className="muted small" style={{ marginBottom: 2 }}>
                Camera {i + 1}: {c.label} <span className="badge" style={{ fontSize: 10 }}>{c.source_type}</span>
              </div>
              <LivePreview cameraIdx={i} anprUrl={anprUrl} />
            </div>
          ))}
        </div>
      )}

      {lastPlate && (
        <div className="row" style={{ gap: 12, alignItems: "center", marginTop: 10 }}>
          <span className="muted small">Last detected:</span>
          <span style={{ fontSize: 18, fontWeight: 700, fontFamily: "monospace" }}>{lastPlate.plate}</span>
          <span className="badge active">{Math.round(lastPlate.confidence * 100)}% confidence</span>
          <span className="muted small">{new Date(lastPlate.timestamp).toLocaleTimeString()}</span>
        </div>
      )}

      {showSetup && (
        <AnprSetupWizard
          onComplete={() => {
            setShowSetup(false);
            setSetupComplete(true);
            doStart();
          }}
          onSkip={() => setShowSetup(false)}
        />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Settings — camera sources + detection sensitivity presets + pending window
// ---------------------------------------------------------------------------

function SettingsTab({
  config,
  cameras,
  actor,
  onRun,
  onConfigSave,
}: {
  config: AnprConfigView | null;
  cameras: CameraSourceView[];
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
  onConfigSave: (changes: Partial<AnprConfigView>) => void;
}) {
  return (
    <div className="stack">
      {config && <SensitivityPanel config={config} onSave={onConfigSave} />}
      <CameraPanel cameras={cameras} actor={actor} onRun={onRun} />
    </div>
  );
}

/** Labeled sensitivity presets — the raw thresholds are hidden behind "Advanced". */
function SensitivityPanel({
  config,
  onSave,
}: {
  config: AnprConfigView;
  onSave: (changes: Partial<AnprConfigView>) => void;
}) {
  const [preset, setPreset] = useState<string>(presetFor(config));
  const [advanced, setAdvanced] = useState(false);
  const [paddle, setPaddle] = useState(config.confidence_threshold_paddleocr);
  const [easy, setEasy] = useState(config.confidence_threshold_easyocr);
  const [pending, setPending] = useState(config.max_pending_duration_hours?.toString() ?? "24");
  const [detectionMethod, setDetectionMethod] = useState(config.detection_method ?? "contour");

  // Resync local state when the saved config reloads after a save, so the
  // panel never shows stale values.
  useEffect(() => {
    setPreset(presetFor(config));
    setPaddle(config.confidence_threshold_paddleocr);
    setEasy(config.confidence_threshold_easyocr);
    setPending(config.max_pending_duration_hours?.toString() ?? "24");
    setDetectionMethod(config.detection_method ?? "contour");
  }, [config]);

  const num = (v: string, fallback: number) => {
    const n = Number(v);
    return Number.isFinite(n) ? n : fallback;
  };

  const applyPreset = (name: string) => {
    setPreset(name);
    const t = PRESETS[name] ?? PRESETS.balanced;
    setPaddle(t.paddle);
    setEasy(t.easy);
  };

  const save = () =>
    onSave({
      confidence_threshold_paddleocr: paddle,
      confidence_threshold_easyocr: easy,
      max_pending_duration_hours: num(pending, 24),
      detection_method: detectionMethod,
    });

  return (
    <div className="card">
      <div className="section-title" style={{ fontSize: 15 }}>Detection Method</div>
      <p className="muted small" style={{ marginTop: -6 }}>
        Choose how plates are detected in video frames.
      </p>

      <div className="stack" style={{ marginTop: 8, gap: 6 }}>
        <label className="radio-card" style={{ cursor: "pointer" }}>
          <input
            type="radio"
            name="detection_method"
            value="contour"
            checked={detectionMethod === "contour"}
            onChange={() => setDetectionMethod("contour")}
            style={{ marginRight: 10 }}
          />
          <div>
            <div style={{ fontWeight: 600 }}>Contour Detection (Default)</div>
            <div className="muted small">Fast, works at CCTV distances. Uses edge/shape analysis to find plate-like regions.</div>
          </div>
        </label>

        <label className="radio-card" style={{ cursor: "pointer" }}>
          <input
            type="radio"
            name="detection_method"
            value="paddleocr"
            checked={detectionMethod === "paddleocr"}
            onChange={() => setDetectionMethod("paddleocr")}
            style={{ marginRight: 10 }}
          />
          <div>
            <div style={{ fontWeight: 600 }}>PaddleOCR Detection</div>
            <div className="muted small">AI-powered detection. Better accuracy but slower. Requires PaddleOCR models.</div>
          </div>
        </label>

        <label className="radio-card" style={{ cursor: "pointer" }}>
          <input
            type="radio"
            name="detection_method"
            value="consecutive"
            checked={detectionMethod === "consecutive"}
            onChange={() => setDetectionMethod("consecutive")}
            style={{ marginRight: 10 }}
          />
          <div>
            <div style={{ fontWeight: 600 }}>Consecutive Reads</div>
            <div className="muted small">Most reliable. Requires 3 matching reads before accepting. Best for controlled gate environments.</div>
          </div>
        </label>
      </div>

      <div className="row" style={{ marginTop: 12 }}>
        <button className="primary" onClick={save}>Save settings</button>
      </div>

      <div style={{ marginTop: 20 }}>
        <div className="section-title" style={{ fontSize: 15 }}>Detection sensitivity</div>
      <p className="muted small" style={{ marginTop: -6 }}>
        How strict the recognition gate is. <b>Balanced</b> is the default; choose <b>Strict</b> if the queue gets too
        many confident-but-wrong reads, or <b>Lenient</b> to log more automatically. Tune the raw values under
        Advanced if presets aren't enough.
      </p>

      <div className="seg" style={{ marginTop: 6 }}>
        {Object.entries(PRESETS).map(([name, t]) => (
          <button key={name} className={preset === name ? "active" : ""} onClick={() => applyPreset(name)}>
            {name[0].toUpperCase() + name.slice(1)}
            <span className="muted small" style={{ marginLeft: 6 }}>
              {Math.round(t.paddle * 100)}%
            </span>
          </button>
        ))}
      </div>

      {!advanced && (
        <div className="row" style={{ marginTop: 10 }}>
          <button className="ghost small" onClick={() => setAdvanced(true)}>Advanced — raw thresholds…</button>
        </div>
      )}

      {advanced && (
        <div className="stack" style={{ marginTop: 10, border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14 }}>
          <div className="row between">
            <div className="section-title" style={{ fontSize: 13 }}>Raw thresholds</div>
            <button className="ghost small" onClick={() => setAdvanced(false)}>Hide advanced</button>
          </div>
          <div className="row">
            <div className="field grow">
              <label>PaddleOCR confidence threshold</label>
              <input type="number" min={0} max={1} step={0.05} value={paddle} onChange={(e) => { setPaddle(num(e.target.value, 0.7)); setPreset("custom"); }} />
            </div>
            <div className="field grow">
              <label>EasyOCR confidence threshold</label>
              <input type="number" min={0} max={1} step={0.05} value={easy} onChange={(e) => { setEasy(num(e.target.value, 0.7)); setPreset("custom"); }} />
            </div>
          </div>
          <div className="field" style={{ maxWidth: 240 }}>
            <label>Max pending duration (hours)</label>
            <input type="number" min={0.5} step={1} value={pending} onChange={(e) => setPending(e.target.value)} />
            <p className="muted small" style={{ marginTop: 4 }}>
              An open entry older than this is closed as "missed exit"; the next sighting starts a fresh entry.
            </p>
          </div>
          <div className="row">
            <button className="primary" onClick={save}>Save sensitivity settings</button>
          </div>
        </div>
      )}
      </div>
    </div>
  );
}

interface SensitivityPreset {
  paddle: number;
  easy: number;
}

const PRESETS: Record<string, SensitivityPreset> = {
  strict: { paddle: 0.9, easy: 0.9 },
  balanced: { paddle: 0.75, easy: 0.75 },
  lenient: { paddle: 0.6, easy: 0.6 },
};

function presetFor(config: AnprConfigView): string {
  const p = config.confidence_threshold_paddleocr;
  const e = config.confidence_threshold_easyocr;
  for (const [name, t] of Object.entries(PRESETS)) {
    if (Math.abs(t.paddle - p) < 0.03 && Math.abs(t.easy - e) < 0.03) return name;
  }
  return "custom";
}

// ---------------------------------------------------------------------------
// Camera sources
// ---------------------------------------------------------------------------

const TYPE_TEMPLATES: Record<string, string> = {
  rtsp: "rtsp://192.168.1.100:554/stream1",
  http: "http://192.168.1.100:8080/video",
  nvr_export: "/path/to/export.mp4",
  usb: "0",
  video_file: "",
  live_test: "http://127.0.0.1:9800/preview",
};

const TYPE_HELP: Record<string, string> = {
  rtsp: "RTSP camera stream (real CCTV / NVR systems)",
  http: "HTTP stream (IP Webcam app, MJPEG cameras)",
  nvr_export: "A video export path from your NVR (local .mp4/.avi file)",
  usb: "USB webcam device index, e.g. 0 for the first camera",
  video_file: "Pick a video file from your computer",
  live_test: "Loopback test through the ANPR service's own annotated stream (/preview)",
};

/** Auto-detect available USB/webcam devices and show live preview. */
function DetectCamerasPanel({ onAdd, configured }: { onAdd: (index: number, name: string) => void; configured?: CameraSourceView[] }) {
  const [cameras, setCameras] = useState<DetectedCamera[]>([]);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selectedIdx, setSelectedIdx] = useState<number | null>(null);

  const scan = async () => {
    setScanning(true);
    setError(null);
    try {
      const found = await api.enumerateCameras();
      setCameras(found);
      if (found.length === 0) setError("No cameras detected.");
    } catch (e) {
      setError(String(e));
      setTimeout(() => setError(null), 6000);
    } finally {
      setScanning(false);
    }
  };

  const addDetected = (cam: DetectedCamera) => {
    onAdd(cam.index, cam.name || `Camera ${cam.index}`);
  };

  return (
    <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, background: "var(--surface-2)", marginTop: 12 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 10, marginBottom: 8 }}>
        <div style={{ fontWeight: 600, fontSize: 13 }}>Detect Cameras</div>
        <button className="ghost small" onClick={scan} disabled={scanning}>
          {scanning ? "Scanning…" : "Scan for cameras"}
        </button>
      </div>
      <p className="muted small" style={{ marginTop: -4 }}>
        Detects real camera hardware connected to this PC. Only cameras with a live feed (frames actually changing) can be added.
      </p>
      {error && <div className="small" style={{ color: "var(--danger)", marginTop: 4 }}>{error}</div>}
      {/* Configured network sources — no probing needed, they're already known */}
      {configured && configured.some((c) => !["usb", "video_file"].includes(c.source_type)) && (
        <div style={{ marginTop: 10 }}>
          <div className="muted small" style={{ marginBottom: 4 }}>Configured network sources (already in your list):</div>
          <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
            {configured
              .filter((c) => !["usb", "video_file"].includes(c.source_type))
              .map((c) => (
                <span key={c.id} className="badge" title={c.connection_string}>
                  {c.source_type} · {c.label}
                </span>
              ))}
          </div>
        </div>
      )}
      {cameras.length > 0 && (
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(220px, 1fr))", gap: 10, marginTop: 8 }}>
          {cameras.map((cam) => (
            <div
              key={cam.index}
              style={{
                border: selectedIdx === cam.index ? "2px solid var(--primary)" : "1px solid var(--border)",
                borderRadius: "var(--radius)",
                padding: 10, cursor: "pointer",
                background: selectedIdx === cam.index ? "var(--surface-3)" : "var(--surface)",
              }}
              onClick={() => setSelectedIdx(selectedIdx === cam.index ? null : cam.index)}
            >
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
                <b style={{ fontSize: 13 }}>{cam.name || `Camera ${cam.index}`}</b>
                {cam.is_live ? (
                  <span className="badge active">{cam.status === "in_service" ? "LIVE · IN SERVICE" : "LIVE"}</span>
                ) : cam.status === "busy" ? (
                  <span className="badge">BUSY</span>
                ) : cam.status === "error" ? (
                  <span className="badge disabled">UNAVAILABLE</span>
                ) : cam.status === "black" ? (
                  <span className="badge disabled">BLACK</span>
                ) : (
                  <span className="badge disabled">STATIC</span>
                )}
              </div>
              <div className="muted small" style={{ marginTop: 4 }}>
                Index {cam.index} · {cam.width}×{cam.height} · {cam.backend}
                {cam.device_type && (
                  <span className="badge" style={{ fontSize: 10, marginLeft: 6 }} title="virtual = software camera output (vMix/OBS/EOS Utility), integrated = built-in, external = real USB device">
                    {cam.device_type}
                  </span>
                )}
              </div>
              {cam.is_live && cam.status === "in_service" ? (
                <div style={{ marginTop: 6, display: "flex", gap: 6, alignItems: "center" }}>
                  <div style={{ width: 8, height: 8, borderRadius: "50%", background: "var(--success)" }} />
                  <span className="small" style={{ color: "var(--success)" }}>Already live in the running ANPR service — verified there</span>
                </div>
              ) : !cam.is_live && cam.status === "busy" ? (
                <div style={{ marginTop: 6, display: "flex", gap: 6, alignItems: "center" }}>
                  <div style={{ width: 8, height: 8, borderRadius: "50%", background: "var(--warning, #f59e0b)" }} />
                  <span className="small" style={{ color: "var(--warning, #f59e0b)" }}>Held by the ANPR service but its feed is down — check the Live tab</span>
                </div>
              ) : !cam.is_live && cam.status === "error" ? (
                <div style={{ marginTop: 6, display: "flex", gap: 6, alignItems: "center" }}>
                  <div style={{ width: 8, height: 8, borderRadius: "50%", background: "var(--warning, #f59e0b)" }} />
                  <span className="small" style={{ color: "var(--warning, #f59e0b)" }}>Cannot open right now — device busy or disconnected</span>
                </div>
              ) : cam.is_live ? (
                <div style={{ marginTop: 6, display: "flex", gap: 6, alignItems: "center" }}>
                  <div style={{ width: 8, height: 8, borderRadius: "50%", background: "var(--success)" }} />
                  <span className="small" style={{ color: "var(--success)" }}>Live feed detected — frames changing</span>
                </div>
              ) : cam.status === "black" ? (
                <div style={{ marginTop: 6, display: "flex", gap: 6, alignItems: "center" }}>
                  <div style={{ width: 8, height: 8, borderRadius: "50%", background: "#ef4444" }} />
                  <span className="small" style={{ color: "#ef4444" }}>Camera delivers pure black — device not exposing. On a DSLR: set Movie mode 🎥, remove lens cap, wake the camera, restart its webcam utility.</span>
                </div>
              ) : (
                <div style={{ marginTop: 6, display: "flex", gap: 6, alignItems: "center" }}>
                  <div style={{ width: 8, height: 8, borderRadius: "50%", background: "var(--warning, #f59e0b)" }} />
                  <span className="small" style={{ color: "var(--warning, #f59e0b)" }}>Static image — no live feed (test pattern or disconnected)</span>
                </div>
              )}
              {selectedIdx === cam.index && cam.is_live && (
                <button className="primary small" style={{ marginTop: 6, width: "100%" }} onClick={(e) => { e.stopPropagation(); addDetected(cam); }}>
                  + Add to sources
                </button>
              )}
              {selectedIdx === cam.index && !cam.is_live && (
                <div className="small muted" style={{ marginTop: 6, textAlign: "center" }}>
                  {cam.status === "black"
                    ? "Pure black feed — the camera hardware is connected but not sending an image. Fix it on the camera: Movie mode, lens cap off, keep it awake, then rescan."
                    : cam.status === "busy" || cam.status === "error"
                    ? "Another program may be using this camera — close it or stop the ANPR service, then rescan"
                    : "This camera shows a static image — check if the lens cap is on or the camera is in the right mode"}
                </div>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function CameraPanel({
  cameras,
  actor,
  onRun,
}: {
  cameras: CameraSourceView[];
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
}) {
  const [label, setLabel] = useState("");
  const [type, setType] = useState<CameraSourceView["source_type"]>("rtsp");
  const [conn, setConn] = useState(TYPE_TEMPLATES.rtsp);
  // Real DirectShow device name captured from detection — persisted with the
  // source so the service can re-resolve the USB index on any machine.
  const [usbDeviceName, setUsbDeviceName] = useState<string | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editLabel, setEditLabel] = useState("");
  const [editType, setEditType] = useState<CameraSourceView["source_type"]>("rtsp");
  const [editConn, setEditConn] = useState("");
  const [testingId, setTestingId] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<Record<string, { ok: boolean; msg: string }>>({});
  const [deleteConfirm, setDeleteConfirm] = useState<string | null>(null);

  const pickType = (t: CameraSourceView["source_type"]) => {
    setType(t);
    setConn(TYPE_TEMPLATES[t] ?? "");
  };

  const browseVideo = async () => {
    const picked = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Video", extensions: ["mp4", "avi", "mkv", "mov", "webm"] }],
    });
    if (typeof picked === "string" && picked) setConn(picked);
  };

  const browseVideoForEdit = async () => {
    const picked = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Video", extensions: ["mp4", "avi", "mkv", "mov", "webm"] }],
    });
    if (typeof picked === "string" && picked) setEditConn(picked);
  };

  const add = () =>
    onRun(async () => {
      await api.addCameraSource(actor.id, label, type, conn, usbDeviceName ?? undefined);
      setUsbDeviceName(null);
      setLabel("");
      setConn(TYPE_TEMPLATES[type] ?? "");
    }, "Camera source added.");

  const startEdit = (c: CameraSourceView) => {
    setEditingId(c.id);
    setEditLabel(c.label);
    setEditType(c.source_type);
    setEditConn(c.connection_string);
  };

  const saveEdit = (sourceId: string) =>
    onRun(async () => {
      await api.updateCameraSource(actor.id, sourceId, editLabel, editType, editConn);
      setEditingId(null);
    }, "Camera source updated.");

  const testConnection = async (sourceId: string) => {
    setTestingId(sourceId);
    setTestResult((prev) => ({ ...prev, [sourceId]: { ok: false, msg: "Testing..." } }));
    try {
      const result = await api.testCameraConnection(actor.id, sourceId);
      const ok = result.status === "active";
      const msg = result.last_connection_check_result ?? (ok ? "Connection successful" : "Connection failed");
      setTestResult((prev) => ({ ...prev, [sourceId]: { ok, msg } }));
    } catch (e) {
      setTestResult((prev) => ({ ...prev, [sourceId]: { ok: false, msg: String(e) } }));
    } finally {
      setTestingId(null);
    }
  };

  const doDelete = (sourceId: string) =>
    onRun(async () => {
      await api.deleteCameraSource(actor.id, sourceId);
      setDeleteConfirm(null);
    }, "Camera source deleted.");

  return (
    <div className="card">
      <div className="section-title" style={{ fontSize: 15 }}>Camera sources ({cameras.length})</div>
      <p className="muted small" style={{ marginTop: -6 }}>
        Where the recognition service gets video from. For real CCTV use <b>RTSP</b>; use a phone app's RTSP mode (not
        MJPEG) for a faithful test. Video files are for development only.
      </p>

      {/* Add new source form */}
      <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, background: "var(--surface-2)" }}>
        <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 8 }}>Add New Camera Source</div>
        <div className="row">
          <div className="field grow">
            <label>Label</label>
            <input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="Main gate camera" />
          </div>
          <div className="field">
            <label>Type</label>
            <select value={type} onChange={(e) => pickType(e.target.value as CameraSourceView["source_type"])}>
              {SOURCE_TYPES.map((t) => (
                <option key={t} value={t}>{t}</option>
              ))}
            </select>
            <div className="muted small" style={{ marginTop: 4, maxWidth: 240 }}>{TYPE_HELP[type]}</div>
          </div>
          {type === "video_file" ? (
            <div className="field grow">
              <label>Video file</label>
              <div className="row" style={{ gap: 6 }}>
                <input value={conn} onChange={(e) => setConn(e.target.value)} placeholder="Click Browse to pick a video…" style={{ flex: 1 }} />
                <button className="ghost" onClick={browseVideo}>Browse…</button>
              </div>
            </div>
          ) : (
            <div className="field grow">
              <label>Connection string</label>
              <input value={conn} onChange={(e) => setConn(e.target.value)} placeholder={TYPE_TEMPLATES[type] || "..."} />
            </div>
          )}
          <div className="field">
            <label>&nbsp;</label>
            <button className="primary" onClick={add} disabled={!label.trim() || !conn.trim()}>+ Add Source</button>
          </div>
        </div>
      </div>

      {/* Auto-detect cameras */}
      <DetectCamerasPanel
        onAdd={(idx, name) => { setType("usb"); setConn(String(idx)); setLabel(name); setUsbDeviceName(name); }}
        configured={cameras}
      />

      {/* Existing camera sources */}
      {cameras.length > 0 && (
        <table className="table" style={{ marginTop: 12 }}>
          <thead>
            <tr>
              <th>Label</th>
              <th>Type</th>
              <th>Connection</th>
              <th>Status</th>
              <th title="Include in ANPR processing when the service starts">Tracked</th>
              <th>Last check</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {cameras.map((c) => {
              const isEditing = editingId === c.id;
              const isTesting = testingId === c.id;
              const tr = testResult[c.id];
              const isDeleting = deleteConfirm === c.id;
              return (
                <tr key={c.id}>
                  <td>
                    {isEditing ? (
                      <input value={editLabel} onChange={(e) => setEditLabel(e.target.value)} style={{ fontWeight: 600 }} />
                    ) : (
                      <b>{c.label}</b>
                    )}
                  </td>
                  <td>
                    {isEditing ? (
                      <select value={editType} onChange={(e) => setEditType(e.target.value as CameraSourceView["source_type"])} style={{ width: "auto", fontSize: 12 }}>
                        {SOURCE_TYPES.map((t) => (<option key={t} value={t}>{t}</option>))}
                      </select>
                    ) : (
                      <span className="badge">{c.source_type}</span>
                    )}
                  </td>
                  <td>
                    {isEditing ? (
                      <div className="row" style={{ gap: 6 }}>
                        <input value={editConn} onChange={(e) => setEditConn(e.target.value)} style={{ fontFamily: "monospace", fontSize: 12 }} />
                        {editType === "video_file" && (
                          <button className="ghost small" onClick={browseVideoForEdit}>Browse…</button>
                        )}
                      </div>
                    ) : (
                      <span className="small" style={{ wordBreak: "break-all" }}>{c.connection_string}</span>
                    )}
                  </td>
                  <td><span className={`badge ${c.status === "active" ? "active" : "disabled"}`}>{c.status}</span></td>
                  <td>
                    <input
                      type="checkbox"
                      title="Include this source in ANPR processing when the service starts"
                      checked={c.tracked}
                      onChange={(e) => {
                        const v = e.target.checked;
                        onRun(() => api.setCameraSourceTracked(actor.id, c.id, v), v ? "Tracking enabled." : "Tracking disabled.");
                      }}
                    />
                  </td>
                  <td className="small muted">{c.last_connection_check_result ?? "—"}</td>
                  <td>
                    {tr && (
                      <div className="small" style={{ marginBottom: 4, color: tr.ok ? "var(--success)" : "var(--danger)" }}>
                        {tr.msg}
                      </div>
                    )}
                    <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
                      {isEditing ? (
                        <>
                          <button className="primary small" onClick={() => saveEdit(c.id)}>Save</button>
                          <button className="ghost small" onClick={() => setEditingId(null)}>Cancel</button>
                        </>
                      ) : isDeleting ? (
                        <>
                          <span className="small" style={{ color: "var(--danger)" }}>Delete?</span>
                          <button className="ghost small" style={{ color: "var(--danger)" }} onClick={() => doDelete(c.id)}>Yes</button>
                          <button className="ghost small" onClick={() => setDeleteConfirm(null)}>No</button>
                        </>
                      ) : (
                        <>
                          <button className="ghost small" onClick={() => testConnection(c.id)} disabled={isTesting}>
                            {isTesting ? "Testing…" : "Test"}
                          </button>
                          <button className="ghost small" onClick={() => startEdit(c)}>Edit</button>
                          {c.status === "active" ? (
                            <button className="ghost small" onClick={() => onRun(() => api.setCameraSourceStatus(actor.id, c.id, "inactive"), "Deactivated.")}>Deactivate</button>
                          ) : (
                            <button className="ghost small" onClick={() => onRun(() => api.setCameraSourceStatus(actor.id, c.id, "active"), "Reactivated.")}>Activate</button>
                          )}
                          <button className="ghost small" style={{ color: "var(--danger)" }} onClick={() => setDeleteConfirm(c.id)}>Delete</button>
                        </>
                      )}
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Engine & Threshold — engine swap + per-engine settings
// ---------------------------------------------------------------------------

/** Shows cloud credential status (masked) with link to Credentials tab. */
function CloudCredentialStatus(_props: { actor: SessionUser }) {
  const [creds, setCreds] = useState<AnprCredentialView[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api.listAnprCredentials()
      .then((c) => { setCreds(c); setLoading(false); })
      .catch(() => setLoading(false));
  }, []);

  const apiKey = creds.find((c) => c.key_name === "cloud_anpr_api_key");
  const hasKey = apiKey && apiKey.masked_value && !apiKey.masked_value.startsWith("••••••••••••");

  if (loading) return null;

  return (
    <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 12, background: "var(--surface-2)", marginTop: 8 }}>
      <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 6 }}>Cloud API credentials</div>
      <div className="row" style={{ gap: 12, alignItems: "center" }}>
        <div className="field grow" style={{ marginBottom: 0 }}>
          <label style={{ fontSize: 11 }}>API endpoint</label>
          <input
            type="text"
            value={hasKey ? "Configured — edit in Credentials tab" : "Not configured"}
            readOnly
            style={{ fontFamily: "monospace", fontSize: 12, opacity: 0.7, cursor: "not-allowed" }}
          />
        </div>
        <div className="field grow" style={{ marginBottom: 0 }}>
          <label style={{ fontSize: 11 }}>API key</label>
          <input
            type="password"
            value={hasKey ? apiKey!.masked_value : ""}
            readOnly
            placeholder={hasKey ? "••••••••" : "Not set — go to Credentials tab"}
            style={{ fontFamily: "monospace", fontSize: 12, opacity: hasKey ? 0.7 : 0.5, cursor: "not-allowed" }}
          />
        </div>
      </div>
      <p className="muted small" style={{ marginTop: 6, marginBottom: 0 }}>
        {hasKey ? (
          <>API key is configured and masked. To change it, go to the <b>Credentials</b> tab.</>
        ) : (
          <>No cloud API key configured. Go to the <b>Credentials</b> tab to add your Roboflow (or other provider) API key.</>
        )}
      </p>
    </div>
  );
}

function EngineTab({
  config,
  onSave,
  onRun,
  actor,
  serviceRunning,
  onStopped,
  anprUrl,
}: {
  config: AnprConfigView;
  onSave: (changes: Partial<AnprConfigView>) => void;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
  actor: SessionUser;
  serviceRunning: boolean;
  onStopped?: () => void;
  anprUrl: string;
}) {
  const [engine, setEngine] = useState<OcrEngine>(config.active_ocr_engine);
  const [paddle, setPaddle] = useState(config.confidence_threshold_paddleocr);
  const [easy, setEasy] = useState(config.confidence_threshold_easyocr);
  const [ratio, setRatio] = useState(config.plate_vehicle_ratio_threshold);
  const [rules, setRules] = useState(config.plate_format_rules ?? "");
  const [confirmRequired, setConfirmRequired] = useState(config.discharge_confirmation_required);
  const [saveImages, setSaveImages] = useState(config.save_recognition_images);
  const [retrain, setRetrain] = useState(config.retrain_candidate_threshold?.toString() ?? "");
  const [isCapturePoint, setIsCapturePoint] = useState(config.is_capture_point);
  const [userAutoStart, setUserAutoStart] = useState(false);
  const [preferCloud, setPreferCloud] = useState(config.prefer_cloud);
  const [plateMode, setPlateMode] = useState<string>('universal');
  const [confirmingSwap, setConfirmingSwap] = useState(false);
  const [machineInfo, setMachineInfo] = useState<MachineInfo | null>(null);
  const [designatedMachine, setDesignatedMachine] = useState<string | null>(config.designated_machine_id ?? null);
  const [machineMatch, setMachineMatch] = useState<boolean | null>(null);
  const [serviceMessage, setServiceMessage] = useState<string | null>(null);

  // Resync local state when saved config reloads
  useEffect(() => {
    setEngine(config.active_ocr_engine);
    setPaddle(config.confidence_threshold_paddleocr);
    setEasy(config.confidence_threshold_easyocr);
    setRatio(config.plate_vehicle_ratio_threshold);
    setRules(config.plate_format_rules ?? "");
    setConfirmRequired(config.discharge_confirmation_required);
    setSaveImages(config.save_recognition_images);
    setRetrain(config.retrain_candidate_threshold?.toString() ?? "");
    setIsCapturePoint(config.is_capture_point);
    setPreferCloud(config.prefer_cloud);
  }, [config]);

  // Load machine info, user auto-start preference, and plate mode on mount
  useEffect(() => {
    api.getMachineInfo().then(setMachineInfo).catch(() => undefined);
    api.checkMachineMatch().then(setMachineMatch).catch(() => undefined);
    api.getUserAutoStart(actor.id).then(setUserAutoStart).catch(() => undefined);
    // Load OCR plate mode from app_settings
    invoke<string | null>('get_app_setting', { key: 'ocr_plate_mode' })
      .then((v: string | null) => { if (v) setPlateMode(v); })
      .catch(() => undefined);
  }, [actor.id]);

  const num = (v: string, fallback: number) => {
    const n = Number(v);
    return Number.isFinite(n) ? n : fallback;
  };

  const activeThreshold = engine === "easyocr" ? easy : paddle;

  const buildChanges = (): Partial<AnprConfigView> => ({
    active_ocr_engine: engine,
    confidence_threshold_paddleocr: paddle,
    confidence_threshold_easyocr: easy,
    plate_vehicle_ratio_threshold: ratio,
    plate_format_rules: rules.trim() ? rules.trim() : null,
    discharge_confirmation_required: confirmRequired,
    save_recognition_images: saveImages,
    retrain_candidate_threshold: retrain === "" ? null : num(retrain, 0),
    is_capture_point: isCapturePoint,
    prefer_cloud: preferCloud,
  });

  const save = async () => {
    // Save plate mode to app_settings (separate from ANPR config)
    if (plateMode !== 'universal') {
      await api.setOcrPlateMode(actor.id, plateMode);
    } else {
      await api.setOcrPlateMode(actor.id, 'universal');
    }
    if (engine !== config.active_ocr_engine) {
      setConfirmingSwap(true);
    } else {
      onSave(buildChanges());
    }
  };

  const handleDesignateMachine = () => {
    onRun(async () => {
      const info = await api.setAnprMachine(actor.id);
      setDesignatedMachine(info.machine_id);
      setMachineMatch(true);
    }, "This machine is now designated for ANPR auto-start.");
  };

  const handleStartService = async () => {
    setServiceMessage(null);
    try {
      // Find first active camera
      const cameras = await api.listCameraSources();
      const active = cameras.find(c => c.status === "active");
      if (!active) {
        setServiceMessage("No active camera source. Add one in Camera Settings.");
        return;
      }
      await api.writeAnprConfig(actor.id, active.connection_string, active.source_type, false);
      await api.startAnprService(actor.id);
      setServiceMessage("ANPR service starting...");
      // Poll until it's up — Python + OCR models can take 15-25s to start
      let tries = 0;
      const check = async () => {
        try {
          const resp = await fetch(`${anprUrl}/health`);
          if (resp.ok) {
            setServiceMessage("ANPR service running.");
            onStopped?.(); // refresh diagnostics (works for both start and stop)
            return;
          }
        } catch {}
        if (++tries < 30) setTimeout(check, 1000);
        else setServiceMessage("ANPR service started but not responding yet — check logs.");
      };
      setTimeout(check, 2000);
    } catch (e: any) {
      setServiceMessage(`Failed: ${e}`);
    }
  };

  const handleStopService = async () => {
    setServiceMessage(null);
    try {
      await api.stopAnprService(actor.id);
      setServiceMessage("ANPR service stopped.");
      onStopped?.();
    } catch (e: any) {
      setServiceMessage(`Failed: ${e}`);
    }
  };

  return (
    <div className="card stack">
      <div className="section-title" style={{ fontSize: 15 }}>Recognition engine</div>
      <p className="muted small" style={{ marginTop: -6 }}>
        The OCR engine that reads plates from the camera feed. A read below the active engine's threshold is queued for
        a human instead of being logged automatically.
      </p>

      <div className="field">
        <label>Active OCR engine</label>
        <div className="seg">
          <button className={engine === "paddleocr" ? "active" : ""} onClick={() => setEngine("paddleocr")}>PaddleOCR</button>
          <button
            className=""
            disabled
            title="EasyOCR is disabled — PaddleOCR is the recommended engine"
            style={{ opacity: 0.5, cursor: "not-allowed", position: "relative" }}
          >
            EasyOCR ⛔
          </button>
          <button className={engine === "cloud_provider" ? "active" : ""} onClick={() => setEngine("cloud_provider")}>
            Cloud provider
          </button>
        </div>
        <p className="muted small">
          {engine === "cloud_provider" ? (
            <>Cloud OCR replaces only the character-reading step — detection, tracking, and entry/exit keep running
              locally. If the cloud API is unreachable, reads fall back to the local engine automatically.
              Configure the API key in the <b>Credentials</b> tab.</>
          ) : (
            <>Thresholds are tuned per engine — the active engine's threshold ({activeThreshold.toFixed(2)}) gates
              recognition confidence right now.</>
          )}
        </p>
      </div>

      {/* Prefer cloud OCR toggle — independent of engine selection */}
      <div className="field">
        <label>Prefer cloud OCR</label>
        <div className="switch">
<input
              type="checkbox"
              checked={preferCloud}
              onChange={(e) => setPreferCloud(e.target.checked)}
              aria-label="Prefer cloud OCR engine for character reading"
            />
          <span className="slider round"></span>
        </div>
        <p className="muted small" style={{ marginTop: 4, marginBottom: 0 }}>
          When enabled, the configured cloud OCR API is tried first for each plate read.
          If the cloud API is unreachable or returns no result, the local OCR engine
          (PaddleOCR) is used automatically — the read never fails.
        </p>
      </div>

      {/* OCR Plate Mode — Universal vs Kenyan */}
      <div className="field">
        <label>Plate format filter</label>
        <div style={{ display: 'flex', gap: 8, marginTop: 4 }}>
          <button
            className={plateMode === 'universal' ? 'active' : ''}
            onClick={() => setPlateMode('universal')}
          >
            Universal (any plate)
          </button>
          <button
            className={plateMode === 'kenyan' ? 'active' : ''}
            onClick={() => setPlateMode('kenyan')}
          >
            Kenyan only (KBA 123A)
          </button>
        </div>
        <p className="muted small" style={{ marginTop: 4, marginBottom: 0 }}>
          {plateMode === 'kenyan'
            ? 'Only plates matching Kenyan format (3 letters + 3 digits + 1 letter) are accepted. Other plates are rejected.'
            : 'Any plate format is accepted. Switch to Kenyan only if you want to filter out non-Kenyan plates.'}
        </p>
      </div>

      {/* Cloud provider credential status — shows if configured, masks the actual value */}
      {preferCloud && (
        <CloudCredentialStatus actor={actor} />
      )}

      {engine !== "cloud_provider" && (
        <div className="row">
          <div className="field grow">
            <label>PaddleOCR threshold</label>
            <input type="number" min={0} max={1} step={0.05} value={paddle} onChange={(e) => setPaddle(num(e.target.value, config.confidence_threshold_paddleocr))} />
          </div>
          <div className="field grow">
            <label>EasyOCR threshold</label>
            <input type="number" min={0} max={1} step={0.05} value={easy} onChange={(e) => setEasy(num(e.target.value, config.confidence_threshold_easyocr))} />
          </div>
          <div className="field grow">
            <label>Plate-vehicle ratio threshold</label>
            <input type="number" min={0} max={1} step={0.01} value={ratio} onChange={(e) => setRatio(num(e.target.value, config.plate_vehicle_ratio_threshold))} />
          </div>
        </div>
      )}

      {engine === "cloud_provider" && (
        <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 12, background: "var(--surface-2)" }}>
          <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 6 }}>Local fallback thresholds</div>
          <p className="muted small" style={{ marginTop: 0, marginBottom: 8 }}>
            These apply when the cloud API is unreachable — the local engine uses them as usual.
          </p>
          <div className="row">
            <div className="field grow">
              <label>PaddleOCR threshold</label>
              <input type="number" min={0} max={1} step={0.05} value={paddle} onChange={(e) => setPaddle(num(e.target.value, config.confidence_threshold_paddleocr))} />
            </div>
            <div className="field grow">
              <label>EasyOCR threshold</label>
              <input type="number" min={0} max={1} step={0.05} value={easy} onChange={(e) => setEasy(num(e.target.value, config.confidence_threshold_easyocr))} />
            </div>
            <div className="field grow">
              <label>Plate-vehicle ratio threshold</label>
              <input type="number" min={0} max={1} step={0.01} value={ratio} onChange={(e) => setRatio(num(e.target.value, config.plate_vehicle_ratio_threshold))} />
            </div>
          </div>
        </div>
      )}

      <div className="field">
        <label>Plate format rules (regex, optional)</label>
        <input value={rules} onChange={(e) => setRules(e.target.value)} placeholder="e.g. ^\d{3}[A-Z]{2,3}$" />
      </div>

      {/* ANPR Service Start/Stop */}
      <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, background: "var(--surface-2)" }}>
        <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 8 }}>ANPR Service Control</div>
        <div className="row" style={{ gap: 8, alignItems: "center" }}>
          <span className={`badge ${serviceRunning ? "active" : "disabled"}`}>
            {serviceRunning ? "Service Running" : "Service Stopped"}
          </span>
          {!serviceRunning ? (
            <button className="primary small" onClick={handleStartService}>Start ANPR Service</button>
          ) : (
            <button className="danger small" onClick={handleStopService}>Stop ANPR Service</button>
          )}
        </div>
        {serviceMessage && (
          <div className="muted small" style={{ marginTop: 6, padding: "4px 8px", background: "var(--surface)", borderRadius: 4 }}>
            {serviceMessage}
          </div>
        )}
      </div>

      {/* Machine Detection for Auto-Start */}
      <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, background: "var(--surface-2)" }}>
        <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 8 }}>Machine & Auto-Start</div>
        <p className="muted small" style={{ marginTop: -4, marginBottom: 10 }}>
          Enable auto-start for your account on this specific computer. Each user on each machine
          controls their own preference independently.
        </p>
        <div className="row" style={{ gap: 8, alignItems: "center", marginBottom: 10 }}>
          <label className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
            <input
              style={{ width: "auto" }}
              type="checkbox"
              checked={userAutoStart}
              onChange={(e) => {
                const next = e.target.checked;
                setUserAutoStart(next);
                api.setUserAutoStart(actor.id, next)
                  .then(() => undefined)
                  .catch((err) => { setUserAutoStart(!next); alert(`Failed: ${err}`); });
              }}
            />
            <span>Auto-start ANPR on this machine for my account</span>
          </label>
        </div>
        {machineInfo && (
          <div style={{ marginBottom: 10 }}>
            <div className="muted small">This machine:</div>
            <div style={{ fontFamily: "monospace", fontSize: 12, padding: "4px 8px", background: "var(--surface)", borderRadius: 4, marginTop: 4 }}>
              <div>Hostname: <b>{machineInfo.hostname}</b></div>
              <div>MAC: <b>{machineInfo.mac_address}</b></div>
              <div>ID: <b>{machineInfo.machine_id}</b></div>
            </div>
          </div>
        )}
        {designatedMachine && (
          <div style={{ marginBottom: 10 }}>
            <div className="muted small">Designated ANPR machine:</div>
            <div style={{ fontFamily: "monospace", fontSize: 12, padding: "4px 8px", background: "var(--surface)", borderRadius: 4, marginTop: 4 }}>
              {designatedMachine}
            </div>
            {machineMatch !== null && (
              <div style={{ marginTop: 4 }}>
                <span className={`badge ${machineMatch ? "active" : "disabled"}`}>
                  {machineMatch ? "This machine matches" : "Different machine — auto-start disabled"}
                </span>
              </div>
            )}
          </div>
        )}
        <div className="row" style={{ gap: 8 }}>
          <button className="primary small" onClick={handleDesignateMachine}>
            Set This Machine as ANPR Machine
          </button>
          {designatedMachine && (
            <button className="ghost small" onClick={() => {
              onRun(async () => {
                await api.updateAnprConfig(actor.id, { designated_machine_id: "" });
                setDesignatedMachine(null);
                setMachineMatch(null);
              }, "Machine designation cleared.");
            }}>
              Clear Designation
            </button>
          )}
        </div>
      </div>

      <div className="row">
        <label className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
          <input style={{ width: "auto" }} type="checkbox" checked={isCapturePoint} onChange={(e) => setIsCapturePoint(e.target.checked)} />
          <span>Act as capture point (process ANPR readings)</span>
        </label>
        <label className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
          <input style={{ width: "auto" }} type="checkbox" checked={confirmRequired} onChange={(e) => setConfirmRequired(e.target.checked)} />
          <span>Discharge confirmation required</span>
        </label>
        <label className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
          <input style={{ width: "auto" }} type="checkbox" checked={saveImages} onChange={(e) => setSaveImages(e.target.checked)} />
          <span>Save recognition images</span>
        </label>
        <div className="field grow">
          <label>Retrain candidate threshold</label>
          <input type="number" min={0} value={retrain} onChange={(e) => setRetrain(e.target.value)} placeholder="unset" />
        </div>
      </div>

      <div>
        <button className="primary" onClick={save}>Save engine settings</button>
      </div>

      {confirmingSwap && (
        <div className="overlay" onClick={() => setConfirmingSwap(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3 style={{ marginTop: 0 }}>Switch the active OCR engine?</h3>
            <p className="muted small">
              Switching from <b>{config.active_ocr_engine}</b> to <b>{engine}</b> changes recognition behavior for every
              subsequent read. The switch is audit-logged (who, when, from/to).
            </p>
            {engine === "cloud_provider" && (
              <div style={{ padding: "8px 12px", background: "var(--surface-2)", borderRadius: 6, marginTop: 8, fontSize: 13, border: "1px solid var(--border)" }}>
                Cloud OCR replaces only the character-reading step. The local ANPR service (detection, tracking, entry/exit) keeps running. If the cloud API is unreachable, reads fall back to the local engine automatically.
              </div>
            )}
            <div className="row" style={{ marginTop: 14, gap: 8 }}>
              <button className="primary" onClick={() => onSave(buildChanges())}>Confirm switch</button>
              <button className="ghost" onClick={() => setConfirmingSwap(false)}>Cancel</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Credentials — masked API/license keys, rotatable
// ---------------------------------------------------------------------------

function CredentialsTab({
  credentials,
  actor,
  onRun,
}: {
  credentials: AnprCredentialView[];
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
}) {
  const [keyName, setKeyName] = useState("cloud_anpr_api_key");
  const [customKeyName, setCustomKeyName] = useState("");
  const [value, setValue] = useState("");
  const [rotating, setRotating] = useState<AnprCredentialView | null>(null);
  const [rotatingValue, setRotatingValue] = useState("");
  const [deleting, setDeleting] = useState<AnprCredentialView | null>(null);

  const effectiveKeyName = keyName === "custom" ? customKeyName.trim() : keyName;

  const predefinedKeys = [
    { value: "cloud_anpr_api_key", label: "Cloud ANPR API key", desc: "Powers the Cloud provider engine in Engine tab" },
    { value: "license_key", label: "TruckFlow license key", desc: "Product license for premium features" },
    { value: "custom", label: "Custom…", desc: "Enter your own key name" },
  ];

  const add = () =>
    onRun(async () => {
      await api.setAnprCredential(actor.id, effectiveKeyName, value);
      setValue("");
      setCustomKeyName("");
    }, "Credential saved.");

  const rotate = (c: AnprCredentialView) =>
    onRun(async () => {
      await api.setAnprCredential(actor.id, c.key_name, rotatingValue);
      setRotating(null);
      setRotatingValue("");
    }, "Credential rotated.");

  return (
    <div className="card stack">
      <div className="section-title" style={{ fontSize: 15 }}>API & license keys</div>
      <p className="muted small" style={{ marginTop: -6 }}>
        Keys are stored in the local database and <b>never shown in full</b> — only a masked preview. Rotating a key
        keeps the old value unusable (audit-logged with who and when).
      </p>

      {credentials.length === 0 ? (
        <p className="muted small">No keys stored yet. Add the cloud ANPR provider key below to enable it as an engine.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Key</th>
              <th>Value</th>
              <th>Rotated</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {credentials.map((c) => (
              <tr key={c.id}>
                <td><code>{c.key_name}</code></td>
                <td><code>{c.masked_value}</code></td>
                <td className="small muted">
                  {c.rotated_at ? `${new Date(c.rotated_at).toLocaleString()}${c.rotated_by ? ` by ${c.rotated_by}` : ""}` : "—"}
                </td>
                <td>
                  <div className="row" style={{ gap: 6 }}>
                    {rotating?.id === c.id ? (
                      <>
                        <input type="password" value={rotatingValue} onChange={(e) => setRotatingValue(e.target.value)} placeholder="New key value" style={{ maxWidth: 220 }} />
                        <button className="primary small" onClick={() => rotate(c)} disabled={!rotatingValue.trim()}>Save</button>
                        <button className="ghost small" onClick={() => { setRotating(null); setRotatingValue(""); }}>Cancel</button>
                      </>
                    ) : (
                      <>
                        <button className="ghost small" onClick={() => { setRotating(c); setRotatingValue(""); }}>Rotate</button>
                        <button className="ghost small" style={{ color: "var(--danger)" }} onClick={() => setDeleting(c)}>Delete</button>
                      </>
                    )}
                  </div>
                  {deleting?.id === c.id && (
                    <div className="row" style={{ gap: 6, marginTop: 6 }}>
                      <span className="small" style={{ color: "var(--danger)" }}>Delete {c.key_name}? This cannot be undone.</span>
                      <button className="ghost small" style={{ color: "var(--danger)" }} onClick={() => onRun(() => api.deleteAnprCredential(actor.id, c.key_name), "Credential deleted.")}>Yes, delete</button>
                      <button className="ghost small" onClick={() => setDeleting(null)}>Cancel</button>
                    </div>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, background: "var(--surface-2)" }}>
        <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 8 }}>Add or replace a key</div>
        <div className="row">
          <div className="field" style={{ maxWidth: 260 }}>
            <label>Key name</label>
            <select value={keyName} onChange={(e) => setKeyName(e.target.value)}>
              {predefinedKeys.map((k) => (
                <option key={k.value} value={k.value}>{k.label}</option>
              ))}
            </select>
            {keyName === "custom" && (
              <input
                value={customKeyName}
                onChange={(e) => setCustomKeyName(e.target.value)}
                placeholder="e.g. my_api_key"
                style={{ marginTop: 6 }}
              />
            )}
          </div>
          <div className="field grow">
            <label>Value</label>
            <input type="password" value={value} onChange={(e) => setValue(e.target.value)} placeholder="Paste the API key or license…" />
          </div>
          <div className="field">
            <label>&nbsp;</label>
            <button className="primary" onClick={add} disabled={!value.trim() || !effectiveKeyName}>Save key</button>
          </div>
        </div>
        <p className="muted small" style={{ margin: 0 }}>
          {predefinedKeys.find(k => k.value === keyName)?.desc || "Enter a custom key name and value."}
        </p>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Models — version history, deploy, rollback
// ---------------------------------------------------------------------------

function ModelPanel({
  versions,
  actor,
  onRun,
}: {
  versions: ModelVersionView[];
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
}) {
  const [label, setLabel] = useState("");
  const [component, setComponent] = useState("detection");
  const [accuracy, setAccuracy] = useState("");

  const register = () =>
    onRun(
      () =>
        api
          .registerModelVersion(actor.id, label, component, accuracy === "" ? null : Number(accuracy))
          .then(() => undefined),
      "Model version registered (non-live).",
    );

  return (
    <div className="card stack">
      <div className="section-title" style={{ fontSize: 15 }}>Models &amp; versions ({versions.length})</div>
      <p className="muted small">
        A model can only go live after validation accuracy is recorded. Deployment and rollback are explicit admin
        actions — never automatic.
      </p>

      <div className="row">
        <div className="field grow">
          <label>Version label</label>
          <input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="plate-det-v3" />
        </div>
        <div className="field">
          <label>Component</label>
          <select value={component} onChange={(e) => setComponent(e.target.value)}>
            <option value="detection">detection</option>
            <option value="recognition">recognition</option>
          </select>
        </div>
        <div className="field">
          <label>Validation accuracy</label>
          <input type="number" min={0} max={1} step={0.001} value={accuracy} onChange={(e) => setAccuracy(e.target.value)} placeholder="optional now" />
        </div>
        <div className="field">
          <label>&nbsp;</label>
          <button className="primary" onClick={register} disabled={!label.trim()}>Register</button>
        </div>
      </div>

      {versions.length > 0 && (
        <table className="table">
          <thead>
            <tr>
              <th>Version</th>
              <th>Component</th>
              <th>Validation</th>
              <th>Status</th>
              <th>Deployed</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {versions.map((v) => (
              <tr key={v.id}>
                <td>{v.version_label}</td>
                <td>{v.component}</td>
                <td>{v.validation_accuracy != null ? (v.validation_accuracy * 100).toFixed(1) + "%" : "—"}</td>
                <td>{v.is_live ? <span className="chip" style={{ color: "var(--accent)", fontWeight: 700 }}>LIVE</span> : "candidate"}</td>
                <td className="muted small">{v.deployed_at ?? (v.rolled_back_from ? "rolled back" : "—")}</td>
                <td>
                  {!v.is_live && v.validation_accuracy != null && (
                    <button className="primary" onClick={() => onRun(() => api.deployModelVersion(actor.id, v.id), "Model deployed.")}>Deploy</button>
                  )}
                  {!v.is_live && versions.some((x) => x.is_live && x.component === v.component) && (
                    <button className="ghost" onClick={() => onRun(() => api.rollbackModelVersion(actor.id, v.id), "Rolled back.")}>Rollback</button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Training — candidates pool + retrain trigger
// ---------------------------------------------------------------------------

function CandidatePanel({
  candidates,
  config,
  actor,
  onRun,
}: {
  candidates: TrainingCandidateView[];
  config: AnprConfigView | null;
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
}) {
  const [threshold, setThreshold] = useState(config?.retrain_candidate_threshold?.toString() ?? "");
  const [dirty, setDirty] = useState(false);
  const [uploadPlate, setUploadPlate] = useState("");
  const [uploading, setUploading] = useState(false);
  const [expandedFrame, setExpandedFrame] = useState<string | null>(null);

  // Keep the input in sync when the saved config reloads after a save.
  useEffect(() => {
    setThreshold(config?.retrain_candidate_threshold?.toString() ?? "");
    setDirty(false);
  }, [config?.retrain_candidate_threshold]);

  const saveThreshold = () => {
    onRun(
      () =>
        api
          .updateAnprConfig(actor.id, { retrain_candidate_threshold: threshold === "" ? null : Number(threshold) })
          .then(() => undefined),
      "Notification threshold saved.",
    );
    setDirty(false);
  };

  const handleFileUpload = async () => {
    const path = await open({
      multiple: false,
      filters: [{ name: "Images", extensions: ["jpg", "jpeg", "png", "bmp"] }],
    });
    if (!path || typeof path !== "string") return;
    setUploading(true);
    try {
      await api.addTrainingCandidate(actor.id, uploadPlate.trim() || "UNKNOWN", path);
      setUploadPlate("");
      onRun(() => api.listTrainingCandidates().then(() => undefined), "Candidate added.");
    } catch (e: any) {
      alert(`Upload failed: ${e}`);
    } finally {
      setUploading(false);
    }
  };

  const handleApprove = (id: string) => {
    onRun(() => api.approveTrainingCandidate(actor.id, id).then(() => undefined), "Candidate approved.");
  };

  const handleReject = (id: string) => {
    onRun(() => api.rejectTrainingCandidate(actor.id, id).then(() => undefined), "Candidate rejected.");
  };

  const handleApproveAll = () => {
    onRun(() => api.approveAllTrainingCandidates(actor.id).then(() => undefined), "All candidates approved.");
  };

  const handleRejectAll = () => {
    onRun(() => api.rejectAllTrainingCandidates(actor.id).then(() => undefined), "All candidates cleared.");
  };

  return (
    <div className="card stack">
      <div className="section-title" style={{ fontSize: 15 }}>Training candidates ({candidates.length})</div>
      <p className="muted small">
        Low-confidence reads and human-corrected plates are auto-collected here for future retraining.
      </p>

      {/* Manual upload section */}
      <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, background: "var(--surface-2)" }}>
        <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 8 }}>Add training image</div>
        <div className="row" style={{ gap: 8, alignItems: "flex-end" }}>
          <div className="field" style={{ maxWidth: 180 }}>
            <label>Plate number</label>
            <input value={uploadPlate} onChange={(e) => setUploadPlate(e.target.value)} placeholder="e.g. ABC 123" />
          </div>
          <div className="field grow">
            <label>Frame image</label>
            <button className="ghost" onClick={handleFileUpload} disabled={uploading}>
              {uploading ? "Uploading…" : "Choose file…"}
            </button>
          </div>
        </div>
      </div>

      {/* Threshold config */}
      <div className="row" style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 12 }}>
        <div className="field grow">
          <label>Notify when candidate pool reaches</label>
          <input
            type="number"
            min={0}
            value={threshold}
            onChange={(e) => { setThreshold(e.target.value); setDirty(true); }}
            placeholder="unset"
          />
        </div>
        <div className="field">
          <label>&nbsp;</label>
          <button className="primary" onClick={saveThreshold} disabled={!dirty}>Save threshold</button>
        </div>
      </div>

      {/* Batch actions */}
      {candidates.length > 0 && (
        <div className="row" style={{ gap: 8 }}>
          <button className="ghost small" onClick={handleApproveAll}>Approve all ({candidates.length})</button>
          <button
            className="ghost small"
            style={{ color: "var(--danger)" }}
            onClick={() => {
              if (!window.confirm(`Remove all ${candidates.length} candidates? This cannot be undone.`)) return;
              handleRejectAll();
            }}
          >
            Clear all
          </button>
        </div>
      )}

      {candidates.length === 0 ? (
        <p className="muted small">No candidates yet — they appear as low-confidence or corrected reads are processed.</p>
      ) : (
        <div className="stack" style={{ gap: 8 }}>
          {candidates.map((c) => (
            <div key={c.id} style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 10, background: "var(--surface)" }}>
              <div
                className="row between"
                style={{ gap: 10, cursor: "pointer" }}
                onClick={() => setExpandedFrame(expandedFrame === c.id ? null : c.id)}
              >
                <div className="row" style={{ gap: 10, alignItems: "center", flex: 1 }}>
                  {/* Frame thumbnail */}
                  {c.frame_ref && (
                    <img
                      src={convertFileSrc(c.frame_ref)}
                      alt="Frame"
                      style={{ width: 80, height: 60, objectFit: "cover", borderRadius: 4, border: "1px solid var(--border)" }}
                      onError={(e) => { (e.target as HTMLImageElement).style.display = "none"; }}
                    />
                  )}
                  <div style={{ flex: 1 }}>
                    <div className="row" style={{ gap: 6, alignItems: "center" }}>
                      <span className="plate-font">{c.plate_number ?? "—"}</span>
                      <span className="badge">{c.reason}</span>
                      {c.confidence != null && (
                        <span className="badge" style={{ background: c.confidence >= 0.7 ? "var(--success)" : c.confidence >= 0.5 ? "var(--warning, #f59e0b)" : "var(--danger)", color: "#fff" }}>
                          {(c.confidence * 100).toFixed(1)}%
                        </span>
                      )}
                    </div>
                    <div className="muted small" style={{ marginTop: 2 }}>
                      {c.source_trip_id ? `Trip ${c.source_trip_id.slice(0, 8)}` : "Manual upload"} · {new Date(c.created_at).toLocaleString()}
                    </div>
                  </div>
                </div>
                <div className="row" style={{ gap: 4, alignItems: "center" }}>
                  <span className="muted small">{expandedFrame === c.id ? "▲" : "▼"}</span>
                  <button className="ghost small" onClick={(e) => { e.stopPropagation(); handleApprove(c.id); }}>Approve</button>
                  <button className="ghost small" style={{ color: "var(--danger)" }} onClick={(e) => { e.stopPropagation(); handleReject(c.id); }}>Reject</button>
                </div>
              </div>
              {/* Expanded details — full info + captured frame */}
              {expandedFrame === c.id && (
                <div style={{ marginTop: 10, padding: 10, background: "var(--surface-2)", borderRadius: 6, border: "1px solid var(--border)" }}>
                  <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr 1fr", gap: 8, marginBottom: 10 }}>
                    <div>
                      <div className="muted small">Plate</div>
                      <div style={{ fontWeight: 600, fontFamily: "monospace" }}>{c.plate_number ?? "—"}</div>
                    </div>
                    <div>
                      <div className="muted small">Confidence</div>
                      <div style={{ fontWeight: 600 }}>{c.confidence != null ? `${(c.confidence * 100).toFixed(1)}%` : "—"}</div>
                    </div>
                    <div>
                      <div className="muted small">OCR Engine</div>
                      <div style={{ fontWeight: 600 }}>{c.ocr_engine ?? "—"}</div>
                    </div>
                    <div>
                      <div className="muted small">Capture method</div>
                      <div style={{ fontWeight: 600 }}>{c.capture_method ?? "—"}</div>
                    </div>
                    <div>
                      <div className="muted small">Captured at</div>
                      <div style={{ fontWeight: 600 }}>{c.captured_at ? new Date(c.captured_at).toLocaleString() : "—"}</div>
                    </div>
                    <div>
                      <div className="muted small">Reason</div>
                      <div style={{ fontWeight: 600 }}>{c.reason === "low_confidence" ? "Low confidence read" : "Human corrected"}</div>
                    </div>
                    <div>
                      <div className="muted small">Source trip</div>
                      <div style={{ fontWeight: 600, fontFamily: "monospace", fontSize: 12 }}>{c.source_trip_id ?? "Manual upload"}</div>
                    </div>
                    <div>
                      <div className="muted small">Added</div>
                      <div style={{ fontWeight: 600 }}>{new Date(c.created_at).toLocaleString()}</div>
                    </div>
                  </div>
                  {/* Captured frame — mandatory display */}
                  {c.frame_ref ? (
                    <div style={{ textAlign: "center" }}>
                      <div className="muted small" style={{ marginBottom: 4 }}>Captured frame:</div>
                      <img
                        src={convertFileSrc(c.frame_ref)}
                        alt="Captured frame"
                        style={{ maxWidth: "100%", maxHeight: 300, borderRadius: 4, border: "1px solid var(--border)" }}
                        onError={(e) => { (e.target as HTMLImageElement).style.display = "none"; }}
                      />
                    </div>
                  ) : (
                    <div className="muted small" style={{ textAlign: "center", padding: 12 }}>No frame image available</div>
                  )}
                </div>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Diagnostics — dependency health, storage, error log, confidence trend
// ---------------------------------------------------------------------------

/** Manages captured detection frames — view, delete individual, or clear all. */
function CapturedFramesManager({ actor, onRefresh }: { actor: SessionUser; onRefresh: () => void }) {
  const [frames, setFrames] = useState<{ trip_id: string; kind: string; filename: string; size_bytes: number; modified: string }[]>([]);
  const [loading, setLoading] = useState(true);
  const [deleting, setDeleting] = useState(false);
  const [expandedFrame, setExpandedFrame] = useState<string | null>(null);
  const [preview, setPreview] = useState<string | null>(null);

  const loadFrames = useCallback(() => {
    setLoading(true);
    api.listDetectionImages(100)
      .then((f) => { setFrames(f); setLoading(false); })
      .catch(() => setLoading(false));
  }, []);

  useEffect(() => { loadFrames(); }, [loadFrames]);

  const totalBytes = frames.reduce((s, f) => s + f.size_bytes, 0);

  const handleDeleteOne = async (tripId: string, _kind: string, _filename: string) => {
    setDeleting(true);
    try {
      await api.deleteDetectionFrames(actor.id, [tripId]);
      loadFrames();
      onRefresh();
    } finally {
      setDeleting(false);
    }
  };

  const handleClearAll = async () => {
    if (!window.confirm(`Delete ALL ${frames.length} captured frames (${formatBytes(totalBytes)})? This cannot be undone.`)) return;
    setDeleting(true);
    try {
      await api.deleteDetectionFrames(actor.id);
      loadFrames();
      onRefresh();
    } finally {
      setDeleting(false);
    }
  };

  const handlePreview = async (tripId: string, kind: string, filename: string) => {
    try {
      const b64 = await api.loadDetectionImage(tripId, kind, filename);
      setPreview(`data:image/jpeg;base64,${b64}`);
      setExpandedFrame(`${tripId}/${kind}/${filename}`);
    } catch {
      setPreview(null);
    }
  };

  return (
    <div style={{ marginTop: 12, padding: 12, border: "1px solid var(--border)", borderRadius: "var(--radius)" }}>
      <div className="row between" style={{ alignItems: "center", marginBottom: 8 }}>
        <div style={{ fontWeight: 600, fontSize: 13 }}>
          Captured frames ({frames.length}) — {formatBytes(totalBytes)}
        </div>
        <div className="row" style={{ gap: 6 }}>
          <button className="ghost small" onClick={loadFrames} disabled={loading}>Refresh</button>
          {frames.length > 0 && (
            <button className="danger small" onClick={handleClearAll} disabled={deleting}>
              {deleting ? "Deleting…" : "Clear All Frames"}
            </button>
          )}
        </div>
      </div>
      {loading ? (
        <div className="center-fill"><div className="spinner" /></div>
      ) : frames.length === 0 ? (
        <p className="muted small">No captured frames yet.</p>
      ) : (
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(180px, 1fr))", gap: 8 }}>
          {frames.map((f, i) => (
            <div key={i} style={{ border: "1px solid var(--border)", borderRadius: 6, overflow: "hidden", background: "var(--surface)" }}>
              <div
                style={{ height: 80, background: "#000", display: "flex", alignItems: "center", justifyContent: "center", cursor: "pointer" }}
                onClick={() => handlePreview(f.trip_id, f.kind, f.filename)}
              >
                <span className="muted small">{f.kind}</span>
              </div>
              <div style={{ padding: "4px 8px" }}>
                <div className="small" style={{ fontWeight: 600 }}>{f.filename}</div>
                <div className="muted small">{f.trip_id.slice(0, 8)}… · {formatBytes(f.size_bytes)}</div>
                <button
                  className="ghost small"
                  style={{ color: "var(--danger)", marginTop: 4, fontSize: 11 }}
                  onClick={() => handleDeleteOne(f.trip_id, f.kind, f.filename)}
                  disabled={deleting}
                >
                  Delete
                </button>
              </div>
            </div>
          ))}
        </div>
      )}
      {/* Preview modal */}
      {expandedFrame && preview && (
        <div className="overlay" onClick={() => { setExpandedFrame(null); setPreview(null); }}>
          <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: "80vw", maxHeight: "80vh" }}>
            <img src={preview} alt="Frame preview" style={{ maxWidth: "100%", maxHeight: "70vh", borderRadius: 6 }} />
            <div className="row" style={{ marginTop: 8, justifyContent: "flex-end" }}>
              <button className="ghost" onClick={() => { setExpandedFrame(null); setPreview(null); }}>Close</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function DiagnosticsTab({
  diagnostics,
  actor,
}: {
  diagnostics: AnprDiagnosticsView | null;
  actor: SessionUser;
}) {
  const [trend, setTrend] = useState<ConfidenceTrendPoint[]>([]);
  const [trendError, setTrendError] = useState<string | null>(null);
  const [storageExpanded, setStorageExpanded] = useState(false);

  useEffect(() => {
    api
      .anprConfidenceTrend(actor.id, null, null)
      .then(setTrend)
      .catch((e) => setTrendError(String(e)));
  }, [actor.id]);

  if (!diagnostics) {
    return (
      <div className="card">
        <div className="center-fill"><div className="spinner" /></div>
      </div>
    );
  }

  return (
    <div className="stack">
      <div className="card">
        <div className="section-title" style={{ fontSize: 15 }}>Dependency health</div>
        <p className="muted small" style={{ marginTop: -6 }}>
          A broken connection should never be a silent mystery — here's exactly what the pipeline needs.
        </p>
        <div className="health-grid">
          {diagnostics.dependencies.map((d) => (
            <div key={d.name} className={`health-card ${d.ok ? "ok" : "offline"}`}>
              <div className="row between">
                <b>{d.name}</b>
                <span className={`badge ${d.ok ? "active" : "disabled"}`}>{d.ok ? "✓ OK" : "✗ MISSING"}</span>
              </div>
              <div className="small muted" style={{ marginTop: 6 }}>{d.detail}</div>
            </div>
          ))}
        </div>
      </div>

      <div className="card">
        <div className="section-title" style={{ fontSize: 15 }}>Storage usage</div>
        <div
          className="row"
          style={{ gap: 8, marginTop: 6, cursor: "pointer", userSelect: "none" }}
          onClick={() => setStorageExpanded(!storageExpanded)}
        >
          <span className="health-value">{formatBytes(diagnostics.storage_bytes)}</span>
          <span className="muted small">{diagnostics.storage_detail}</span>
          <span className="muted small" style={{ marginLeft: "auto" }}>{storageExpanded ? "▲" : "▼"}</span>
        </div>
        {storageExpanded && diagnostics.storage_breakdown && diagnostics.storage_breakdown.length > 0 && (
          <div style={{ marginTop: 10, paddingLeft: 12, borderLeft: "2px solid var(--border)" }}>
            {diagnostics.storage_breakdown.map((item, i) => (
              <div key={i} className="row" style={{ gap: 8, padding: "4px 0" }}>
                <span className="small" style={{ minWidth: 140 }}>{item.label}</span>
                <span className="small muted">{formatBytes(item.bytes)}</span>
                <div style={{ flex: 1, height: 6, background: "var(--surface-2)", borderRadius: 3, overflow: "hidden" }}>
                  <div style={{
                    height: "100%",
                    width: `${diagnostics.storage_bytes > 0 ? (item.bytes / diagnostics.storage_bytes) * 100 : 0}%`,
                    background: "var(--accent)",
                    borderRadius: 3,
                  }} />
                </div>
              </div>
            ))}
          </div>
        )}
        {storageExpanded && (
          <CapturedFramesManager actor={actor} onRefresh={() => api.anprDiagnostics().then(() => {}).catch(() => {})} />
        )}
      </div>

      <div className="card">
        <div className="section-title" style={{ fontSize: 15 }}>Confidence trend</div>
        {trendError ? (
          <p className="muted small">{trendError}</p>
        ) : trend.length === 0 ? (
          <p className="muted small">No reads yet — the trend appears once the pipeline captures plates.</p>
        ) : (
          <div className="bar-chart" style={{ marginTop: 10 }}>
            {trend.slice(-14).map((t) => (
              <div key={t.date} className="bar-row" style={{ gridTemplateColumns: "90px 1fr 60px" }}>
                <div className="bar-label">{t.date}</div>
                <div className="bar-track">
                  <div className="bar-fill" style={{ width: `${(t.avg_confidence ?? 0) * 100}%`, background: "var(--accent)" }} />
                </div>
                <div className="bar-value small">
                  {t.avg_confidence != null ? `${Math.round(t.avg_confidence * 100)}%` : "—"}
                  <span className="muted"> · {t.reads}</span>
                </div>
              </div>
            ))}
          </div>
        )}
        {trend.length > 14 && <p className="muted small" style={{ marginTop: 8 }}>Showing the last 14 days.</p>}
      </div>

      <div className="card">
        <div className="section-title" style={{ fontSize: 15 }}>Service error log</div>
        {diagnostics.error_log.length === 0 ? (
          <p className="muted small">No recent ANPR service errors — the pipeline has been healthy.</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Status</th>
                <th>Detail</th>
              </tr>
            </thead>
            <tbody>
              {diagnostics.error_log.map((e) => (
                <tr key={e.id}>
                  <td className="small muted">{new Date(e.detected_at).toLocaleString()}</td>
                  <td><span className={`badge ${e.status === "offline" ? "disabled" : "pin"}`}>{e.status}</span></td>
                  <td className="small">{e.detail ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

function formatBytes(bytes: number): string {
  if (bytes >= 1_073_741_824) return `${(bytes / 1_073_741_824).toFixed(1)} GB`;
  if (bytes >= 1_048_576) return `${(bytes / 1_048_576).toFixed(1)} MB`;
  return `${Math.max(0, Math.round(bytes / 1024))} KB`;
}
