import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type {
  AnprConfigView,
  CameraSourceView,
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

export default function AnprConfig({ user }: { user: SessionUser }) {
  const [config, setConfig] = useState<AnprConfigView | null>(null);
  const [cameras, setCameras] = useState<CameraSourceView[]>([]);
  const [versions, setVersions] = useState<ModelVersionView[]>([]);
  const [candidates, setCandidates] = useState<TrainingCandidateView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .getAnprConfig()
      .then(setConfig)
      .catch((e) => setError(String(e)));
    api.listCameraSources().then(setCameras).catch((e) => setError(String(e)));
    api.listModelVersions().then(setVersions).catch((e) => setError(String(e)));
    api.listTrainingCandidates().then(setCandidates).catch(() => undefined);
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
      <h2 className="section-title">ANPR Engine Configuration</h2>
      <p className="section-sub">
        Four simple parts, top to bottom: <b>1</b> which OCR reads plates, <b>2</b> where the camera feeds come from,
        <b> 3</b> which model version is live, and <b>4</b> the pool of plates collected for retraining. Every change
        here is audit-logged.
      </p>

      {error && <div className="error-banner">{error}</div>}
      {notice && <div className="success-banner">{notice}</div>}

      <CameraPreview />

      {config && <EnginePanel config={config} onSave={(changes) => run(() => api.updateAnprConfig(user.id, changes), "Engine settings saved.")} />}

      <CameraPanel cameras={cameras} actor={user} onRun={run} />
      <ModelPanel versions={versions} actor={user} onRun={run} />
      <CandidatePanel candidates={candidates} />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Engine + thresholds
// ---------------------------------------------------------------------------

function EnginePanel({
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
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="section-title" style={{ fontSize: 15 }}>
        1. Recognition engine
      </div>
      <p className="muted small" style={{ marginTop: -6 }}>
        The OCR engine that reads plates from the camera feed, and the confidence gate: a read below the active
        engine's threshold is queued for a human instead of being logged automatically.
      </p>

      <div className="field">
        <label>Active OCR engine</label>
        <div className="seg">
          <button className={engine === "paddleocr" ? "active" : ""} onClick={() => setEngine("paddleocr")}>
            PaddleOCR
          </button>
          <button className={engine === "easyocr" ? "active" : ""} onClick={() => setEngine("easyocr")}>
            EasyOCR
          </button>
        </div>
        <p className="muted small">
          Thresholds are tuned per engine — the active engine's threshold ({activeThreshold.toFixed(2)}) gates
          recognition confidence right now.
        </p>
      </div>

      <div className="row">
        <div className="field grow">
          <label>PaddleOCR threshold</label>
          <input
            type="number"
            min={0}
            max={1}
            step={0.05}
            value={paddle}
            onChange={(e) => setPaddle(num(e.target.value, config.confidence_threshold_paddleocr))}
          />
        </div>
        <div className="field grow">
          <label>EasyOCR threshold</label>
          <input
            type="number"
            min={0}
            max={1}
            step={0.05}
            value={easy}
            onChange={(e) => setEasy(num(e.target.value, config.confidence_threshold_easyocr))}
          />
        </div>
        <div className="field grow">
          <label>Plate-vehicle ratio threshold</label>
          <input
            type="number"
            min={0}
            max={1}
            step={0.01}
            value={ratio}
            onChange={(e) => setRatio(num(e.target.value, config.plate_vehicle_ratio_threshold))}
          />
        </div>
      </div>

      <div className="field">
        <label>Plate format rules (regex, optional)</label>
        <input
          value={rules}
          onChange={(e) => setRules(e.target.value)}
          placeholder="e.g. ^\d{3}[A-Z]{2,3}$"
        />
      </div>

      <div className="row">
<label className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
            <input
              style={{ width: "auto" }}
              type="checkbox"
              checked={isCapturePoint}
              onChange={(e) => setIsCapturePoint(e.target.checked)}
            />
            <span>This machine is a capture point</span>
          </label>
          <label className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
            <input
              style={{ width: "auto" }}
              type="checkbox"
              checked={confirmRequired}
              onChange={(e) => setConfirmRequired(e.target.checked)}
            />
            <span>Discharge confirmation required</span>
          </label>
        <label className="small" style={{ display: "flex", alignItems: "center", gap: 6, width: "auto" }}>
          <input
            style={{ width: "auto" }}
            type="checkbox"
            checked={saveImages}
            onChange={(e) => setSaveImages(e.target.checked)}
          />
          <span>Save recognition images</span>
        </label>
        <div className="field grow">
          <label>Retrain candidate threshold</label>
          <input
            type="number"
            min={0}
            value={retrain}
            onChange={(e) => setRetrain(e.target.value)}
            placeholder="unset"
          />
        </div>
      </div>

      <div>
        <button className="primary" onClick={save}>
          Save engine settings
        </button>
      </div>

      {confirmingSwap && (
        <div className="overlay" onClick={() => setConfirmingSwap(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3 style={{ marginTop: 0 }}>Switch the active OCR engine?</h3>
            <p className="muted small">
              Switching from <b>{config.active_ocr_engine}</b> to <b>{engine}</b> changes recognition behavior for
              every subsequent read. The switch is audit-logged (who, when, from/to) and per-engine confidence
              thresholds apply independently.
            </p>
            <div className="row" style={{ marginTop: 14, gap: 8 }}>
              <button className="primary" onClick={() => onSave(buildChanges())}>
                Confirm switch
              </button>
              <button className="ghost" onClick={() => setConfirmingSwap(false)}>
                Cancel
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Camera sources
// ---------------------------------------------------------------------------

// Each source type has its own expected format — switching types pre-fills the
// right shape instead of always defaulting to an RTSP URL.
const TYPE_TEMPLATES: Record<string, string> = {
  rtsp: "rtsp://192.168.1.100:554/stream1",
  http: "http://192.168.1.100:8080/video",
  nvr_export: "C:\\NVR Exports\\gate_2026-08-14.mp4",
  usb: "0",
  video_file: "",
  live_test: "http://127.0.0.1:9800/stream",
};

const TYPE_HELP: Record<string, string> = {
  rtsp: "RTSP camera stream (IP cameras, NVRs)",
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
  const [draftUrl, setDraftUrl] = useState<string | null>(null);
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editLabel, setEditLabel] = useState("");
  const [editConn, setEditConn] = useState("");
  const [testingId, setTestingId] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<Record<string, { ok: boolean; msg: string }>>({});
  const [deleteConfirm, setDeleteConfirm] = useState<string | null>(null);

  const pickType = (t: CameraSourceView["source_type"]) => {
    setType(t);
    setConn(TYPE_TEMPLATES[t] ?? "");
    setDraftUrl(null);
  };

  const pickFile = (file: File | undefined) => {
    if (!file) return;
    if (draftUrl) URL.revokeObjectURL(draftUrl);
    setDraftUrl(URL.createObjectURL(file));
    setConn(file.name);
  };

  const add = () =>
    onRun(async () => {
      const cam = await api.addCameraSource(actor.id, label, type, conn);
      if (draftUrl) {
        setPreviews((prev) => ({ ...prev, [cam.id]: draftUrl as string }));
        setDraftUrl(null);
      }
      setLabel("");
      setConn(TYPE_TEMPLATES[type] ?? "");
    }, "Camera source added.");

  const startEdit = (c: CameraSourceView) => {
    setEditingId(c.id);
    setEditLabel(c.label);
    setEditConn(c.connection_string);
  };

  const saveEdit = (sourceId: string) =>
    onRun(async () => {
      await api.updateCameraSource(actor.id, sourceId, editLabel, editConn);
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
      onRun(() => Promise.resolve(result), "Connection test complete.");
    } catch (e) {
      setTestResult((prev) => ({ ...prev, [sourceId]: { ok: false, msg: String(e) } }));
    } finally {
      setTestingId(null);
    }
  };

  const confirmDelete = (sourceId: string) => {
    setDeleteConfirm(sourceId);
  };

  const doDelete = (sourceId: string) =>
    onRun(async () => {
      await api.deleteCameraSource(actor.id, sourceId);
      setDeleteConfirm(null);
    }, "Camera source deleted.");

  return (
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="section-title" style={{ fontSize: 15 }}>
        2. Camera feeds ({cameras.length})
      </div>
      <p className="muted small" style={{ marginTop: -6 }}>
        Where the recognition service gets video from. Add sources, test connections, and manage your camera setup.
      </p>

      {/* Add new source form */}
      <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, background: "var(--surface)" }}>
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
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
            <div className="muted small" style={{ marginTop: 4, maxWidth: 240 }}>
              {TYPE_HELP[type]}
            </div>
          </div>
          {type === "video_file" ? (
            <div className="field grow">
              <label>Video file</label>
              <input type="file" accept="video/*" onChange={(e) => pickFile(e.target.files?.[0])} />
              {draftUrl && (
                <video
                  src={draftUrl}
                  controls
                  muted
                  style={{ width: "100%", maxHeight: 140, marginTop: 8, borderRadius: 8, background: "#000" }}
                />
              )}
            </div>
          ) : (
            <div className="field grow">
              <label>Connection string</label>
              <input value={conn} onChange={(e) => setConn(e.target.value)} placeholder={TYPE_TEMPLATES[type] || "..."} />
            </div>
          )}
          <div className="field">
            <label>&nbsp;</label>
            <button
              className="primary"
              onClick={add}
              disabled={!label.trim() || (type === "video_file" ? !draftUrl : !conn.trim())}
            >
              + Add Source
            </button>
          </div>
        </div>
      </div>

      {/* Existing camera sources */}
      {cameras.length > 0 && (
        <div className="health-grid">
          {cameras.map((c) => {
            const url = previews[c.id];
            const isHttp = c.connection_string.startsWith("http://") || c.connection_string.startsWith("https://");
            const isEditing = editingId === c.id;
            const isTesting = testingId === c.id;
            const tr = testResult[c.id];
            const isDeleting = deleteConfirm === c.id;

            return (
              <div key={c.id} className="health-card" style={{ marginBottom: 0 }}>
                {/* Header */}
                <div className="row between" style={{ alignItems: "center" }}>
                  {isEditing ? (
                    <input
                      value={editLabel}
                      onChange={(e) => setEditLabel(e.target.value)}
                      style={{ fontWeight: 600, fontSize: 14, flex: 1 }}
                    />
                  ) : (
                    <b>{c.label}</b>
                  )}
                  <span className={`badge ${c.status}`}>{c.status}</span>
                </div>

                {/* Type + Connection */}
                <div className="muted small" style={{ marginTop: 4 }}>
                  {c.source_type}
                </div>
                {isEditing ? (
                  <input
                    value={editConn}
                    onChange={(e) => setEditConn(e.target.value)}
                    style={{ marginTop: 8, fontSize: 12, fontFamily: "monospace" }}
                  />
                ) : (
                  <div className="small muted" style={{ wordBreak: "break-all", marginTop: 4 }}>
                    {c.connection_string}
                  </div>
                )}

                {/* Preview */}
                <div
                  style={{
                    margin: "8px 0",
                    height: 100,
                    borderRadius: 8,
                    overflow: "hidden",
                    background: "#000",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                  }}
                >
                  {url || isHttp ? (
                    <video
                      src={url ?? c.connection_string}
                      controls
                      muted
                      style={{ width: "100%", height: "100%", objectFit: "contain" }}
                    />
                  ) : (
                    <span className="muted small" style={{ padding: 8, textAlign: "center" }}>
                      {c.source_type === "rtsp"
                        ? "RTSP — test connection below"
                        : c.source_type === "video_file"
                          ? "Video preview after adding"
                          : "No preview for this type"}
                    </span>
                  )}
                </div>

                {/* Connection check result */}
                {c.last_connection_check_result && (
                  <div className="small muted" style={{ marginBottom: 4 }}>
                    Last check: {c.last_connection_check_result}
                  </div>
                )}

                {/* Test result */}
                {tr && (
                  <div
                    className="small"
                    style={{
                      marginBottom: 4,
                      padding: "4px 8px",
                      borderRadius: 4,
                      background: tr.ok ? "rgba(34,197,94,0.15)" : "rgba(239,68,68,0.15)",
                      color: tr.ok ? "#22c55e" : "#ef4444",
                    }}
                  >
                    {tr.msg}
                  </div>
                )}

                {/* Action buttons */}
                <div className="row" style={{ marginTop: 8, gap: 6, flexWrap: "wrap" }}>
                  {isEditing ? (
                    <>
                      <button className="primary small" onClick={() => saveEdit(c.id)}>
                        Save
                      </button>
                      <button className="ghost small" onClick={() => setEditingId(null)}>
                        Cancel
                      </button>
                    </>
                  ) : isDeleting ? (
                    <>
                      <span className="small" style={{ color: "#ef4444" }}>Delete this source?</span>
                      <button className="ghost small" style={{ color: "#ef4444" }} onClick={() => doDelete(c.id)}>
                        Yes, delete
                      </button>
                      <button className="ghost small" onClick={() => setDeleteConfirm(null)}>
                        Cancel
                      </button>
                    </>
                  ) : (
                    <>
                      <button
                        className="ghost small"
                        onClick={() => testConnection(c.id)}
                        disabled={isTesting}
                      >
                        {isTesting ? "Testing..." : "Test"}
                      </button>
                      <button className="ghost small" onClick={() => startEdit(c)}>
                        Edit
                      </button>
                      {c.status === "active" ? (
                        <button
                          className="ghost small"
                          onClick={() => onRun(() => api.setCameraSourceStatus(actor.id, c.id, "inactive"), "Deactivated.")}
                        >
                          Deactivate
                        </button>
                      ) : (
                        <button
                          className="ghost small"
                          onClick={() => onRun(() => api.setCameraSourceStatus(actor.id, c.id, "active"), "Reactivated.")}
                        >
                          Activate
                        </button>
                      )}
                      <button
                        className="ghost small"
                        style={{ color: "#ef4444" }}
                        onClick={() => confirmDelete(c.id)}
                      >
                        Delete
                      </button>
                    </>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Model versions — deploy is never automatic
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
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="section-title" style={{ fontSize: 15 }}>
        3. Models &amp; versions ({versions.length})
      </div>
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
          <button className="primary" onClick={register} disabled={!label.trim()}>
            Register
          </button>
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
                    <button className="primary" onClick={() => onRun(() => api.deployModelVersion(actor.id, v.id), "Model deployed.")}>
                      Deploy
                    </button>
                  )}
                  {!v.is_live && versions.some((x) => x.is_live && x.component === v.component) && (
                    <button className="ghost" onClick={() => onRun(() => api.rollbackModelVersion(actor.id, v.id), "Rolled back.")}>
                      Rollback
                    </button>
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
// Training candidates
// ---------------------------------------------------------------------------

function CandidatePanel({ candidates }: { candidates: TrainingCandidateView[] }) {
  return (
    <div className="card stack">
      <div className="section-title" style={{ fontSize: 15 }}>
        4. Training candidates ({candidates.length})
      </div>
      <p className="muted small">
        Low-confidence reads and human-corrected plates are auto-collected here for future retraining.
      </p>
      {candidates.length === 0 ? (
        <p className="muted small">No candidates yet — they appear as low-confidence or corrected reads are processed.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Plate</th>
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
// Live camera preview + ANPR service status
// ---------------------------------------------------------------------------

const ANPR_SERVICE_URL = "http://127.0.0.1:9800";

function CameraPreview() {
  const [status, setStatus] = useState<AnprServiceStatus | null>(null);
  const [lastPlate, setLastPlate] = useState<{ plate: string; confidence: number; timestamp: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [useStream, setUseStream] = useState(true);
  const [streamKey, setStreamKey] = useState(0);

  const refreshStatus = useCallback(async () => {
    try {
      const resp = await fetch(`${ANPR_SERVICE_URL}/status`);
      if (resp.ok) {
        const data = await resp.json();
        setStatus(data);
        setError(null);
      } else {
        setStatus(null);
      }
    } catch {
      setStatus(null);
    }
  }, []);

  const fetchLatest = useCallback(async () => {
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
    refreshStatus();
    fetchLatest();
    const t = setInterval(() => {
      refreshStatus();
      fetchLatest();
    }, 2000);
    return () => clearInterval(t);
  }, [refreshStatus, fetchLatest]);

  const isRunning = status?.running ?? false;
  const uptime = status?.uptime_seconds ?? 0;
  const uptimeStr = uptime > 3600
    ? `${Math.floor(uptime / 3600)}h ${Math.floor((uptime % 3600) / 60)}m`
    : uptime > 60
      ? `${Math.floor(uptime / 60)}m ${uptime % 60}s`
      : `${uptime}s`;

  // MJPEG stream URL (live, continuous) vs single frame (polled)
  const streamUrl = `${ANPR_SERVICE_URL}/preview?t=${streamKey}`;
  const frameUrl = `${ANPR_SERVICE_URL}/preview_frame?t=${streamKey}`;

  return (
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="section-title" style={{ fontSize: 15 }}>
        Live Camera Preview
      </div>
      <p className="muted small" style={{ marginTop: -6 }}>
        See what the ANPR service sees in real-time. Start the service with a camera source, then watch plates being
        detected below.
      </p>

      {/* Status bar */}
      <div className="row" style={{ gap: 12, flexWrap: "wrap" }}>
        <span className={`badge ${isRunning ? "active" : "disabled"}`}>
          {isRunning ? "ANPR Running" : "ANPR Stopped"}
        </span>
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

      {/* Stream mode toggle */}
      {isRunning && (
        <div className="row" style={{ gap: 8, alignItems: "center" }}>
          <span className="muted small">View:</span>
          <button
            className={useStream ? "ghost small" : "ghost small"}
            style={useStream ? { background: "var(--accent)", color: "white" } : {}}
            onClick={() => { setUseStream(true); setStreamKey((k) => k + 1); }}
          >
            Live Stream
          </button>
          <button
            className="ghost small"
            style={!useStream ? { background: "var(--accent)", color: "white" } : {}}
            onClick={() => setUseStream(false)}
          >
            Snapshot
          </button>
        </div>
      )}

      {/* Live preview */}
      <div style={{
        width: "100%",
        maxHeight: 400,
        borderRadius: 8,
        overflow: "hidden",
        background: "#000",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        position: "relative",
      }}>
        {isRunning ? (
          useStream ? (
            // MJPEG stream — continuous live feed (industry standard for IP cameras)
            <img
              key={`stream-${streamKey}`}
              src={streamUrl}
              alt="Live camera feed"
              style={{ width: "100%", maxHeight: 400, objectFit: "contain" }}
              onError={() => setError("Stream failed — try Snapshot mode or check ANPR service.")}
            />
          ) : (
            // Single frame — polled every 2 seconds
            <img
              key={`frame-${streamKey}`}
              src={frameUrl}
              alt="Camera snapshot"
              style={{ width: "100%", maxHeight: 400, objectFit: "contain" }}
              onError={() => setError("Cannot load snapshot — check ANPR service.")}
            />
          )
        ) : (
          <div style={{ padding: 40, textAlign: "center" }}>
            <p className="muted" style={{ fontSize: 14 }}>ANPR service is not running</p>
            <p className="muted small" style={{ marginTop: 8 }}>
              Start the service with a camera source to see live preview here.
            </p>
            <p className="muted small" style={{ marginTop: 12, fontFamily: "monospace", fontSize: 11, background: "#1a1a2e", padding: 8, borderRadius: 4 }}>
              python anpr-service/main.py --source http://PHONE_IP:8080/videofeed
            </p>
          </div>
        )}
      </div>

      {error && <div className="error-banner" style={{ marginTop: 8 }}>{error}</div>}

      {/* Last detected plate */}
      {lastPlate && (
        <div className="row" style={{ gap: 12, alignItems: "center", marginTop: 8 }}>
          <span className="muted small">Last detected:</span>
          <span style={{ fontSize: 18, fontWeight: 700, fontFamily: "monospace" }}>{lastPlate.plate}</span>
          <span className="badge active">{Math.round(lastPlate.confidence * 100)}% confidence</span>
          <span className="muted small">{new Date(lastPlate.timestamp).toLocaleTimeString()}</span>
        </div>
      )}

      {/* Setup instructions */}
      <div style={{ border: "1px solid var(--border)", borderRadius: "var(--radius)", padding: 14, marginTop: 8 }}>
        <div className="section-title" style={{ fontSize: 13 }}>Quick Setup — Phone as CCTV</div>
        <ol className="muted small" style={{ margin: "8px 0 0", paddingLeft: 20, lineHeight: 1.8 }}>
          <li>Install <b>IP Webcam</b> from Play Store on your Android phone</li>
          <li>Open IP Webcam → Start server → note the IP address (e.g. <code>192.168.1.5:8080</code>)</li>
          <li>Make sure phone and computer are on the <b>same WiFi network</b></li>
          <li>Add camera source: Type = <code>http</code>, URL = <code>http://PHONE_IP:8080/videofeed</code></li>
          <li>Start ANPR service: <code style={{ fontSize: 11 }}>python anpr-service/main.py --source http://PHONE_IP:8080/videofeed</code></li>
          <li>Point your phone camera at a vehicle plate</li>
          <li>Watch the preview above — plates will be detected and logged automatically!</li>
        </ol>
        <p className="muted small" style={{ marginTop: 8 }}>
          For testing without a phone: <code style={{ fontSize: 11 }}>python anpr-service/main.py --source ./test_images/</code>
        </p>
      </div>
    </div>
  );
}
