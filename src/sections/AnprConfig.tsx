import { convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import { useReferenceFields } from "../lib/referenceFields";
import type {
  AnprConfigView,
  AnprCredentialView,
  AnprDiagnosticsView,
  CameraSourceView,
  ConfidenceTrendPoint,
  ModelVersionView,
  OcrEngine,
  SessionUser,
  TrainingCandidateView,
} from "../lib/types";

/** Live status from the ANPR service /status endpoint. */
interface AnprServiceStatus {
  running: boolean;
  source_type: string;
  source_url: string;
  models_loaded: boolean;
  plates_detected: number;
  frames_processed: number;
  fps: number;
  last_plate_time: number | null;
  uptime_seconds: number;
}

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

  const refresh = useCallback(() => {
    api
      .getAnprConfig()
      .then(setConfig)
      .catch((e) => setError(String(e)));
    api.listCameraSources().then(setCameras).catch((e) => setError(String(e)));
    api.listModelVersions().then(setVersions).catch((e) => setError(String(e)));
    api.listTrainingCandidates().then(setCandidates).catch(() => undefined);
    api.listAnprCredentials().then(setCredentials).catch(() => undefined);
    api.anprDiagnostics().then(setDiagnostics).catch(() => undefined);
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const run = async (fn: () => Promise<unknown>, okMsg: string) => {
    setError(null);
    setNotice(null);
    try {
      await fn();
      setNotice(okMsg);
      refresh();
    } catch (e) {
      setError(String(e));
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

      {activeTab === "live" && <LivePreviewTab cameras={cameras} actor={user} onRun={run} />}

      {activeTab === "settings" && (
        <SettingsTab config={config} cameras={cameras} actor={user} onRun={run} onConfigSave={(c) => run(() => api.updateAnprConfig(user.id, c), "Settings saved.")} />
      )}

      {activeTab === "engine" && config && (
        <EngineTab config={config} onSave={(changes) => run(() => api.updateAnprConfig(user.id, changes), "Engine settings saved.")} />
      )}

      {activeTab === "credentials" && <CredentialsTab credentials={credentials} actor={user} onRun={run} />}

      {activeTab === "models" && <ModelPanel versions={versions} actor={user} onRun={run} />}

      {activeTab === "training" && (
        <CandidatePanel candidates={candidates} config={config} actor={user} onRun={run} />
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
}: {
  cameras: CameraSourceView[];
  actor: SessionUser;
  onRun: (fn: () => Promise<unknown>, okMsg: string) => void;
}) {
  const [expanded, setExpanded] = useState<CameraSourceView | null>(null);
  const [orientation, setOrientation] = useState<"landscape" | "portrait">("landscape");
  const [previews, setPreviews] = useState<Record<string, string>>({});

  useEffect(() => {
    const next: Record<string, string> = {};
    for (const c of cameras) {
      if (c.source_type === "video_file" || c.source_type === "nvr_export") {
        next[c.id] = convertFileSrc(c.connection_string);
      }
    }
    setPreviews(next);
  }, [cameras]);

  const isPortrait = orientation === "portrait";

  const online = (c: CameraSourceView) =>
    c.status === "active" && (c.source_type === "http" || c.source_type === "rtsp" || c.source_type === "live_test");

  return (
    <div className="stack" style={{ height: "100%" }}>
      <ServiceStatusBar cameras={cameras} actor={actor} />

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
            gridTemplateColumns: isPortrait
              ? `repeat(${Math.min(cameras.length, 3)}, 1fr)`
              : `repeat(${Math.min(cameras.length, 2)}, 1fr)`,
            gap: 12,
            flex: 1,
            minHeight: 0,
          }}
        >
          {cameras.map((c) => (
            <div
              key={c.id}
              className={`health-card ${c.status === "active" ? "ok" : "offline"}`}
              style={{
                cursor: "pointer",
                display: "flex",
                flexDirection: "column",
                overflow: "hidden",
              }}
              onClick={() => setExpanded(c)}
            >
              <div className="row between" style={{ alignItems: "center", padding: "8px 10px 0" }}>
                <b style={{ fontSize: 13 }}>{c.label}</b>
                <span className={`badge ${c.status === "active" ? "active" : "disabled"}`} style={{ fontSize: 11 }}>
                  {c.status}
                </span>
              </div>
              <div className="muted small" style={{ padding: "2px 10px" }}>
                {c.source_type} · {c.connection_string}
              </div>

              {/* Camera preview — fills available space */}
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
                }}
              >
                <CameraThumb camera={c} previewUrl={previews[c.id]} />
              </div>

              <div className="row" style={{ gap: 4, padding: "0 8px 8px" }}>
                {online(c) ? (
                  <button
                    className="ghost small"
                    onClick={(e) => {
                      e.stopPropagation();
                      onRun(() => api.setCameraSourceStatus(actor.id, c.id, "inactive"), "Feed paused.");
                    }}
                  >
                    Pause
                  </button>
                ) : (
                  <button
                    className="ghost small"
                    onClick={(e) => {
                      e.stopPropagation();
                      onRun(() => api.setCameraSourceStatus(actor.id, c.id, "active"), "Feed resumed.");
                    }}
                  >
                    Resume
                  </button>
                )}
                <button className="ghost small" onClick={(e) => { e.stopPropagation(); setExpanded(c); }}>
                  Expand
                </button>
              </div>
            </div>
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
            <div style={{
              flex: 1,
              minHeight: 0,
              borderRadius: 8,
              overflow: "hidden",
              background: "#000",
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
            }}>
              <CameraThumb camera={expanded} previewUrl={previews[expanded.id]} large />
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

function CameraThumb({ camera, previewUrl, large }: { camera: CameraSourceView; previewUrl?: string; large?: boolean }) {
  const h = large ? "65vh" : "100%";
  // Always proxy through ANPR service — it handles the camera connection,
  // so the UI just needs to show the locally-served preview frame.
  const proxySrc = `${ANPR_SERVICE_URL}/preview_frame?t=${Date.now()}`;
  // For video files, use the converted file src
  if (camera.source_type === "video_file" || camera.source_type === "nvr_export") {
    return <video src={previewUrl ?? convertFileSrc(camera.connection_string)} controls={!!large} muted autoPlay={!!large} loop={!!large} style={{ width: "100%", height: h, objectFit: "contain" }} />;
  }
  // For live sources (HTTP, RTSP, USB), try the ANPR service proxy first
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

/** Auto-refreshing live preview from the ANPR service. Updates every 500ms. */
function LivePreview() {
  const [src, setSrc] = useState(`${ANPR_SERVICE_URL}/preview_frame?t=${Date.now()}`);

  useEffect(() => {
    const timer = setInterval(() => {
      setSrc(`${ANPR_SERVICE_URL}/preview_frame?t=${Date.now()}`);
    }, 500);
    return () => clearInterval(timer);
  }, []);

  return (
    <img
      src={src}
      alt="Live camera feed"
      style={{ width: "100%", maxHeight: 300, objectFit: "contain" }}
      onError={() => {
        // If preview_frame fails, try the /preview endpoint
        setSrc(`${ANPR_SERVICE_URL}/preview?t=${Date.now()}`);
      }}
    />
  );
}

const ANPR_SERVICE_URL = "http://127.0.0.1:9800";

function ServiceStatusBar({ cameras, actor }: { cameras: CameraSourceView[]; actor: SessionUser }) {
  const [status, setStatus] = useState<AnprServiceStatus | null>(null);
  const [lastPlate, setLastPlate] = useState<{ plate: string; confidence: number; timestamp: string } | null>(null);
  const [starting, setStarting] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const resp = await fetch(`${ANPR_SERVICE_URL}/status`);
      if (resp.ok) setStatus(await resp.json());
      else setStatus(null);
    } catch {
      setStatus(null);
    }
    try {
      const resp = await fetch(`${ANPR_SERVICE_URL}/latest`);
      if (resp.ok) {
        const data = await resp.json();
        setLastPlate({ plate: data.plate, confidence: data.confidence, timestamp: data.timestamp });
      }
    } catch {
      // Service not running — ignore
    }
  }, []);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 2000);
    return () => clearInterval(t);
  }, [refresh]);

  const isRunning = status?.running ?? false;

  /** Find the first active camera source */
  const activeCamera = cameras.find(c => c.status === "active");

  /** Start the ANPR service using the active camera source */
  const handleStart = useCallback(async () => {
    if (!activeCamera) {
      setMessage("No active camera source. Add one in Camera Settings and set it active.");
      return;
    }
    setStarting(true);
    setMessage(null);
    try {
      // 1. Write config.json with the active camera source
      await api.writeAnprConfig(actor.id, activeCamera.connection_string, activeCamera.source_type, false);
      // 2. Start the ANPR service
      const pid = await api.startAnprService(actor.id);
      setMessage(`ANPR service started (PID ${pid}). Connecting to ${activeCamera.connection_string}...`);
      // 3. Poll until it's up
      let tries = 0;
      const check = async () => {
        try {
          const resp = await fetch(`${ANPR_SERVICE_URL}/health`);
          if (resp.ok) {
            setMessage(`ANPR service running — connected to ${activeCamera.connection_string}`);
            refresh();
            return;
          }
        } catch {}
        if (++tries < 15) setTimeout(check, 1000);
        else setMessage("ANPR service started but not responding yet — check logs.");
      };
      setTimeout(check, 2000);
    } catch (e: any) {
      setMessage(`Failed: ${e}`);
    } finally {
      setStarting(false);
    }
  }, [activeCamera, actor, refresh]);

  /** Stop the ANPR service */
  const handleStop = useCallback(async () => {
    try {
      await api.stopAnprService(actor.id);
      setMessage("ANPR service stopped.");
      refresh();
    } catch (e: any) {
      setMessage(`Failed: ${e}`);
    }
  }, [actor, refresh]);
  const uptime = status?.uptime_seconds ?? 0;
  const uptimeStr = uptime > 3600
    ? `${Math.floor(uptime / 3600)}h ${Math.floor((uptime % 3600) / 60)}m`
    : uptime > 60
      ? `${Math.floor(uptime / 60)}m ${uptime % 60}s`
      : `${uptime}s`;

  return (
    <div className="card">
      <div className="row" style={{ gap: 12, flexWrap: "wrap", alignItems: "center" }}>
        <span className={`badge ${isRunning ? "active" : "disabled"}`}>
          {isRunning ? "ANPR Running" : "ANPR Stopped"}
        </span>
        {!isRunning ? (
          <button className="small" onClick={handleStart} disabled={starting} style={{ marginLeft: 4 }}>
            {starting ? "Starting..." : "Start ANPR"}
          </button>
        ) : (
          <button className="small danger" onClick={handleStop} style={{ marginLeft: 4 }}>
            Stop ANPR
          </button>
        )}
        {status && (
          <>
            <span className="muted small">Source: {status.source_type} {status.source_url ? `(${status.source_url})` : ""}</span>
            <span className="muted small">FPS: {status.fps}</span>
            <span className="muted small">Plates: {status.plates_detected}</span>
            <span className="muted small">Frames: {status.frames_processed}</span>
            <span className="muted small">Up: {uptimeStr}</span>
          </>
        )}
      </div>
      {message && (
        <div className="muted small" style={{ marginTop: 6, padding: "4px 8px", background: "var(--surface-2, #1a1a2e)", borderRadius: 4 }}>
          {message}
        </div>
      )}

      {/* Live service preview */}
      <div
        style={{
          width: "100%",
          maxHeight: 300,
          marginTop: 12,
          borderRadius: 8,
          overflow: "hidden",
          background: "#000",
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          position: "relative",
        }}
      >
        {isRunning ? (
          <LivePreview />
        ) : (
          <div style={{ padding: 30, textAlign: "center" }}>
            <p className="muted" style={{ fontSize: 14, margin: 0 }}>ANPR service is not running</p>
            <p className="muted small" style={{ marginTop: 6 }}>
              Start it with a camera source to see the live feed here.
            </p>
          </div>
        )}
      </div>

      {lastPlate && (
        <div className="row" style={{ gap: 12, alignItems: "center", marginTop: 10 }}>
          <span className="muted small">Last detected:</span>
          <span style={{ fontSize: 18, fontWeight: 700, fontFamily: "monospace" }}>{lastPlate.plate}</span>
          <span className="badge active">{Math.round(lastPlate.confidence * 100)}% confidence</span>
          <span className="muted small">{new Date(lastPlate.timestamp).toLocaleTimeString()}</span>
        </div>
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

  // Resync local state when the saved config reloads after a save, so the
  // panel never shows stale values.
  useEffect(() => {
    setPreset(presetFor(config));
    setPaddle(config.confidence_threshold_paddleocr);
    setEasy(config.confidence_threshold_easyocr);
    setPending(config.max_pending_duration_hours?.toString() ?? "24");
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
    });

  return (
    <div className="card">
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
  nvr_export: "C:\\NVR Exports\\gate_2026-08-14.mp4",
  usb: "0",
  video_file: "",
  live_test: "http://127.0.0.1:9800/stream",
};

const TYPE_HELP: Record<string, string> = {
  rtsp: "RTSP camera stream (real CCTV / NVR systems)",
  http: "HTTP stream (IP Webcam app, MJPEG cameras)",
  nvr_export: "A video export path from your NVR (local .mp4/.avi file)",
  usb: "USB webcam device index, e.g. 0 for the first camera",
  video_file: "Pick a video file from your computer",
  live_test: "Test HTTP stream URL for pipeline verification",
};

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
      await api.addCameraSource(actor.id, label, type, conn);
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

      {/* Existing camera sources */}
      {cameras.length > 0 && (
        <table className="table" style={{ marginTop: 12 }}>
          <thead>
            <tr>
              <th>Label</th>
              <th>Type</th>
              <th>Connection</th>
              <th>Status</th>
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

function EngineTab({
  config,
  onSave,
}: {
  config: AnprConfigView;
  onSave: (changes: Partial<AnprConfigView>) => void;
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
  const [confirmingSwap, setConfirmingSwap] = useState(false);

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
  });

  const save = () => {
    if (engine !== config.active_ocr_engine) {
      setConfirmingSwap(true);
    } else {
      onSave(buildChanges());
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
          <button className={engine === "easyocr" ? "active" : ""} onClick={() => setEngine("easyocr")}>EasyOCR</button>
          <button className={engine === "cloud_provider" ? "active" : ""} onClick={() => setEngine("cloud_provider")}>
            Cloud provider
          </button>
        </div>
        <p className="muted small">
          {engine === "cloud_provider" ? (
            <>Cloud ANPR — the plate read calls your provider's API instead of a local engine. Configure the API key in
              the <b>Credentials</b> tab. Nothing downstream changes; matching, tracking and sync work identically.</>
          ) : (
            <>Thresholds are tuned per engine — the active engine's threshold ({activeThreshold.toFixed(2)}) gates
              recognition confidence right now.</>
          )}
        </p>
      </div>

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

      <div className="field">
        <label>Plate format rules (regex, optional)</label>
        <input value={rules} onChange={(e) => setRules(e.target.value)} placeholder="e.g. ^\d{3}[A-Z]{2,3}$" />
      </div>

      <div className="row">
        <label className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
          <input style={{ width: "auto" }} type="checkbox" checked={isCapturePoint} onChange={(e) => setIsCapturePoint(e.target.checked)} />
          <span>This machine is a capture point</span>
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
  const [value, setValue] = useState("");
  const [rotating, setRotating] = useState<AnprCredentialView | null>(null);
  const [rotatingValue, setRotatingValue] = useState("");
  const [deleting, setDeleting] = useState<AnprCredentialView | null>(null);

  const add = () =>
    onRun(async () => {
      await api.setAnprCredential(actor.id, keyName.trim(), value);
      setValue("");
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
              <option value="cloud_anpr_api_key">cloud_anpr_api_key</option>
              <option value="license_key">license_key</option>
              <option value="custom">Custom…</option>
            </select>
          </div>
          <div className="field grow">
            <label>Value</label>
            <input type="password" value={value} onChange={(e) => setValue(e.target.value)} placeholder="Paste the API key or license…" />
          </div>
          <div className="field">
            <label>&nbsp;</label>
            <button className="primary" onClick={add} disabled={!value.trim()}>Save key</button>
          </div>
        </div>
        <p className="muted small" style={{ margin: 0 }}>
          The <code>cloud_anpr_api_key</code> powers the <b>Cloud provider</b> engine option in Engine &amp; Threshold.
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
  const { label } = useReferenceFields();
  const [threshold, setThreshold] = useState(config?.retrain_candidate_threshold?.toString() ?? "");
  const [dirty, setDirty] = useState(false);

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

  return (
    <div className="card stack">
      <div className="section-title" style={{ fontSize: 15 }}>Training candidates ({candidates.length})</div>
      <p className="muted small">
        Low-confidence reads and human-corrected plates are auto-collected here for future retraining.
      </p>

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

      {candidates.length === 0 ? (
        <p className="muted small">No candidates yet — they appear as low-confidence or corrected reads are processed.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>{label("vehicle", "plate_number")}</th>
              <th>Reason</th>
              <th>Source trip</th>
              <th>Captured</th>
            </tr>
          </thead>
          <tbody>
            {candidates.map((c) => (
              <tr key={c.id}>
                <td>{c.plate_number ?? "—"}</td>
                <td>{c.reason}</td>
                <td className="muted small">{c.source_trip_id?.slice(0, 8) ?? "—"}</td>
                <td className="muted small">{new Date(c.created_at).toLocaleString()}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Diagnostics — dependency health, storage, error log, confidence trend
// ---------------------------------------------------------------------------

function DiagnosticsTab({
  diagnostics,
  actor,
}: {
  diagnostics: AnprDiagnosticsView | null;
  actor: SessionUser;
}) {
  const [trend, setTrend] = useState<ConfidenceTrendPoint[]>([]);
  const [trendError, setTrendError] = useState<string | null>(null);

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
        <div className="row" style={{ gap: 8, marginTop: 6 }}>
          <span className="health-value">{formatBytes(diagnostics.storage_bytes)}</span>
          <span className="muted small">{diagnostics.storage_detail}</span>
        </div>
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
