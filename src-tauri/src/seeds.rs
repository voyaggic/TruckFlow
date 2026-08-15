//! Demo seed data for stress testing (dev tooling, not production code).
//!
//! `truckflow seed-demo` populates the local database with a realistic Kenyan
//! reference set (companies, drivers, vehicles) and several weeks of trips, all
//! left `synced = 0` / `pushed_to_sheets = 0` so the sync engine has real work
//! to push to the central Postgres and Google Sheets, and reporting has
//! something to chart. Safe: seeds only once (marker in `app_settings`), never
//! touches existing rows, and always references real users/vehicles/companies.

use std::collections::HashSet;

use rand::RngExt;
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::db::now_iso;

const SEED_MARKER: &str = "demo_seed_run";

const COMPANIES: &[&str] = &[
    "Mombasa Exhaust Centre Ltd",
    "Coastal Truck Services",
    "Kenya Exhaust & Parts",
    "Nairobi Logistics Ltd",
    "Jumbo Exhaust Systems",
    "Highway Hauliers Ltd",
    "Pwani Truck Repairs",
    "Eldoret Freight Services",
    "Kisumu Transport Co Ltd",
    "Bomas Exhaust Works",
    "Athi River Truck Centre",
    "Nakuru Auto Spares",
    "Malindi Road Exhausts",
    "Central Kenya Haulage",
    "Rift Valley Logistics",
    "Diani Trucking Ltd",
];

const DRIVERS: &[&str] = &[
    "James Otieno", "Peter Mwangi", "John Kamau", "David Ochieng", "Samuel Kipchoge",
    "Daniel Wanjiru", "Joseph Njoroge", "Patrick Achieng", "Michael Kariuki", "George Barasa",
    "Charles Odhiambo", "Francis Mutua", "Simon Kiprop", "Kevin Maina", "Brian Omondi",
    "Erick Wafula", "Dennis Kilonzo", "Anthony Karanja", "Robert Chebet", "Lawrence Ngan'ga",
    "Victor Kipkoech", "Edwin Mutiso", "Moses Atieno", "Felix Kimutai",
];

// 3-letter prefix + 3 digits + 1 letter, e.g. KDE465T (matches existing data).
const PLATE_PREFIXES: &[&str] = &[
    "KDA", "KDB", "KDC", "KDD", "KDE", "KDF", "KDG", "KDH", "KDJ", "KDK", "KDL", "KDM",
    "KDN", "KDP", "KDR", "KDS", "KDT", "KDU", "KDV", "KDW", "KDX", "KDY", "KEA", "KEB",
    "KEC", "KED", "KEE", "KEF", "KEG", "KEH", "KEJ", "KEK", "KEL", "KEM", "KEN", "KEP",
];

// No I/O to avoid plate-letter ambiguity.
const PLATE_LETTERS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";

const DAYS_BACK: i64 = 35;

struct SeedVehicle {
    id: String,
    company_id: String,
    registered_capacity: f64,
    default_driver_id: Option<String>,
}

pub fn seed_demo(conn: &Connection) -> Result<String, String> {
    let already: i64 = conn
        .query_row("SELECT COUNT(*) FROM app_settings WHERE key = ?1", params![SEED_MARKER], |r| r.get(0))
        .map_err(|e| format!("seed guard failed: {e}"))?;
    if already > 0 {
        return Err(
            "Demo data was already seeded. To force a fresh seed, delete the app_settings row \
             with key 'demo_seed_run' (and any demo rows you want replaced), then run again."
                .to_string(),
        );
    }

    let officer_id: String = conn
        .query_row(
            "SELECT id FROM users WHERE status = 'active' ORDER BY created_at ASC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "no active user found — create the first admin before seeding".to_string())?;

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("cannot begin seed transaction: {e}"))?;

    let now = now_iso();
    let mut rng = rand::rng();

    // --- Companies ---------------------------------------------------------
    let mut company_ids = Vec::with_capacity(COMPANIES.len());
    for name in COMPANIES {
        let id = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO companies (id, name, status, extra_fields, created_at, updated_at, synced)
             VALUES (?1, ?2, 'active', NULL, ?3, ?3, 0)",
            params![id, name, now],
        )
        .map_err(|e| format!("company insert failed: {e}"))?;
        company_ids.push(id);
    }

    // --- Drivers -----------------------------------------------------------
    let mut driver_ids = Vec::with_capacity(DRIVERS.len());
    for name in DRIVERS {
        let id = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO drivers (id, name, status, extra_fields, created_at, updated_at, synced)
             VALUES (?1, ?2, 'active', NULL, ?3, ?3, 0)",
            params![id, name, now],
        )
        .map_err(|e| format!("driver insert failed: {e}"))?;
        driver_ids.push(id);
    }

    // --- Vehicles ----------------------------------------------------------
    let mut used_plates = HashSet::new();
    let mut vehicles: Vec<SeedVehicle> = Vec::with_capacity(PLATE_PREFIXES.len());
    let mut plate_seq = 0usize;
    for _ in 0..36 {
        // Rotate prefixes and digits so every plate is unique.
        let prefix = PLATE_PREFIXES[plate_seq % PLATE_PREFIXES.len()];
        let digits = 100 + (plate_seq / PLATE_PREFIXES.len()) % 900;
        let letter = PLATE_LETTERS[rng.random_range(0..PLATE_LETTERS.len())] as char;
        let plate = format!("{prefix}{digits:03}{letter}");
        plate_seq += 1;
        if !used_plates.insert(plate.clone()) {
            continue;
        }
        let id = Uuid::new_v4().to_string();
        let company_id = company_ids[rng.random_range(0..company_ids.len())].clone();
        let capacity = (rng.random_range(45..=80) * 1000) as f64;
        let default_driver = driver_ids[rng.random_range(0..driver_ids.len())].clone();
        tx.execute(
            "INSERT INTO vehicles (id, plate_number, company_id, registered_capacity, default_driver_id,
                    status, extra_fields, capacity_unit, created_at, updated_at, synced)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', NULL, 'litres', ?6, ?6, 0)",
            params![id, plate, company_id, capacity, default_driver, now],
        )
        .map_err(|e| format!("vehicle insert failed: {e}"))?;
        vehicles.push(SeedVehicle {
            id,
            company_id,
            registered_capacity: capacity,
            default_driver_id: Some(default_driver),
        });
    }

    // --- Trips: several weeks of gate activity ----------------------------
    let mut trip_count = 0usize;
    let mut declined = 0usize;
    let mut receipt_seq = 1001usize;
    let base = chrono::Utc::now() - chrono::Duration::days(DAYS_BACK);
    for day in 0..DAYS_BACK {
        let per_day = rng.random_range(6..=18);
        for _ in 0..per_day {
            let v = &vehicles[rng.random_range(0..vehicles.len())];
            // 05:00 – 23:00 local activity, as UTC instants.
            let minutes = rng.random_range(300..=1380);
            let when = base + chrono::Duration::days(day) + chrono::Duration::minutes(minutes);
            let when_iso = when.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

            let auto = rng.random_bool(0.85);
            let capture_method = if auto { "auto" } else { "manual_entry" };
            let confidence = if auto {
                Some((rng.random_range(820..=995) as f64) / 1000.0)
            } else {
                None
            };
            let model_version = if auto { Some("yolov8n-v1") } else { None };
            let ocr_engine = if auto {
                if rng.random_bool(0.9) { "paddleocr" } else { "easyocr" }
            } else {
                "manual"
            };
            let is_discharge = if rng.random_bool(0.3) {
                None
            } else {
                Some(rng.random_bool(0.5))
            };
            let declined_row = rng.random_bool(0.02);
            let status = if declined_row { "declined" } else { "logged" };
            let receipt_no = if declined_row || rng.random_bool(0.3) {
                None
            } else {
                receipt_seq += 1;
                Some(format!("R-2026-{receipt_seq:04}"))
            };
            let driver_id = v.default_driver_id.clone().or_else(|| {
                Some(driver_ids[rng.random_range(0..driver_ids.len())].clone())
            });
            let capacity_at_trip = v.registered_capacity * (0.95 + rng.random_range(0..=10) as f64 / 100.0);

            tx.execute(
                "INSERT INTO trips (id, vehicle_id, driver_id, company_id, capacity_at_trip, capacity_unit,
                        time_in, receipt_no, officer_id, capture_method, confidence_score, photo_refs, status,
                        resolution_notes, pushed_to_sheets, is_discharge_trip, model_version, ocr_engine,
                        created_at, updated_at, synced)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'litres', ?6, ?7, ?8, ?9, ?10, NULL, ?11, NULL, 0, ?12, ?13, ?14, ?15, ?15, 0)",
                params![
                    Uuid::new_v4().to_string(),
                    v.id,
                    driver_id,
                    v.company_id,
                    capacity_at_trip,
                    when_iso,
                    receipt_no,
                    officer_id,
                    capture_method,
                    confidence,
                    status,
                    is_discharge,
                    model_version,
                    ocr_engine,
                    when_iso,
                ],
            )
            .map_err(|e| format!("trip insert failed: {e}"))?;
            trip_count += 1;
            if declined_row {
                declined += 1;
            }
        }
    }

    tx.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)",
        params![SEED_MARKER, now_iso()],
    )
    .map_err(|e| format!("seed marker failed: {e}"))?;

    tx.commit().map_err(|e| format!("seed commit failed: {e}"))?;

    Ok(format!(
        "Seeded demo data: {} companies, {} drivers, {} vehicles, {} trips ({} declined). \
         All rows are left unsynced so Postgres/Sheets sync and reporting have real data to work on.",
        company_ids.len(),
        driver_ids.len(),
        vehicles.len(),
        trip_count,
        declined
    ))
}
