//! Reference database management — companies, vehicles, drivers.
//! Add / edit / deactivate (never hard delete). All commands gated by
//! `manage_reference_database` (05-ui-screens.md §6b).

use rusqlite::{params, Connection, OptionalExtension};
use tauri::State;

use crate::commands::ensure_admin_permission;
use crate::db::{append_audit, now_iso, AppState};
use crate::models::{CompanyView, DriverView, FieldDefinition, VehicleView};

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
    append_audit(&conn, &actor_id, "created_company", Some(&id), None)?;
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
    append_audit(&conn, &actor_id, "created_driver", Some(&id), None)?;
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
// Dynamic field definitions (migration 14)
// ---------------------------------------------------------------------------

const VALID_ENTITY_TYPES: &[&str] = &["company", "vehicle", "driver"];
const VALID_FIELD_TYPES: &[&str] = &["text", "number", "boolean", "mixed"];

fn read_field_def(row: &rusqlite::Row) -> rusqlite::Result<FieldDefinition> {
    Ok(FieldDefinition {
        id: row.get(0)?,
        entity_type: row.get(1)?,
        field_key: row.get(2)?,
        field_label: row.get(3)?,
        field_type: row.get(4)?,
        is_required: row.get::<_, i32>(5)? != 0,
        sort_order: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

#[tauri::command]
pub fn list_field_definitions(
    state: State<AppState>,
    entity_type: String,
) -> Result<Vec<FieldDefinition>, String> {
    if !VALID_ENTITY_TYPES.contains(&entity_type.as_str()) {
        return Err(format!("Invalid entity type '{entity_type}'. Must be one of: {}.", VALID_ENTITY_TYPES.join(", ")));
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, entity_type, field_key, field_label, field_type, is_required, sort_order, created_at, updated_at
             FROM field_definitions WHERE entity_type = ?1 ORDER BY sort_order, field_label",
        )
        .map_err(|e| format!("field_definitions list failed: {e}"))?;
    let rows = stmt
        .query_map(params![entity_type], read_field_def)
        .map_err(|e| format!("field_definitions list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("field_definitions read failed: {e}"))
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
) -> Result<FieldDefinition, String> {
    if !VALID_ENTITY_TYPES.contains(&entity_type.as_str()) {
        return Err(format!("Invalid entity type '{entity_type}'. Must be one of: {}.", VALID_ENTITY_TYPES.join(", ")));
    }
    if !VALID_FIELD_TYPES.contains(&field_type.as_str()) {
        return Err(format!("Invalid field type '{field_type}'. Must be one of: {}.", VALID_FIELD_TYPES.join(", ")));
    }
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
        "INSERT INTO field_definitions (id, entity_type, field_key, field_label, field_type, is_required, sort_order, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![id, entity_type, key, label, field_type, is_required as i32, order, now],
    )
    .map_err(|e| format!("field_definitions create failed: {e}"))?;
    append_audit(&conn, &actor_id, "created_field_definition", Some(&id), Some(serde_json::json!({ "entity_type": entity_type, "field_key": key, "field_label": label })))?;
    Ok(FieldDefinition {
        id,
        entity_type,
        field_key: key,
        field_label: label,
        field_type,
        is_required,
        sort_order: order,
        created_at: now.clone(),
        updated_at: now,
    })
}

#[tauri::command]
pub fn update_field_definition(
    state: State<AppState>,
    actor_id: String,
    field_id: String,
    field_label: Option<String>,
    field_type: Option<String>,
    is_required: Option<bool>,
    sort_order: Option<i32>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REF_PERM)?;
    if let Some(ref ft) = field_type {
        if !VALID_FIELD_TYPES.contains(&ft.as_str()) {
            return Err(format!("Invalid field type '{ft}'. Must be one of: {}.", VALID_FIELD_TYPES.join(", ")));
        }
    }
    let now = now_iso();
    let mut sets = vec!["updated_at = ?1".to_string()];
    let mut idx = 2;
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
    let sql = format!("UPDATE field_definitions SET {} WHERE id = ?{idx}", sets.join(", "));
    let mut bound: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now.clone())];
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
    bound.push(Box::new(field_id.clone()));
    let params_ref: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let n = conn.execute(&sql, params_ref.as_slice()).map_err(|e| format!("field_definitions update failed: {e}"))?;
    if n == 0 {
        return Err("Field definition not found.".to_string());
    }
    append_audit(&conn, &actor_id, "updated_field_definition", Some(&field_id), None)?;
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
    let n = conn
        .execute("DELETE FROM field_definitions WHERE id = ?1", params![field_id])
        .map_err(|e| format!("field_definitions delete failed: {e}"))?;
    if n == 0 {
        return Err("Field definition not found.".to_string());
    }
    append_audit(&conn, &actor_id, "deleted_field_definition", Some(&field_id), None)?;
    Ok(())
}
