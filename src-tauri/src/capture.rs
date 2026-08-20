//! Capture pipeline — ANPR reads, cross-reference, trip creation, manual entry.
//!
//! All matching/business logic lives HERE (in the app), never in the ANPR service
//! (02-architecture.md §4). The ANPR source only reports "what it saw".

use std::collections::VecDeque;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::db::{append_audit, now_iso, AppState};
use crate::models::{
    AnprRead, AnprStatus, CaptureSettings, IngestResult, MatchOutcome, TripView, VehicleView,
};
use crate::reference::normalize_plate;

const UNKNOWN_CHARS: &[char] = &['*', '?', '.', ' '];

// ---------------------------------------------------------------------------
// ANPR source abstraction (02-architecture.md §4)
// ---------------------------------------------------------------------------

pub trait AnprSource: Send + Sync {
    fn poll(&self) -> Option<AnprRead>;
}

/// Dev simulator: emits structured reads from a scriptable queue, so the full
/// pipeline is testable with zero camera hardware.
pub struct SimulatorSource {
    queue: Mutex<VecDeque<AnprRead>>,
}

impl SimulatorSource {
    pub fn new() -> Self {
        Self { queue: Mutex::new(VecDeque::new()) }
    }

    pub fn push(&self, read: AnprRead) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(read);
        }
    }

    pub fn pending(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }
}

/// Append one row to the persistent ANPR read-event series (05-ui-screens.md
/// §6h). Every read the pipeline consumes — from the poller or a simulator —
/// lands here so System Monitor can plot confidence over time. Best-effort:
/// a failure to record must never break the capture flow.
pub fn record_read_event(
    conn: &Connection,
    read: &AnprRead,
    source: &str,
    status: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO anpr_read_events (id, timestamp, plate, confidence, engine, source, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?2)",
        params![
            uuid::Uuid::new_v4().to_string(),
            read.timestamp,
            read.plate,
            read.confidence,
            read.ocr_engine,
            source,
            status
        ],
    )
    .map_err(|e| format!("read event record failed: {e}"))?;
    Ok(())
}


impl AnprSource for SimulatorSource {
    fn poll(&self) -> Option<AnprRead> {
        self.queue.lock().ok()?.pop_front()
    }
}

/// Polls a real local ANPR service over HTTP (`{plate, confidence, timestamp,
/// frames}` JSON). The production Plate Recognizer adapter plugs in here.
pub struct HttpSource {
    last_timestamp: Mutex<Option<String>>,
}

impl HttpSource {
    pub fn new() -> Self {
        Self { last_timestamp: Mutex::new(None) }
    }
}

impl AnprSource for HttpSource {
    fn poll(&self) -> Option<AnprRead> {
        let url = format!("http://127.0.0.1:9800/latest");
        let body = fetch_http(&url)?;
        let v: Value = serde_json::from_str(&body).ok()?;
        let read: AnprRead = serde_json::from_value(v).ok()?;
        let mut last = self.last_timestamp.lock().ok()?;
        if last.as_ref() == Some(&read.timestamp) {
            return None; // unchanged since last poll
        }
        *last = Some(read.timestamp.clone());
        Some(read)
    }
}

impl HttpSource {
    /// Whether the local ANPR service accepts connections. `poll` returns None
    /// for both "unreachable" and "unchanged timestamp", so System Monitor uses
    /// this cheap TCP probe to distinguish a genuine outage from an
    /// idle-but-healthy service.
    pub fn reachable(&self) -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        match "127.0.0.1:9800".parse() {
            Ok(addr) => TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok(),
            Err(_) => false,
        }
    }
}

/// Minimal dependency-free HTTP GET to 127.0.0.1. Returns None on any error so
/// an unreachable ANPR service degrades gracefully (resilience, 02 §5).
fn fetch_http(url: &str) -> Option<String> {
    let (host, port, path) = split_url(url)?;
    let mut stream = std::net::TcpStream::connect((host, port)).ok()?;
    use std::io::{Read, Write};
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let idx = text.find("\r\n\r\n")?;
    Some(text[idx + 4..].to_string())
}

fn split_url(url: &str) -> Option<(&str, u16, &str)> {
    let rest = url.strip_prefix("http://")?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.find(':') {
        Some(i) => (&host_port[..i], host_port[i + 1..].parse().ok()?),
        None => (host_port, 80),
    };
    Some((host, port, path))
}

// ---------------------------------------------------------------------------
// Settings helpers
// ---------------------------------------------------------------------------

fn get_setting(conn: &Connection, key: &str, default: &str) -> String {
    conn.query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |r| r.get::<_, String>(0))
        .unwrap_or_else(|_| default.to_string())
}

fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(|_| ())
    .map_err(|e| format!("settings update failed: {e}"))
}

pub fn consent_mode(conn: &Connection) -> String {
    let m = get_setting(conn, "consent_mode_default", "confirm_required");
    if m == "fully_automatic" { "fully_automatic".to_string() } else { "confirm_required".to_string() }
}

/// The active engine's confidence threshold (08 §3: thresholds are tuned
/// independently per OCR engine — never one shared number). Falls back to the
/// legacy shared app-setting value so older installs migrate cleanly.
pub fn confidence_threshold(conn: &Connection) -> f64 {
    let engine = active_ocr_engine(conn);
    let per_engine = crate::anpr::confidence_threshold_for(conn, &engine);
    if per_engine >= 0.0 && per_engine <= 1.0 {
        return per_engine;
    }
    get_setting(conn, "anpr_confidence_threshold", "0.7").parse().unwrap_or(0.7)
}

/// The currently active OCR engine (08-anpr-integration.md §3). Exactly one
/// engine is active at any time, admin-selected, each with its own threshold.
pub fn active_ocr_engine(conn: &Connection) -> String {
    conn.query_row(
        "SELECT active_ocr_engine FROM anpr_config WHERE id = ?1",
        params![crate::db::ANPR_CONFIG_ID],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "paddleocr".to_string())
}

/// Whether the Yes/No discharge classification step is enforced for confirm-mode
/// approvals (08-anpr-integration.md §9, 01-database-schema.md `anpr_config`).
pub fn discharge_confirmation_required(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT discharge_confirmation_required FROM anpr_config WHERE id = ?1",
        params![crate::db::ANPR_CONFIG_ID],
        |r| r.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(true)
}

/// Effective OCR engine for a trip row: the source's reported engine, or the
/// active engine for auto reads, or `manual` for manual entry. Auto and
/// manual-confirm reads must never omit this (01-database-schema.md `trips`).
fn effective_ocr_engine(conn: &Connection, read: &AnprRead, capture_method: &str) -> String {
    read.ocr_engine
        .clone()
        .unwrap_or_else(|| {
            if capture_method == "manual_entry" {
                "manual".to_string()
            } else {
                active_ocr_engine(conn)
            }
        })
}

/// Effective model version for a trip row. Manual entry has no model; auto reads
/// without an explicit version fall back to the simulator marker for dev/tests.
fn effective_model_version(read: &AnprRead, capture_method: &str) -> Option<String> {
    read.model_version.clone().or_else(|| {
        if capture_method == "manual_entry" {
            None
        } else {
            Some("simulator".to_string())
        }
    })
}

pub fn anpr_enabled(conn: &Connection) -> bool {
    get_setting(conn, "anpr_enabled", "true") == "true"
}

pub fn anpr_source(conn: &Connection) -> String {
    get_setting(conn, "anpr_source", "simulator")
}

pub fn is_capture_point(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT is_capture_point FROM anpr_config WHERE id = ?1",
        params![crate::db::ANPR_CONFIG_ID],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0) != 0
}

pub fn anpr_service_url(conn: &Connection) -> String {
    get_setting(conn, "anpr_service_url", "http://127.0.0.1:9800")
}

// ---------------------------------------------------------------------------
// Cross-reference (04-capture-pipeline.md §4)
// ---------------------------------------------------------------------------

fn load_active_vehicles(conn: &Connection) -> Result<Vec<VehicleView>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT v.id, v.plate_number, v.company_id, c.name, v.registered_capacity,
                    v.capacity_unit, v.default_driver_id, d.name, v.status, v.created_at
             FROM vehicles v
             LEFT JOIN companies c ON c.id = v.company_id
             LEFT JOIN drivers d ON d.id = v.default_driver_id
             WHERE v.status = 'active'",
        )
        .map_err(|e| format!("vehicle query failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(VehicleView {
                id: r.get(0)?,
                plate_number: r.get(1)?,
                company_id: r.get(2)?,
                company_name: r.get(3)?,
                registered_capacity: r.get(4)?,
                capacity_unit: r.get(5)?,
                default_driver_id: r.get(6)?,
                default_driver_name: r.get(7)?,
                status: r.get(8)?,
                extra_fields: None,
                created_at: r.get(9)?,
            })
        })
        .map_err(|e| format!("vehicle query failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("vehicle read failed: {e}"))
}

fn partial_consistent(read: &str, plate: &str) -> bool {
    let r: Vec<char> = read.chars().collect();
    let p: Vec<char> = plate.chars().collect();
    if r.len() != p.len() {
        return false;
    }
    for i in 0..r.len() {
        if UNKNOWN_CHARS.contains(&r[i]) {
            continue;
        }
        if r[i] != p[i] {
            return false;
        }
    }
    true
}

pub fn cross_reference(conn: &Connection, raw_plate: &str) -> Result<MatchOutcome, String> {
    let read = normalize_plate(raw_plate);
    if read.is_empty() {
        return Err("Read contained no plate.".to_string());
    }
    let vehicles = load_active_vehicles(conn)?;

    let exact = vehicles.iter().find(|v| normalize_plate(&v.plate_number) == read).cloned();
    if let Some(v) = exact {
        return Ok(MatchOutcome {
            state: "exact".to_string(),
            matched_vehicle_id: Some(v.id.clone()),
            candidates: vec![v],
        });
    }

    let has_unknown = read.chars().any(|c| UNKNOWN_CHARS.contains(&c));
    if has_unknown {
        let matched: Vec<VehicleView> = vehicles
            .iter()
            .filter(|v| partial_consistent(&read, &normalize_plate(&v.plate_number)))
            .cloned()
            .collect();
        return match matched.len() {
            0 => Ok(MatchOutcome {
                state: "zero".to_string(),
                matched_vehicle_id: None,
                candidates: vec![],
            }),
            1 => {
                let v = matched.into_iter().next().unwrap();
                Ok(MatchOutcome {
                    state: "narrowed".to_string(),
                    matched_vehicle_id: Some(v.id.clone()),
                    candidates: vec![v],
                })
            }
            _ => Ok(MatchOutcome {
                state: "multiple".to_string(),
                matched_vehicle_id: None,
                candidates: matched,
            }),
        };
    }

    Ok(MatchOutcome { state: "zero".to_string(), matched_vehicle_id: None, candidates: vec![] })
}

// ---------------------------------------------------------------------------
// Trip row helpers
// ---------------------------------------------------------------------------

pub(crate) const TRIP_SELECT: &str = "SELECT t.id,
    COALESCE(v.plate_number, json_extract(t.resolution_notes, '$.plate'), ''),
    t.company_id, c.name, t.driver_id, d.name, t.capacity_at_trip, t.capacity_unit,
    COALESCE(t.entry_time, t.time_in), t.receipt_no,
    t.officer_id, u.name, t.capture_method, t.confidence_score,
    COALESCE(t.entry_photo_refs, t.photo_refs), t.status, t.resolution_notes,
    t.vehicle_id, t.is_discharge_trip, t.model_version, t.ocr_engine,
    t.exit_time, t.trip_status, t.exit_photo_refs
    FROM trips t
    LEFT JOIN vehicles v ON v.id = t.vehicle_id
    LEFT JOIN companies c ON c.id = t.company_id
    LEFT JOIN drivers d ON d.id = t.driver_id
    LEFT JOIN users u ON u.id = t.officer_id";

pub(crate) fn read_trip(row: &rusqlite::Row) -> rusqlite::Result<TripView> {
    let photo_refs: Option<String> = row.get(14)?;
    let photo_count = photo_refs
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
        .map(|a| a.len())
        .unwrap_or(0);
    let resolution: Option<String> = row.get(16)?;
    let (reason, candidates) = match resolution.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
        Some(Value::Object(map)) => {
            let reason = map.get("reason").and_then(|v| v.as_str()).map(String::from);
            let candidates = map
                .get("candidates")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            (reason, candidates)
        }
        _ => (None, vec![]),
    };
    let discharge: Option<i64> = row.get(18)?;
    let entry_time: String = row.get(8)?;
    let exit_photo_refs: Option<String> = row.get(23)?;
    let exit_photo_count = exit_photo_refs
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
        .map(|a| a.len())
        .unwrap_or(0);
    Ok(TripView {
        id: row.get(0)?,
        vehicle_id: row.get(17)?,
        plate_number: row.get(1)?,
        company_id: row.get(2)?,
        company_name: row.get(3)?,
        driver_id: row.get(4)?,
        driver_name: row.get(5)?,
        capacity_at_trip: row.get(6)?,
        capacity_unit: row.get(7)?,
        entry_time: entry_time.clone(),
        exit_time: row.get(21)?,
        trip_status: row.get(22)?,
        time_in: entry_time,
        receipt_no: row.get(9)?,
        officer_id: row.get(10)?,
        officer_name: row.get(11)?,
        capture_method: row.get(12)?,
        confidence_score: row.get(13)?,
        entry_photo_count: photo_count,
        exit_photo_count: exit_photo_count,
        photo_count,
        status: row.get(15)?,
        reason,
        candidates,
        is_discharge_trip: discharge.map(|v| v != 0),
        model_version: row.get(19)?,
        ocr_engine: row.get(20)?,
    })
}

fn trip_by_id(conn: &Connection, id: &str) -> Result<TripView, String> {
    conn.query_row(&format!("{TRIP_SELECT} WHERE t.id = ?1"), params![id], read_trip)
        .map_err(|_| "Trip not found.".to_string())
}

fn resolution_json(plate: &str, reason: Option<&str>, candidates: &[VehicleView]) -> String {
    let mut m = serde_json::Map::new();
    m.insert("plate".to_string(), Value::String(plate.to_string()));
    if let Some(r) = reason {
        m.insert("reason".to_string(), Value::String(r.to_string()));
    }
    if !candidates.is_empty() {
        m.insert(
            "candidates".to_string(),
            Value::Array(candidates.iter().map(|v| Value::String(v.id.clone())).collect()),
        );
    }
    Value::Object(m).to_string()
}

/// Create a `logged` trip for a resolved match. All point-in-time fields are
/// copied onto the row, never live-referenced (01-database-schema.md note).
#[allow(clippy::too_many_arguments)]
fn insert_trip(
    conn: &Connection,
    officer_id: Option<String>,
    read: &AnprRead,
    vehicle: &VehicleView,
    capture_method: &str,
    status: &str,
    frames_dir: &std::path::Path,
) -> Result<TripView, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    // A new logged trip is an ENTRY sighting: entry photo set, exit pending.
    // Its trip_status starts "open" until the matching exit is seen (§4.3).
    let entry_photo_refs = crate::evidence::persist_frames(frames_dir, &id, &read.frames, "entry")?;
    let trip_status = if status == "queued" || status == "pending_approval" { "open" } else { "open" };
    let time_in = if read.timestamp.is_empty() { now.clone() } else { read.timestamp.clone() };
    // Manual entry carries no ANPR confidence — score stays null (04 §8).
    let confidence = if capture_method == "manual_entry" { None } else { Some(read.confidence) };
    let ocr_engine = effective_ocr_engine(conn, read, capture_method);
    let model_version = effective_model_version(read, capture_method);
    conn.execute(
        "INSERT INTO trips (id, vehicle_id, driver_id, company_id, capacity_at_trip, capacity_unit, time_in,
                officer_id, capture_method, confidence_score, photo_refs, status, resolution_notes,
                model_version, ocr_engine, created_at, updated_at,
                entry_time, trip_status, entry_photo_refs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16,
                ?17, ?18, ?19)",
        params![
            id,
            vehicle.id,
            vehicle.default_driver_id,
            vehicle.company_id,
            vehicle.registered_capacity,
            vehicle.capacity_unit,
            time_in,
            officer_id,
            capture_method,
            confidence,
            entry_photo_refs,
            status,
            resolution_json(&normalize_plate(&read.plate), None, &[]),
            model_version,
            ocr_engine,
            now,
            time_in,
            trip_status,
            entry_photo_refs,
        ],
    )
    .map_err(|e| format!("trip creation failed: {e}"))?;
    trip_by_id(conn, &id)
}

/// Create a queued trip (exception routing, 04 §6). Keeps the original capture
/// timestamp untouched and all frames for later resolution.
#[allow(clippy::too_many_arguments)]
fn insert_queued(
    conn: &Connection,
    officer_id: Option<String>,
    read: &AnprRead,
    plate: &str,
    reason: &str,
    outcome: &MatchOutcome,
    capture_method: &str,
    frames_dir: &std::path::Path,
) -> Result<TripView, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    let entry_photo_refs = crate::evidence::persist_frames(frames_dir, &id, &read.frames, "entry")?;
    let vehicle = outcome.matched_vehicle_id.as_deref().and_then(|vid| {
        load_active_vehicles(conn)
            .ok()
            .and_then(|vs| vs.into_iter().find(|v| v.id == vid))
    });
    let time_in = if read.timestamp.is_empty() { now.clone() } else { read.timestamp.clone() };
    let ocr_engine = effective_ocr_engine(conn, read, capture_method);
    let model_version = effective_model_version(read, capture_method);
    conn.execute(
        "INSERT INTO trips (id, vehicle_id, driver_id, company_id, capacity_at_trip, capacity_unit, time_in,
                officer_id, capture_method, confidence_score, photo_refs, status, resolution_notes,
                model_version, ocr_engine, created_at, updated_at,
                entry_time, trip_status, entry_photo_refs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'queued', ?12, ?13, ?14, ?15, ?15,
                ?16, 'open', ?17)",
        params![
            id,
            vehicle.as_ref().map(|v| v.id.clone()),
            vehicle.as_ref().and_then(|v| v.default_driver_id.clone()),
            vehicle.as_ref().and_then(|v| v.company_id.clone()),
            vehicle.as_ref().and_then(|v| v.registered_capacity),
            vehicle.as_ref().map(|v| v.capacity_unit.clone()).unwrap_or_else(|| "litres".to_string()),
            time_in,
            officer_id,
            capture_method,
            if capture_method == "auto" { Some(read.confidence) } else { None },
            entry_photo_refs,
            resolution_json(plate, Some(reason), &outcome.candidates),
            model_version,
            ocr_engine,
            now,
            time_in,
            entry_photo_refs,
        ],
    )
    .map_err(|e| format!("trip queue failed: {e}"))?;
    trip_by_id(conn, &id)
}

// ---------------------------------------------------------------------------
// Core ingest — auto (from ANPR) and manual entry. Both run the SAME
// cross-reference logic (04 §8).
// ---------------------------------------------------------------------------

/// Flag a trip's frames into `training_candidates` (08-anpr-integration.md §6.2):
/// low-confidence reads and human-corrected verification-queue items are the most
/// valuable examples for future retraining. No-op when the trip carries no frames
/// (manual entry) or was already flagged (a low-confidence read later corrected
/// keeps its ingest-time `low_confidence` row rather than a duplicate).
pub fn flag_training_candidates(conn: &Connection, trip_id: &str, reason: &str) -> Result<(), String> {
    let already: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM training_candidates WHERE source_trip_id = ?1",
            params![trip_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("training candidate check failed: {e}"))?;
    if already > 0 {
        return Ok(());
    }
    let photo_refs: String = conn
        .query_row(
            "SELECT COALESCE(entry_photo_refs, photo_refs, '[]') FROM trips WHERE id = ?1",
            params![trip_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("trip photo_refs read failed: {e}"))?;
    let refs: Vec<Value> = serde_json::from_str(&photo_refs).unwrap_or_default();
    let now = now_iso();
    for entry in refs {
        let frame_ref = entry.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if frame_ref.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT INTO training_candidates (id, source_trip_id, frame_ref, reason, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![uuid::Uuid::new_v4().to_string(), trip_id, frame_ref, reason, now],
        )
        .map_err(|e| format!("training candidate insert failed: {e}"))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn ingest_read(
    conn: &Connection,
    officer_id: Option<String>,
    read: &AnprRead,
    capture_method: &str,
    frames_dir: &std::path::Path,
) -> Result<IngestResult, String> {
    let plate = normalize_plate(&read.plate);
    if plate.is_empty() {
        return Err("Read contained no plate.".to_string());
    }
    // At least 1 frame required for any capture (entry/exit evidence).
    // ANPR service sends 1 frame per sighting (the best-confidence crop).
    if capture_method == "auto" && read.frames.is_empty() {
        return Err("ANPR read must carry at least 1 frame.".to_string());
    }

    let threshold = confidence_threshold(conn);
    let outcome = cross_reference(conn, &plate)?;

    // Confidence thresholding always applies to auto reads (04 §3, §6).
    if capture_method == "auto" && read.confidence < threshold {
        let trip = insert_queued(conn, officer_id, read, &plate, "low_confidence", &outcome, "auto", frames_dir)?;
        flag_training_candidates(conn, &trip.id, "low_confidence")?;
        return Ok(IngestResult {
            trip: None,
            queued: Some(trip),
            outcome,
            message: "Low confidence read — queued for verification.".to_string(),
        });
    }

    match outcome.state.as_str() {
        "exact" | "narrowed" => {
            let vehicle = load_active_vehicles(conn)?
                .into_iter()
                .find(|v| Some(&v.id) == outcome.matched_vehicle_id.as_ref())
                .ok_or_else(|| "Matched vehicle no longer active.".to_string())?;
            // Schema status enum: logged / queued / resolved / discarded /
            // declined (01-database-schema.md `trips`). Confirm-required
            // captures are `queued` with reason `pending_approval` until
            // one-tap approval.
            let status = if capture_method == "manual_entry" || consent_mode(conn) == "fully_automatic" {
                "logged"
            } else {
                "queued"
            };
            if status == "queued" {
                let trip = insert_queued(conn, officer_id, read, &plate, "pending_approval", &outcome, capture_method, frames_dir)?;
                return Ok(IngestResult {
                    trip: None,
                    queued: Some(trip),
                    outcome,
                    message: "Trip captured — awaiting approval.".to_string(),
                });
            }
            // §4.3 entry/exit: a confirmed match either starts a new entry,
            // closes an open trip as its exit, or (when the pending window has
            // lapsed) marks the old trip missed_exit and starts fresh.
            let result = match_entry_exit(conn, officer_id, read, &vehicle, capture_method, frames_dir)?;
            match result {
                EntryExitOutcome::NewEntry(trip) => Ok(IngestResult {
                    trip: Some(trip),
                    queued: None,
                    outcome,
                    message: "Entry logged — vehicle is inside.".to_string(),
                }),
                EntryExitOutcome::ExitMatched(trip) => Ok(IngestResult {
                    trip: Some(trip),
                    queued: None,
                    outcome,
                    message: "Exit matched — trip complete.".to_string(),
                }),
                EntryExitOutcome::MissedExitThenNewEntry(trip) => Ok(IngestResult {
                    trip: Some(trip),
                    queued: None,
                    outcome,
                    message: "Previous entry closed as missed exit — new entry logged.".to_string(),
                }),
            }
        }
        "multiple" => {
            let trip = insert_queued(conn, officer_id, read, &plate, "multiple_matches", &outcome, capture_method, frames_dir)?;
            Ok(IngestResult {
                trip: None,
                queued: Some(trip),
                outcome,
                message: "Multiple possible matches — queued for verification.".to_string(),
            })
        }
        _ => {
            let trip = insert_queued(conn, officer_id, read, &plate, "no_match", &outcome, capture_method, frames_dir)?;
            Ok(IngestResult {
                trip: None,
                queued: Some(trip),
                outcome,
                message: "No match found — possible new vehicle queued.".to_string(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// §4.3 Entry/exit matching — the core pipeline change per 09-anpr-page-complete-spec.md
// ---------------------------------------------------------------------------

/// §4.3 entry/exit outcome.
enum EntryExitOutcome {
    NewEntry(TripView),
    ExitMatched(TripView),
    MissedExitThenNewEntry(TripView),
}

fn max_pending_hours(conn: &Connection) -> f64 {
    conn.query_row(
        "SELECT COALESCE(max_pending_duration_hours, 24.0) FROM anpr_config LIMIT 1",
        [],
        |r| r.get::<_, f64>(0),
    ).unwrap_or(24.0)
}

/// §4.3: look up an open trip for this vehicle and either close it as an exit,
/// mark it missed and start fresh, or create a brand-new entry.
fn match_entry_exit(
    conn: &Connection,
    officer_id: Option<String>,
    read: &AnprRead,
    vehicle: &VehicleView,
    capture_method: &str,
    frames_dir: &std::path::Path,
) -> Result<EntryExitOutcome, String> {
    let open: Option<(String, String)> = conn.query_row(
        "SELECT id, entry_time FROM trips
         WHERE vehicle_id = ?1 AND trip_status = 'open' AND exit_time IS NULL
           AND status = 'logged'
         ORDER BY entry_time DESC LIMIT 1",
        params![vehicle.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).ok();

    let pending_hours = max_pending_hours(conn);

    if let Some((open_id, entry_time_str)) = open {
        let open_dt = chrono::DateTime::parse_from_rfc3339(&entry_time_str)
            .map(|dt| dt.with_timezone(&chrono::Utc));
        let now_dt = chrono::Utc::now();
        let elapsed_hours = open_dt.map(|dt| (now_dt - dt).num_minutes() as f64 / 60.0).unwrap_or(f64::MAX);

        if elapsed_hours < pending_hours {
            // Within window → EXIT matched on the same open trip.
            let exit_refs = crate::evidence::persist_frames(frames_dir, &open_id, &read.frames, "exit")?;
            conn.execute(
                "UPDATE trips SET exit_time = ?1, trip_status = 'complete',
                        exit_photo_refs = ?2, pushed_to_sheets = 0, updated_at = ?1
                 WHERE id = ?3",
                params![read.timestamp, exit_refs, open_id],
            ).map_err(|e| format!("exit update failed: {e}"))?;
            return Ok(EntryExitOutcome::ExitMatched(trip_by_id(conn, &open_id)?));
        }
        // Beyond window → mark old missed_exit, then fall through to create fresh entry.
        conn.execute(
            "UPDATE trips SET trip_status = 'missed_exit', updated_at = ?1 WHERE id = ?2",
            params![now_iso(), open_id],
        ).map_err(|e| format!("missed_exit update failed: {e}"))?;
    }

    let trip = insert_trip(conn, officer_id, read, vehicle, capture_method, "logged", frames_dir)?;
    Ok(EntryExitOutcome::NewEntry(trip))
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

fn officer_id_for_session(state: &State<AppState>) -> Result<String, String> {
    let session = state.session.lock().map_err(|e| e.to_string())?;
    session
        .as_ref()
        .map(|s| s.user_id.clone())
        .ok_or_else(|| "No user is logged in. Log in to capture trips.".to_string())
}

/// Dev tool: push a structured read straight through the pipeline and emit the
/// updated state to the UI.
#[tauri::command]
pub fn simulate_read(
    app: AppHandle,
    state: State<AppState>,
    plate: String,
    confidence: f64,
) -> Result<IngestResult, String> {
    let officer = officer_id_for_session(&state)?;
    let frames = (0..3)
        .map(|i| crate::models::AnprFrame {
            index: i,
            captured_at: now_iso(),
            kind: "simulated".to_string(),
            data: None,
        })
        .collect();
    let read = AnprRead {
        plate,
        confidence: confidence.clamp(0.0, 1.0),
        timestamp: now_iso(),
        frames,
        model_version: Some("simulator".to_string()),
        ocr_engine: None,
    };
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let result = ingest_read(&conn, Some(officer), &read, "auto", &state.frames_dir)?;
    drop(conn);
    emit_capture_update(&app);
    Ok(result)
}

#[tauri::command]
pub fn manual_entry(
    app: AppHandle,
    state: State<AppState>,
    plate: String,
    officer_id: String,
) -> Result<IngestResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let result = manual_entry_impl(&conn, &officer_id, &plate, &state.frames_dir)?;
    drop(conn);
    emit_capture_update(&app);
    Ok(result)
}

/// Shared manual-entry logic (04 §8): full cross-reference, `capture_method =
/// manual_entry`, `confidence_score = null`, `ocr_engine = manual`. Works with
/// the ANPR service fully stopped. Exposed for tests.
pub fn manual_entry_impl(
    conn: &Connection,
    officer_id: &str,
    plate: &str,
    frames_dir: &std::path::Path,
) -> Result<IngestResult, String> {
    let frames: Vec<crate::models::AnprFrame> = vec![];
    let read = AnprRead {
        plate: plate.to_string(),
        confidence: 0.0,
        timestamp: now_iso(),
        frames,
        model_version: None,
        ocr_engine: Some("manual".to_string()),
    };
    ingest_read(conn, Some(officer_id.to_string()), &read, "manual_entry", frames_dir)
}

#[tauri::command]
pub fn approve_trip(
    app: AppHandle,
    state: State<AppState>,
    trip_id: String,
    officer_id: String,
) -> Result<TripView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let trip = approve_trip_impl(&conn, &trip_id, &officer_id)?;
    drop(conn);
    emit_capture_update(&app);
    Ok(trip)
}

/// Approve a pending trip (confirm-required capture). Preserves the original
/// capture `time_in` (04 §6) — only the status and resolution notes change.
/// Accepts schema status `queued` with reason `pending_approval` (and legacy
/// `pending_approval` rows). Exposed for tests.
pub fn approve_trip_impl(conn: &Connection, trip_id: &str, officer_id: &str) -> Result<TripView, String> {
    let status: String = conn
        .query_row("SELECT status FROM trips WHERE id = ?1", params![trip_id], |r| r.get(0))
        .map_err(|_| "Trip not found.".to_string())?;
    let resolution: String = conn
        .query_row(
            "SELECT COALESCE(resolution_notes, '{}') FROM trips WHERE id = ?1",
            params![trip_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());
    let reason = serde_json::from_str::<Value>(&resolution)
        .ok()
        .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(String::from));
    let approvable = status == "pending_approval"
        || (status == "queued" && reason.as_deref() == Some("pending_approval"));
    if !approvable {
        return Err("Only trips awaiting approval can be approved.".to_string());
    }
    let mut map = serde_json::from_str::<Value>(&resolution).unwrap_or(Value::Object(serde_json::Map::new()));
    if let Value::Object(m) = &mut map {
        m.insert("approved_by".to_string(), Value::String(officer_id.to_string()));
        m.insert("approved_at".to_string(), Value::String(now_iso()));
    }
    conn.execute(
        "UPDATE trips SET status = 'logged', resolution_notes = ?1, updated_at = ?2 WHERE id = ?3",
        params![map.to_string(), now_iso(), trip_id],
    )
    .map_err(|e| format!("trip approval failed: {e}"))?;
    append_audit(conn, officer_id, "approved_trip", Some(trip_id), None)?;
    trip_by_id(conn, trip_id)
}

/// Update an auto-filled trip before finalizing (edit-before-confirm). Preserves
/// the original `time_in` (04 §6).
#[tauri::command]
pub fn update_trip_fields(
    app: AppHandle,
    state: State<AppState>,
    trip_id: String,
    officer_id: String,
    company_id: Option<String>,
    driver_id: Option<String>,
    capacity_at_trip: Option<f64>,
    receipt_no: Option<String>,
) -> Result<TripView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let trip = update_trip_fields_impl(
        &conn,
        &trip_id,
        &officer_id,
        company_id,
        driver_id,
        capacity_at_trip,
        receipt_no,
    )?;
    drop(conn);
    emit_capture_update(&app);
    Ok(trip)
}

/// Edit-before-confirm for an auto-filled trip. Preserves the original
/// `time_in` (04 §6). Exposed for tests.
pub fn update_trip_fields_impl(
    conn: &Connection,
    trip_id: &str,
    officer_id: &str,
    company_id: Option<String>,
    driver_id: Option<String>,
    capacity_at_trip: Option<f64>,
    receipt_no: Option<String>,
) -> Result<TripView, String> {
    let n = conn
        .execute(
            "UPDATE trips SET company_id = ?1, driver_id = ?2, capacity_at_trip = ?3,
                    receipt_no = ?4, updated_at = ?5
             WHERE id = ?6",
            params![company_id, driver_id, capacity_at_trip, receipt_no, now_iso(), trip_id],
        )
        .map_err(|e| format!("trip update failed: {e}"))?;
    if n == 0 {
        return Err("Trip not found.".to_string());
    }
    append_audit(conn, officer_id, "edited_trip", Some(trip_id), None)?;
    trip_by_id(conn, trip_id)
}

#[tauri::command]
pub fn list_today_trips(state: State<AppState>) -> Result<Vec<TripView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let day = chrono::Utc::now().format("%Y-%m-%d");
    let from = format!("{day}T00:00:00Z");
    let mut stmt = conn
        .prepare(&format!(
            "{TRIP_SELECT} WHERE t.time_in >= ?1 AND t.status != 'declined' AND t.archived = 0 ORDER BY t.time_in DESC LIMIT 200"
        ))
        .map_err(|e| format!("trip list failed: {e}"))?;
    let rows = stmt
        .query_map(params![from], read_trip)
        .map_err(|e| format!("trip list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("trip read failed: {e}"))
}

/// Export today's trips (or filtered subset) to a CSV file and return the path.
#[tauri::command]
pub fn export_today_csv(
    state: State<AppState>,
    actor_id: String,
    target_path: String,
    trip_ids: Option<Vec<String>>,
) -> Result<String, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let day = chrono::Utc::now().format("%Y-%m-%d");
    let from = format!("{day}T00:00:00Z");
    let sql = if trip_ids.as_ref().map_or(false, |v| !v.is_empty()) {
        let placeholders: Vec<String> = (0..trip_ids.as_ref().unwrap().len())
            .map(|i| format!("?{}", i + 2))
            .collect();
        format!(
            "{TRIP_SELECT} WHERE t.time_in >= ?1 AND t.id IN ({}) ORDER BY t.time_in DESC",
            placeholders.join(", ")
        )
    } else {
        format!("{TRIP_SELECT} WHERE t.time_in >= ?1 AND t.status != 'declined' ORDER BY t.time_in DESC")
    };
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(from.clone())];
    if let Some(ref ids) = trip_ids {
        for id in ids {
            params.push(Box::new(id.clone()));
        }
    }
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("export query failed: {e}"))?;
    let rows: Vec<TripView> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), read_trip)
        .map_err(|e| format!("export query failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("export read failed: {e}"))?;

    // Build CSV
    let mut csv = String::from("Plate,Company,Driver,Capacity,Unit,Entry Time,Exit Time,Status,Type,Source,Officer\n");
    let cell = |v: &str| -> String {
        if v.contains([',', '"', '\n']) {
            format!("\"{}\"", v.replace('"', "\"\""))
        } else {
            v.to_string()
        }
    };
    for r in &rows {
        let discharge = match r.is_discharge_trip {
            Some(true) => "Discharge",
            Some(false) => "Non-discharge",
            None => "Unclassified",
        };
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            cell(&r.plate_number),
            cell(r.company_name.as_deref().unwrap_or("—")),
            cell(r.driver_name.as_deref().unwrap_or("—")),
            r.capacity_at_trip.map(|v| v.to_string()).unwrap_or_else(|| "—".into()),
            cell(&r.capacity_unit),
            cell(&r.entry_time),
            cell(r.exit_time.as_deref().unwrap_or("")),
            cell(&r.status),
            cell(discharge),
            cell(&r.capture_method),
            cell(r.officer_name.as_deref().unwrap_or("—")),
        ));
    }

    let path = std::path::PathBuf::from(&target_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("export dir failed: {e}"))?;
    }
    std::fs::write(&path, csv).map_err(|e| format!("export write failed: {e}"))?;
    crate::db::append_audit(&conn, &actor_id, "exported_gate_entries", None, Some(serde_json::json!({ "count": rows.len(), "path": path.to_string_lossy() })))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Archive a single trip (soft-delete).
#[tauri::command]
pub fn archive_trip(
    state: State<AppState>,
    actor_id: String,
    trip_id: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE trips SET archived = 1, updated_at = ?1 WHERE id = ?2",
        params![crate::db::now_iso(), trip_id],
    ).map_err(|e| format!("archive trip failed: {e}"))?;
    crate::db::append_audit(&conn, &actor_id, "archived_trip", Some(&trip_id), None)?;
    Ok(())
}

/// Clear today's trips for the gate officer (soft-delete to archive).
#[tauri::command]
pub fn clear_today_trips(
    state: State<AppState>,
    actor_id: String,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let day = chrono::Utc::now().format("%Y-%m-%d");
    let from = format!("{day}T00:00:00Z");
    let n = conn
        .execute(
            "UPDATE trips SET archived = 1, updated_at = ?1 WHERE time_in >= ?2 AND status != 'declined' AND archived = 0",
            params![crate::db::now_iso(), from],
        )
        .map_err(|e| format!("clear trips failed: {e}"))?;
    crate::db::append_audit(&conn, &actor_id, "cleared_gate_entries", None, Some(serde_json::json!({ "count": n })))?;
    Ok(n as i64)
}

#[tauri::command]
pub fn search_trips(state: State<AppState>, query: String) -> Result<Vec<TripView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let q = query.trim().to_string();
    let mut stmt = conn
        .prepare(&format!(
            "{TRIP_SELECT} WHERE t.status != 'declined' AND (?1 = ''
               OR upper(COALESCE(v.plate_number, json_extract(t.resolution_notes, '$.plate'), '')) LIKE '%' || upper(?1) || '%'
               OR lower(COALESCE(c.name, '')) LIKE '%' || lower(?1) || '%'
               OR lower(COALESCE(d.name, '')) LIKE '%' || lower(?1) || '%')
             ORDER BY t.time_in DESC LIMIT 200"
        ))
        .map_err(|e| format!("trip search failed: {e}"))?;
    let rows = stmt
        .query_map(params![q], read_trip)
        .map_err(|e| format!("trip search failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("trip read failed: {e}"))
}

#[tauri::command]
pub fn list_queued(state: State<AppState>) -> Result<Vec<TripView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(&format!("{TRIP_SELECT} WHERE t.status = 'queued' ORDER BY t.time_in ASC"))
        .map_err(|e| format!("queue list failed: {e}"))?;
    let rows = stmt
        .query_map([], read_trip)
        .map_err(|e| format!("queue list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("queue read failed: {e}"))
}

#[tauri::command]
pub fn get_capture_settings(state: State<AppState>) -> Result<CaptureSettings, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    Ok(CaptureSettings {
        consent_mode: consent_mode(&conn),
        confidence_threshold: confidence_threshold(&conn),
        anpr_enabled: anpr_enabled(&conn),
        anpr_source: anpr_source(&conn),
        anpr_service_url: anpr_service_url(&conn),
        discharge_confirmation_required: discharge_confirmation_required(&conn),
        is_capture_point: is_capture_point(&conn),
    })
}

#[tauri::command]
pub fn set_capture_settings(
    state: State<AppState>,
    actor_id: String,
    consent_mode: Option<String>,
    confidence_threshold: Option<f64>,
    anpr_enabled: Option<bool>,
    anpr_source: Option<String>,
    is_capture_point: Option<bool>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    if let Some(m) = consent_mode {
        if m != "confirm_required" && m != "fully_automatic" {
            return Err("Invalid consent mode.".to_string());
        }
        set_setting(&conn, "consent_mode_default", &m)?;
    }
    if let Some(t) = confidence_threshold {
        if !(0.0..=1.0).contains(&t) {
            return Err("Confidence threshold must be between 0 and 1.".to_string());
        }
        // Thresholds are per-engine (08 §3) — write to the active engine's
        // threshold in anpr_config, not a shared value.
        let engine = active_ocr_engine(&conn);
        let col = if engine == "easyocr" { "confidence_threshold_easyocr" } else { "confidence_threshold_paddleocr" };
        conn.execute(
            &format!("UPDATE anpr_config SET {col} = ?1, updated_at = ?2 WHERE id = ?3"),
            params![t, crate::db::now_iso(), crate::db::ANPR_CONFIG_ID],
        )
        .map_err(|e| format!("threshold update failed: {e}"))?;
    }
    if let Some(e) = anpr_enabled {
        set_setting(&conn, "anpr_enabled", if e { "true" } else { "false" })?;
    }
    if let Some(s) = anpr_source {
        if s != "simulator" && s != "http" {
            return Err("Unknown ANPR source.".to_string());
        }
        set_setting(&conn, "anpr_source", &s)?;
        // When this machine is configured with an HTTP ANPR source,
        // it becomes a capture point — the ANPR service will auto-launch
        // on subsequent startups for this machine.
        if s == "http" {
            conn.execute(
                "UPDATE anpr_config SET is_capture_point = 1, updated_at = ?1 WHERE id = ?2",
                params![now_iso(), crate::db::ANPR_CONFIG_ID],
            )
            .map_err(|e| format!("update is_capture_point failed: {e}"))?;
        } else {
            // simulator or removed — not a capture point
            conn.execute(
                "UPDATE anpr_config SET is_capture_point = 0, updated_at = ?1 WHERE id = ?2",
                params![now_iso(), crate::db::ANPR_CONFIG_ID],
            )
            .map_err(|e| format!("update is_capture_point failed: {e}"))?;
        }
    }
    if let Some(m) = is_capture_point {
        // Explicit flag override (from ANPR Engine Configuration page)
        let val = if m { 1 } else { 0 };
        conn.execute(
            "UPDATE anpr_config SET is_capture_point = ?1, updated_at = ?2 WHERE id = ?3",
            params![val, now_iso(), crate::db::ANPR_CONFIG_ID],
        )
        .map_err(|e| format!("update is_capture_point failed: {e}"))?;
    }
    append_audit(&conn, &actor_id, "updated_capture_settings", None, None)?;
    Ok(())
}

#[tauri::command]
pub fn anpr_status(state: State<AppState>) -> Result<AnprStatus, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let source = anpr_source(&conn);
    let last = state.anpr_last.lock().map_err(|e| e.to_string())?;
    let (last_at, last_plate) = last.as_ref().map(|(a, p)| (Some(a.clone()), Some(p.clone()))).unwrap_or((None, None));
    let pending = if source == "simulator" {
        state.simulator.pending()
    } else {
        0
    };
    Ok(AnprStatus {
        enabled: anpr_enabled(&conn),
        source,
        last_read_at: last_at,
        last_plate,
        pending_reads: pending,
    })
}

/// Feed scripted reads into the simulator queue (dev/testing only).
#[tauri::command]
pub fn simulator_push_reads(
    state: State<AppState>,
    reads: Vec<AnprRead>,
) -> Result<usize, String> {
    for r in reads {
        state.simulator.push(r);
    }
    Ok(state.simulator.pending())
}

// ---------------------------------------------------------------------------
// Verification-queue resolution (Phase 3, 04-capture-pipeline.md §6/§9)
// ---------------------------------------------------------------------------

fn ensure_queued(conn: &Connection, trip_id: &str) -> Result<(), String> {
    let status: String = conn
        .query_row("SELECT status FROM trips WHERE id = ?1", params![trip_id], |r| r.get(0))
        .map_err(|_| "Trip not found.".to_string())?;
    if status != "queued" {
        return Err("Trip is not awaiting resolution.".to_string());
    }
    Ok(())
}

/// Attach a (possibly newly registered) vehicle to a queued trip and log it.
/// Preserves the original `time_in` (04 §6); only status, attribution and the
/// vehicle/point-in-time columns change. Returns the resolved TripView.
fn resolve_to_logged(
    conn: &Connection,
    trip_id: &str,
    officer_id: &str,
    vehicle: &VehicleView,
    resolution: &str,
    overrides: (Option<String>, Option<String>, Option<f64>, String, Option<String>),
) -> Result<TripView, String> {
    ensure_queued(conn, trip_id)?;
    let (company_id, driver_id, capacity_at_trip, capacity_unit, receipt_no) = overrides;
    let resolution_row: String = conn
        .query_row(
            "SELECT COALESCE(resolution_notes, '{}') FROM trips WHERE id = ?1",
            params![trip_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());
    let mut map = serde_json::from_str::<Value>(&resolution_row).unwrap_or(Value::Object(serde_json::Map::new()));
    if let Value::Object(m) = &mut map {
        m.insert("resolution".to_string(), Value::String(resolution.to_string()));
        m.insert("resolved_by".to_string(), Value::String(officer_id.to_string()));
        m.insert("resolved_at".to_string(), Value::String(now_iso()));
        m.insert("vehicle_id".to_string(), Value::String(vehicle.id.clone()));
    }
    // Point-in-time fields finalize here: explicit overrides win, otherwise the
    // confirmed vehicle's registered values (04 §6). The unit follows the
    // capacity value the same way the snapshot rule works.
    let eff_company = company_id.or(vehicle.company_id.clone());
    let eff_driver = driver_id.or(vehicle.default_driver_id.clone());
    let eff_capacity = capacity_at_trip.or(vehicle.registered_capacity);
    conn.execute(
        "UPDATE trips SET vehicle_id = ?1, company_id = ?2, driver_id = ?3,
                capacity_at_trip = ?4, capacity_unit = ?5, receipt_no = COALESCE(?6, receipt_no),
                status = 'logged', resolution_notes = ?7, updated_at = ?8
         WHERE id = ?9",
        params![
            vehicle.id,
            eff_company,
            eff_driver,
            eff_capacity,
            capacity_unit,
            receipt_no,
            map.to_string(),
            now_iso(),
            trip_id,
        ],
    )
    .map_err(|e| format!("resolution failed: {e}"))?;
    // A human selected/entered the correct vehicle here — the frames plus the
    // corrected answer are prime retraining data (08 §6.2).
    flag_training_candidates(conn, trip_id, "human_corrected")?;
    append_audit(conn, officer_id, "resolved_queue_confirm", Some(trip_id), None)?;
    trip_by_id(conn, trip_id)
}

/// Confirm an existing vehicle candidate for a queued trip, with optional inline
/// edits before finalizing (05-ui-screens.md §3). Preserves `time_in`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn resolve_queued_existing(
    app: AppHandle,
    state: State<AppState>,
    trip_id: String,
    officer_id: String,
    vehicle_id: String,
    company_id: Option<String>,
    driver_id: Option<String>,
    capacity_at_trip: Option<f64>,
    capacity_unit: String,
    receipt_no: Option<String>,
) -> Result<TripView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let trip = resolve_queued_existing_impl(
        &conn,
        &trip_id,
        &officer_id,
        &vehicle_id,
        company_id,
        driver_id,
        capacity_at_trip,
        capacity_unit,
        receipt_no,
    )?;
    drop(conn);
    emit_capture_update(&app);
    Ok(trip)
}

/// Confirm-existing logic, exposed for tests.
#[allow(clippy::too_many_arguments)]
pub fn resolve_queued_existing_impl(
    conn: &Connection,
    trip_id: &str,
    officer_id: &str,
    vehicle_id: &str,
    company_id: Option<String>,
    driver_id: Option<String>,
    capacity_at_trip: Option<f64>,
    capacity_unit: String,
    receipt_no: Option<String>,
) -> Result<TripView, String> {
    let vehicle = load_active_vehicles(conn)?
        .into_iter()
        .find(|v| v.id == vehicle_id)
        .ok_or_else(|| "Selected vehicle is not active.".to_string())?;
    resolve_to_logged(
        conn,
        trip_id,
        officer_id,
        &vehicle,
        "confirm_existing",
        (company_id, driver_id, capacity_at_trip, capacity_unit, receipt_no),
    )
}

/// Register a brand-new vehicle from the verification screen and log the trip
/// against it. Returns Err with a duplicate-plate warning when the plate already
/// exists and `confirm_duplicate_plate` is false — the UI re-submits with the
/// flag set to reuse the existing vehicle. Preserves `time_in`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn resolve_queued_new(
    app: AppHandle,
    state: State<AppState>,
    trip_id: String,
    officer_id: String,
    plate_number: String,
    company_id: Option<String>,
    registered_capacity: Option<f64>,
    capacity_unit: String,
    default_driver_id: Option<String>,
    confirm_duplicate_plate: bool,
    extra_fields: Option<String>,
) -> Result<TripView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let trip = resolve_queued_new_impl(
        &conn,
        &trip_id,
        &officer_id,
        &plate_number,
        company_id,
        registered_capacity,
        capacity_unit,
        default_driver_id,
        confirm_duplicate_plate,
        extra_fields,
    )?;
    drop(conn);
    emit_capture_update(&app);
    Ok(trip)
}

/// Register-new logic, exposed for tests.
#[allow(clippy::too_many_arguments)]
pub fn resolve_queued_new_impl(
    conn: &Connection,
    trip_id: &str,
    officer_id: &str,
    plate_number: &str,
    company_id: Option<String>,
    registered_capacity: Option<f64>,
    capacity_unit: String,
    default_driver_id: Option<String>,
    confirm_duplicate_plate: bool,
    extra_fields: Option<String>,
) -> Result<TripView, String> {
    let plate = crate::reference::normalize_plate(plate_number);
    if plate.is_empty() {
        return Err("Plate number is required.".to_string());
    }
    let unit = crate::reference::normalize_capacity_unit(&capacity_unit)?;
    let vehicles = load_active_vehicles(conn)?;
    if let Some(existing) = vehicles.iter().find(|v| normalize_plate(&v.plate_number) == plate) {
        if !confirm_duplicate_plate {
            return Err(format!(
                "Plate {plate} is already registered to a vehicle. Re-submit to attach this trip to it."
            ));
        }
        return resolve_to_logged(
            conn,
            trip_id,
            officer_id,
            existing,
            "registered_new",
            (company_id, default_driver_id, registered_capacity, existing.capacity_unit.clone(), None),
        );
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    conn.execute(
        "INSERT INTO vehicles (id, plate_number, company_id, registered_capacity, capacity_unit, default_driver_id, status, extra_fields, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?8)",
        params![id, plate, company_id, registered_capacity, unit, default_driver_id, extra_fields, now],
    )
    .map_err(|e| format!("vehicle creation failed: {e}"))?;
    append_audit(
        conn,
        officer_id,
        "registered_vehicle_at_resolution",
        Some(&id),
        Some(serde_json::json!({ "plate_number": plate, "company_id": company_id, "registered_capacity": registered_capacity, "capacity_unit": unit, "extra_fields": extra_fields })),
    )?;
    let mut vehicle = VehicleView {
        id,
        plate_number: plate,
        company_id,
        company_name: None,
        registered_capacity,
        capacity_unit: unit.clone(),
        default_driver_id,
        default_driver_name: None,
        status: "active".to_string(),
        extra_fields: extra_fields
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
        created_at: now,
    };
    if let Some(cid) = &vehicle.company_id {
        vehicle.company_name = conn
            .query_row("SELECT name FROM companies WHERE id = ?1", params![cid], |r| r.get(0))
            .ok();
    }
    resolve_to_logged(conn, trip_id, officer_id, &vehicle, "registered_new", (None, None, None, unit, None))
}

/// Discard a queued trip. Hard evidence stays on disk and the row is retained
/// (status `discarded`) — nothing is deleted (01-database-schema.md invariant).
/// `time_in` is preserved.
#[tauri::command]
pub fn discard_trip(
    app: AppHandle,
    state: State<AppState>,
    trip_id: String,
    officer_id: String,
) -> Result<TripView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let trip = discard_trip_impl(&conn, &trip_id, &officer_id)?;
    drop(conn);
    emit_capture_update(&app);
    Ok(trip)
}

/// Discard logic, exposed for tests.
pub fn discard_trip_impl(conn: &Connection, trip_id: &str, officer_id: &str) -> Result<TripView, String> {
    ensure_queued(conn, trip_id)?;
    let resolution_row: String = conn
        .query_row(
            "SELECT COALESCE(resolution_notes, '{}') FROM trips WHERE id = ?1",
            params![trip_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());
    let mut map = serde_json::from_str::<Value>(&resolution_row).unwrap_or(Value::Object(serde_json::Map::new()));
    if let Value::Object(m) = &mut map {
        m.insert("resolution".to_string(), Value::String("discarded".to_string()));
        m.insert("resolved_by".to_string(), Value::String(officer_id.to_string()));
        m.insert("resolved_at".to_string(), Value::String(now_iso()));
    }
    conn.execute(
        "UPDATE trips SET status = 'discarded', resolution_notes = ?1, updated_at = ?2 WHERE id = ?3",
        params![map.to_string(), now_iso(), trip_id],
    )
    .map_err(|e| format!("discard failed: {e}"))?;
    append_audit(conn, officer_id, "discarded_trip", Some(trip_id), None)?;
    trip_by_id(conn, trip_id)
}

/// Decline a read during confirm-mode (08-anpr-integration.md §9). The record is
/// saved locally with `status = declined`, excluded from trip counting and the
/// main trip views, and is purgeable by an officer/admin. `time_in` is preserved.
#[tauri::command]
pub fn decline_trip(
    app: AppHandle,
    state: State<AppState>,
    trip_id: String,
    officer_id: String,
) -> Result<TripView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let trip = decline_trip_impl(&conn, &trip_id, &officer_id)?;
    drop(conn);
    emit_capture_update(&app);
    Ok(trip)
}

/// Decline logic, exposed for tests.
pub fn decline_trip_impl(conn: &Connection, trip_id: &str, officer_id: &str) -> Result<TripView, String> {
    ensure_queued(conn, trip_id)?;
    let resolution_row: String = conn
        .query_row(
            "SELECT COALESCE(resolution_notes, '{}') FROM trips WHERE id = ?1",
            params![trip_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());
    let mut map = serde_json::from_str::<Value>(&resolution_row).unwrap_or(Value::Object(serde_json::Map::new()));
    if let Value::Object(m) = &mut map {
        m.insert("resolution".to_string(), Value::String("declined".to_string()));
        m.insert("resolved_by".to_string(), Value::String(officer_id.to_string()));
        m.insert("resolved_at".to_string(), Value::String(now_iso()));
    }
    conn.execute(
        "UPDATE trips SET status = 'declined', resolution_notes = ?1, updated_at = ?2 WHERE id = ?3",
        params![map.to_string(), now_iso(), trip_id],
    )
    .map_err(|e| format!("decline failed: {e}"))?;
    append_audit(conn, officer_id, "declined_trip", Some(trip_id), None)?;
    trip_by_id(conn, trip_id)
}

/// List locally-saved `declined` records (08-anpr-integration.md §9) — the
/// dedicated view for these entries; they never appear in normal trip listings.
#[tauri::command]
pub fn list_declined(state: State<AppState>) -> Result<Vec<TripView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(&format!("{TRIP_SELECT} WHERE t.status = 'declined' ORDER BY t.time_in DESC"))
        .map_err(|e| format!("declined list failed: {e}"))?;
    let rows = stmt
        .query_map([], read_trip)
        .map_err(|e| format!("declined list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("declined read failed: {e}"))
}

/// Permanently remove a `declined` record and its frame evidence. This is the
/// ONLY place `declined` rows are physically deleted; authorized for officers
/// (`resolve_queue`) and admins (`manage_users`). Confirmation is a UI concern.
#[tauri::command]
pub fn purge_declined(
    app: AppHandle,
    state: State<AppState>,
    trip_id: String,
    actor_id: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let frames_dir = state.frames_dir.clone();
    let res = purge_declined_impl(&conn, &frames_dir, &trip_id, &actor_id);
    drop(conn);
    if res.is_ok() {
        emit_capture_update(&app);
    }
    res
}

/// Purge logic, exposed for tests.
pub fn purge_declined_impl(
    conn: &Connection,
    frames_dir: &std::path::Path,
    trip_id: &str,
    actor_id: &str,
) -> Result<(), String> {
    let authorized: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
             WHERE up.user_id = ?1 AND p.key IN ('resolve_queue', 'manage_users')",
            params![actor_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("permission check failed: {e}"))?;
    if authorized == 0 {
        return Err("You do not have permission to purge declined entries.".to_string());
    }
    let status: String = conn
        .query_row("SELECT status FROM trips WHERE id = ?1", params![trip_id], |r| r.get(0))
        .map_err(|_| "Trip not found.".to_string())?;
    if status != "declined" {
        return Err("Only declined entries can be purged.".to_string());
    }
    conn.execute("DELETE FROM trips WHERE id = ?1", params![trip_id])
        .map_err(|e| format!("purge failed: {e}"))?;
    let trip_dir = frames_dir.join(trip_id);
    if trip_dir.is_dir() {
        let _ = std::fs::remove_dir_all(&trip_dir);
    }
    append_audit(conn, actor_id, "purged_declined", Some(trip_id), None)?;
    Ok(())
}

/// Record the officer's discharge Yes/No classification for a logged trip
/// (08-anpr-integration.md §9). `is_discharge_trip` stays null until classified;
/// non-discharge entries are excluded from analytics but retained for records.
#[tauri::command]
pub fn classify_discharge(
    app: AppHandle,
    state: State<AppState>,
    trip_id: String,
    officer_id: String,
    is_discharge: bool,
) -> Result<TripView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let trip = classify_discharge_impl(&conn, &trip_id, &officer_id, is_discharge)?;
    drop(conn);
    emit_capture_update(&app);
    Ok(trip)
}

/// Discharge-classification logic, exposed for tests.
pub fn classify_discharge_impl(
    conn: &Connection,
    trip_id: &str,
    officer_id: &str,
    is_discharge: bool,
) -> Result<TripView, String> {
    let status: String = conn
        .query_row("SELECT status FROM trips WHERE id = ?1", params![trip_id], |r| r.get(0))
        .map_err(|_| "Trip not found.".to_string())?;
    if status != "logged" {
        return Err("Discharge classification applies to logged trips only.".to_string());
    }
    conn.execute(
        "UPDATE trips SET is_discharge_trip = ?1, updated_at = ?2 WHERE id = ?3",
        params![if is_discharge { 1 } else { 0 }, now_iso(), trip_id],
    )
    .map_err(|e| format!("discharge classification failed: {e}"))?;
    append_audit(
        conn,
        officer_id,
        "classified_discharge",
        Some(trip_id),
        Some(serde_json::json!({ "is_discharge": is_discharge })),
    )?;
    trip_by_id(conn, trip_id)
}

/// Frame evidence for a trip (04 §7.4) — base64 payloads for the verification
/// screen. Frames remain available for logged trips too.
#[tauri::command]
pub fn trip_frames(state: State<AppState>, trip_id: String) -> Result<Vec<crate::models::FrameEvidence>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::evidence::trip_evidence(&conn, &state.frames_dir, &trip_id)
}

pub fn emit_capture_update(app: &AppHandle) {
    let _ = app.emit("capture-updated", ());
}

// ---------------------------------------------------------------------------
// ANPR service management — start / stop / write config
// ---------------------------------------------------------------------------

use std::fs;
use std::process::{Command as StdCommand, Stdio};

/// Path to the ANPR service config.json (written by the app, read by the service)
fn anpr_config_path() -> String {
    let base = std::env::current_dir().unwrap_or_default();
    base.join("anpr-service").join("config.json").to_string_lossy().to_string()
}

/// Write the ANPR service config.json so it picks up the active camera source.
#[tauri::command]
pub fn write_anpr_config(
    state: State<AppState>,
    actor_id: String,
    source_url: String,
    source_type: Option<String>,
    mock: Option<bool>,
) -> Result<String, String> {
    let cfg = serde_json::json!({
        "source": source_url,
        "source_type": source_type,
        "mock": mock.unwrap_or(false),
    });
    let path = anpr_config_path();
    fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).map_err(|e| e.to_string())?;
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    append_audit(&conn, &actor_id, "wrote_anpr_config", None, Some(serde_json::json!({"source": source_url})))?;
    Ok(path)
}

/// Start the ANPR service process. Returns the PID.
#[tauri::command]
pub fn start_anpr_service(
    state: State<AppState>,
    actor_id: String,
) -> Result<u32, String> {
    // Kill any existing process first
    let _ = stop_anpr_service_inner(&state);

    let anpr_dir = std::env::current_dir().map_err(|e| e.to_string())?.join("anpr-service");
    if !anpr_dir.exists() {
        return Err(format!("ANPR service directory not found: {}", anpr_dir.display()));
    }

    // Build the command — reads source from config.json
    let mut cmd = StdCommand::new("python");
    cmd.arg("-u").arg("main.py").arg("--port").arg("9800");
    cmd.current_dir(&anpr_dir);

    // Capture stdout/stderr to a log file
    let log_path = anpr_dir.join("anpr.log");
    let log_file = fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let log_file2 = log_file.try_clone().map_err(|e| e.to_string())?;
    cmd.stdout(Stdio::from(log_file));
    cmd.stderr(Stdio::from(log_file2));

    let child = cmd.spawn().map_err(|e| format!("Failed to start ANPR service: {e}"))?;
    let pid = child.id();

    // Store the child handle
    {
        let mut procs = state.anpr_processes.lock().map_err(|e| e.to_string())?;
        procs.push(child);
    }

    let conn = state.db.lock().map_err(|e| e.to_string())?;
    append_audit(&conn, &actor_id, "started_anpr_service", None, Some(serde_json::json!({"pid": pid})))?;
    Ok(pid)
}

/// Stop all ANPR service processes.
#[tauri::command]
pub fn stop_anpr_service(
    state: State<AppState>,
    actor_id: String,
) -> Result<String, String> {
    let count = stop_anpr_service_inner(&state)?;
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    append_audit(&conn, &actor_id, "stopped_anpr_service", None, None)?;
    Ok(format!("Stopped {count} ANPR process(es)."))
}

fn stop_anpr_service_inner(state: &State<AppState>) -> Result<usize, String> {
    let mut procs = state.anpr_processes.lock().map_err(|e| e.to_string())?;
    let mut count = 0;
    for mut child in procs.drain(..) {
        let _ = child.kill();
        let _ = child.wait();
        count += 1;
    }
    Ok(count)
}

/// List recent detection images from the frames directory.
/// Returns a list of {trip_id, kind, file, base64} for browsing.
#[tauri::command]
pub fn list_detection_images(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<DetectionImage>, String> {
    let max = limit.unwrap_or(50);
    let mut images = Vec::new();
    if !state.frames_dir.exists() {
        return Ok(images);
    }
    // Walk the frames directory: frames_dir/<trip_id>/<entry|exit>/<file.jpg>
    let entries = std::fs::read_dir(&state.frames_dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let trip_id = entry.file_name().to_string_lossy().to_string();
        if !entry.path().is_dir() { continue; }
        let Ok(kind_iter) = std::fs::read_dir(entry.path()) else { continue };
        for kind_dir in kind_iter.flatten() {
            let kind = kind_dir.file_name().to_string_lossy().to_string();
            if !kind_dir.path().is_dir() { continue; }
            let Ok(file_iter) = std::fs::read_dir(kind_dir.path()) else { continue };
            for file in file_iter.flatten() {
                let fname = file.file_name().to_string_lossy().to_string();
                let meta = std::fs::metadata(file.path()).ok();
                images.push(DetectionImage {
                    trip_id: trip_id.clone(),
                    kind: kind.clone(),
                    filename: fname,
                    size_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    modified: meta.and_then(|m| m.modified().ok())
                        .map(|t| {
                            let d: chrono::DateTime<chrono::Utc> = t.into();
                            d.to_rfc3339()
                        })
                        .unwrap_or_default(),
                });
            }
        }
    }
    // Sort by modified desc and limit
    images.sort_by(|a, b| b.modified.cmp(&a.modified));
    images.truncate(max);
    Ok(images)
}

#[derive(serde::Serialize)]
pub struct DetectionImage {
    pub trip_id: String,
    pub kind: String,
    pub filename: String,
    pub size_bytes: u64,
    pub modified: String,
}

/// Load a single detection image as base64 for preview.
#[tauri::command]
pub fn load_detection_image(
    state: State<AppState>,
    trip_id: String,
    kind: String,
    filename: String,
) -> Result<String, String> {
    let path = state.frames_dir.join(&trip_id).join(&kind).join(&filename);
    if !path.exists() {
        return Err("Image not found".to_string());
    }
    let data = std::fs::read(&path).map_err(|e| e.to_string())?;
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(&data))
}
