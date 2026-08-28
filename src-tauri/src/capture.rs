//! Capture pipeline — ANPR reads, cross-reference, trip creation, manual entry.
//!
//! All matching/business logic lives HERE (in the app), never in the ANPR service
//! (02-architecture.md §4). The ANPR source only reports "what it saw".

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
    pub service_url: String,
    last_timestamp: Mutex<Option<String>>,
}

impl HttpSource {
    pub fn new(service_url: String) -> Self {
        Self { service_url, last_timestamp: Mutex::new(None) }
    }
}

impl AnprSource for HttpSource {
    fn poll(&self) -> Option<AnprRead> {
        let url = format!("{}/latest", self.service_url.trim_end_matches('/'));
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
        // Extract host:port from service_url (e.g. "http://127.0.0.1:9800" -> "127.0.0.1:9800")
        let addr_str = self.service_url
            .strip_prefix("http://").or_else(|| self.service_url.strip_prefix("https://"))
            .unwrap_or(&self.service_url)
            .split('/').next().unwrap_or("127.0.0.1:9800");
        match addr_str.parse() {
            Ok(addr) => TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok(),
            Err(_) => false,
        }
    }
}

/// Minimal dependency-free HTTP GET to 127.0.0.1. Returns None on any error so
/// an unreachable ANPR service degrades gracefully (resilience, 02 §5).
/// Uses a 2-second connect timeout so the ANPR poller thread never blocks
/// for the full OS TCP timeout (21s on Windows) when the service is down.
fn fetch_http(url: &str) -> Option<String> {
    use std::net::TcpStream;
    use std::time::Duration;
    let (host, port, path) = split_url(url)?;
    let addr: std::net::SocketAddr = format!("{host}:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
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
        .prepare(&format!("{TRIP_SELECT} WHERE t.status = 'queued' AND COALESCE(t.archived, 0) = 0 ORDER BY t.time_in ASC"))
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
    let mut should_kill_anpr = false;
    if let Some(e) = anpr_enabled {
        set_setting(&conn, "anpr_enabled", if e { "true" } else { "false" })?;
        if !e {
            should_kill_anpr = true;
        }
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
    drop(conn);
    if should_kill_anpr {
        let _ = stop_anpr_service_inner(&state);
    }
    Ok(())
}

#[tauri::command]
pub fn anpr_status(state: State<AppState>) -> Result<AnprStatus, String> {
    // Phase 1: read db fields (fast), then release db before acquiring anpr_last.
    let (enabled, source, pending) = {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        let source = anpr_source(&conn);
        let enabled = anpr_enabled(&conn);
        let pending = if source == "simulator" {
            state.simulator.pending()
        } else {
            0
        };
        (enabled, source, pending)
    };
    // Phase 2: acquire anpr_last separately — never hold db and anpr_last together.
    let (last_at, last_plate) = match state.anpr_last.try_lock() {
        Ok(last) => last.as_ref().map(|(a, p)| (Some(a.clone()), Some(p.clone()))).unwrap_or((None, None)),
        Err(_) => (None, None),
    };
    Ok(AnprStatus {
        enabled,
        source,
        last_read_at: last_at,
        last_plate,
        pending_reads: pending,
    })
}

/// Query the ANPR service /status endpoint to get detailed diagnostics.
/// Returns the raw JSON from the service so the frontend can display it.
#[tauri::command]
pub fn anpr_service_status(state: State<AppState>) -> Result<serde_json::Value, String> {
    let svc_url = {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        anpr_service_url(&conn)
    };
    let status_url = format!("{}/status", svc_url.trim_end_matches('/'));
    let body = fetch_http(&status_url).ok_or_else(|| format!("ANPR service not reachable at {svc_url}"))?;
    serde_json::from_str(&body).map_err(|e| format!("Invalid status response: {e}"))
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

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// Windows creation flag to prevent a console window from flashing
/// when spawning subprocesses from a GUI application.
#[cfg(target_os = "windows")]
pub const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Same as CREATE_NO_WINDOW but also sets BELOW_NORMAL_PRIORITY_CLASS so the
/// ANPR Python process (which loads heavy ML models) doesn't starve the
/// Tauri WebView of CPU/GPU and cause the app to become unresponsive.
/// 0x08000000 | 0x00004000
#[cfg(target_os = "windows")]
const ANPR_PROCESS_FLAGS: u32 = 0x08004000;

/// Cached Python path — probed once, reused for all subsequent calls.
/// This avoids spawning ~9 subprocesses every time `find_python()` is called.
static CACHED_PYTHON: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Find a working Python executable by trying common names.
/// Returns the first one that resolves to an actual file.
/// Result is cached after the first successful probe.
pub fn find_python() -> String {
    if let Some(cached) = CACHED_PYTHON.get() {
        return cached.clone();
    }

    let found = find_python_inner();
    let _ = CACHED_PYTHON.set(found.clone());
    found
}

fn find_python_inner() -> String {
    // Probe known Python paths directly — no dependency on `where`/`which`
    // or the system PATH, which may differ in the Tauri app context.
    #[cfg(target_os = "windows")]
    {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let probe: Vec<String> = vec![
            // Standard Python.org installer (per-user)
            format!(r"{local}\Programs\Python\Python313\python.exe"),
            format!(r"{local}\Programs\Python\Python312\python.exe"),
            format!(r"{local}\Programs\Python\Python311\python.exe"),
            format!(r"{local}\Programs\Python\Python310\python.exe"),
            // System-wide installs
            r"C:\Python313\python.exe".into(),
            r"C:\Python312\python.exe".into(),
            r"C:\Python311\python.exe".into(),
            r"C:\Python310\python.exe".into(),
            // Python in PATH
            "python.exe".into(),
            "python3.exe".into(),
            "py.exe".into(),
        ];
        for path in &probe {
            let mut cmd = StdCommand::new(path);
            cmd.arg("--version");
            cmd.creation_flags(CREATE_NO_WINDOW);
            if let Ok(out) = cmd.output() {
                if out.status.success() {
                    let ver = String::from_utf8_lossy(&out.stdout);
                    crate::log::log(&format!("[ANPR] Found Python: {path} ({ver})"));
                    return path.clone();
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let probe = ["python3", "python"];
        for name in &probe {
            if let Ok(out) = StdCommand::new(name).arg("--version").output() {
                if out.status.success() {
                    crate::log::log(&format!("[ANPR] Found Python: {name}"));
                    return name.to_string();
                }
            }
        }
    }
    crate::log::log(&format!("[ANPR] WARNING: No Python found!"));
    "python".to_string()
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
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    // Read cloud settings from anpr_config and anpr_credentials
    let active_engine: String = conn
        .query_row(
            "SELECT active_ocr_engine FROM anpr_config WHERE id = ?1",
            params![crate::db::ANPR_CONFIG_ID],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "paddleocr".to_string());
    let prefer_cloud = active_engine == "cloud_provider";

    let cloud_api_url: String = conn
        .query_row(
            "SELECT value FROM key_value_ref WHERE key = 'cloud_anpr_api_url'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "https://api.platerecognizer.com".to_string());

    let cloud_api_key: String = conn
        .query_row(
            "SELECT encrypted_value FROM anpr_credentials WHERE key_name = 'cloud_anpr_api_key' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    drop(conn);

    let cfg = serde_json::json!({
        "source": source_url,
        "source_type": source_type,
        "mock": mock.unwrap_or(false),
        "prefer_cloud": prefer_cloud,
        "cloud_api_url": cloud_api_url,
        "cloud_api_key": cloud_api_key,
    });
    // Use find_anpr_dir() (which walks up ancestor directories) instead of the
    // old anpr_config_path() helper that only checked current_dir — the old
    // helper broke when cargo tauri dev set cwd to src-tauri instead of the
    // project root, producing "os error 3 (path not found)".
    let anpr_dir = crate::find_anpr_dir();
    if !anpr_dir.exists() {
        return Err(format!(
            "ANPR service directory not found: {}. Make sure anpr-service/ exists in the project root.",
            anpr_dir.display()
        ));
    }
    let config_path = anpr_dir.join("config.json");
    let path_str = config_path.to_string_lossy().to_string();
    fs::write(&config_path, serde_json::to_string_pretty(&cfg).unwrap())
        .map_err(|e| format!("Failed to write config.json to {}: {e}", config_path.display()))?;
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    append_audit(&conn, &actor_id, "wrote_anpr_config", None, Some(serde_json::json!({"source": source_url})))?;
    Ok(path_str)
}

/// Emit an `anpr-setup-progress` event to the frontend for real-time UI updates.
fn emit_anpr_progress(handle: Option<&AppHandle>, step: &str, message: &str, extra: Option<serde_json::Value>) {
    if let Some(h) = handle {
        let mut payload = serde_json::json!({
            "step": step,
            "message": message,
        });
        if let Some(e) = extra {
            if let (Some(obj), Some(extra_obj)) = (payload.as_object_mut(), e.as_object()) {
                for (k, v) in extra_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        let _ = h.emit("anpr-setup-progress", payload);
    }
}

/// Auto-setup: ensure Python + pip deps are available for the ANPR service.
///
/// This is the FALLBACK path — it only runs when no compiled PyInstaller exe
/// is found. With a properly built release (build_anpr.py has been run), this
/// code path should never be triggered on end-user machines.
///
/// When it does run, it:
///   - Logs every step to eprintln! (visible in Tauri dev console and system logs).
///   - Emits `anpr-setup-progress` Tauri events so the UI can show real progress.
///   - Enforces a 15-minute total timeout — never hangs silently forever.
pub fn ensure_anpr_deps(anpr_dir: &std::path::Path, handle: Option<&AppHandle>) -> Result<String, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15 * 60);

    let check_timeout = |step: &str| -> Result<(), String> {
        if std::time::Instant::now() > deadline {
            Err(format!("[ANPR] Timeout exceeded during '{step}' — aborting dep setup after 15 minutes"))
        } else {
            Ok(())
        }
    };

    // 1. If compiled exe exists, no Python needed at all.
    if let Some(exe) = crate::find_anpr_exe() {
        crate::log::log(&format!("[ANPR] Compiled exe found at {} — skipping dep setup", exe.display()));
        emit_anpr_progress(handle, "complete", "ANPR engine found — ready to use", None);
        return Ok(exe.to_string_lossy().to_string());
    }

    crate::log::log(&format!("[ANPR] *** FALLBACK PATH: No compiled exe found — setting up Python environment ***"));

    // 2. If Python is already available, just install deps.
    let python = find_python();
    crate::log::log(&format!("[ANPR] [1/4] Checking for Python at: {python}"));
    emit_anpr_progress(handle, "checking", "Checking for Python...", None);
    let python_works = std::process::Command::new(&python)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if python_works {
        crate::log::log(&format!("[ANPR] [1/4] Python found — installing pip dependencies..."));
        emit_anpr_progress(handle, "python_found", &format!("Python found at {python}"), None);
        check_timeout("install pip deps (system Python)")?;
        emit_anpr_progress(handle, "installing_deps", "Installing ANPR packages...", None);
        install_pip_deps(anpr_dir, &python)?;
        crate::log::log(&format!("[ANPR] [4/4] Done — using system Python: {python}"));
        emit_anpr_progress(handle, "complete", "ANPR environment ready!", None);
        return Ok(python);
    }

    // 3. No Python found — download Python embeddable package.
    crate::log::log(&format!("[ANPR] [1/4] Python not found — downloading Python embeddable package..."));
    emit_anpr_progress(handle, "downloading_python", "Python not found — downloading Python 3.12.7...", None);
    let py_ver = "3.12.7";
    let py_dir = anpr_dir.join("python-embed");
    let py_exe = py_dir.join("python.exe");

    if !py_exe.exists() {
        check_timeout("download Python")?;
        let zip_url = format!(
            "https://www.python.org/ftp/python/{}/python-{}-embed-amd64.zip",
            py_ver, py_ver
        );
        let zip_path = anpr_dir.join("python-embed.zip");

        crate::log::log(&format!("[ANPR] [1/4] Downloading Python {py_ver} from python.org..."));
        let dl_msg = format!("Downloading Python {py_ver}...");
        emit_anpr_progress(handle, "downloading_python", &dl_msg, None);
        download_file(&zip_url, &zip_path)?;
        crate::log::log(&format!("[ANPR] [1/4] Python download complete."));
        emit_anpr_progress(handle, "python_downloaded", "Python downloaded — extracting...", None);

        check_timeout("extract Python")?;
        crate::log::log(&format!("[ANPR] [2/4] Extracting Python embeddable package..."));
        emit_anpr_progress(handle, "extracting_python", "Extracting Python...", None);
        extract_zip(&zip_path, &py_dir)?;
        let _ = std::fs::remove_file(&zip_path);

        // Enable pip: append "import site" to the ._pth file so Python can find site-packages.
        let pth_files: Vec<_> = std::fs::read_dir(&py_dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter(|e| {
                e.path().extension().map(|s| s == "pth").unwrap_or(false)
                    && e.path().file_name().unwrap_or_default().to_string_lossy().contains('_')
            })
            .collect();
        for pth in pth_files {
            let mut content = std::fs::read_to_string(pth.path()).unwrap_or_default();
            if !content.contains("import site") {
                content.push_str("\nimport site\n");
                let _ = std::fs::write(pth.path(), content);
            }
        }

        check_timeout("install pip")?;
        crate::log::log(&format!("[ANPR] [2/4] Installing pip into embeddable Python..."));
        emit_anpr_progress(handle, "installing_pip", "Installing pip...", None);
        let get_pip_url = "https://bootstrap.pypa.io/get-pip.py";
        let get_pip_path = py_dir.join("get-pip.py");
        download_file(get_pip_url, &get_pip_path)?;

        let mut pip_install = StdCommand::new(&py_exe);
        pip_install.arg(&get_pip_path);
        pip_install.current_dir(&py_dir);
        pip_install.stdout(Stdio::null());
        pip_install.stderr(Stdio::piped());
        #[cfg(target_os = "windows")]
        pip_install.creation_flags(CREATE_NO_WINDOW);
        let pip_out = pip_install.output().map_err(|e| format!("pip bootstrap failed: {e}"))?;
        if !pip_out.status.success() {
            let err = String::from_utf8_lossy(&pip_out.stderr);
            crate::log::log(&format!("[ANPR] pip bootstrap stderr: {err}"));
        }
        let _ = std::fs::remove_file(&get_pip_path);
        crate::log::log(&format!("[ANPR] [2/4] pip installed."));
        emit_anpr_progress(handle, "pip_installed", "pip installed successfully", None);
    } else {
        crate::log::log(&format!("[ANPR] [1/4] Python embeddable package already exists — skipping download."));
        emit_anpr_progress(handle, "python_found", "Embedded Python already installed", None);
    }

    // 4. Install pip dependencies (numpy, opencv-python, paddleocr, etc.)
    check_timeout("install pip deps")?;
    crate::log::log(&format!("[ANPR] [3/4] Installing pip dependencies (paddleocr + opencv may take 5-15 minutes)..."));
    emit_anpr_progress(handle, "installing_deps", "Installing ANPR packages (numpy, opencv, paddleocr)...", None);
    let python_str = py_exe.to_string_lossy().to_string();
    install_pip_deps(anpr_dir, &python_str)?;

    crate::log::log(&format!("[ANPR] [4/4] ANPR Python environment ready at {}", py_dir.display()));
    emit_anpr_progress(handle, "complete", "ANPR environment ready!", None);

    // Update the cached Python path.
    let _ = CACHED_PYTHON.set(python_str.clone());
    Ok(python_str)
}


fn download_file(url: &str, dest: &std::path::Path) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;
    let resp = client.get(url).send().map_err(|e| format!("Download {url} failed: {e}"))?;
    let bytes = resp.bytes().map_err(|e| format!("Download read failed: {e}"))?;
    std::fs::write(dest, &bytes).map_err(|e| format!("Write {} failed: {e}", dest.display()))?;
    Ok(())
}

fn extract_zip(zip_path: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    // Use PowerShell to extract (no external zip library needed)
    let mut cmd = StdCommand::new("powershell");
    cmd.args([
        "-NoProfile", "-NonInteractive", "-Command",
        &format!("Expand-Archive -Path '{}' -DestinationPath '{}' -Force", zip_path.display(), dest.display()),
    ]);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.output().map_err(|e| format!("Extract failed: {e}"))?;
    Ok(())
}

fn install_pip_deps(anpr_dir: &std::path::Path, python_path: &str) -> Result<(), String> {
    let req_file = anpr_dir.join("requirements.txt");
    if !req_file.exists() {
        return Ok(());
    }
    crate::log::log(&format!("[ANPR] Installing pip dependencies from {}...", req_file.display()));
    let mut cmd = StdCommand::new(python_path);
    cmd.args(["-m", "pip", "install", "-r", &req_file.to_string_lossy(), "--quiet", "--disable-pip-version-check"]);
    cmd.current_dir(anpr_dir);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd.output().map_err(|e| format!("pip install failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pip install failed (exit {}): {}", output.status.code().unwrap_or(-1), stderr));
    }
    crate::log::log(&format!("[ANPR] pip dependencies installed successfully"));
    Ok(())
}

/// Start the ANPR service process. Returns immediately — heavy work runs in background.
#[tauri::command]
pub fn start_anpr_service(
    state: State<AppState>,
    actor_id: String,
    handle: AppHandle,
) -> Result<String, String> {
    crate::log::log(&format!("[ANPR] start_anpr_service called by actor={actor_id}"));

    let anpr_dir = crate::find_anpr_dir();
    crate::log::log(&format!("[ANPR] anpr_dir={:?}, exists={}", anpr_dir, anpr_dir.exists()));
    if !anpr_dir.exists() {
        return Err(format!("ANPR service directory not found: {}", anpr_dir.display()));
    }

    // Check if ANPR is ready (Python + deps installed). If not, tell the
    // frontend to show the setup wizard instead of failing silently.
    let setup_status = check_anpr_ready(State::clone(&state))?;
    if !setup_status.ready {
        return Err("anpr_not_ready".to_string());
    }

    // Read DB settings + write config.json (brief lock hold)
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        let config = crate::anpr::read_anpr_config(&conn)?;
        let cloud_api_url: String = conn
            .query_row("SELECT value FROM key_value_ref WHERE key = 'cloud_anpr_api_url'", [], |r| r.get(0))
            .unwrap_or_default();
        let cloud_api_key: String = conn
            .query_row("SELECT encrypted_value FROM anpr_credentials WHERE key_name = 'cloud_anpr_api_key' LIMIT 1", [], |r| r.get(0))
            .unwrap_or_default();
        // Read ALL active + tracked camera sources for multi-camera support.
        // USB indices are re-resolved against THIS machine's device order —
        // stored "usb:N" indices are machine-specific and may differ here.
        let mut sources: Vec<serde_json::Value> = resolve_camera_sources(&conn);
        // Fallback: if no active sources, use empty defaults
        if sources.is_empty() {
            sources.push(serde_json::json!({
                "source": "",
                "source_type": "",
            }));
        }
        // Re-enable ANPR — start_anpr_service is the user's explicit intent.
        // stop_anpr_service sets this to false to prevent poller restart.
        let _ = set_setting(&conn, "anpr_enabled", "true");
        drop(conn);
        let config_path = anpr_dir.join("config.json");
        let cfg = serde_json::json!({
            // Primary source (first active) for backward compat with single-source mode
            "source": sources[0].get("source"),
            "source_type": sources[0].get("source_type"),
            // All sources for multi-camera mode
            "sources": sources,
            "prefer_cloud": config.prefer_cloud,
            "cloud_api_url": cloud_api_url,
            "cloud_api_key": cloud_api_key,
        });
        if let Err(e) = std::fs::write(&config_path, serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".to_string())) {
            crate::log::log(&format!("[ANPR] Failed to write config.json: {e}"));
        }
        crate::log::log(&format!("[ANPR] Config written: {} camera source(s)", sources.len()));
    }

    // Set the starting guard so the ANPR poller won't auto-restart during
    // this window — prevents the race where the poller kills our fresh process.
    state.anpr_starting.store(true, std::sync::atomic::Ordering::SeqCst);

    // Run stop → start in a SINGLE background thread so the stop always
    // completes before the start.  The old code spawned them as two
    // independent threads which raced on the anpr_processes Mutex —
    // the stop thread could drain and kill the freshly spawned child.
    let db = state.db.clone();
    let procs = state.anpr_processes.clone();
    let anpr_dir_clone = anpr_dir.clone();
    let anpr_starting_flag = state.anpr_starting.clone();
    spawn_anpr_restart_thread(db, procs, anpr_dir_clone, anpr_starting_flag, handle);

    Ok("starting".to_string())
}

/// Spawn the stop → start worker thread (shared by the Start command and the
/// auto-restart-on-config-change path).
fn spawn_anpr_restart_thread(
    db: Arc<Mutex<Connection>>,
    procs: Arc<Mutex<Vec<std::process::Child>>>,
    anpr_dir: std::path::PathBuf,
    anpr_starting_flag: Arc<std::sync::atomic::AtomicBool>,
    handle: AppHandle,
) {
    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // --- Phase 1: Stop old process (synchronous, must finish first) ---
            let _ = stop_anpr_service_inner_parts(&db, &procs);

            // --- Phase 2: Start new process ---
            let main_py = anpr_dir.join("main.py");
            let python = find_python();
            let mut cmd = StdCommand::new(&python);
            cmd.arg("-u").arg(&main_py).arg("--port").arg("9800");
            cmd.current_dir(&anpr_dir);
            // Pipe stdout/stderr to log files
            let log_file = anpr_dir.join("anpr-service.log");
            let err_file = anpr_dir.join("anpr-service.err");
            if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_file) {
                cmd.stdout(Stdio::from(f));
            }
            if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&err_file) {
                cmd.stderr(Stdio::from(f));
            }
            #[cfg(target_os = "windows")]
            cmd.creation_flags(ANPR_PROCESS_FLAGS);

            match cmd.spawn() {
                Ok(child) => {
                    let pid = child.id();
                    crate::log::log(&format!("[ANPR] Spawned PID={pid}"));
                    if let Ok(mut p) = procs.lock() {
                        p.push(child);
                    }
                    let _ = handle.emit("anpr-started", serde_json::json!({"pid": pid}));
                }
                Err(e) => {
                    crate::log::log(&format!("[ANPR] Spawn failed: {e}"));
                    let _ = handle.emit("anpr-start-error", serde_json::json!({"error": e.to_string()}));
                }
            }
        }));
        // Clear the starting guard so the poller can resume monitoring.
        anpr_starting_flag.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// If the ANPR service is currently running, restart it in the background so
/// it picks up the new camera-source configuration. Called after every camera
/// source mutation (add/update/delete/activate/pause/tracked). Without this,
/// a running service keeps its OLD pipeline set — observed: a removed black
/// EOS camera stayed on pipeline 0 while a newly added video file never got a
/// pipeline at all (its tile stayed black).
pub fn restart_anpr_if_running(state: &crate::AppState, handle: &AppHandle) {
    // Probe the service port — a live listener means the service is running.
    // (Cheap TCP connect; never touches the DB lock.)
    let port = {
        let Ok(conn) = state.db.lock() else { return };
        let url = anpr_service_url(&conn);
        url.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(9800)
    };
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let running = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(400)).is_ok();
    if !running {
        return; // service stopped — new config is picked up on next Start
    }
    let anpr_dir = crate::find_anpr_dir();
    if !anpr_dir.exists() {
        return;
    }
    // Rewrite config.json from the CURRENT db state, then stop → start.
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        if let Err(e) = write_anpr_config_file(&conn, &anpr_dir) {
            crate::log::log(&format!("[ANPR] config rewrite for auto-restart failed: {e}"));
        }
    }
    state.anpr_starting.store(true, std::sync::atomic::Ordering::SeqCst);
    spawn_anpr_restart_thread(
        state.db.clone(),
        state.anpr_processes.clone(),
        anpr_dir,
        state.anpr_starting.clone(),
        handle.clone(),
    );
}

/// Read active+tracked camera sources and re-resolve USB indices against the
/// CURRENT machine's DirectShow device order.
///
/// Why: sources are stored as "usb:N" where N is a DirectShow index — which is
/// MACHINE-SPECIFIC and can even change on one machine when virtual camera
/// drivers (vMix/OBS) are installed/removed. On a different PC, usb:0 may be
/// a different device or nothing at all. So for each USB source we match its
/// stored device name (extra_fields.device_name, falling back to the label)
/// against the live device list and use the CURRENT index. Non-USB sources
/// (rtsp/http URLs, file paths) pass through unchanged.
fn resolve_camera_sources(conn: &Connection) -> Vec<serde_json::Value> {
    let mut rows: Vec<(String, String, String, Option<String>)> = Vec::new();
    {
        let mut stmt = match conn.prepare(
            "SELECT connection_string, source_type, COALESCE(label, ''), extra_fields \
             FROM camera_sources WHERE status = 'active' AND COALESCE(tracked, 1) = 1 \
             ORDER BY created_at ASC",
        ) {
            Ok(s) => s,
            Err(e) => {
                crate::log::log(&format!("[ANPR] camera query failed: {e}"));
                return Vec::new();
            }
        };
        let mapped = match stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?))
        }) {
            Ok(m) => m,
            Err(e) => {
                crate::log::log(&format!("[ANPR] camera query failed: {e}"));
                return Vec::new();
            }
        };
        for row in mapped.flatten() {
            rows.push(row);
        }
    }

    // Does any USB source need index resolution?
    let has_usb = rows.iter().any(|(_, t, _, _)| t == "usb");
    let mut name_to_index: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut max_index: i64 = -1;
    if has_usb {
        let python = find_python();
        if let Some(script) = crate::find_anpr_dir().join("_enum_cameras.py").to_str() {
            let mut cmd = std::process::Command::new(&python);
            cmd.arg(script).arg("--fast");
            #[cfg(target_os = "windows")]
            {
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            if let Ok(out) = cmd.output() {
                if out.status.success() {
                    if let Ok(list) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&out.stdout)) {
                        if let Some(arr) = list.as_array() {
                            for cam in arr {
                                let idx = cam.get("index").and_then(|v| v.as_i64()).unwrap_or(-1);
                                if let Some(n) = cam.get("name").and_then(|v| v.as_str()) {
                                    name_to_index.insert(n.to_string(), idx);
                                }
                                max_index = max_index.max(idx);
                            }
                        }
                    }
                }
            }
        }
        if name_to_index.is_empty() {
            crate::log::log("[ANPR] USB source present but device enumeration unavailable — using stored indices as-is");
        }
    }

    let mut sources: Vec<serde_json::Value> = Vec::new();
    for (conn_str, src_type, label, extra) in rows {
        if src_type == "usb" && !name_to_index.is_empty() {
            // Stored device name: extra_fields {"device_name": ...} set at add
            // time; fall back to the label for legacy rows.
            let dev_name = extra
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|v| v.get("device_name").and_then(|d| d.as_str()).map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| label.clone());

            if let Some(&current_idx) = name_to_index.get(dev_name.as_str()) {
                let stored_idx: i64 = conn_str.strip_prefix("usb:").and_then(|s| s.parse().ok()).unwrap_or(-1);
                if current_idx != stored_idx {
                    crate::log::log(&format!(
                        "[ANPR] USB remap: '{}' moved from index {} to {} on this machine",
                        dev_name, stored_idx, current_idx
                    ));
                }
                sources.push(serde_json::json!({ "source": format!("usb:{}", current_idx), "source_type": "usb" }));
                continue;
            }
            // Name not found on this machine: keep the stored index only if it
            // is within range; otherwise skip the source entirely with an
            // honest log instead of silently capturing the WRONG camera.
            let stored_idx: i64 = conn_str.strip_prefix("usb:").and_then(|s| s.parse().ok()).unwrap_or(-1);
            if stored_idx >= 0 && stored_idx <= max_index {
                crate::log::log(&format!(
                    "[ANPR] USB source '{}' (device_name '{}') not matched by name — keeping stored index {}",
                    conn_str, dev_name, stored_idx
                ));
                sources.push(serde_json::json!({ "source": conn_str, "source_type": "usb" }));
            } else {
                crate::log::log(&format!(
                    "[ANPR] Skipping USB source '{}' — device '{}' does not exist on this machine",
                    conn_str, dev_name
                ));
            }
            continue;
        }
        sources.push(serde_json::json!({ "source": conn_str, "source_type": src_type }));
    }
    sources
}

/// Write config.json from the current camera_sources table.
pub(crate) fn write_anpr_config_file(conn: &Connection, anpr_dir: &std::path::Path) -> Result<(), String> {
    let config = crate::anpr::read_anpr_config(conn)?;
    let cloud_api_url: String = conn
        .query_row("SELECT value FROM key_value_ref WHERE key = 'cloud_anpr_api_url'", [], |r| r.get(0))
        .unwrap_or_default();
    let cloud_api_key: String = conn
        .query_row("SELECT encrypted_value FROM anpr_credentials WHERE key_name = 'cloud_anpr_api_key' LIMIT 1", [], |r| r.get(0))
        .unwrap_or_default();
    let mut sources: Vec<serde_json::Value> = resolve_camera_sources(conn);
    if sources.is_empty() {
        sources.push(serde_json::json!({ "source": "", "source_type": "" }));
    }
    let ocr_plate_mode: String = conn
        .query_row("SELECT value FROM app_settings WHERE key = 'ocr_plate_mode'", [], |r| r.get(0))
        .unwrap_or_else(|_| "universal".to_string());
    let cfg = serde_json::json!({
        "source": sources[0].get("source"),
        "source_type": sources[0].get("source_type"),
        "sources": sources,
        "prefer_cloud": config.prefer_cloud,
        "cloud_api_url": cloud_api_url,
        "cloud_api_key": cloud_api_key,
        "ocr_plate_mode": ocr_plate_mode,
    });
    let config_path = anpr_dir.join("config.json");
    std::fs::write(&config_path, serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".to_string()))
        .map_err(|e| format!("write config.json failed: {e}"))
}

/// Stop all ANPR service processes.
#[tauri::command]
pub fn stop_anpr_service(
    state: State<AppState>,
    actor_id: String,
) -> Result<String, String> {
    let count = stop_anpr_service_inner(&state)?;
    // Disable ANPR so the auto-restart poller does NOT bring it back.
    // The user explicitly stopped it — respect that decision.
    // start_anpr_service / auto_start_anpr will re-enable when called.
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        set_setting(&conn, "anpr_enabled", "false")?;
        let _ = append_audit(&conn, &actor_id, "stopped_anpr_service", None, None);
    }
    Ok(format!("Stopped {count} ANPR process(es)."))
}

// ---------------------------------------------------------------------------
// ANPR setup check + on-demand installer
// ---------------------------------------------------------------------------

/// Status of the ANPR environment — returned by `check_anpr_ready`.
#[derive(serde::Serialize)]
pub struct AnprSetupStatus {
    pub ready: bool,
    pub has_python: bool,
    pub has_deps: bool,
    pub has_main_py: bool,
    pub has_exe: bool,
    pub anpr_dir: String,
}

/// Check whether the ANPR service is ready to start.
///
/// Returns a structured status so the frontend can decide whether to show
/// the setup wizard or start the service directly.
#[tauri::command]
pub fn check_anpr_ready(_state: State<AppState>) -> Result<AnprSetupStatus, String> {
    let anpr_dir = crate::find_anpr_dir();
    let has_main_py = anpr_dir.join("main.py").is_file();
    let has_exe = crate::find_anpr_exe().is_some();

    let python = find_python();
    let has_python = !python.is_empty() && std::process::Command::new(&python)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let deps_installed = if has_python {
        check_pip_deps_installed(&python, &anpr_dir)
    } else {
        false
    };

    Ok(AnprSetupStatus {
        ready: has_exe || (has_main_py && has_python && deps_installed),
        has_python,
        has_deps: deps_installed,
        has_main_py,
        has_exe,
        anpr_dir: anpr_dir.to_string_lossy().to_string(),
    })
}

/// Check if pip dependencies from requirements.txt are installed.
fn check_pip_deps_installed(python_path: &str, anpr_dir: &std::path::Path) -> bool {
    let req_file = anpr_dir.join("requirements.txt");
    if !req_file.exists() {
        return false;
    }
    // Quick check: try importing the key packages
    let test_imports = ["numpy", "cv2", "paddleocr"];
    for pkg in &test_imports {
        let out = std::process::Command::new(python_path)
            .args(["-c", &format!("import {pkg}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        if !out.map(|o| o.status.success()).unwrap_or(false) {
            return false;
        }
    }
    true
}

/// Trigger ANPR environment setup (Python + pip deps) with progress events.
///
/// Runs on a background thread. Progress is emitted via `anpr-setup-progress`
/// Tauri events. Completion emits `anpr-setup-done`, failure emits `anpr-setup-error`.
#[tauri::command]
pub fn ensure_anpr_setup(
    _state: State<AppState>,
    handle: AppHandle,
) -> Result<String, String> {
    let anpr_dir = crate::find_anpr_dir();

    std::thread::spawn(move || {
        let result = ensure_anpr_deps(&anpr_dir, Some(&handle));
        match result {
            Ok(python_path) => {
                let _ = handle.emit("anpr-setup-done", serde_json::json!({
                    "python": python_path,
                }));
            }
            Err(e) => {
                let _ = handle.emit("anpr-setup-error", serde_json::json!({
                    "error": e,
                }));
            }
        }
    });

    Ok("setup_started".to_string())
}

/// Variant of stop_anpr_service_inner that takes cloned Arc<Mutex<…>> directly,
/// so it can be called from a background thread without needing a &State borrow.
fn stop_anpr_service_inner_parts(
    db: &Arc<Mutex<Connection>>,
    anpr_processes: &Arc<Mutex<Vec<std::process::Child>>>,
) -> Result<usize, String> {
    // Get the service URL under a brief lock
    let service_url = {
        let Ok(conn) = db.lock() else {
            return Err("Cannot lock DB".to_string());
        };
        anpr_service_url(&conn)
    };
    let host_port = service_url
        .strip_prefix("http://")
        .or_else(|| service_url.strip_prefix("https://"))
        .unwrap_or(&service_url)
        .split('/')
        .next()
        .unwrap_or("127.0.0.1:9800")
        .to_string();
    // Try graceful shutdown via HTTP endpoint (with 1s timeout so we never hang)
    {
        if let Ok(mut stream) = std::net::TcpStream::connect(&host_port) {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
            let host = host_port.split(':').next().unwrap_or("127.0.0.1");
            let req = format!("GET /shutdown HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
            let _ = std::io::Write::write_all(&mut stream, req.as_bytes());
        }
    }
    // Kill any remaining processes — use child.kill() (graceful) then
    // taskkill /F /PID (without /T) to avoid killing the entire process tree,
    // which can destroy the WebView if the PID was reused by a renderer.
    let mut procs = anpr_processes.lock().map_err(|e| e.to_string())?;
    let mut count = 0;
    for mut child in procs.drain(..) {
        let pid = child.id();
        // Try graceful kill first
        let _ = child.kill();
        let _ = child.try_wait();
        // Force-kill if still alive (no /T — don't kill the process tree)
        #[cfg(target_os = "windows")]
        {
            let _ = {
                let mut cmd = std::process::Command::new("taskkill");
                cmd.args(["/F", "/PID", &pid.to_string()]);
                cmd.creation_flags(CREATE_NO_WINDOW);
                cmd.output()
            };
        }
        count += 1;
    }
    drop(procs);
    // Kill ORPHANED listeners: processes still bound to the service port that
    // are NOT tracked children (e.g. left over from a previous app session
    // before a rebuild/restart). Without this, a stale instance keeps serving
    // the OLD camera config — and because Python's ThreadingHTTPServer sets
    // SO_REUSEADDR, a freshly spawned instance can double-bind the same port,
    // after which requests are answered by whichever process won the race
    // (observed: the stale one serving black frames from a removed camera).
    #[cfg(target_os = "windows")]
    {
        let port = host_port.rsplit(':').next().unwrap_or("9800").to_string();
        let port_pat = format!(":{port}");
        if let Ok(out) = {
            let mut cmd = std::process::Command::new("netstat");
            cmd.args(["-ano", "-p", "TCP"]);
            cmd.creation_flags(CREATE_NO_WINDOW);
            cmd.output()
        } {
            let txt = String::from_utf8_lossy(&out.stdout);
            let self_pid = std::process::id().to_string();
            let mut killed: Vec<String> = Vec::new();
            for line in txt.lines() {
                if line.contains(&port_pat) && line.contains("LISTENING") {
                    if let Some(pid_str) = line.split_whitespace().last() {
                        if pid_str != &self_pid
                            && pid_str.parse::<u32>().map(|p| p > 0).unwrap_or(false)
                            && !killed.contains(&pid_str.to_string())
                        {
                            let mut cmd = std::process::Command::new("taskkill");
                            cmd.args(["/F", "/PID", pid_str]);
                            cmd.creation_flags(CREATE_NO_WINDOW);
                            let _ = cmd.output();
                            killed.push(pid_str.to_string());
                        }
                    }
                }
            }
            if !killed.is_empty() {
                crate::log::log(&format!("[ANPR] Killed orphaned port-{port} listener(s): {:?}", killed));
            }
        }
    }
    Ok(count)
}

fn stop_anpr_service_inner(state: &State<AppState>) -> Result<usize, String> {
    stop_anpr_service_inner_parts(&state.db, &state.anpr_processes)
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

/// Delete detection frames. If trip_ids is provided, deletes only those trips'
/// frames. Otherwise deletes ALL captured frames (clear all).
#[tauri::command]
pub fn delete_detection_frames(
    state: State<AppState>,
    actor_id: String,
    trip_ids: Option<Vec<String>>,
) -> Result<usize, String> {
    let frames_dir = &state.frames_dir;
    if !frames_dir.exists() {
        return Ok(0);
    }
    let mut deleted = 0;
    if let Some(ref ids) = trip_ids {
        // Delete specific trip frames
        for tid in ids {
            let trip_dir = frames_dir.join(tid);
            if trip_dir.exists() {
                deleted += count_files(&trip_dir);
                let _ = std::fs::remove_dir_all(&trip_dir);
            }
        }
    } else {
        // Delete ALL captured frames
        for entry in std::fs::read_dir(frames_dir).map_err(|e| e.to_string())?.flatten() {
            if entry.path().is_dir() {
                deleted += count_files(&entry.path());
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    append_audit(&conn, &actor_id, "deleted_detection_frames", None,
        Some(serde_json::json!({"count": deleted, "trip_ids": trip_ids.as_ref()})))?;
    Ok(deleted)
}

fn count_files(dir: &std::path::Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                count += count_files(&entry.path());
            } else {
                count += 1;
            }
        }
    }
    count
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
