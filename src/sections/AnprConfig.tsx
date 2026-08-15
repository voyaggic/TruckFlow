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

const SOURCE_TYPES = ["rtsp", "nvr_export", "usb", "video_file", "live_test"] as const;

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
  rtsp: "rtsp://user:pass@192.168.1.100:554/stream1",
  nvr_export: "C:\\NVR Exports\\gate_2026-08-14.mp4",
  usb: "0",
  video_file: "",
  live_test: "http://127.0.0.1:9800/stream",
};

const TYPE_HELP: Record<string, string> = {
  rtsp: "IP camera stream — rtsp://user:pass@host:554/path",
  nvr_export: "A video export path from your NVR (a local .mp4/.avi file)",
  usb: "USB webcam device index, e.g. 0 for the first camera",
  video_file: "Pick a video file from your computer — it previews live below",
  live_test: "A test HTTP stream URL used to verify the pipeline",
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

  return (
    <div className="card stack" style={{ marginBottom: 16 }}>
      <div className="section-title" style={{ fontSize: 15 }}>
        2. Camera feeds ({cameras.length})
      </div>
      <p className="muted small" style={{ marginTop: -6 }}>
        Where the recognition service gets video from. Each card below shows the feed live when it can, plus its
        status and last connection check — so you can see whether a source is really working.
      </p>

      <div className="row">
        <div className="field grow">
          <label>Label</label>
          <input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="Main gate" />
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
            <input value={conn} onChange={(e) => setConn(e.target.value)} placeholder={TYPE_TEMPLATES[type] || "…"} />
          </div>
        )}
        <div className="field">
          <label>&nbsp;</label>
          <button
            className="primary"
            onClick={add}
            disabled={!label.trim() || (type === "video_file" ? !draftUrl : !conn.trim())}
          >
            Add
          </button>
        </div>
      </div>

      {cameras.length > 0 && (
        <div className="health-grid">
          {cameras.map((c) => {
            const url = previews[c.id];
            const isHttp = c.connection_string.startsWith("http://") || c.connection_string.startsWith("https://");
            return (
              <div key={c.id} className="health-card" style={{ marginBottom: 0 }}>
                <div className="row between">
                  <b>{c.label}</b>
                  <span className={`badge ${c.status}`}>{c.status}</span>
                </div>
                <div className="muted small">{c.source_type}</div>
                <div
                  style={{
                    margin: "8px 0",
                    height: 120,
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
                        ? "RTSP can't play inside the app window — verify the stream in VLC and check the status below."
                        : c.source_type === "video_file"
                          ? "Video file preview is available in this session after adding it."
                          : "No live preview for this source type."}
                    </span>
                  )}
                </div>
                <div className="small muted" style={{ wordBreak: "break-all" }}>
                  {c.connection_string}
                </div>
                <div className="small muted">
                  {c.last_connection_check_result
                    ? `${c.last_connection_check_at} — ${c.last_connection_check_result}`
                    : "No connection check yet"}
                </div>
                <div className="row" style={{ marginTop: 8, gap: 8 }}>
                  {c.status === "active" ? (
                    <button
                      className="ghost small"
                      onClick={() => onRun(() => api.setCameraSourceStatus(actor.id, c.id, "inactive"), "Camera source deactivated.")}
                    >
                      Deactivate
                    </button>
                  ) : (
                    <button
                      className="ghost small"
                      onClick={() => onRun(() => api.setCameraSourceStatus(actor.id, c.id, "active"), "Camera source reactivated.")}
                    >
                      Reactivate
                    </button>
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
