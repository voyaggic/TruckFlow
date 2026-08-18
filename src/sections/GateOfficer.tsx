import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "../lib/api";
import { useReferenceFields } from "../lib/referenceFields";
import { DynamicFieldInput, MEASUREMENT_UNIT_GROUPS } from "./AdminPanel";
import type {
  AnprStatus,
  CaptureSettings,
  CompanyView,
  DriverView,
  FrameEvidence,
  SessionUser,
  SyncStatusView,
  TripView,
  VehicleView,
} from "../lib/types";

function reasonLabel(reason: string, entityLabel: string): string {
  switch (reason) {
    case "multiple_matches":
      return "Multiple possible matches";
    case "no_match":
      return `No match — possible new ${entityLabel.toLowerCase()}`;
    case "low_confidence":
      return "Low confidence read";
    case "pending_approval":
      return "Awaiting officer approval";
    default:
      return reason;
  }
}

export default function GateOfficer({ user, canResolve }: { user: SessionUser; canResolve: boolean }) {
  const { label, entityLabel } = useReferenceFields();
  const [today, setToday] = useState<TripView[]>([]);
  const [queued, setQueued] = useState<TripView[]>([]);
  const [anpr, setAnpr] = useState<AnprStatus | null>(null);
  const [settings, setSettings] = useState<CaptureSettings | null>(null);
  const [sync, setSync] = useState<SyncStatusView | null>(null);
  const [query, setQuery] = useState("");
  const [manualPlate, setManualPlate] = useState("");
  const [simPlate, setSimPlate] = useState("");
  const [simConf, setSimConf] = useState("0.95");
  const [flash, setFlash] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [resolving, setResolving] = useState<TripView | null>(null);
  const [declineTarget, setDeclineTarget] = useState<TripView | null>(null);
  const [dischargePrompt, setDischargePrompt] = useState<TripView | null>(null);
  const mountedAt = useRef(Date.now());

  const refresh = useCallback(async () => {
    const [t, q, s, c, sy] = await Promise.all([
      api.listTodayTrips(),
      api.listQueued(),
      api.anprStatus(),
      api.getCaptureSettings(),
      api.syncStatus(),
    ]);
    setToday(t);
    setQueued(q);
    setAnpr(s);
    setSettings(c);
    setSync(sy);
  }, []);

  useEffect(() => {
    refresh().catch((e) => setError(String(e)));
    const unlisten = listen("capture-updated", () => {
      refresh().catch((e) => setError(String(e)));
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [refresh]);

  const now = new Date();
  const sessionMinutes = Math.round((now.getTime() - mountedAt.current) / 60000);
  const handledToday = today.filter((t) => t.officer_id === user.id).length;

  const approve = async (trip: TripView) => {
    setError(null);
    try {
      const logged = await api.approveTrip(trip.id, user.id);
      if (settings?.discharge_confirmation_required && logged.is_discharge_trip == null) {
        setDischargePrompt(logged);
      } else {
        setFlash(`Trip ${trip.plate_number} approved`);
        setTimeout(() => setFlash(null), 2500);
        await refresh();
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const decline = async (trip: TripView) => {
    setError(null);
    try {
      await api.declineTrip(trip.id, user.id);
      setDeclineTarget(null);
      setFlash(`Read ${trip.plate_number} declined — kept locally, not counted.`);
      setTimeout(() => setFlash(null), 2500);
      await refresh();
    } catch (e) {
      setError(String(e));
      throw e;
    }
  };

  const classifyDischarge = async (trip: TripView, isDischarge: boolean) => {
    setError(null);
    try {
      await api.classifyDischarge(trip.id, user.id, isDischarge);
      setDischargePrompt(null);
      setFlash(
        isDischarge
          ? `Trip ${trip.plate_number} logged as a discharge trip.`
          : `Trip ${trip.plate_number} logged as a non-discharge entry.`,
      );
      setTimeout(() => setFlash(null), 2500);
      await refresh();
    } catch (e) {
      setError(String(e));
      throw e;
    }
  };

  const manualLog = async () => {
    setError(null);
    const plate = manualPlate.trim();
    if (!plate) return;
    try {
      const res = await api.manualEntry(plate, user.id);
      setFlash(res.message);
      setTimeout(() => setFlash(null), 2500);
      setManualPlate("");
      // Manual entries always require discharge Yes/No + Confirm before they
      // are eligible to reach Google Sheets; unclassified trips stay local.
      if (res.trip && res.trip.status === "logged" && res.trip.is_discharge_trip == null) {
        setDischargePrompt(res.trip);
      } else {
        await refresh();
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const simulate = async () => {
    setError(null);
    const plate = simPlate.trim();
    if (!plate) return;
    try {
      const res = await api.simulateRead(plate, parseFloat(simConf) || 0.95);
      setFlash(res.message);
      setTimeout(() => setFlash(null), 2500);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const toggleConsent = async () => {
    if (!settings) return;
    setError(null);
    try {
      await api.setCaptureSettings(user.id, {
        consent_mode: settings.consent_mode === "confirm_required" ? "fully_automatic" : "confirm_required",
      });
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const awaitingApproval = today.find((t) => t.status === "queued" && t.reason === "pending_approval");
  const current: TripView | null = awaitingApproval ?? today.find((t) => t.status === "logged") ?? today[0] ?? null;
  const q = query.trim().toLowerCase();
  const filtered = today.filter(
    (t) =>
      !q ||
      t.plate_number.toLowerCase().includes(q) ||
      (t.company_name ?? "").toLowerCase().includes(q) ||
      t.time_in.toLowerCase().includes(q),
  );

  const openResolve = async (trip: TripView) => {
    setError(null);
    setResolving(trip);
  };

  return (
    <div className="stack">
      <div className="row between">
        <div>
          <h2 className="section-title">Gate</h2>
          <p className="section-sub">
            {handledToday} trip{handledToday === 1 ? "" : "s"} handled this session (~{sessionMinutes} min)
          </p>
        </div>
        <div className="row">
          {sync && (
            <span className={`badge ${sync.online ? "active" : "disabled"}`} title="Non-actionable status — sync runs in the background">
              {sync.online
                ? "Online — synced"
                : `Offline — ${sync.pg.tables.reduce((n, t) => n + t.pending, 0) + sync.sheets.pending} pending`}
            </span>
          )}
          {anpr && (
            <span className={`badge ${anpr.enabled ? "active" : "disabled"}`}>
              ANPR {anpr.enabled ? `online (${anpr.source})` : "offline"} · manual entry ready
            </span>
          )}
        </div>
      </div>

      {error && <div className="error-banner">{error}</div>}
      {flash && <div className="success-banner">{flash}</div>}

      <div className="card">
        <div className="row between">
          <h3 style={{ margin: 0, fontSize: 15 }}>Current Entry</h3>
          {settings && (
            <div className="row">
              <span className="muted small">Consent mode</span>
              <div className="seg">
                <button
                  className={settings.consent_mode === "confirm_required" ? "active" : ""}
                  onClick={() => settings.consent_mode !== "confirm_required" && toggleConsent()}
                >
                  Confirm required
                </button>
                <button
                  className={settings.consent_mode === "fully_automatic" ? "active" : ""}
                  onClick={() => settings.consent_mode !== "fully_automatic" && toggleConsent()}
                >
                  Fully automatic
                </button>
              </div>
            </div>
          )}
        </div>

        {current ? (
          <div>
            <div className="row" style={{ margin: "12px 0", gap: 8 }}>
              <span className="badge">{current.capture_method === "auto" ? "Auto" : "Manual"}</span>
              <span className={`badge ${current.status === "queued" ? "pin" : current.status === "logged" ? "active" : "disabled"}`}>
                {statusLabel(current.status)}
              </span>
              {current.reason && (
                <span className="badge">{reasonLabel(current.reason, entityLabel("vehicle"))}</span>
              )}
            </div>
            <div className="entry-grid">
              <div>
                <div className="muted small">{label("vehicle", "plate_number")}</div>
                <div className="plate-font">{current.plate_number}</div>
              </div>
              <div>
                <div className="muted small">{label("vehicle", "company")}</div>
                <div>{current.company_name ?? "—"}</div>
              </div>
              <div>
                <div className="muted small">{label("vehicle", "driver")}</div>
                <div>{current.driver_name ?? "—"}</div>
              </div>
              <div>
                <div className="muted small">{label("vehicle", "registered_capacity")}</div>
                <div>{current.capacity_at_trip != null ? `${current.capacity_at_trip} t` : "—"}</div>
              </div>
              <div>
                <div className="muted small">Capture time</div>
                <div>{fmtTime(current.time_in)}</div>
              </div>
              <div>
                <div className="muted small">Confidence</div>
                <div>{current.confidence_score != null ? `${Math.round(current.confidence_score * 100)}%` : "—"}</div>
              </div>
              <div>
                <div className="muted small">Frames</div>
                <div>{current.photo_count}</div>
              </div>
              <div>
                <div className="muted small">Officer</div>
                <div>{current.officer_name ?? "—"}</div>
              </div>
            </div>
            {current.status === "queued" && current.reason === "pending_approval" && canResolve && (
              <div className="row" style={{ marginTop: 14 }}>
                <button className="primary" onClick={() => approve(current)}>
                  Approve
                </button>
                <button onClick={() => openResolve(current)}>Edit before confirming</button>
                <button className="danger" onClick={() => setDeclineTarget(current)}>
                  Decline read
                </button>
              </div>
            )}
            {current.status === "logged" &&
              current.is_discharge_trip == null &&
              settings?.discharge_confirmation_required && (
                <div className="row" style={{ marginTop: 14, gap: 8 }}>
                  <span className="muted small">Was this a discharge trip?</span>
                  <button className="primary" onClick={() => setDischargePrompt(current)}>
                    Classify now
                  </button>
                </div>
              )}
          </div>
        ) : (
          <div className="placeholder" style={{ marginTop: 12 }}>
            No entries yet today. Capture one with the simulator or log via Manual Entry.
          </div>
        )}
      </div>

      <div className="row">
        <div className="card grow">
          <h3 style={{ margin: "0 0 10px", fontSize: 15 }}>Manual Entry</h3>
          <div className="row">
            <input
              className="plate-input"
              value={manualPlate}
              onChange={(e) => setManualPlate(e.target.value.toUpperCase())}
              onKeyDown={(e) => e.key === "Enter" && manualLog()}
              placeholder={`${label("vehicle", "plate_number")}, e.g. A123AB`}
            />
            <button className="primary" onClick={manualLog} disabled={!manualPlate.trim()}>
              Log trip
            </button>
          </div>
          <p className="muted small" style={{ marginBottom: 0 }}>
            Runs the same cross-reference logic as the camera. Works with ANPR fully offline.
          </p>
        </div>
        <div className="card">
          <h3 style={{ margin: "0 0 10px", fontSize: 15 }}>Simulator (dev)</h3>
          <div className="row">
            <input
              className="plate-input"
              value={simPlate}
              onChange={(e) => setSimPlate(e.target.value.toUpperCase())}
              onKeyDown={(e) => e.key === "Enter" && simulate()}
              placeholder={label("vehicle", "plate_number")}
            />
            <input
              style={{ width: 70 }}
              value={simConf}
              onChange={(e) => setSimConf(e.target.value)}
              placeholder="0.95"
            />
            <button className="ghost" onClick={simulate} disabled={!simPlate.trim()}>
              Simulate read
            </button>
          </div>
          {anpr && (
            <p className="muted small" style={{ marginBottom: 0 }}>
              Queue: {anpr.pending_reads} pending · last: {anpr.last_plate ? `${anpr.last_plate} @ ${fmtTime(anpr.last_read_at ?? "")}` : "none"}
            </p>
          )}
        </div>
      </div>

      <div className="card">
        <div className="row between">
          <h3 style={{ margin: 0, fontSize: 15 }}>
            Verification Queue <span className="badge">{queued.length}</span>
          </h3>
        </div>
        {queued.length === 0 ? (
          <p className="muted small" style={{ margin: "10px 0 0" }}>
            No pending items — every read resolved cleanly.
          </p>
        ) : (
          <div className="stack" style={{ marginTop: 10 }}>
            {queued.map((t) => (
              <div key={t.id} className="row between queue-item">
                <div className="row">
                  <span className="plate-font">{t.plate_number}</span>
                  <span className="badge">{reasonLabel(t.reason ?? "", entityLabel("vehicle"))}</span>
                  <span className="muted small">{fmtTime(t.time_in)}</span>
                </div>
                {canResolve ? (
                  <button className="primary" onClick={() => openResolve(t)}>
                    Resolve
                  </button>
                ) : (
                  <span className="muted small">view only</span>
                )}
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="card">
        <div className="row between">
          <h3 style={{ margin: 0, fontSize: 15 }}>Recent Entries</h3>
          <input
            style={{ width: 260 }}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search plate, company, time…"
          />
        </div>
        {filtered.length === 0 ? (
          <p className="muted small" style={{ margin: "10px 0 0" }}>
            No matching entries today.
          </p>
        ) : (
          <table className="table" style={{ marginTop: 10 }}>
            <thead>
              <tr>
                <th>{label("vehicle", "plate_number")}</th>
                <th>{label("vehicle", "company")}</th>
                <th>{label("vehicle", "driver")}</th>
                <th>{label("vehicle", "registered_capacity")}</th>
                <th>Time</th>
                <th>Source</th>
                <th>Status</th>
                <th>Type</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((t) => (
                <tr key={t.id}>
                  <td className="plate-font">{t.plate_number}</td>
                  <td>{t.company_name ?? "—"}</td>
                  <td>{t.driver_name ?? "—"}</td>
                  <td>{t.capacity_at_trip != null ? `${t.capacity_at_trip} t` : "—"}</td>
                  <td>{fmtTime(t.time_in)}</td>
                  <td>
                    <span className={`badge ${t.capture_method === "auto" ? "pin" : "active"}`}>
                      {t.capture_method === "auto" ? "Auto" : "Manual"}
                    </span>
                  </td>
                  <td>{statusLabel(t.status)}</td>
                  <td>
                    {t.status === "logged" && t.is_discharge_trip == null && settings?.discharge_confirmation_required ? (
                      <button className="ghost" onClick={() => setDischargePrompt(t)}>
                        Classify
                      </button>
                    ) : t.is_discharge_trip == null ? (
                      "—"
                    ) : t.is_discharge_trip ? (
                      <span className="badge active">Discharge</span>
                    ) : (
                      <span className="badge">Non-discharge</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {resolving && (
        <ResolveScreen
          trip={resolving}
          officerId={user.id}
          onClose={() => setResolving(null)}
          onDecline={(t) => {
            setResolving(null);
            setDeclineTarget(t);
          }}
          onDone={async (msg, trip) => {
            setResolving(null);
            setFlash(msg);
            setTimeout(() => setFlash(null), 2500);
            if (trip && settings?.discharge_confirmation_required && trip.is_discharge_trip == null) {
              setDischargePrompt(trip);
            } else {
              await refresh();
            }
          }}
        />
      )}

      {declineTarget && (
        <ConfirmAction
          title="Decline this read?"
          body={`Plate ${declineTarget.plate_number} will be saved locally as declined, excluded from trip counts, and retained for reference. It can be purged later by an officer or admin.`}
          confirmLabel="Decline"
          danger
          onConfirm={async () => {
            await decline(declineTarget);
          }}
          onCancel={() => setDeclineTarget(null)}
        />
      )}

      {dischargePrompt && (
        <DischargeStep
          trip={dischargePrompt}
          onConfirm={(isDischarge) => classifyDischarge(dischargePrompt, isDischarge)}
          onClose={() => {
            setDischargePrompt(null);
            refresh().catch((e) => setError(String(e)));
          }}
        />
      )}
    </div>
  );
}

/** Verification-queue "Resolve" screen — 05-ui-screens.md §3. */
function ResolveScreen({
  trip,
  officerId,
  onClose,
  onDecline,
  onDone,
}: {
  trip: TripView;
  officerId: string;
  onClose: () => void;
  onDecline: (trip: TripView) => void;
  onDone: (message: string, trip: TripView | null) => Promise<void>;
}) {
  const { label, entityLabel, fieldsFor } = useReferenceFields();
  const vehicleDefs = fieldsFor("vehicle");
  const customVehicleDefs = vehicleDefs.filter((fd) => !fd.is_hidden && !fd.is_standard);
  const [frames, setFrames] = useState<FrameEvidence[]>([]);
  const [companies, setCompanies] = useState<CompanyView[]>([]);
  const [drivers, setDrivers] = useState<DriverView[]>([]);
  const [vehicles, setVehicles] = useState<VehicleView[]>([]);
  const [selectedVehicleId, setSelectedVehicleId] = useState<string | null>(trip.vehicle_id);
  const [extraFields, setExtraFields] = useState<Record<string, unknown>>({});
  const [companyId, setCompanyId] = useState<string>(trip.company_id ?? "");
  const [driverId, setDriverId] = useState<string>(trip.driver_id ?? "");
  const [capacity, setCapacity] = useState<string>(
    trip.capacity_at_trip != null ? String(trip.capacity_at_trip) : "",
  );
  const [capacityUnit, setCapacityUnit] = useState<string>(trip.capacity_unit ?? "litres");
  const [receipt, setReceipt] = useState<string>(trip.receipt_no ?? "");
  const [registerNew, setRegisterNew] = useState(false);
  const [newPlate, setNewPlate] = useState("");
  const [dupWarning, setDupWarning] = useState<string | null>(null);
  const [confirmingDiscard, setConfirmingDiscard] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .tripFrames(trip.id)
      .then(setFrames)
      .catch(() => setFrames([]));
    Promise.all([api.listCompanies(), api.listDrivers(), api.listVehicles()])
      .then(([c, d, v]) => {
        setCompanies(c);
        setDrivers(d);
        setVehicles(v);
        // Preselect the trip's matched vehicle when known (pending approval, etc.).
        const known = v.find((veh) => veh.id === trip.vehicle_id);
        if (known) {
          setSelectedVehicleId(known.id);
          if (!trip.company_id) setCompanyId(known.company_id ?? "");
          if (!trip.driver_id) setDriverId(known.default_driver_id ?? "");
          if (trip.capacity_at_trip == null) setCapacity(known.registered_capacity != null ? String(known.registered_capacity) : "");
          setCapacityUnit(known.capacity_unit ?? "litres");
        } else {
          setCapacityUnit(trip.capacity_unit ?? "litres");
        }
      })
      .catch((e) => setError(String(e)));
  }, [trip]);

  const candidates =
    trip.reason === "multiple_matches" || trip.reason === "no_match" ? vehicles : vehicles.filter((v) => v.id === trip.vehicle_id);

  const selectVehicle = (v: VehicleView) => {
    setSelectedVehicleId(v.id);
    setCompanyId(v.company_id ?? "");
    setDriverId(v.default_driver_id ?? "");
    setCapacity(v.registered_capacity != null ? String(v.registered_capacity) : "");
    setCapacityUnit(v.capacity_unit ?? "litres");
    setExtraFields(v.extra_fields ?? {});
    setRegisterNew(false);
    setDupWarning(null);
  };

  const confirmExisting = async () => {
    if (!selectedVehicleId) {
      setError("Select a matching vehicle first.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const cap = capacity.trim() === "" ? null : Number(capacity);
      const logged = await api.resolveQueuedExisting(
        trip.id,
        officerId,
        selectedVehicleId,
        companyId || null,
        driverId || null,
        Number.isFinite(cap as number) ? (cap as number) : null,
        capacityUnit,
        receipt.trim() || null,
      );
      await onDone(`Trip ${logged.plate_number} resolved.`, logged);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const registerAndLog = async (confirmDuplicate: boolean) => {
    setBusy(true);
    setError(null);
    setDupWarning(null);
    try {
      const cap = capacity.trim() === "" ? null : Number(capacity);
      const logged = await api.resolveQueuedNew(
        trip.id,
        officerId,
        newPlate.trim(),
        companyId || null,
        Number.isFinite(cap as number) ? (cap as number) : null,
        capacityUnit,
        driverId || null,
        confirmDuplicate,
        extraFields,
      );
      await onDone(`Trip ${logged.plate_number} resolved — vehicle ${newPlate.trim().toUpperCase()} registered.`, logged);
    } catch (e) {
      const msg = String(e);
      if (!confirmDuplicate && msg.toLowerCase().includes("already registered")) {
        setDupWarning(msg);
      } else {
        setError(msg);
      }
    } finally {
      setBusy(false);
    }
  };

  const discard = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.discardTrip(trip.id, officerId);
      await onDone(`Trip ${trip.plate_number} discarded — not counted.`, null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="row between">
          <h3 style={{ margin: 0 }}>
            Resolve <span className="plate-font">{trip.plate_number}</span>
          </h3>
          <button className="ghost" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </div>

        <div className="row" style={{ margin: "10px 0", gap: 8 }}>
          <span className="badge pin">{reasonLabel(trip.reason ?? "", entityLabel("vehicle"))}</span>
          <span className="muted small">
            Best guess: <b>{trip.plate_number}</b>
          </span>
          {trip.confidence_score != null && (
            <span className="muted small">confidence {Math.round(trip.confidence_score * 100)}%</span>
          )}
        </div>

        <div className="row muted small" style={{ gap: 16 }}>
          <span>
            Entry <b>{fmtDateTime(trip.entry_time)}</b>
          </span>
          <span>
            Resolving now <b>{fmtDateTime(new Date().toISOString())}</b>
          </span>
        </div>

        {frames.length > 0 ? (
          <>
            {(() => {
              const entryFrames = frames.filter((f) => !f.kind || f.kind === "entry");
              const exitFrames = frames.filter((f) => f.kind === "exit");
              return (
                <>
                  {entryFrames.length > 0 && (
                    <div>
                      <div className="muted small" style={{ marginBottom: 4, fontWeight: 600 }}>▶ Entry photos</div>
                      <div className="frame-strip">
                        {entryFrames.map((f) => (
                          <div key={f.index} className="frame-card">
                            {f.data_base64 ? (
                              <img src={`data:image/png;base64,${f.data_base64}`} alt={`entry frame ${f.index}`} />
                            ) : (
                              <div className="frame-missing">frame {f.index}</div>
                            )}
                            <div className="muted small">{fmtTime(f.captured_at)}</div>
                          </div>
                        ))}
                      </div>
                    </div>
                  )}
                  {exitFrames.length > 0 && (
                    <div style={{ marginTop: 10 }}>
                      <div className="muted small" style={{ marginBottom: 4, fontWeight: 600 }}>▼ Exit photos</div>
                      <div className="frame-strip">
                        {exitFrames.map((f) => (
                          <div key={f.index} className="frame-card">
                            {f.data_base64 ? (
                              <img src={`data:image/png;base64,${f.data_base64}`} alt={`exit frame ${f.index}`} />
                            ) : (
                              <div className="frame-missing">frame {f.index}</div>
                            )}
                            <div className="muted small">{fmtTime(f.captured_at)}</div>
                          </div>
                        ))}
                      </div>
                    </div>
                  )}
                </>
              );
            })()}
          </>
        ) : (
          <p className="muted small">No camera frames on record (manual entry).</p>
        )}

        {error && <div className="error-banner">{error}</div>}

        {!registerNew ? (
          <>
            <div className="muted small" style={{ marginTop: 10 }}>
              Possible matches — select one to auto-fill:
            </div>
            <div className="match-list">
              {candidates.map((v) => (
                <button
                  key={v.id}
                  className={`match-card ${selectedVehicleId === v.id ? "selected" : ""}`}
                  onClick={() => selectVehicle(v)}
                >
                  <span className="plate-font">{v.plate_number}</span>
                  <span>{v.company_name ?? "—"}</span>
                  <span>{v.default_driver_name ?? "—"}</span>
                  <span>{v.registered_capacity != null ? `${v.registered_capacity} t` : "—"}</span>
                </button>
              ))}
            </div>

            <div className="entry-grid" style={{ marginTop: 12 }}>
              <div>
                <div className="muted small">{label("vehicle", "company")}</div>
                <select value={companyId} onChange={(e) => setCompanyId(e.target.value)}>
                  <option value="">— none —</option>
                  {companies.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name}
                    </option>
                  ))}
                </select>
              </div>
              <div>
                <div className="muted small">{label("vehicle", "driver")}</div>
                <select value={driverId} onChange={(e) => setDriverId(e.target.value)}>
                  <option value="">— none —</option>
                  {drivers.map((d) => (
                    <option key={d.id} value={d.id}>
                      {d.name}
                    </option>
                  ))}
                </select>
              </div>
              <div>
                <div className="muted small">{label("vehicle", "registered_capacity")} at trip</div>
                <input value={capacity} onChange={(e) => setCapacity(e.target.value)} placeholder="20" />
              </div>
              <div>
                <div className="muted small">Receipt no.</div>
                <input value={receipt} onChange={(e) => setReceipt(e.target.value)} placeholder="RC-…" />
              </div>
            </div>

            <div className="row" style={{ marginTop: 14, gap: 8 }}>
              <button className="primary" onClick={confirmExisting} disabled={busy || !selectedVehicleId}>
                Confirm & log trip
              </button>
              <button className="ghost" onClick={() => setRegisterNew(true)} disabled={busy}>
                New {entityLabel("vehicle").toLowerCase()} / none of these
              </button>
              <button className="danger" onClick={() => setConfirmingDiscard(true)} disabled={busy}>
                Discard
              </button>
              <button className="danger" onClick={() => onDecline(trip)} disabled={busy}>
                Decline read
              </button>
              <button className="ghost" onClick={onClose} disabled={busy}>
                Skip / resolve later
              </button>
            </div>
          </>
        ) : (
          <>
            <div className="entry-grid" style={{ marginTop: 10 }}>
              <div>
                <div className="muted small">
                  {label("vehicle", "plate_number")} (new {entityLabel("vehicle").toLowerCase()}) *
                </div>
                <input
                  className="plate-input"
                  value={newPlate}
                  onChange={(e) => setNewPlate(e.target.value.toUpperCase())}
                  placeholder="e.g. Z777ZC"
                />
              </div>
              <div>
                <div className="muted small">{label("vehicle", "company")}</div>
                <select value={companyId} onChange={(e) => setCompanyId(e.target.value)}>
                  <option value="">— none —</option>
                  {companies.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name}
                    </option>
                  ))}
                </select>
              </div>
              <div>
                <div className="muted small">{label("vehicle", "driver")}</div>
                <select value={driverId} onChange={(e) => setDriverId(e.target.value)}>
                  <option value="">— none —</option>
                  {drivers.map((d) => (
                    <option key={d.id} value={d.id}>
                      {d.name}
                    </option>
                  ))}
                </select>
              </div>
              <div>
                <div className="muted small">Registered capacity (L)</div>
                <div className="row" style={{ gap: 8, alignItems: "flex-end" }}>
                  <input
                    style={{ flex: 1 }}
                    value={capacity}
                    onChange={(e) => setCapacity(e.target.value)}
                    placeholder="20"
                  />
                  <select
                    style={{ width: 140 }}
                    value={capacityUnit}
                    onChange={(e) => setCapacityUnit(e.target.value)}
                  >
                    {MEASUREMENT_UNIT_GROUPS.map((g) => (
                      <optgroup key={g.label} label={g.label}>
                        {g.units.map((u) => (
                          <option key={u.value} value={u.value}>
                            {u.label}
                          </option>
                        ))}
                      </optgroup>
                    ))}
                  </select>
                </div>
              </div>
              {customVehicleDefs.map((fd) => (
                <DynamicFieldInput
                  key={fd.id}
                  fd={fd}
                  value={extraFields[fd.field_key]}
                  onChange={(v) => setExtraFields((prev) => ({ ...prev, [fd.field_key]: v }))}
                />
              ))}
            </div>
            <p className="muted small" style={{ marginTop: 8 }}>
              Registers the vehicle in the reference database for all future trips.
            </p>

            {dupWarning && (
              <div className="warn-banner">
                {dupWarning}
                <div className="row" style={{ marginTop: 8, gap: 8 }}>
                  <button className="primary" onClick={() => registerAndLog(true)} disabled={busy}>
                    Attach to existing vehicle instead
                  </button>
                  <button className="ghost" onClick={() => setDupWarning(null)} disabled={busy}>
                    Edit plate
                  </button>
                </div>
              </div>
            )}

            <div className="row" style={{ marginTop: 14, gap: 8 }}>
              <button className="primary" onClick={() => registerAndLog(false)} disabled={busy || !newPlate.trim()}>
                Register & log trip
              </button>
              <button className="ghost" onClick={() => setRegisterNew(false)} disabled={busy}>
                ← Back to matches
              </button>
            </div>
          </>
        )}
      </div>

      {confirmingDiscard && (
        <ConfirmAction
          title="Discard this trip?"
          body={`Trip ${trip.plate_number} will be marked discarded and will not be counted. This is for false detections and non-vehicles only.`}
          confirmLabel="Discard trip"
          danger
          onConfirm={async () => {
            await discard();
            setConfirmingDiscard(false);
          }}
          onCancel={() => setConfirmingDiscard(false)}
        />
      )}
    </div>
  );
}

/** Generic confirmation dialog for a high-impact action (05-ui-screens.md §7). */
function ConfirmAction({
  title,
  body,
  confirmLabel,
  danger,
  onConfirm,
  onCancel,
}: {
  title: string;
  body: string;
  confirmLabel: string;
  danger?: boolean;
  onConfirm: () => Promise<void>;
  onCancel: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      await onConfirm();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <div className="overlay" onClick={onCancel}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3 style={{ marginTop: 0 }}>{title}</h3>
        <p className="muted small">{body}</p>
        {error && <div className="error-banner">{error}</div>}
        <div className="row" style={{ marginTop: 14, gap: 8 }}>
          <button className={danger ? "danger" : "primary"} onClick={run} disabled={busy}>
            {confirmLabel}
          </button>
          <button className="ghost" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}

/** Two-step discharge classification (08-anpr-integration.md §9): Yes/No first,
 * then each answer requires its own Confirm/Cancel before anything commits. */
function DischargeStep({
  trip,
  onConfirm,
  onClose,
}: {
  trip: TripView;
  onConfirm: (isDischarge: boolean) => Promise<void>;
  onClose: () => void;
}) {
  const [pick, setPick] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = async () => {
    if (pick == null) return;
    setBusy(true);
    setError(null);
    try {
      await onConfirm(pick);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3 style={{ marginTop: 0 }}>Was this a discharge trip?</h3>
        <p className="muted small">
          Plate <b className="plate-font">{trip.plate_number}</b> · entry {fmtDateTime(trip.entry_time)}{trip.exit_time ? ` · exit ${fmtDateTime(trip.exit_time)}` : ""} · <span className={`badge ${trip.trip_status === 'complete' ? 'active' : trip.trip_status === 'missed_exit' ? 'disabled' : 'pin'}`}>{trip.trip_status}</span>
        </p>
        <p className="muted small">
          Discharge classification is a judgment call only the officer on site can make. Nothing is committed until you
          confirm.
        </p>
        {pick == null ? (
          <div className="row" style={{ marginTop: 14, gap: 8 }}>
            <button className="primary" onClick={() => setPick(true)} disabled={busy}>
              Yes — discharge trip
            </button>
            <button className="ghost" onClick={() => setPick(false)} disabled={busy}>
              No — non-discharge entry
            </button>
          </div>
        ) : (
          <>
            <div className={pick ? "warn-banner" : "success-banner"} style={{ marginTop: 12 }}>
              {pick
                ? "This will be logged as a discharge trip."
                : "This will be logged as a non-discharge entry — excluded from trip analytics but retained for record-keeping."}
            </div>
            {error && <div className="error-banner">{error}</div>}
            <div className="row" style={{ marginTop: 14, gap: 8 }}>
              <button className="primary" onClick={run} disabled={busy}>
                Confirm
              </button>
              <button className="ghost" onClick={() => setPick(null)} disabled={busy}>
                Cancel
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function statusLabel(status: string): string {
  switch (status) {
    case "logged":
      return "Logged";
    case "queued":
      return "Queued";
    case "discarded":
      return "Discarded";
    case "declined":
      return "Declined";
    default:
      return status;
  }
}

function fmtTime(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function fmtDateTime(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return `${d.toLocaleDateString()} ${fmtTime(iso)}`;
}
