use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PermissionView {
    pub id: String,
    pub key: String,
    pub min_auth_level: String,
    pub description: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct UserView {
    pub id: String,
    pub name: String,
    pub auth_type: String,
    pub status: String,
    pub phone_number: Option<String>,
    pub theme_mode: Option<String>,
    pub theme_accent: Option<String>,
    pub created_at: String,
    pub permissions: Vec<String>,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct SessionUser {
    pub id: String,
    pub name: String,
    pub auth_type: String,
    pub permissions: Vec<PermissionView>,
    pub theme_mode: Option<String>,
    pub theme_accent: Option<String>,
    pub phone_number: Option<String>,
    pub profile_photo_ref: Option<String>,
    pub language_preference: Option<String>,
    pub notification_sound: Option<bool>,
    /// Set when an admin reset this account's password — the user must choose a
    /// new password at the next sign-in before using the app.
    pub must_change_password: bool,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct LoginResult {
    pub user: SessionUser,
    pub must_change_password: bool,
    /// Present only on first-run admin creation — the one-time recovery code.
    pub recovery_code: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AppStatus {
    pub needs_first_run: bool,
    pub current_user: Option<SessionUser>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct RolePresetView {
    pub id: String,
    pub name: String,
    pub permission_keys: Vec<String>,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PermissionChangeResult {
    pub applied: bool,
    pub auth_upgrade_required: bool,
    pub message: String,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PasswordStrength {
    pub length: bool,
    pub uppercase: bool,
    pub lowercase: bool,
    pub digit: bool,
    pub symbol: bool,
    pub valid: bool,
}

// ---- Phase 2: reference database ---------------------------------------

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct CompanyView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub extra_fields: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct DriverView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub extra_fields: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct VehicleView {
    pub id: String,
    pub plate_number: String,
    pub company_id: Option<String>,
    pub company_name: Option<String>,
    pub registered_capacity: Option<f64>,
    pub capacity_unit: String,
    pub default_driver_id: Option<String>,
    pub default_driver_name: Option<String>,
    pub status: String,
    pub extra_fields: Option<serde_json::Value>,
    pub created_at: String,
}

// ---- Phase 2: capture pipeline ------------------------------------------

/// A single structured read emitted by the ANPR source (02-architecture.md §4).
/// `model_version` and `ocr_engine` are recorded on every auto / manual-confirm
/// read and never omitted for those capture methods (01-database-schema.md).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AnprRead {
    pub plate: String,
    pub confidence: f64,
    pub timestamp: String,
    pub frames: Vec<AnprFrame>,
    #[serde(default)]
    pub model_version: Option<String>,
    #[serde(default)]
    pub ocr_engine: Option<String>,
}

/// One captured frame of evidence. `data` is optional base64 image payload from
/// the ANPR service; the simulator substitutes placeholder images. Frame files
/// are persisted to disk and referenced by `file` (04-capture-pipeline.md §7.4).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AnprFrame {
    pub index: u32,
    pub captured_at: String,
    pub kind: String,
    #[serde(default)]
    pub data: Option<String>,
}

/// A frame ready for the UI: metadata plus base64 image payload (or None if the
/// file is missing/too large).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct FrameEvidence {
    pub index: u32,
    pub captured_at: String,
    pub kind: String,
    pub data_base64: Option<String>,
}

/// Outcome of cross-referencing a plate read against the reference database.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MatchOutcome {
    pub state: String, // exact | narrowed | multiple | zero
    pub matched_vehicle_id: Option<String>,
    pub candidates: Vec<VehicleView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TripView {
    pub id: String,
    pub vehicle_id: Option<String>,
    pub plate_number: String,
    pub company_id: Option<String>,
    pub company_name: Option<String>,
    pub driver_id: Option<String>,
    pub driver_name: Option<String>,
    pub capacity_at_trip: Option<f64>,
    pub capacity_unit: String,
    pub time_in: String,
    pub receipt_no: Option<String>,
    pub officer_id: Option<String>,
    pub officer_name: Option<String>,
    pub capture_method: String, // auto | manual_entry
    pub confidence_score: Option<f64>,
    pub photo_count: usize,
    pub status: String,
    pub reason: Option<String>,
    pub candidates: Vec<String>,
    pub is_discharge_trip: Option<bool>,
    pub model_version: Option<String>,
    pub ocr_engine: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct IngestResult {
    pub trip: Option<TripView>,
    pub queued: Option<TripView>,
    pub outcome: MatchOutcome,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AnprStatus {
    pub enabled: bool,
    pub source: String,
    pub last_read_at: Option<String>,
    pub last_plate: Option<String>,
    pub pending_reads: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CaptureSettings {
    pub consent_mode: String,
    pub confidence_threshold: f64,
    pub anpr_enabled: bool,
    pub anpr_source: String,
    pub anpr_service_url: String,
    pub discharge_confirmation_required: bool,
    pub is_capture_point: bool,
}

/// Full ANPR Engine Configuration payload (08-anpr-integration.md §5).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AnprConfigView {
    pub active_ocr_engine: String,
    pub confidence_threshold_paddleocr: f64,
    pub confidence_threshold_easyocr: f64,
    pub plate_vehicle_ratio_threshold: f64,
    pub plate_format_rules: Option<String>,
    pub discharge_confirmation_required: bool,
    pub save_recognition_images: bool,
    pub retrain_candidate_threshold: Option<i64>,
    pub is_capture_point: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CameraSourceView {
    pub id: String,
    pub label: String,
    pub source_type: String,
    pub connection_string: String,
    pub status: String,
    pub last_connection_check_at: Option<String>,
    pub last_connection_check_result: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ModelVersionView {
    pub id: String,
    pub version_label: String,
    pub component: String,
    pub validation_accuracy: Option<f64>,
    pub is_live: bool,
    pub deployed_by: Option<String>,
    pub deployed_at: Option<String>,
    pub rolled_back_from: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TrainingCandidateView {
    pub id: String,
    pub source_trip_id: Option<String>,
    pub plate_number: Option<String>,
    pub frame_ref: String,
    pub reason: String,
    pub used_in_model_version_id: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TablePending {
    pub table: String,
    pub display: String,
    pub pending: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PgSyncStateView {
    pub connected: bool,
    pub adapter: String,
    pub tables: Vec<TablePending>,
    pub last_synced_at: Option<String>,
    /// Whether a connection string has been configured (real adapter).
    pub configured: bool,
    /// Most recent failure detail, surfaced in the UI (None = no error).
    pub last_error: Option<String>,
    /// Admin-set retention window for daily trip entries (days), cleared from
    /// local + Postgres; None = keep forever. Separate from sheet retention.
    pub trip_retention_days: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SheetsStateView {
    pub connected: bool,
    pub adapter: String,
    pub pending: i64,
    pub target_sheet_id: Option<String>,
    pub shared_group: Option<String>,
    pub frequency: String,
    pub last_synced_at: Option<String>,
    pub status: String,
    /// Whether credentials + target sheet have been configured.
    pub configured: bool,
    /// Service-account email that owns the export (display only).
    pub service_account_email: Option<String>,
    /// Most recent failure detail, surfaced in the UI (None = no error).
    pub last_error: Option<String>,
    /// Admin-set retention window for the sheet (days); None = no pruning.
    pub retention_days: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SyncRunResult {
    pub pushed: i64,
    pub tables: Vec<TablePending>,
    pub last_run_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SyncStatusView {
    pub online: bool,
    pub pg: PgSyncStateView,
    pub sheets: SheetsStateView,
}

// ---------------------------------------------------------------------------
// Phase 5 — Reporting & oversight (05-ui-screens.md §5, §6c, §6g)
// ---------------------------------------------------------------------------

/// Shared date-range + company filter for all reporting queries. Dates are ISO
/// datetime strings; ISO values compare correctly as strings. `None` = unbounded.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct ReportFilters {
    pub from: Option<String>,
    pub to: Option<String>,
    pub company_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PriorPeriodComparison {
    pub prior_trips: i64,
    pub delta_trips: i64,
    /// None when the prior period had zero trips (division-by-zero guard).
    pub delta_percent: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ReportSummary {
    pub total_trips: i64,
    pub active_companies: i64,
    pub avg_trips_per_day: f64,
    pub prior_period: PriorPeriodComparison,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DailyTripCount {
    pub date: String,
    pub count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CompanyTripCount {
    pub company_id: Option<String>,
    pub company_name: String,
    pub count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct VehicleTripRow {
    pub plate_number: String,
    pub company_name: Option<String>,
    pub trip_count: i64,
    pub total_capacity: f64,
}

/// Everything the Reporting Dashboard renders in one call (05 §5).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ReportDashboard {
    pub summary: ReportSummary,
    pub trips_over_time: Vec<DailyTripCount>,
    pub top_companies: Vec<CompanyTripCount>,
    pub trips_by_vehicle: Vec<VehicleTripRow>,
    /// Which store produced these numbers: "postgres" (permanent archive) or
    /// "local" (working buffer fallback while the central DB is unreachable).
    pub data_source: String,
}

/// One flat export row for Excel / CSV (mirrors the Sheets export shape).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ReportExportRow {
    pub id: String,
    pub plate: String,
    pub time_in: String,
    pub company: String,
    pub driver: String,
    pub capacity_at_trip: Option<f64>,
    pub capacity_unit: String,
    pub receipt_no: Option<String>,
    pub capture_method: String,
    pub confidence_score: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AuditEntry {
    pub id: String,
    pub actor_id: Option<String>,
    pub actor_name: Option<String>,
    pub action: String,
    pub target_id: Option<String>,
    pub details: Option<serde_json::Value>,
    pub timestamp: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct AuditFilters {
    pub from: Option<String>,
    pub to: Option<String>,
    pub actor_id: Option<String>,
    pub action: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OfficerActivityView {
    pub officer_id: String,
    pub officer_name: String,
    pub trips_logged: i64,
    pub queue_resolved: i64,
    pub last_active_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Phase 5 — System Monitor (05-ui-screens.md §6h, 08-anpr-integration.md §5)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct HealthEventView {
    pub id: String,
    pub component: String,
    pub status: String,
    pub detail: Option<String>,
    pub detected_at: String,
    pub acknowledged_by: Option<String>,
    pub acknowledged_at: Option<String>,
    pub resolved_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ComponentHealth {
    pub component: String,
    pub status: String,
    pub detail: Option<String>,
    pub last_detected_at: Option<String>,
    pub open_events: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct HealthDashboard {
    pub components: Vec<ComponentHealth>,
    pub open_alerts: Vec<HealthEventView>,
    pub recent_history: Vec<HealthEventView>,
    pub sync: SyncStatusView,
    pub anpr: AnprStatus,
}

/// One point in the ANPR confidence-over-time series (05-ui-screens.md §6h):
/// per-day average confidence plus the read volume that point is based on.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ConfidenceTrendPoint {
    pub date: String,
    pub avg_confidence: Option<f64>,
    pub reads: i64,
}

// ---------------------------------------------------------------------------
// Dynamic field definitions for the reference database
// ---------------------------------------------------------------------------

/// Admin-configurable field definition for companies, vehicles, or drivers.
/// Values are stored in the existing `extra_fields` JSON column on each entity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FieldDefinition {
    pub id: String,
    pub entity_type: String,
    pub field_key: String,
    pub field_label: String,
    pub field_type: String, // text | number | boolean | mixed
    pub is_required: bool,
    /// True for the seeded built-in fields (plate, company, driver, name…).
    /// Standard fields map to real database columns; custom fields map to the
    /// `extra_fields` JSON column.
    pub is_standard: bool,
    /// Hidden fields are excluded from forms, import, and export. Standard
    /// fields are hidden (never hard-deleted) because they back real columns.
    pub is_hidden: bool,
    /// For standard fields: the fixed internal column binding (e.g.
    /// "plate_number"). The user-editable `field_key` may differ from it.
    /// Custom fields have binding = None.
    pub binding: Option<String>,
    pub sort_order: i32,
    pub created_at: String,
    pub updated_at: String,
}

/// Per-entity outcome of a reference import (CSV/XLSX).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReferenceImportSummary {
    pub entity_type: String,
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Combined reference import/export (one spreadsheet, all entity types)
// ---------------------------------------------------------------------------

/// How a spreadsheet header has been classified during import preview.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ColumnInfo {
    /// A recognised standard column (plate_number, company, name…).
    Standard {
        header: String,
        field_key: String,
        sample_values: Vec<String>,
    },
    /// A custom field already defined in the database.
    ExistingCustom {
        header: String,
        field_key: String,
        field_type: String,
        sample_values: Vec<String>,
    },
    /// A header that matches nothing yet and needs confirmation.
    NewCustom {
        header: String,
        field_key: String,
        field_type: String,
        is_required: bool,
        sample_values: Vec<String>,
    },
}

/// One worksheet of the uploaded spreadsheet preview.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SheetPreview {
    pub sheet_name: String,
    /// "company" | "driver" | "vehicle" | "unknown" (admin picks when unknown).
    pub entity_type: String,
    pub columns: Vec<ColumnInfo>,
    pub row_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReferenceImportPreview {
    pub file_path: String,
    pub sheets: Vec<SheetPreview>,
}

/// A column mapping confirmed by the admin before applying the import.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConfirmedColumn {
    pub header: String,
    /// "ignore" | existing field key | "new" (create a new custom field).
    pub mapping: String,
    pub new_field_key: Option<String>,
    pub new_field_type: Option<String>,
    pub new_is_required: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConfirmedSheet {
    pub sheet_name: String,
    pub entity_type: String,
    pub columns: Vec<ConfirmedColumn>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReferenceImportRequest {
    pub file_path: String,
    pub sheets: Vec<ConfirmedSheet>,
}

/// Full summary covering all three entity types in a combined import.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CombinedImportSummary {
    pub companies: ReferenceImportSummary,
    pub drivers: ReferenceImportSummary,
    pub vehicles: ReferenceImportSummary,
}

/// A parent entity of the reference database (Vehicles / Companies / Drivers
/// plus any the admin adds, e.g. "Trailers"). Each parent owns its field
/// definitions (children).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReferenceEntity {
    pub entity_type: String,
    pub label: String,
    /// True for the seeded pipeline entities (vehicle/company/driver) that the
    /// gate, trips, and reports depend on — they can be renamed but not deleted.
    pub is_core: bool,
    pub sort_order: i32,
    pub created_at: String,
    pub updated_at: String,
}

/// One record of a non-core parent entity (stored as JSON in entity_records).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EntityRecordView {
    pub id: String,
    pub entity_type: String,
    pub data: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
}
