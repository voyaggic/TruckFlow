//! Reference database management — companies, vehicles, drivers.
//! Add / edit / deactivate (never hard delete). All commands gated by
//! `manage_reference_database` (05-ui-screens.md §6b).

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use tauri::State;

use calamine::Reader as _; // worksheet_range_at

use crate::commands::{ensure_admin_permission, verify_actor_password};
use crate::db::{append_audit, now_iso, AppState};
use crate::models::{
    ColumnInfo, CombinedImportSummary, CompanyView, DriverView, EntityRecordView, FieldDefinition,
    ReferenceEntity, ReferenceImportPreview, ReferenceImportRequest, ReferenceImportSummary,
    SheetPreview, VehicleView,
};

const REF_PERM: &str = "manage_reference_database";

/// Parse an optional JSON string column into a serde_json::Value.
fn parse_json_opt(row: &rusqlite::Row, idx: usize) -> rusqlite::Result<Option<serde_json::Value>> {
    let raw: Option<String> = row.get(idx)?;
    Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
}

fn read_company(row: &rusqlite::Row) -> rusqlite::Result<CompanyView> {
    Ok(CompanyView {
        id: row.get(0)?,
        name: row.get(1)?,
        status: row.get(2)?,
        extra_fields: parse_json_opt(row, 3)?,
        created_at: row.get(4)?,
    })
}

fn read_driver(row: &rusqlite::Row) -> rusqlite::Result<DriverView> {
    Ok(DriverView {
        id: row.get(0)?,
        name: row.get(1)?,
        status: row.get(2)?,
        extra_fields: parse_json_opt(row, 3)?,
        created_at: row.get(4)?,
    })
}

fn read_vehicle(row: &rusqlite::Row) -> rusqlite::Result<VehicleView> {
    Ok(VehicleView {
        id: row.get(0)?,
        plate_number: row.get(1)?,
        company_id: row.get(2)?,
        company_name: row.get(3)?,
        registered_capacity: row.get(4)?,
        capacity_unit: row.get(5)?,
        default_driver_id: row.get(6)?,
        default_driver_name: row.get(7)?,
        status: row.get(8)?,
        extra_fields: parse_json_opt(row, 9)?,
        created_at: row.get(10)?,
    })
}

/// The units a vehicle's capacity may be recorded in. Default is litres —
/// the paper log column is "Capacity(L)" (00-project-overview.md §3).
pub const CAPACITY_UNITS: &[&str] = &["litres", "cubic_meters", "gallons", "tonnes", "kg"];

pub fn normalize_capacity_unit(unit: &str) -> Result<String, String> {
    let unit = unit.trim().to_lowercase();
    if CAPACITY_UNITS.contains(&unit.as_str()) {
        Ok(unit)
    } else {
        Err(format!(
            "Unsupported capacity unit '{unit}'. Choose one of: {}.",
            CAPACITY_UNITS.join(", ")
        ))
    }
}


fn assert_unique_plate(conn: &Connection, plate: &str, exclude_id: Option<&str>) -> Result<(), String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM vehicles WHERE upper(plate_number) = upper(?1) AND id != COALESCE(?2, '')",
            params![plate, exclude_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("plate lookup failed: {e}"))?;
    if existing.is_some() {
        return Err("A vehicle with this plate number already exists.".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Companies
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_companies(state: State<AppState>, search: Option<String>) -> Result<Vec<CompanyView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let search = search.unwrap_or_default().trim().to_string();
    let mut stmt = conn
        .prepare(
            "SELECT id, name, status, extra_fields, created_at FROM companies
             WHERE (?1 = '' OR lower(name) LIKE '%' || lower(?1) || '%')
             ORDER BY name",
        )
        .map_err(|e| format!("company list failed: {e}"))?;
    let rows = stmt
        .query_map(params![search], read_company)
        .map_err(|e| format!("company list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("company read failed: {e}"))
}

#[tauri::command]
pub fn create_company(
    state: State<AppState>,
    actor_id: String,
    name: String,
    extra_fields: Option<String>,
) -> Result<CompanyView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Company name is required.".to_string());
    }
    let dup: Option<String> = conn
        .query_row("SELECT id FROM companies WHERE upper(name) = upper(?1)", params![name], |r| r.get(0))
        .optional()
        .map_err(|e| format!("company lookup failed: {e}"))?;
    if dup.is_some() {
        return Err("A company with this name already exists.".to_string());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    conn.execute(
        "INSERT INTO companies (id, name, status, extra_fields, created_at, updated_at) VALUES (?1, ?2, 'active', ?3, ?4, ?4)",
        params![id, name, extra_fields, now],
    )
    .map_err(|e| format!("company creation failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "created_company",
        Some(&id),
        Some(serde_json::json!({ "name": name })),
    )?;
    Ok(CompanyView {
        id,
        name,
        status: "active".to_string(),
        extra_fields: extra_fields.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: now,
    })
}

#[tauri::command]
pub fn update_company(
    state: State<AppState>,
    actor_id: String,
    company_id: String,
    name: String,
    extra_fields: Option<String>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Company name is required.".to_string());
    }
    let dup: Option<String> = conn
        .query_row(
            "SELECT id FROM companies WHERE upper(name) = upper(?1) AND id != ?2",
            params![name, company_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("company lookup failed: {e}"))?;
    if dup.is_some() {
        return Err("A company with this name already exists.".to_string());
    }
    let n = conn
        .execute(
            "UPDATE companies SET name = ?1, extra_fields = ?2, updated_at = ?3 WHERE id = ?4",
            params![name, extra_fields, now_iso(), company_id],
        )
        .map_err(|e| format!("company update failed: {e}"))?;
    if n == 0 {
        return Err("Company not found.".to_string());
    }
    append_audit(&conn, &actor_id, "updated_company", Some(&company_id), Some(serde_json::json!({ "name": name })))?;
    Ok(())
}

#[tauri::command]
pub fn set_company_status(state: State<AppState>, actor_id: String, company_id: String, status: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if status != "active" && status != "inactive" {
        return Err("Invalid status.".to_string());
    }
    conn.execute(
        "UPDATE companies SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status, now_iso(), company_id],
    )
    .map_err(|e| format!("company update failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        if status == "inactive" { "deactivated_company" } else { "reactivated_company" },
        Some(&company_id),
        None,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Drivers
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_drivers(state: State<AppState>, search: Option<String>) -> Result<Vec<DriverView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let search = search.unwrap_or_default().trim().to_string();
    let mut stmt = conn
        .prepare(
            "SELECT id, name, status, extra_fields, created_at FROM drivers
             WHERE (?1 = '' OR lower(name) LIKE '%' || lower(?1) || '%')
             ORDER BY name",
        )
        .map_err(|e| format!("driver list failed: {e}"))?;
    let rows = stmt
        .query_map(params![search], read_driver)
        .map_err(|e| format!("driver list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("driver read failed: {e}"))
}

#[tauri::command]
pub fn create_driver(
    state: State<AppState>,
    actor_id: String,
    name: String,
    extra_fields: Option<String>,
) -> Result<DriverView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Driver name is required.".to_string());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    conn.execute(
        "INSERT INTO drivers (id, name, status, extra_fields, created_at, updated_at) VALUES (?1, ?2, 'active', ?3, ?4, ?4)",
        params![id, name, extra_fields, now],
    )
    .map_err(|e| format!("driver creation failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "created_driver",
        Some(&id),
        Some(serde_json::json!({ "name": name })),
    )?;
    Ok(DriverView {
        id,
        name,
        status: "active".to_string(),
        extra_fields: extra_fields.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: now,
    })
}

#[tauri::command]
pub fn update_driver(
    state: State<AppState>,
    actor_id: String,
    driver_id: String,
    name: String,
    extra_fields: Option<String>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Driver name is required.".to_string());
    }
    let n = conn
        .execute(
            "UPDATE drivers SET name = ?1, extra_fields = ?2, updated_at = ?3 WHERE id = ?4",
            params![name, extra_fields, now_iso(), driver_id],
        )
        .map_err(|e| format!("driver update failed: {e}"))?;
    if n == 0 {
        return Err("Driver not found.".to_string());
    }
    append_audit(&conn, &actor_id, "updated_driver", Some(&driver_id), None)?;
    Ok(())
}

#[tauri::command]
pub fn set_driver_status(state: State<AppState>, actor_id: String, driver_id: String, status: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if status != "active" && status != "inactive" {
        return Err("Invalid status.".to_string());
    }
    conn.execute(
        "UPDATE drivers SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status, now_iso(), driver_id],
    )
    .map_err(|e| format!("driver update failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        if status == "inactive" { "deactivated_driver" } else { "reactivated_driver" },
        Some(&driver_id),
        None,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Vehicles
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_vehicles(state: State<AppState>, search: Option<String>) -> Result<Vec<VehicleView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let search = search.unwrap_or_default().trim().to_string();
    let mut stmt = conn
        .prepare(
            "SELECT v.id, v.plate_number, v.company_id, c.name, v.registered_capacity,
                    v.capacity_unit, v.default_driver_id, d.name, v.status, v.extra_fields, v.created_at
             FROM vehicles v
             LEFT JOIN companies c ON c.id = v.company_id
             LEFT JOIN drivers d ON d.id = v.default_driver_id
             WHERE (?1 = ''
                    OR upper(v.plate_number) LIKE '%' || upper(?1) || '%'
                    OR lower(c.name) LIKE '%' || lower(?1) || '%'
                    OR lower(d.name) LIKE '%' || lower(?1) || '%')
             ORDER BY v.plate_number",
        )
        .map_err(|e| format!("vehicle list failed: {e}"))?;
    let rows = stmt
        .query_map(params![search], read_vehicle)
        .map_err(|e| format!("vehicle list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("vehicle read failed: {e}"))
}

#[tauri::command]
pub fn create_vehicle(
    state: State<AppState>,
    actor_id: String,
    plate_number: String,
    company_id: Option<String>,
    registered_capacity: Option<f64>,
    capacity_unit: String,
    default_driver_id: Option<String>,
    extra_fields: Option<String>,
) -> Result<VehicleView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let plate = normalize_plate(&plate_number);
    if plate.is_empty() {
        return Err("Plate number is required.".to_string());
    }
    let unit = normalize_capacity_unit(&capacity_unit)?;
    assert_unique_plate(&conn, &plate, None)?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    conn.execute(
        "INSERT INTO vehicles (id, plate_number, company_id, registered_capacity, capacity_unit, default_driver_id, status, extra_fields, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?8)",
        params![id, plate, company_id, registered_capacity, unit, default_driver_id, extra_fields, now],
    )
    .map_err(|e| format!("vehicle creation failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "created_vehicle",
        Some(&id),
        Some(serde_json::json!({ "plate_number": plate, "company_id": company_id, "registered_capacity": registered_capacity, "capacity_unit": unit })),
    )?;
    Ok(VehicleView {
        id,
        plate_number: plate,
        company_id,
        company_name: None,
        registered_capacity,
        capacity_unit: unit,
        default_driver_id,
        default_driver_name: None,
        status: "active".to_string(),
        extra_fields: extra_fields.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: now,
    })
}

#[tauri::command]
pub fn update_vehicle(
    state: State<AppState>,
    actor_id: String,
    vehicle_id: String,
    plate_number: String,
    company_id: Option<String>,
    registered_capacity: Option<f64>,
    capacity_unit: String,
    default_driver_id: Option<String>,
    extra_fields: Option<String>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let plate = normalize_plate(&plate_number);
    if plate.is_empty() {
        return Err("Plate number is required.".to_string());
    }
    let unit = normalize_capacity_unit(&capacity_unit)?;
    assert_unique_plate(&conn, &plate, Some(&vehicle_id))?;
    let n = conn
        .execute(
            "UPDATE vehicles SET plate_number = ?1, company_id = ?2, registered_capacity = ?3,
                    capacity_unit = ?4, default_driver_id = ?5, extra_fields = ?6, updated_at = ?7
             WHERE id = ?8",
            params![plate, company_id, registered_capacity, unit, default_driver_id, extra_fields, now_iso(), vehicle_id],
        )
        .map_err(|e| format!("vehicle update failed: {e}"))?;
    if n == 0 {
        return Err("Vehicle not found.".to_string());
    }
    append_audit(
        &conn,
        &actor_id,
        "updated_vehicle",
        Some(&vehicle_id),
        Some(serde_json::json!({ "plate_number": plate, "company_id": company_id, "registered_capacity": registered_capacity, "capacity_unit": unit })),
    )?;
    Ok(())
}

#[tauri::command]
pub fn set_vehicle_status(state: State<AppState>, actor_id: String, vehicle_id: String, status: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if status != "active" && status != "inactive" {
        return Err("Invalid status.".to_string());
    }
    conn.execute(
        "UPDATE vehicles SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status, now_iso(), vehicle_id],
    )
    .map_err(|e| format!("vehicle update failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        if status == "inactive" { "deactivated_vehicle" } else { "reactivated_vehicle" },
        Some(&vehicle_id),
        None,
    )?;
    Ok(())
}

/// Normalize a plate for storage and matching: trim, uppercase, drop separators.
pub fn normalize_plate(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

// ---------------------------------------------------------------------------
// Hard delete (whole records) — reference.rs's "never hard delete" applies to
// deactivation, but admins must be able to remove a record as a whole.
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn delete_company(state: State<AppState>, actor_id: String, company_id: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    conn.execute(
        "UPDATE vehicles SET company_id = NULL, updated_at = ?2 WHERE company_id = ?1",
        params![company_id, now_iso()],
    )
    .map_err(|e| format!("company unlink failed: {e}"))?;
    // Trips keep the company's name as a snapshot but must not keep the FK.
    conn.execute(
        "UPDATE trips SET company_id = NULL WHERE company_id = ?1",
        params![company_id],
    )
    .map_err(|e| format!("trip company unlink failed: {e}"))?;
    let n = conn
        .execute("DELETE FROM companies WHERE id = ?1", params![company_id])
        .map_err(|e| format!("company delete failed: {e}"))?;
    if n == 0 {
        return Err("Company not found.".to_string());
    }
    append_audit(&conn, &actor_id, "deleted_company", Some(&company_id), None)?;
    Ok(())
}

#[tauri::command]
pub fn delete_driver(state: State<AppState>, actor_id: String, driver_id: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    conn.execute(
        "UPDATE vehicles SET default_driver_id = NULL, updated_at = ?2 WHERE default_driver_id = ?1",
        params![driver_id, now_iso()],
    )
    .map_err(|e| format!("driver unlink failed: {e}"))?;
    // Trips keep the driver's name as a snapshot but must not keep the FK,
    // otherwise deleting the driver fails the foreign-key constraint.
    conn.execute(
        "UPDATE trips SET driver_id = NULL WHERE driver_id = ?1",
        params![driver_id],
    )
    .map_err(|e| format!("trip driver unlink failed: {e}"))?;
    let n = conn
        .execute("DELETE FROM drivers WHERE id = ?1", params![driver_id])
        .map_err(|e| format!("driver delete failed: {e}"))?;
    if n == 0 {
        return Err("Driver not found.".to_string());
    }
    append_audit(&conn, &actor_id, "deleted_driver", Some(&driver_id), None)?;
    Ok(())
}

#[tauri::command]
pub fn delete_vehicle(state: State<AppState>, actor_id: String, vehicle_id: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    // Keep trip history readable: trips snapshot plate/company/driver, so
    // unlink the vehicle id rather than leaving a dangling reference.
    conn.execute(
        "UPDATE trips SET vehicle_id = NULL WHERE vehicle_id = ?1",
        params![vehicle_id],
    )
    .map_err(|e| format!("vehicle unlink failed: {e}"))?;
    let n = conn
        .execute("DELETE FROM vehicles WHERE id = ?1", params![vehicle_id])
        .map_err(|e| format!("vehicle delete failed: {e}"))?;
    if n == 0 {
        return Err("Vehicle not found.".to_string());
    }
    append_audit(&conn, &actor_id, "deleted_vehicle", Some(&vehicle_id), None)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dynamic field definitions (migration 14)
// ---------------------------------------------------------------------------

const VALID_ENTITY_TYPES: &[&str] = &["company", "vehicle", "driver"];
const VALID_FIELD_TYPES: &[&str] = &["text", "number", "boolean", "mixed", "measurement"];

/// Common measurement units for `measurement` fields (fuel litres, weight kg…).
const VALID_FIELD_UNITS: &[&str] = &[
    "litres", "cubic_meters", "gallons", "tonnes", "kg", "grams", "cm", "m", "km", "hours", "minutes", "seconds",
];

fn read_field_def(row: &rusqlite::Row) -> rusqlite::Result<FieldDefinition> {
    Ok(FieldDefinition {
        id: row.get(0)?,
        entity_type: row.get(1)?,
        field_key: row.get(2)?,
        field_label: row.get(3)?,
        field_type: row.get(4)?,
        is_required: row.get::<_, i32>(5)? != 0,
        field_unit: row.get(6)?,
        is_standard: row.get::<_, i32>(7)? != 0,
        is_hidden: row.get::<_, i32>(8)? != 0,
        binding: row.get(9)?,
        sort_order: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

/// Read field definitions for any registered entity (core or admin-added).
fn list_field_defs_raw(conn: &Connection, entity_type: &str) -> Result<Vec<FieldDefinition>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, entity_type, field_key, field_label, field_type, is_required, field_unit, is_standard, is_hidden, binding, sort_order, created_at, updated_at
             FROM field_definitions WHERE entity_type = ?1
             ORDER BY is_standard DESC, sort_order, field_label",
        )
        .map_err(|e| format!("field_definitions list failed: {e}"))?;
    let rows = stmt
        .query_map(params![entity_type], read_field_def)
        .map_err(|e| format!("field_definitions list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("field_definitions read failed: {e}"))
}

#[tauri::command]
pub fn list_field_definitions(
    state: State<AppState>,
    entity_type: String,
) -> Result<Vec<FieldDefinition>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    if !entity_exists(&conn, &entity_type) {
        return Err(format!("Invalid entity type '{entity_type}'."));
    }
    list_field_defs_raw(&conn, &entity_type)
}

#[tauri::command]
pub fn create_field_definition(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    field_key: String,
    field_label: String,
    field_type: String,
    is_required: bool,
    sort_order: Option<i32>,
    field_unit: Option<String>,
) -> Result<FieldDefinition, String> {
    if !VALID_FIELD_TYPES.contains(&field_type.as_str()) {
        return Err(format!("Invalid field type '{field_type}'. Must be one of: {}.", VALID_FIELD_TYPES.join(", ")));
    }
    let unit = if field_type == "measurement" {
        let u = field_unit.unwrap_or_default().trim().to_lowercase().replace(' ', "_");
        if u.is_empty() {
            return Err("Measurement fields need a unit (e.g. litres, kg, cm).".to_string());
        }
        Some(u)
    } else {
        None
    };
    let key = field_key.trim().to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
    if key.is_empty() {
        return Err("Field key is required.".to_string());
    }
    let label = field_label.trim().to_string();
    if label.is_empty() {
        return Err("Field label is required.".to_string());
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if !entity_exists(&conn, &entity_type) {
        return Err(format!("Invalid entity type '{entity_type}'."));
    }
    // Check unique per entity_type
    let dup: Option<String> = conn
        .query_row(
            "SELECT id FROM field_definitions WHERE entity_type = ?1 AND field_key = ?2",
            params![entity_type, key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("field_definitions lookup failed: {e}"))?;
    if dup.is_some() {
        return Err(format!("A field with key '{key}' already exists for {entity_type}s."));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    let order = sort_order.unwrap_or(0);
    conn.execute(
        "INSERT INTO field_definitions (id, entity_type, field_key, field_label, field_type, is_required, field_unit, sort_order, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        params![id, entity_type, key, label, field_type, is_required as i32, unit, order, now],
    )
    .map_err(|e| format!("field_definitions create failed: {e}"))?;
    append_audit(&conn, &actor_id, "created_field_definition", Some(&id), Some(serde_json::json!({ "entity_type": entity_type, "field_key": key, "field_label": label, "field_unit": unit })))?;
    Ok(FieldDefinition {
        id,
        entity_type,
        field_key: key,
        field_label: label,
        field_type,
        is_required,
        field_unit: unit,
        is_standard: false,
        is_hidden: false,
        binding: None,
        sort_order: order,
        created_at: now.clone(),
        updated_at: now,
    })
}

/// Rename a custom field's key inside every entity row's `extra_fields` JSON
/// (old key → new key) so the data follows the rename.
fn migrate_extra_field_key(
    conn: &Connection,
    entity_type: &str,
    old_key: &str,
    new_key: &str,
) -> Result<(), String> {
    let table = match entity_type {
        "company" => "companies",
        "driver" => "drivers",
        _ => "vehicles",
    };
    let rows: Vec<(String, Option<String>)> = {
        let mut stmt = conn
            .prepare(&format!("SELECT id, extra_fields FROM {table}"))
            .map_err(|e| format!("extra_fields scan failed: {e}"))?;
        let mapped = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
            .map_err(|e| format!("extra_fields scan failed: {e}"))?;
        mapped.collect::<Result<Vec<_>, _>>().map_err(|e| format!("extra_fields read failed: {e}"))?
    };
    for (id, extra) in rows {
        let Some(extra) = extra else { continue };
        let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&extra) else {
            continue;
        };
        let Some(obj) = val.as_object_mut() else {
            continue;
        };
        if let Some(old_val) = obj.remove(old_key) {
            obj.insert(new_key.to_string(), old_val);
            let updated = serde_json::Value::Object(obj.clone()).to_string();
            conn.execute(
                &format!("UPDATE {table} SET extra_fields = ?1, updated_at = ?2 WHERE id = ?3"),
                params![updated, now_iso(), id],
            )
            .map_err(|e| format!("extra_fields update failed: {e}"))?;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn update_field_definition(
    state: State<AppState>,
    actor_id: String,
    field_id: String,
    field_key: Option<String>,
    field_label: Option<String>,
    field_type: Option<String>,
    is_required: Option<bool>,
    sort_order: Option<i32>,
    is_hidden: Option<bool>,
    field_unit: Option<String>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if let Some(ref ft) = field_type {
        if !VALID_FIELD_TYPES.contains(&ft.as_str()) {
            return Err(format!("Invalid field type '{ft}'. Must be one of: {}.", VALID_FIELD_TYPES.join(", ")));
        }
        if ft == "measurement" {
            let u = field_unit.as_deref().unwrap_or("").trim();
            if u.is_empty() {
                return Err("Measurement fields need a unit (e.g. litres, kg, cm).".to_string());
            }
        }
    }
    // Load the current row so a key rename can migrate its data.
    let cur: Option<(String, String, String, bool)> = conn
        .query_row(
            "SELECT entity_type, field_key, field_label, is_standard FROM field_definitions WHERE id = ?1",
            params![field_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i32>(3)? != 0)),
        )
        .optional()
        .map_err(|e| format!("field_definitions lookup failed: {e}"))?;
    let (entity_type, old_key, _old_label, is_standard) =
        cur.ok_or_else(|| "Field definition not found.".to_string())?;

    let mut sets = vec!["updated_at = ?1".to_string()];
    let mut idx = 2;
    let mut key_used = old_key.clone();
    if let Some(ref new_key) = field_key {
        let key = new_key.trim().to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
        if key.is_empty() {
            return Err("Field key is required.".to_string());
        }
        if key != old_key {
            // Unique per entity type.
            let dup: Option<String> = conn
                .query_row(
                    "SELECT id FROM field_definitions WHERE entity_type = ?1 AND field_key = ?2 AND id != ?3",
                    params![entity_type, key, field_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| format!("field_definitions lookup failed: {e}"))?;
            if dup.is_some() {
                return Err(format!("A field with key '{key}' already exists for {entity_type}s."));
            }
            // Standard fields keep their fixed binding; only the identifier
            // changes. Custom fields store data in extra_fields by key, so the
            // stored values must follow the rename.
            if !is_standard {
                migrate_extra_field_key(&conn, &entity_type, &old_key, &key)?;
            }
            sets.push(format!("field_key = ?{idx}"));
            idx += 1;
            key_used = key;
        }
    }
    if field_label.is_some() {
        sets.push(format!("field_label = ?{idx}"));
        idx += 1;
    }
    if field_type.is_some() {
        sets.push(format!("field_type = ?{idx}"));
        idx += 1;
    }
    if is_required.is_some() {
        sets.push(format!("is_required = ?{idx}"));
        idx += 1;
    }
    if sort_order.is_some() {
        sets.push(format!("sort_order = ?{idx}"));
        idx += 1;
    }
    if is_hidden.is_some() {
        sets.push(format!("is_hidden = ?{idx}"));
        idx += 1;
    }
    if field_type.is_some() || field_unit.is_some() {
        sets.push(format!("field_unit = ?{idx}"));
        idx += 1;
    }
    let sql = format!("UPDATE field_definitions SET {} WHERE id = ?{idx}", sets.join(", "));
    let mut bound: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now_iso())];
    if field_key.is_some() && key_used != old_key {
        bound.push(Box::new(key_used.clone()));
    }
    if let Some(ref label) = field_label {
        bound.push(Box::new(label.clone()));
    }
    if let Some(ref ft) = field_type {
        bound.push(Box::new(ft.clone()));
    }
    if let Some(req) = is_required {
        bound.push(Box::new(req as i32));
    }
    if let Some(ord) = sort_order {
        bound.push(Box::new(ord));
    }
    if let Some(hidden) = is_hidden {
        bound.push(Box::new(hidden as i32));
    }
    if field_type.is_some() || field_unit.is_some() {
        let unit = if field_type.as_deref() == Some("measurement") {
            Some(field_unit.as_deref().unwrap_or("").trim().to_lowercase().replace(' ', "_"))
        } else {
            None
        };
        bound.push(Box::new(unit));
    }
    bound.push(Box::new(field_id.clone()));
    let params_ref: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let n = conn.execute(&sql, params_ref.as_slice()).map_err(|e| format!("field_definitions update failed: {e}"))?;
    if n == 0 {
        return Err("Field definition not found.".to_string());
    }
    append_audit(
        &conn,
        &actor_id,
        "updated_field_definition",
        Some(&field_id),
        Some(serde_json::json!({ "field_key": key_used })),
    )?;
    Ok(())
}

#[tauri::command]
pub fn delete_field_definition(
    state: State<AppState>,
    actor_id: String,
    field_id: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    // Fields are deleted as a whole — standard and custom alike. The real
    // database column behind a standard field is kept (history stays intact),
    // but the field disappears from forms, import, and export everywhere.
    let n = conn
        .execute("DELETE FROM field_definitions WHERE id = ?1", params![field_id])
        .map_err(|e| format!("field_definitions delete failed: {e}"))?;
    if n == 0 {
        return Err("Field definition not found.".to_string());
    }
    append_audit(&conn, &actor_id, "deleted_field_definition", Some(&field_id), None)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reference import / export (CSV & XLSX)
// ---------------------------------------------------------------------------

fn entity_headers(entity_type: &str) -> Vec<&'static str> {
    match entity_type {
        "company" | "driver" => vec!["name", "status", "extra_fields"],
        _ => vec![
            "plate_number",
            "company",
            "driver",
            "registered_capacity",
            "capacity_unit",
            "status",
            "extra_fields",
        ],
    }
}

fn export_entity_rows(conn: &Connection, entity_type: &str) -> Result<Vec<Vec<String>>, String> {
    match entity_type {
        "company" => {
            let mut stmt = conn
                .prepare("SELECT name, status, COALESCE(extra_fields, '') FROM companies ORDER BY name")
                .map_err(|e| format!("company export failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(vec![
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ])
                })
                .map_err(|e| format!("company export failed: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("company export read failed: {e}"))
        }
        "driver" => {
            let mut stmt = conn
                .prepare("SELECT name, status, COALESCE(extra_fields, '') FROM drivers ORDER BY name")
                .map_err(|e| format!("driver export failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(vec![
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ])
                })
                .map_err(|e| format!("driver export failed: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("driver export read failed: {e}"))
        }
        _ => {
            let mut stmt = conn
                .prepare(
                    "SELECT v.plate_number, COALESCE(c.name, ''), COALESCE(d.name, ''),
                            v.registered_capacity, COALESCE(v.capacity_unit, ''),
                            v.status, COALESCE(v.extra_fields, '')
                     FROM vehicles v
                     LEFT JOIN companies c ON c.id = v.company_id
                     LEFT JOIN drivers d ON d.id = v.default_driver_id
                     ORDER BY v.plate_number",
                )
                .map_err(|e| format!("vehicle export failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    let cap: Option<f64> = r.get(3)?;
                    Ok(vec![
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        cap.map(|v| v.to_string()).unwrap_or_default(),
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ])
                })
                .map_err(|e| format!("vehicle export failed: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("vehicle export read failed: {e}"))
        }
    }
}

/// Export one entity type (company/driver/vehicle) to a CSV or XLSX file and
/// return the absolute path written. If `target_path` is provided the file is
/// written there (from the frontend's native Save-As dialog).
#[tauri::command]
pub fn reference_export(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    format: String,
    target_path: Option<String>,
) -> Result<String, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if !VALID_ENTITY_TYPES.contains(&entity_type.as_str()) {
        return Err(format!(
            "Invalid entity type '{entity_type}'. Must be one of: {}.",
            VALID_ENTITY_TYPES.join(", ")
        ));
    }
    if format != "csv" && format != "xlsx" {
        return Err(format!("Invalid export format '{format}'. Must be 'csv' or 'xlsx'."));
    }
    let path = if let Some(tp) = target_path.as_deref() {
        std::path::PathBuf::from(tp)
    } else {
        let dir = state
            .frames_dir
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("exports");
        std::fs::create_dir_all(&dir).map_err(|e| format!("export dir create failed: {e}"))?;
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        dir.join(format!("truckflow-{entity_type}-{ts}.{format}"))
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("export dir create failed: {e}"))?;
    }
    let rows = export_entity_rows(&conn, &entity_type)?;
    let headers = entity_headers(&entity_type);
    match format.as_str() {
        "csv" => {
            let mut wtr = csv::Writer::from_path(&path).map_err(|e| format!("csv create failed: {e}"))?;
            wtr.write_record(&headers).map_err(|e| format!("csv header failed: {e}"))?;
            for row in &rows {
                wtr.write_record(row).map_err(|e| format!("csv row failed: {e}"))?;
            }
            wtr.flush().map_err(|e| format!("csv flush failed: {e}"))?;
        }
        _ => {
            let mut workbook = rust_xlsxwriter::Workbook::new();
            let worksheet = workbook.add_worksheet();
            for (i, h) in headers.iter().enumerate() {
                let _ = worksheet.write_string(0, i as u16, *h);
            }
            for (ri, row) in rows.iter().enumerate() {
                let row_idx = ri as u32 + 1;
                for (ci, cell) in row.iter().enumerate() {
                    let _ = worksheet.write_string(row_idx, ci as u16, cell);
                }
            }
            workbook
                .save(&path)
                .map_err(|e| format!("xlsx save failed: {e}"))?;
        }
    }
    append_audit(
        &conn,
        &actor_id,
        "reference_exported",
        None,
        Some(serde_json::json!({
            "entity_type": entity_type,
            "format": format,
            "path": path.to_string_lossy(),
        })),
    )?;
    Ok(path.to_string_lossy().into_owned())
}

fn read_csv_rows(path: &std::path::Path) -> Result<Vec<Vec<String>>, String> {
    // has_headers(false): the header row is read as data so the caller can
    // inspect it (same shape as the XLSX reader, which returns all rows).
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_path(path)
        .map_err(|e| format!("csv open failed: {e}"))?;
    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| format!("csv record failed: {e}"))?;
        out.push(rec.iter().map(|s| s.to_string()).collect());
    }
    Ok(out)
}

fn cell_to_string(d: &calamine::Data) -> String {
    match d {
        calamine::Data::String(s) => s.clone(),
        calamine::Data::Float(f) => f.to_string(),
        calamine::Data::Int(i) => i.to_string(),
        calamine::Data::Bool(b) => b.to_string(),
        calamine::Data::DateTime(dt) => dt.to_string(),
        calamine::Data::DateTimeIso(s) => s.clone(),
        calamine::Data::DurationIso(s) => s.clone(),
        calamine::Data::Error(e) => format!("{e:?}"),
        calamine::Data::Empty => String::new(),
    }
}

fn read_xlsx_rows(path: &std::path::Path) -> Result<Vec<Vec<String>>, String> {
    let mut workbook = calamine::open_workbook_auto(path).map_err(|e| format!("xlsx open failed: {e}"))?;
    let sheet = workbook
        .worksheet_range_at(0)
        .ok_or_else(|| "xlsx has no sheets".to_string())?
        .map_err(|e| format!("xlsx sheet read failed: {e}"))?;
    Ok(sheet
        .rows()
        .map(|row| row.iter().map(cell_to_string).collect())
        .collect())
}

/// Normalise a spreadsheet header: lowercase, trim, spaces/dashes → underscore.
fn norm_header(h: &str) -> String {
    h.trim()
        .to_lowercase()
        .replace([' ', '-', '.'], "_")
}

fn col_idx(cols: &HashMap<String, usize>, aliases: &[&str]) -> Option<usize> {
    for a in aliases {
        if let Some(&i) = cols.get(*a) {
            return Some(i);
        }
    }
    None
}

/// Apply one spreadsheet row: upsert by plate (vehicles) or name
/// (companies/drivers). Returns Ok(true) when a row was created,
/// Ok(false) when updated, Err for a per-row problem.
fn import_row(
    conn: &Connection,
    actor_id: &str,
    entity_type: &str,
    cols: &HashMap<String, usize>,
    row: &[String],
) -> Result<bool, String> {
    let get = |aliases: &[&str]| -> Option<String> {
        col_idx(cols, aliases).and_then(|i| row.get(i).map(|s| s.trim().to_string())).filter(|s| !s.is_empty())
    };
    let status = get(&["status"]).unwrap_or_else(|| "active".to_string());
    if status != "active" && status != "inactive" {
        return Err(format!("Invalid status '{status}'"));
    }
    let extra = get(&["extra_fields", "extra"]);
    match entity_type {
        "company" => {
            let name = get(&["name"]).ok_or_else(|| "Company name is required.".to_string())?;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM companies WHERE upper(name) = upper(?1)",
                    params![name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            match existing {
                Some(id) => {
                    conn.execute(
                        "UPDATE companies SET status = ?1, extra_fields = ?2, updated_at = ?3 WHERE id = ?4",
                        params![status, extra, now_iso(), id],
                    )
                    .map_err(|e| format!("company update failed: {e}"))?;
                    append_audit(conn, actor_id, "updated_company", Some(&id), Some(serde_json::json!({ "name": name })))?;
                    Ok(false)
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO companies (id, name, status, extra_fields, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                        params![id, name, status, extra, now_iso()],
                    )
                    .map_err(|e| format!("company create failed: {e}"))?;
                    append_audit(conn, actor_id, "created_company", Some(&id), Some(serde_json::json!({ "name": name })))?;
                    Ok(true)
                }
            }
        }
        "driver" => {
            let name = get(&["name"]).ok_or_else(|| "Driver name is required.".to_string())?;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM drivers WHERE upper(name) = upper(?1)",
                    params![name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            match existing {
                Some(id) => {
                    conn.execute(
                        "UPDATE drivers SET status = ?1, extra_fields = ?2, updated_at = ?3 WHERE id = ?4",
                        params![status, extra, now_iso(), id],
                    )
                    .map_err(|e| format!("driver update failed: {e}"))?;
                    append_audit(conn, actor_id, "updated_driver", Some(&id), Some(serde_json::json!({ "name": name })))?;
                    Ok(false)
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO drivers (id, name, status, extra_fields, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                        params![id, name, status, extra, now_iso()],
                    )
                    .map_err(|e| format!("driver create failed: {e}"))?;
                    append_audit(conn, actor_id, "created_driver", Some(&id), Some(serde_json::json!({ "name": name })))?;
                    Ok(true)
                }
            }
        }
        _ => {
            let plate = normalize_plate(&get(&["plate_number", "plate"]).ok_or_else(|| "Plate number is required.".to_string())?);
            if plate.is_empty() {
                return Err("Plate number is required.".to_string());
            }
            let company_id = match get(&["company", "company_name"]) {
                Some(cname) => {
                    let cid: Option<String> = conn
                        .query_row(
                            "SELECT id FROM companies WHERE upper(name) = upper(?1)",
                            params![cname],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?;
                    match cid {
                        Some(cid) => Some(cid),
                        None => return Err(format!("Unrecognised company '{cname}'")),
                    }
                }
                None => None,
            };
            let driver_id = match get(&["driver", "driver_name"]) {
                Some(dname) => {
                    let did: Option<String> = conn
                        .query_row(
                            "SELECT id FROM drivers WHERE upper(name) = upper(?1)",
                            params![dname],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?;
                    match did {
                        Some(did) => Some(did),
                        None => return Err(format!("Unrecognised driver '{dname}'")),
                    }
                }
                None => None,
            };
            let capacity = get(&["registered_capacity", "capacity"]).and_then(|v| v.parse::<f64>().ok());
            let unit = get(&["capacity_unit", "unit"]).unwrap_or_else(|| "litres".to_string());
            let unit = normalize_capacity_unit(&unit)?;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM vehicles WHERE upper(plate_number) = upper(?1)",
                    params![plate],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            match existing {
                Some(id) => {
                    conn.execute(
                        "UPDATE vehicles SET company_id = ?1, registered_capacity = ?2, capacity_unit = ?3,
                                default_driver_id = ?4, status = ?5, extra_fields = ?6, updated_at = ?7
                         WHERE id = ?8",
                        params![company_id, capacity, unit, driver_id, status, extra, now_iso(), id],
                    )
                    .map_err(|e| format!("vehicle update failed: {e}"))?;
                    append_audit(conn, actor_id, "updated_vehicle", Some(&id), Some(serde_json::json!({ "plate_number": plate })))?;
                    Ok(false)
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO vehicles (id, plate_number, company_id, registered_capacity, capacity_unit,
                                default_driver_id, status, extra_fields, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                        params![id, plate, company_id, capacity, unit, driver_id, status, extra, now_iso()],
                    )
                    .map_err(|e| format!("vehicle create failed: {e}"))?;
                    append_audit(conn, actor_id, "created_vehicle", Some(&id), Some(serde_json::json!({ "plate_number": plate })))?;
                    Ok(true)
                }
            }
        }
    }
}

/// Import one entity type from a CSV or XLSX file. Rows are upserted by plate
/// (vehicles) or name (companies/drivers); per-row failures are collected in
/// `summary.errors` instead of aborting the whole file.
#[tauri::command]
pub fn reference_import(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    file_path: String,
) -> Result<ReferenceImportSummary, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if !VALID_ENTITY_TYPES.contains(&entity_type.as_str()) {
        return Err(format!(
            "Invalid entity type '{entity_type}'. Must be one of: {}.",
            VALID_ENTITY_TYPES.join(", ")
        ));
    }
    let path = std::path::Path::new(&file_path);
    if !path.exists() {
        return Err(format!("Import file not found: {file_path}"));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mut rows: Vec<Vec<String>> = match ext.as_str() {
        "csv" => read_csv_rows(path)?,
        "xlsx" => read_xlsx_rows(path)?,
        _ => return Err(format!("Unsupported import format '.{ext}'. Use .csv or .xlsx.")),
    };
    if rows.is_empty() {
        return Err("Import file is empty.".to_string());
    }
    let header = rows.remove(0);
    let mut cols: HashMap<String, usize> = HashMap::new();
    for (i, h) in header.iter().enumerate() {
        cols.entry(norm_header(h)).or_insert(i);
    }
    let mut summary = ReferenceImportSummary {
        entity_type: entity_type.clone(),
        created: 0,
        updated: 0,
        skipped: 0,
        errors: Vec::new(),
    };
    for (idx, row) in rows.iter().enumerate() {
        let row_no = idx + 2; // header is row 1
        match import_row(&conn, &actor_id, &entity_type, &cols, row) {
            Ok(true) => summary.created += 1,
            Ok(false) => summary.updated += 1,
            Err(e) => {
                summary.errors.push(format!("Row {row_no}: {e}"));
                summary.skipped += 1;
            }
        }
    }
    append_audit(
        &conn,
        &actor_id,
        "reference_imported",
        None,
        Some(serde_json::json!({
            "entity_type": entity_type,
            "file": file_path,
            "created": summary.created,
            "updated": summary.updated,
            "skipped": summary.skipped,
            "errors": summary.errors.len(),
        })),
    )?;
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Parent entities — the reference database's parents (Vehicles / Companies /
// Drivers plus any the admin adds, e.g. "Trailers"). Labels, add/delete/rename
// all live here and drive every screen through the shared context.
// ---------------------------------------------------------------------------

fn default_entity_label(entity_type: &str) -> &'static str {
    match entity_type {
        "company" => "Companies",
        "driver" => "Drivers",
        _ => "Vehicles",
    }
}

fn entity_label(conn: &Connection, entity_type: &str) -> String {
    conn.query_row(
        "SELECT label FROM reference_entities WHERE entity_type = ?1",
        params![entity_type],
        |r| r.get(0),
    )
    .unwrap_or_else(|_| default_entity_label(entity_type).to_string())
}

fn entity_exists(conn: &Connection, entity_type: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM reference_entities WHERE entity_type = ?1",
        params![entity_type],
        |_| Ok(()),
    )
    .is_ok()
}

/// Every registered parent, core first then creation order.
fn all_entities(conn: &Connection) -> Result<Vec<ReferenceEntity>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT entity_type, label, is_core, sort_order, created_at, updated_at
             FROM reference_entities ORDER BY is_core DESC, sort_order, created_at",
        )
        .map_err(|e| format!("reference_entities list failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ReferenceEntity {
                entity_type: r.get(0)?,
                label: r.get(1)?,
                is_core: r.get::<_, i32>(2)? != 0,
                sort_order: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })
        .map_err(|e| format!("reference_entities list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("reference_entities read failed: {e}"))
}

#[tauri::command]
pub fn list_reference_entities(state: State<AppState>) -> Result<Vec<ReferenceEntity>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    all_entities(&conn)
}

#[tauri::command]
pub fn create_reference_entity(
    state: State<AppState>,
    actor_id: String,
    label: String,
) -> Result<ReferenceEntity, String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("The parent's display name is required.".to_string());
    }
    if label.len() > 40 {
        return Err("Keep the display name under 40 characters.".to_string());
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let entity_type = format!("custom_{}", uuid::Uuid::new_v4().simple());
    let max_order: i32 = conn
        .query_row(
            "SELECT COALESCE(MAX(sort_order), 0) FROM reference_entities",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("reference_entities order failed: {e}"))?;
    let now = now_iso();
    conn.execute(
        "INSERT INTO reference_entities (entity_type, label, is_core, sort_order, created_at, updated_at)
         VALUES (?1, ?2, 0, ?3, ?4, ?4)",
        params![entity_type, label, max_order + 1, now],
    )
    .map_err(|e| format!("reference_entities create failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "created_reference_entity",
        Some(&entity_type),
        Some(serde_json::json!({ "entity_type": entity_type, "label": label })),
    )?;
    Ok(ReferenceEntity {
        entity_type,
        label,
        is_core: false,
        sort_order: max_order + 1,
        created_at: now.clone(),
        updated_at: now,
    })
}

#[tauri::command]
pub fn rename_reference_entity(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    label: String,
) -> Result<String, String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("The display name can't be empty.".to_string());
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if !entity_exists(&conn, &entity_type) {
        return Err(format!("Unknown entity '{entity_type}'."));
    }
    conn.execute(
        "UPDATE reference_entities SET label = ?1, updated_at = ?2 WHERE entity_type = ?3",
        params![label, now_iso(), entity_type],
    )
    .map_err(|e| format!("reference_entities rename failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "renamed_reference_entity",
        Some(&entity_type),
        Some(serde_json::json!({ "entity_type": entity_type, "label": label })),
    )?;
    Ok(label)
}

/// Delete a parent and everything under it (its child fields and, for non-core
/// parents, its records). Core pipeline entities (vehicle/company/driver) are
/// protected — the gate, trips, and reports depend on them.
#[tauri::command]
pub fn delete_reference_entity(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    actor_credential: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let is_core: Option<i32> = conn
        .query_row(
            "SELECT is_core FROM reference_entities WHERE entity_type = ?1",
            params![entity_type],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("reference_entities lookup failed: {e}"))?;
    match is_core {
        None => return Err(format!("Unknown entity '{entity_type}'.")),
        Some(1) => {
            return Err(format!(
                "'{}' is a core entity the gate and trip pipeline depend on, so it can't be deleted. You can rename it, or delete any parent you added instead.",
                entity_label(&conn, &entity_type)
            ));
        }
        _ => {}
    }
    verify_actor_password(&conn, &actor_id, &actor_credential)
        .map_err(|e| format!("Password check failed: {e}"))?;
    conn.execute(
        "DELETE FROM entity_records WHERE entity_type = ?1",
        params![entity_type],
    )
    .map_err(|e| format!("entity_records delete failed: {e}"))?;
    conn.execute(
        "DELETE FROM field_definitions WHERE entity_type = ?1",
        params![entity_type],
    )
    .map_err(|e| format!("field_definitions delete failed: {e}"))?;
    conn.execute(
        "DELETE FROM reference_entities WHERE entity_type = ?1",
        params![entity_type],
    )
    .map_err(|e| format!("reference_entities delete failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "deleted_reference_entity",
        Some(&entity_type),
        Some(serde_json::json!({ "entity_type": entity_type })),
    )?;
    Ok(())
}

#[tauri::command]
pub fn list_entity_labels(state: State<AppState>) -> Result<HashMap<String, String>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("SELECT entity_type, label FROM reference_entities")
        .map_err(|e| format!("reference_entities list failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("reference_entities list failed: {e}"))?;
    rows.collect::<Result<HashMap<_, _>, _>>()
        .map_err(|e| format!("reference_entities read failed: {e}"))
}

#[tauri::command]
pub fn set_entity_label(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    label: String,
) -> Result<String, String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("Display name is required.".to_string());
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if !entity_exists(&conn, &entity_type) {
        return Err(format!("Unknown entity '{entity_type}'."));
    }
    conn.execute(
        "UPDATE reference_entities SET label = ?1, updated_at = ?2 WHERE entity_type = ?3",
        params![label, now_iso(), entity_type],
    )
    .map_err(|e| format!("reference_entities update failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "updated_entity_label",
        Some(&entity_type),
        Some(serde_json::json!({ "entity_type": entity_type, "label": label })),
    )?;
    Ok(label)
}

// ---------------------------------------------------------------------------
// Generic records for non-core parent entities (e.g. Trailers, Fuel Cards)
// ---------------------------------------------------------------------------

/// Look up a record row for any parent entity. Returns (id, data_json).
fn record_row(conn: &Connection, entity_type: &str, id: &str) -> Result<(String, String), String> {
    conn.query_row(
        "SELECT id, data FROM entity_records WHERE entity_type = ?1 AND id = ?2",
        params![entity_type, id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(|e| format!("entity_records lookup failed: {e}"))
}

fn list_entity_records_inner(conn: &Connection, entity_type: &str) -> Result<Vec<EntityRecordView>, String> {
    let mut stmt = conn
        .prepare("SELECT id, entity_type, data, created_at, updated_at FROM entity_records WHERE entity_type = ?1 ORDER BY created_at")
        .map_err(|e| format!("entity_records list failed: {e}"))?;
    let rows = stmt
        .query_map(params![entity_type], |r| {
            Ok(EntityRecordView {
                id: r.get(0)?,
                entity_type: r.get(1)?,
                data: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(serde_json::Value::Null),
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })
        .map_err(|e| format!("entity_records list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("entity_records read failed: {e}"))
}

#[tauri::command]
pub fn list_entity_records(state: State<AppState>, entity_type: String) -> Result<Vec<EntityRecordView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    if !entity_exists(&conn, &entity_type) {
        return Err(format!("Unknown entity '{entity_type}'."));
    }
    list_entity_records_inner(&conn, &entity_type)
}

fn validate_record_data(conn: &Connection, entity_type: &str, data: &serde_json::Value) -> Result<(), String> {
    let obj = data
        .as_object()
        .ok_or_else(|| "Record data must be an object.".to_string())?;
    let defs = list_field_defs_raw(conn, entity_type)?;
    for fd in &defs {
        if fd.is_required && fd.is_hidden == false {
            let v = obj.get(&fd.field_key);
            let empty = match v {
                None => true,
                Some(serde_json::Value::Null) => true,
                Some(serde_json::Value::String(s)) => s.trim().is_empty(),
                _ => false,
            };
            if empty {
                return Err(format!("'{}' is required. Fill it in and try again.", fd.field_label));
            }
        }
        if let Some(v) = obj.get(&fd.field_key) {
            if (fd.field_type == "number" || fd.field_type == "measurement") && !v.is_null() {
                if let Some(s) = v.as_str() {
                    if !s.trim().is_empty() && s.parse::<f64>().is_err() {
                        return Err(format!(
                            "'{}' must be a number (e.g. 42), got '{}'. Correct the value and try again.",
                            fd.field_label, s
                        ));
                    }
                }
            }
            if fd.field_type == "boolean" && !v.is_null() {
                if let Some(s) = v.as_str() {
                    let low = s.trim().to_lowercase();
                    if !low.is_empty() && !["true", "false", "yes", "no", "y", "n", "0", "1"].contains(&low.as_str()) {
                        return Err(format!(
                            "'{}' must be Yes or No, got '{}'. Correct the value and try again.",
                            fd.field_label, s
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn create_entity_record(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    data: serde_json::Value,
) -> Result<EntityRecordView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if !entity_exists(&conn, &entity_type) {
        return Err(format!("Unknown entity '{entity_type}'."));
    }
    validate_record_data(&conn, &entity_type, &data)?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    conn.execute(
        "INSERT INTO entity_records (id, entity_type, data, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
        params![id, entity_type, data.to_string(), now],
    )
    .map_err(|e| format!("entity_records create failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "created_entity_record",
        Some(&id),
        Some(serde_json::json!({ "entity_type": entity_type })),
    )?;
    Ok(EntityRecordView {
        id,
        entity_type,
        data,
        created_at: now.clone(),
        updated_at: now,
    })
}

#[tauri::command]
pub fn update_entity_record(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    record_id: String,
    data: serde_json::Value,
) -> Result<EntityRecordView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if !entity_exists(&conn, &entity_type) {
        return Err(format!("Unknown entity '{entity_type}'."));
    }
    validate_record_data(&conn, &entity_type, &data)?;
    let (_, _) = record_row(&conn, &entity_type, &record_id)?;
    conn.execute(
        "UPDATE entity_records SET data = ?1, updated_at = ?2 WHERE id = ?3 AND entity_type = ?4",
        params![data.to_string(), now_iso(), record_id, entity_type],
    )
    .map_err(|e| format!("entity_records update failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "updated_entity_record",
        Some(&record_id),
        Some(serde_json::json!({ "entity_type": entity_type })),
    )?;
    Ok(EntityRecordView {
        id: record_id,
        entity_type,
        data,
        created_at: now_iso(),
        updated_at: now_iso(),
    })
}

#[tauri::command]
pub fn delete_entity_record(
    state: State<AppState>,
    actor_id: String,
    entity_type: String,
    record_id: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let n = conn
        .execute(
            "DELETE FROM entity_records WHERE id = ?1 AND entity_type = ?2",
            params![record_id, entity_type],
        )
        .map_err(|e| format!("entity_records delete failed: {e}"))?;
    if n == 0 {
        return Err("Record not found.".to_string());
    }
    append_audit(
        &conn,
        &actor_id,
        "deleted_entity_record",
        Some(&record_id),
        Some(serde_json::json!({ "entity_type": entity_type })),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Combined reference import/export (one spreadsheet, all entity types)
// ---------------------------------------------------------------------------

/// Ordered visible field keys for an entity: `(field_key, is_standard)`.
fn visible_fields(conn: &Connection, entity_type: &str) -> Result<Vec<(String, bool)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(binding, field_key), is_standard FROM field_definitions
             WHERE entity_type = ?1 AND is_hidden = 0
             ORDER BY is_standard DESC, sort_order, field_label",
        )
        .map_err(|e| format!("field_definitions list failed: {e}"))?;
    let rows = stmt
        .query_map(params![entity_type], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)? != 0)))
        .map_err(|e| format!("field_definitions list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("field_definitions read failed: {e}"))
}

/// Every row of an entity as a key → value map (custom fields flattened from
/// the `extra_fields` JSON so export headers line up with field definitions).
fn entity_row_maps(conn: &Connection, entity_type: &str) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, String> {
    let mut out = Vec::new();
    let base_sql = match entity_type {
        "company" => "SELECT name, status, COALESCE(extra_fields, '{}') FROM companies ORDER BY name",
        "driver" => "SELECT name, status, COALESCE(extra_fields, '{}') FROM drivers ORDER BY name",
        _ => "SELECT v.plate_number, COALESCE(c.name, ''), COALESCE(d.name, ''), v.registered_capacity,
                     COALESCE(v.capacity_unit, 'litres'), v.status, COALESCE(v.extra_fields, '{}')
              FROM vehicles v
              LEFT JOIN companies c ON c.id = v.company_id
              LEFT JOIN drivers d ON d.id = v.default_driver_id
              ORDER BY v.plate_number",
    };
    let mut stmt = conn.prepare(base_sql).map_err(|e| format!("export query failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            if entity_type == "vehicle" {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            } else {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, None, String::new(), String::new(), String::new()))
            }
        })
        .map_err(|e| format!("export query failed: {e}"))?;
    for row in rows {
        let row = row.map_err(|e| format!("export read failed: {e}"))?;
        let mut map = serde_json::Map::new();
        if entity_type == "vehicle" {
            map.insert("plate_number".into(), serde_json::Value::String(row.0));
            map.insert("company".into(), serde_json::Value::String(row.1));
            map.insert("driver".into(), serde_json::Value::String(row.2));
            map.insert(
                "registered_capacity".into(),
                match row.3 {
                    Some(v) => serde_json::Value::from(v),
                    None => serde_json::Value::Null,
                },
            );
            map.insert("capacity_unit".into(), serde_json::Value::String(row.4));
            map.insert("status".into(), serde_json::Value::String(row.5));
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row.6) {
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        map.insert(k.clone(), val.clone());
                    }
                }
            }
        } else {
            map.insert("name".into(), serde_json::Value::String(row.0));
            map.insert("status".into(), serde_json::Value::String(row.1));
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row.2) {
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        map.insert(k.clone(), val.clone());
                    }
                }
            }
        }
        out.push(map);
    }
    Ok(out)
}

fn json_cell_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Export the whole reference database as one XLSX workbook with a sheet per
/// entity (Companies / Drivers / Vehicles). Column headers come from the field
/// definitions (labels), so renamed built-ins and custom fields round-trip.
#[tauri::command]
pub fn reference_export_combined(
    state: State<AppState>,
    actor_id: String,
    target_path: Option<String>,
) -> Result<String, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let path = if let Some(tp) = target_path.as_deref() {
        std::path::PathBuf::from(tp)
    } else {
        let dir = state
            .frames_dir
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("exports");
        std::fs::create_dir_all(&dir).map_err(|e| format!("export dir create failed: {e}"))?;
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        dir.join(format!("truckflow-reference-{ts}.xlsx"))
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("export dir create failed: {e}"))?;
    }
    let mut workbook = rust_xlsxwriter::Workbook::new();
    for entity in all_entities(&conn)? {
        let entity_type = &entity.entity_type;
        let fields = visible_fields(&conn, entity_type)?;
        if fields.is_empty() {
            continue;
        }
        let labels: Vec<String> = fields
            .iter()
            .map(|(key, _)| field_label(&conn, entity_type, key).unwrap_or_else(|| key.clone()))
            .collect();
        let rows = if entity.is_core {
            entity_row_maps(&conn, entity_type)?
        } else {
            // Admin-added parents: records are JSON blobs in entity_records.
            let mut out = Vec::new();
            for rec in list_entity_records_inner(&conn, entity_type)? {
                if let Some(obj) = rec.data.as_object() {
                    out.push(obj.clone());
                }
            }
            out
        };
        let worksheet = workbook.add_worksheet();
        worksheet
            .set_name(entity_label(&conn, entity_type))
            .map_err(|e| format!("worksheet name failed: {e}"))?;
        for (ci, label) in labels.iter().enumerate() {
            let _ = worksheet.write_string(0, ci as u16, label);
        }
        for (ri, map) in rows.iter().enumerate() {
            let row_idx = ri as u32 + 1;
            for (ci, (key, _)) in fields.iter().enumerate() {
                let _ = worksheet.write_string(row_idx, ci as u16, &json_cell_string(map.get(key).unwrap_or(&serde_json::Value::Null)));
            }
        }
    }
    workbook
        .save(&path)
        .map_err(|e| format!("xlsx save failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "reference_exported_combined",
        None,
        Some(serde_json::json!({ "path": path.to_string_lossy() })),
    )?;
    Ok(path.to_string_lossy().into_owned())
}

fn field_label(conn: &Connection, entity_type: &str, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT field_label FROM field_definitions WHERE entity_type = ?1 AND field_key = ?2",
        params![entity_type, key],
        |r| r.get(0),
    )
    .ok()
}

/// Read every worksheet of an XLSX file as (sheet name, rows incl. header).
fn read_xlsx_sheets(path: &std::path::Path) -> Result<Vec<(String, Vec<Vec<String>>)>, String> {
    let mut workbook = calamine::open_workbook_auto(path).map_err(|e| format!("xlsx open failed: {e}"))?;
    let names: Vec<String> = workbook.sheet_names().to_vec();
    let mut out = Vec::new();
    for name in names {
        let range = workbook
            .worksheet_range(&name)
            .map_err(|e| format!("xlsx sheet '{name}' read failed: {e}"))?;
        out.push((
            name,
            range.rows().map(|row| row.iter().map(cell_to_string).collect()).collect(),
        ));
    }
    Ok(out)
}

/// Guess the entity type of a sheet from its headers ("unknown" when
/// company-vs-driver is ambiguous — the admin picks in the UI).
/// Guess which entity a worksheet holds from its header row. Uses the same
/// alias vocabulary as `standard_key_for` so renamed columns (Business Name,
/// Full Name, Fleet, Reg No…) still resolve to the right parent. Scores each
/// candidate by how many header keys match it; the highest wins.
fn infer_entity_type(header: &[String]) -> String {
    let keys: Vec<String> = header.iter().map(|h| norm_header(h)).collect();

    let vehicle_plate = [
        "plate_number", "plate", "license_plate", "number_plate", "reg_no",
        "registration", "plate_no", "registration_number",
    ];
    let vehicle_capacity = [
        "registered_capacity", "capacity", "capacity_l", "capacity_litres",
        "tonnage", "payload", "load_capacity",
    ];
    let company_name = [
        "name", "company", "company_name", "business_name", "business",
        "organisation", "organization", "firm", "enterprise", "trading_name",
    ];
    let driver_name = [
        "driver", "driver_name", "full_name", "chauffeur", "driver_full_name", "trucker",
    ];

    let mut score = [("vehicle", 0usize), ("company", 0usize), ("driver", 0usize)];
    for k in &keys {
        if vehicle_plate.contains(&k.as_str()) || vehicle_capacity.contains(&k.as_str()) {
            score[0].1 += 2; // plate/capacity is a strong vehicle signal
        }
        if company_name.contains(&k.as_str()) {
            score[1].1 += 1;
        }
        if driver_name.contains(&k.as_str()) {
            score[2].1 += 1;
        }
    }
    // A sheet with both company and driver columns is a vehicle sheet.
    if score[1].1 > 0 && score[2].1 > 0 && score[0].1 > 0 {
        return "vehicle".to_string();
    }
    let best = score.iter().max_by_key(|(_, s)| *s).map(|(e, _)| *e).unwrap_or("unknown");
    if score.iter().all(|(_, s)| *s == 0) {
        return "unknown".to_string();
    }
    best.to_string()
}

/// Standard field key a normalised header maps to, if any.
fn standard_key_for(entity_type: &str, h: &str) -> Option<&'static str> {
    match entity_type {
        "vehicle" => match h {
            "plate_number" | "plate" | "license_plate" | "number_plate"
            | "reg_no" | "registration" | "plate_no" | "registration_number" => Some("plate_number"),
            "company" | "company_name" | "fleet" | "fleet_name"
            | "owner" | "owner_name" | "trucking_company" => Some("company"),
            "driver" | "driver_name" | "truck_driver" | "chauffeur"
            | "driver_full_name" => Some("driver"),
            "registered_capacity" | "capacity" | "capacity_l"
            | "capacity_litres" | "tonnage" | "payload" | "load_capacity" => Some("registered_capacity"),
            "capacity_unit" | "unit" | "measurement_unit" => Some("capacity_unit"),
            "status" | "operational" | "active" | "is_active"
            | "operational_status" => Some("status"),
            _ => None,
        },
        "company" => match h {
            "name" | "company_name" | "business_name" | "business"
            | "organisation" | "organization" | "firm" | "enterprise"
            | "trading_name" => Some("name"),
            "status" | "operational" | "active" | "is_active" => Some("status"),
            _ => None,
        },
        "driver" => match h {
            "name" | "driver_name" | "full_name" | "chauffeur"
            | "driver_full_name" | "trucker" => Some("name"),
            "status" | "operational" | "active" | "is_active" => Some("status"),
            _ => None,
        },
        _ => match h {
            "name" => Some("name"),
            _ => None,
        },
    }
}

/// Guess a field type from the non-empty values in a column.
fn infer_field_type(values: &[String]) -> String {
    let vals: Vec<&str> = values.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if vals.is_empty() {
        return "text".to_string();
    }
    if vals.iter().all(|v| v.parse::<f64>().is_ok()) {
        return "number".to_string();
    }
    let bools = ["true", "false", "yes", "no", "y", "n", "0", "1"];
    if vals.iter().all(|v| bools.contains(&v.to_lowercase().as_str())) {
        return "boolean".to_string();
    }
    let has_digit = vals.iter().any(|v| v.chars().any(|c| c.is_ascii_digit()));
    let has_letter = vals.iter().any(|v| v.chars().any(|c| c.is_ascii_alphabetic()));
    if has_digit && has_letter {
        "mixed"
    } else if has_digit {
        "number"
    } else {
        "text"
    }
    .to_string()
}

fn derive_field_key(header: &str, existing: &[String]) -> String {
    let base = norm_header(header).replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
    let base = if base.is_empty() { "custom_field".to_string() } else { base };
    let mut key = base.clone();
    let mut n = 2;
    while existing.contains(&key) {
        key = format!("{base}_{n}");
        n += 1;
    }
    key
}

/// Classify one spreadsheet column against the entity's field definitions.
fn classify_column(
    conn: &Connection,
    entity_type: &str,
    header: &str,
    values: &[String],
) -> Result<ColumnInfo, String> {
    let h = norm_header(header);
    let samples: Vec<String> = values.iter().take(5).cloned().collect();
    if let Some(key) = standard_key_for(entity_type, &h) {
        return Ok(ColumnInfo::Standard {
            header: header.to_string(),
            field_key: key.to_string(),
            sample_values: samples,
        });
    }
    let mut defs: Vec<(String, String, String, bool, Option<String>)> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT field_key, field_label, field_type, is_standard, binding FROM field_definitions WHERE entity_type = ?1")
            .map_err(|e| format!("field_definitions lookup failed: {e}"))?;
        let rows = stmt
            .query_map(params![entity_type], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i32>(3)? != 0,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| format!("field_definitions lookup failed: {e}"))?;
        for row in rows {
            defs.push(row.map_err(|e| format!("field_definitions read failed: {e}"))?);
        }
    }
    for (key, label, ftype, is_std, binding) in &defs {
        if h == norm_header(key) || h == norm_header(label) {
            let field_key = if *is_std { binding.clone().unwrap_or_else(|| key.clone()) } else { key.clone() };
            return if *is_std {
                Ok(ColumnInfo::Standard {
                    header: header.to_string(),
                    field_key,
                    sample_values: samples,
                })
            } else {
                Ok(ColumnInfo::ExistingCustom {
                    header: header.to_string(),
                    field_key,
                    field_type: ftype.clone(),
                    sample_values: samples,
                })
            };
        }
    }
    let existing_keys: Vec<String> = defs.iter().map(|d| d.0.clone()).collect();
    Ok(ColumnInfo::NewCustom {
        header: header.to_string(),
        field_key: derive_field_key(header, &existing_keys),
        field_type: infer_field_type(values),
        is_required: false,
        sample_values: samples,
    })
}

/// Build the import preview: read the file, guess each sheet's entity and
/// classify every column so the admin can confirm the mapping.
#[tauri::command]
pub fn reference_import_preview(
    state: State<AppState>,
    actor_id: String,
    file_path: String,
) -> Result<ReferenceImportPreview, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let path = std::path::Path::new(&file_path);
    if !path.exists() {
        return Err(format!("Import file not found: {file_path}"));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let sheets_data: Vec<(String, Vec<Vec<String>>)> = match ext.as_str() {
        "csv" => vec![("Sheet1".to_string(), read_csv_rows(path)?)],
        "xlsx" => read_xlsx_sheets(path)?,
        _ => return Err(format!("Unsupported import format '.{ext}'. Use .csv or .xlsx.")),
    };
    let mut previews = Vec::new();
    for (sheet_name, rows) in sheets_data {
        if rows.is_empty() {
            continue;
        }
        let header = &rows[0];
        let data = &rows[1..];
        let entity_type = infer_entity_type(header);
        let mut columns = Vec::new();
        for (i, h) in header.iter().enumerate() {
            let values: Vec<String> = data
                .iter()
                .filter_map(|r| r.get(i))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .take(20)
                .collect();
            columns.push(classify_column(&conn, &entity_type, h, &values)?);
        }
        previews.push(SheetPreview {
            sheet_name,
            entity_type,
            columns,
            row_count: data.len(),
        });
    }
    Ok(ReferenceImportPreview {
        file_path: file_path.clone(),
        sheets: previews,
    })
}

fn find_or_create_company(conn: &Connection, actor_id: &str, name: &str) -> Result<String, String> {
    let cid: Option<String> = conn
        .query_row(
            "SELECT id FROM companies WHERE upper(name) = upper(?1)",
            params![name],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("company lookup failed: {e}"))?;
    if let Some(id) = cid {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO companies (id, name, status, extra_fields, created_at, updated_at) VALUES (?1, ?2, 'active', NULL, ?3, ?3)",
        params![id, name, now_iso()],
    )
    .map_err(|e| format!("company create failed: {e}"))?;
    append_audit(conn, actor_id, "created_company", Some(&id), Some(serde_json::json!({ "name": name })))?;
    Ok(id)
}

fn find_or_create_driver(conn: &Connection, actor_id: &str, name: &str) -> Result<String, String> {
    let did: Option<String> = conn
        .query_row(
            "SELECT id FROM drivers WHERE upper(name) = upper(?1)",
            params![name],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("driver lookup failed: {e}"))?;
    if let Some(id) = did {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO drivers (id, name, status, extra_fields, created_at, updated_at) VALUES (?1, ?2, 'active', NULL, ?3, ?3)",
        params![id, name, now_iso()],
    )
    .map_err(|e| format!("driver create failed: {e}"))?;
    append_audit(conn, actor_id, "created_driver", Some(&id), Some(serde_json::json!({ "name": name })))?;
    Ok(id)
}

/// Apply one row using a confirmed field_key → column-index map. Vehicles
/// upsert by plate and auto-create their companies/drivers by name; custom
/// field values land in `extra_fields`. Returns Ok(true) on create.
fn apply_import_row_combined(
    conn: &Connection,
    actor_id: &str,
    entity_type: &str,
    cols: &HashMap<String, usize>,
    row: &[String],
) -> Result<bool, String> {
    let get = |key: &str| -> Option<String> {
        cols.get(key)
            .and_then(|&i| row.get(i))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let is_core_entity = matches!(entity_type, "company" | "driver" | "vehicle");
    let status = get("status").unwrap_or_else(|| "active".to_string());
    if is_core_entity && status != "active" && status != "inactive" {
        return Err(format!(
            "Invalid status '{status}'. Fix: the status column must contain only 'active' or 'inactive' — correct the value in the spreadsheet and import again."
        ));
    }
    // Custom values: every mapped key that isn't a standard key for the entity.
    let standard: Vec<&str> = match entity_type {
        "vehicle" => vec!["plate_number", "company", "driver", "registered_capacity", "capacity_unit", "status"],
        _ => vec!["name", "status"],
    };
    let mut extra = serde_json::Map::new();
    for key in cols.keys() {
        if !standard.contains(&key.as_str()) {
            if let Some(v) = get(key) {
                extra.insert(key.clone(), serde_json::Value::String(v));
            }
        }
    }
    let extra_json = if extra.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(extra).to_string())
    };
    match entity_type {
        "company" => {
            let name = get("name").ok_or_else(|| {
                "Company name is required. Fix: make sure every row has a name in the 'name' column (or map the right column to it in the mapping screen)."
                    .to_string()
            })?;
            let existing: Option<String> = conn
                .query_row("SELECT id FROM companies WHERE upper(name) = upper(?1)", params![name], |r| r.get(0))
                .optional()
                .map_err(|e| e.to_string())?;
            match existing {
                Some(id) => {
                    conn.execute(
                        "UPDATE companies SET status = ?1, extra_fields = ?2, updated_at = ?3 WHERE id = ?4",
                        params![status, extra_json, now_iso(), id],
                    )
                    .map_err(|e| format!("company update failed: {e}"))?;
                    Ok(false)
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO companies (id, name, status, extra_fields, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                        params![id, name, status, extra_json, now_iso()],
                    )
                    .map_err(|e| format!("company create failed: {e}"))?;
                    append_audit(conn, actor_id, "created_company", Some(&id), Some(serde_json::json!({ "name": name })))?;
                    Ok(true)
                }
            }
        }
        "driver" => {
            let name = get("name").ok_or_else(|| {
                "Driver name is required. Fix: make sure every row has a name in the 'name' column (or map the right column to it in the mapping screen)."
                    .to_string()
            })?;
            let existing: Option<String> = conn
                .query_row("SELECT id FROM drivers WHERE upper(name) = upper(?1)", params![name], |r| r.get(0))
                .optional()
                .map_err(|e| e.to_string())?;
            match existing {
                Some(id) => {
                    conn.execute(
                        "UPDATE drivers SET status = ?1, extra_fields = ?2, updated_at = ?3 WHERE id = ?4",
                        params![status, extra_json, now_iso(), id],
                    )
                    .map_err(|e| format!("driver update failed: {e}"))?;
                    Ok(false)
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO drivers (id, name, status, extra_fields, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                        params![id, name, status, extra_json, now_iso()],
                    )
                    .map_err(|e| format!("driver create failed: {e}"))?;
                    append_audit(conn, actor_id, "created_driver", Some(&id), Some(serde_json::json!({ "name": name })))?;
                    Ok(true)
                }
            }
        }
        "vehicle" => {
            let plate = normalize_plate(&get("plate_number").ok_or_else(|| {
                "Plate number is required. Fix: every row needs a value in the plate column (or map the right column to it in the mapping screen)."
                    .to_string()
            })?);
            if plate.is_empty() {
                return Err("Plate number is required. Fix: every row needs a plate value — correct the empty cells in the spreadsheet and import again.".to_string());
            }
            let company_id = match get("company") {
                Some(cname) => Some(find_or_create_company(conn, actor_id, &cname)?),
                None => None,
            };
            let driver_id = match get("driver") {
                Some(dname) => Some(find_or_create_driver(conn, actor_id, &dname)?),
                None => None,
            };
            let capacity = match get("registered_capacity") {
                Some(v) => Some(
                    v.parse::<f64>().map_err(|_| {
                        format!("Invalid capacity '{v}'. Fix: the capacity column must contain only numbers (e.g. 30000) — correct the value in the spreadsheet and import again.")
                    })?,
                ),
                None => None,
            };
            let unit = normalize_capacity_unit(&get("capacity_unit").unwrap_or_else(|| "litres".to_string()))?;
            let existing: Option<String> = conn
                .query_row("SELECT id FROM vehicles WHERE upper(plate_number) = upper(?1)", params![plate], |r| r.get(0))
                .optional()
                .map_err(|e| e.to_string())?;
            match existing {
                Some(id) => {
                    conn.execute(
                        "UPDATE vehicles SET company_id = ?1, registered_capacity = ?2, capacity_unit = ?3,
                                default_driver_id = ?4, status = ?5, extra_fields = ?6, updated_at = ?7
                         WHERE id = ?8",
                        params![company_id, capacity, unit, driver_id, status, extra_json, now_iso(), id],
                    )
                    .map_err(|e| format!("vehicle update failed: {e}"))?;
                    Ok(false)
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO vehicles (id, plate_number, company_id, registered_capacity, capacity_unit,
                                default_driver_id, status, extra_fields, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                        params![id, plate, company_id, capacity, unit, driver_id, status, extra_json, now_iso()],
                    )
                    .map_err(|e| format!("vehicle create failed: {e}"))?;
                    append_audit(conn, actor_id, "created_vehicle", Some(&id), Some(serde_json::json!({ "plate_number": plate })))?;
                    Ok(true)
                }
            }
        }
        // Admin-added parents: store the row's values as a JSON record.
        _ => {
            let mut data = serde_json::Map::new();
            for key in cols.keys() {
                if let Some(v) = get(key) {
                    data.insert(key.clone(), serde_json::Value::String(v));
                }
            }
            if data.is_empty() {
                return Err("Row has no values to import.".to_string());
            }
            let data_str = serde_json::Value::Object(data.clone()).to_string();
            // Treat an identical existing record as an update (round-trip safe).
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM entity_records WHERE entity_type = ?1 AND data = ?2",
                    params![entity_type, data_str],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            match existing {
                Some(id) => {
                    conn.execute(
                        "UPDATE entity_records SET data = ?1, updated_at = ?2 WHERE id = ?3",
                        params![data_str, now_iso(), id],
                    )
                    .map_err(|e| format!("entity_records update failed: {e}"))?;
                    Ok(false)
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO entity_records (id, entity_type, data, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                        params![id, entity_type, data_str, now_iso()],
                    )
                    .map_err(|e| format!("entity_records create failed: {e}"))?;
                    append_audit(conn, actor_id, "created_entity_record", Some(&id), Some(serde_json::json!({ "entity_type": entity_type })))?;
                    Ok(true)
                }
            }
        }
    }
}

fn ensure_custom_field(
    conn: &Connection,
    actor_id: &str,
    entity_type: &str,
    key: &str,
    label: &str,
    field_type: &str,
    is_required: bool,
) -> Result<(), String> {
    if !VALID_FIELD_TYPES.contains(&field_type) {
        return Err(format!("Invalid field type '{field_type}'."));
    }
    let unit: Option<String> = if field_type == "measurement" {
        Some(label.to_lowercase().replace(' ', "_"))
    } else {
        None
    };
    let exists: Option<String> = conn
        .query_row(
            "SELECT id FROM field_definitions WHERE entity_type = ?1 AND field_key = ?2",
            params![entity_type, key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("field_definitions lookup failed: {e}"))?;
    if exists.is_some() {
        return Ok(());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    conn.execute(
        "INSERT INTO field_definitions
            (id, entity_type, field_key, field_label, field_type, is_required, field_unit, sort_order, is_standard, is_hidden, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, ?9, ?9)",
        params![id, entity_type, key, label, field_type, is_required as i32, unit, 100, now],
    )
    .map_err(|e| format!("field_definitions create failed: {e}"))?;
    append_audit(
        conn,
        actor_id,
        "created_field_definition",
        Some(&id),
        Some(serde_json::json!({ "entity_type": entity_type, "field_key": key, "field_label": label })),
    )?;
    Ok(())
}

fn summary_for<'a>(summary: &'a mut CombinedImportSummary, entity: &str) -> &'a mut ReferenceImportSummary {
    match entity {
        "company" => &mut summary.companies,
        "driver" => &mut summary.drivers,
        _ => &mut summary.vehicles,
    }
}

/// Apply a confirmed combined import: create any new custom fields, then upsert
/// every row by plate (vehicles) or name (companies/drivers). Per-row failures
/// are collected per entity instead of aborting.
#[tauri::command]
pub fn reference_import_combined(
    state: State<AppState>,
    actor_id: String,
    request: ReferenceImportRequest,
) -> Result<CombinedImportSummary, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    let path = std::path::Path::new(&request.file_path);
    if !path.exists() {
        return Err(format!("Import file not found: {}", request.file_path));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let sheets_data: Vec<(String, Vec<Vec<String>>)> = match ext.as_str() {
        "csv" => vec![("Sheet1".to_string(), read_csv_rows(path)?)],
        "xlsx" => read_xlsx_sheets(path)?,
        _ => return Err(format!("Unsupported import format '.{ext}'. Use .csv or .xlsx.")),
    };
    let mut summary = CombinedImportSummary {
        companies: ReferenceImportSummary { entity_type: "company".into(), created: 0, updated: 0, skipped: 0, errors: Vec::new() },
        drivers: ReferenceImportSummary { entity_type: "driver".into(), created: 0, updated: 0, skipped: 0, errors: Vec::new() },
        vehicles: ReferenceImportSummary { entity_type: "vehicle".into(), created: 0, updated: 0, skipped: 0, errors: Vec::new() },
    };
    for sheet in &request.sheets {
        let Some((_, rows)) = sheets_data.iter().find(|(name, _)| name == &sheet.sheet_name) else {
            continue;
        };
        if rows.is_empty() {
            continue;
        }
        if !entity_exists(&conn, &sheet.entity_type) {
            summary_for(&mut summary, &sheet.entity_type)
                .errors
                .push(format!("{}: unknown entity '{}' — create the parent first in Fields, then import again.", sheet.sheet_name, sheet.entity_type));
            continue;
        }
        // Resolve column mappings → field_key → column index; create new fields.
        let header = &rows[0];
        let mut colmap: HashMap<String, usize> = HashMap::new();
        for col in &sheet.columns {
            let Some(idx) = header.iter().position(|h| h == &col.header) else {
                continue;
            };
            match col.mapping.as_str() {
                "ignore" => {}
                "new" => {
                    let key = col.new_field_key.clone().unwrap_or_else(|| derive_field_key(&col.header, &[]));
                    ensure_custom_field(
                        &conn,
                        &actor_id,
                        &sheet.entity_type,
                        &key,
                        &col.header,
                        col.new_field_type.as_deref().unwrap_or("text"),
                        col.new_is_required.unwrap_or(false),
                    )?;
                    colmap.insert(key, idx);
                }
                key => {
                    colmap.insert(key.to_string(), idx);
                }
            }
        }
        for (i, row) in rows.iter().enumerate().skip(1) {
            let row_no = i + 2; // header is row 1
            match apply_import_row_combined(&conn, &actor_id, &sheet.entity_type, &colmap, row) {
                Ok(true) => summary_for(&mut summary, &sheet.entity_type).created += 1,
                Ok(false) => summary_for(&mut summary, &sheet.entity_type).updated += 1,
                Err(e) => {
                    let s = summary_for(&mut summary, &sheet.entity_type);
                    s.errors.push(format!("{} row {row_no}: {e}", sheet.sheet_name));
                    s.skipped += 1;
                }
            }
        }
    }
    append_audit(
        &conn,
        &actor_id,
        "reference_imported_combined",
        None,
        Some(serde_json::json!({
            "file": request.file_path,
            "companies": summary.companies.created + summary.companies.updated,
            "drivers": summary.drivers.created + summary.drivers.updated,
            "vehicles": summary.vehicles.created + summary.vehicles.updated,
            "errors": summary.companies.errors.len() + summary.drivers.errors.len() + summary.vehicles.errors.len(),
        })),
    )?;
    Ok(summary)
}

#[cfg(test)]
mod alias_tests {
    use super::standard_key_for;

    #[test]
    fn renamed_headers_map_to_standard_keys() {
        // Vehicles — different naming conventions
        assert_eq!(standard_key_for("vehicle", "reg_no"), Some("plate_number"));
        assert_eq!(standard_key_for("vehicle", "fleet"), Some("company"));
        assert_eq!(standard_key_for("vehicle", "fleet_name"), Some("company"));
        assert_eq!(standard_key_for("vehicle", "truck_driver"), Some("driver"));
        assert_eq!(standard_key_for("vehicle", "tonnage"), Some("registered_capacity"));
        assert_eq!(standard_key_for("vehicle", "operational"), Some("status"));
        assert_eq!(standard_key_for("vehicle", "unit"), Some("capacity_unit"));
        // Companies — different naming conventions
        assert_eq!(standard_key_for("company", "business_name"), Some("name"));
        assert_eq!(standard_key_for("company", "trading_name"), Some("name"));
        assert_eq!(standard_key_for("company", "operational"), Some("status"));
        // Drivers — different naming conventions
        assert_eq!(standard_key_for("driver", "full_name"), Some("name"));
        assert_eq!(standard_key_for("driver", "operational"), Some("status"));
        // Headers that are genuinely new stay None (become custom fields)
        assert_eq!(standard_key_for("company", "location"), None);
        assert_eq!(standard_key_for("vehicle", "insurance_expiry"), None);
        assert_eq!(standard_key_for("vehicle", "route"), None);
    }

    #[test]
    fn infer_entity_type_from_renamed_headers() {
        use super::infer_entity_type;
        let v = |hs: &[&str]| hs.iter().map(|h| h.to_string()).collect::<Vec<_>>();
        // Vehicles sheet: Reg No + Fleet + Truck Driver + Tonnage
        assert_eq!(
            infer_entity_type(&v(&["Reg No", "Fleet", "Truck Driver", "Tonnage", "Unit", "Operational"])),
            "vehicle"
        );
        // Companies sheet: Business Name + Operational + Location + Contact
        assert_eq!(
            infer_entity_type(&v(&["Business Name", "Operational", "Location", "Contact"])),
            "company"
        );
        // Drivers sheet: Full Name + Operational + License Type + Phone
        assert_eq!(
            infer_entity_type(&v(&["Full Name", "Operational", "License Type", "Phone"])),
            "driver"
        );
        // Plain English headers too
        assert_eq!(infer_entity_type(&v(&["Plate", "Company", "Driver", "Capacity"])), "vehicle");
        assert_eq!(infer_entity_type(&v(&["Company Name", "Status"])), "company");
        assert_eq!(infer_entity_type(&v(&["Driver Name", "Status"])), "driver");
        // No recognizable headers
        assert_eq!(infer_entity_type(&v(&["Foo", "Bar", "Baz"])), "unknown");
    }
}
