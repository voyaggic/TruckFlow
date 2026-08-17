export type AuthLevel = "password";

export type RoleName = "admin" | "staff";

export interface PermissionView {
  id: string;
  key: string;
  min_auth_level: AuthLevel;
  description: string | null;
}

export interface SessionUser {
  id: string;
  name: string;
  auth_type: AuthLevel;
  permissions: PermissionView[];
  theme_mode: string | null;
  theme_accent: string | null;
  phone_number: string | null;
  profile_photo_ref: string | null;
  language_preference: string | null;
  notification_sound: boolean | null;
  must_change_password: boolean;
}

export interface AppStatus {
  needs_first_run: boolean;
  current_user: SessionUser | null;
}

export interface LoginResult {
  user: SessionUser;
  must_change_password: boolean;
  recovery_code: string | null;
}

export interface UserView {
  id: string;
  name: string;
  auth_type: AuthLevel;
  status: "active" | "disabled" | "deleted";
  phone_number: string | null;
  theme_mode: string | null;
  theme_accent: string | null;
  created_at: string;
  permissions: string[];
}

export interface RolePresetView {
  id: string;
  name: string;
  permission_keys: string[];
}

export interface ListPermissionItem {
  id: string;
  key: string;
  min_auth_level: AuthLevel;
  description: string | null;
  granted: boolean;
}

export interface PermissionChangeResult {
  applied: boolean;
  auth_upgrade_required: boolean;
  message: string;
}

export interface PendingUpgradeInfo {
  permission_keys: string[];
  previous_permission_keys: string[];
  requested_by: string;
  requester_name: string;
  requested_at: string;
}

export interface PasswordStrength {
  length: boolean;
  uppercase: boolean;
  lowercase: boolean;
  digit: boolean;
  symbol: boolean;
  valid: boolean;
}

export interface PasswordResetRequestView {
  id: string;
  username: string;
  requested_at: string;
}

export interface RecoveryCodeInfo {
  code: string;
  file_path: string;
}

// ---------------------------------------------------------------------------
// Phase 2 — reference database, capture pipeline
// ---------------------------------------------------------------------------

export interface CompanyView {
  id: string;
  name: string;
  status: "active" | "inactive";
  extra_fields: Record<string, unknown> | null;
  created_at: string;
}

export interface DriverView {
  id: string;
  name: string;
  status: "active" | "inactive";
  extra_fields: Record<string, unknown> | null;
  created_at: string;
}

export interface VehicleView {
  id: string;
  plate_number: string;
  company_id: string | null;
  company_name: string | null;
  registered_capacity: number | null;
  capacity_unit: string;
  default_driver_id: string | null;
  default_driver_name: string | null;
  status: "active" | "inactive";
  extra_fields: Record<string, unknown> | null;
  created_at: string;
}

export interface AnprFrame {
  index: number;
  captured_at: string;
  kind: string;
}

/** A frame ready for the Resolve screen: metadata plus base64 image payload. */
export interface FrameEvidence {
  index: number;
  captured_at: string;
  kind: string;
  data_base64: string | null;
}

export interface AnprRead {
  plate: string;
  confidence: number;
  timestamp: string;
  frames: AnprFrame[];
  model_version: string | null;
  ocr_engine: string | null;
}

export interface MatchOutcome {
  state: "exact" | "narrowed" | "multiple" | "zero";
  matched_vehicle_id: string | null;
  candidates: VehicleView[];
}

export interface TripView {
  id: string;
  vehicle_id: string | null;
  plate_number: string;
  company_id: string | null;
  company_name: string | null;
  driver_id: string | null;
  driver_name: string | null;
  capacity_at_trip: number | null;
  capacity_unit: string;
  time_in: string;
  receipt_no: string | null;
  officer_id: string | null;
  officer_name: string | null;
  capture_method: "auto" | "manual_entry";
  confidence_score: number | null;
  photo_count: number;
  status: string;
  reason: string | null;
  candidates: string[];
  is_discharge_trip: boolean | null;
  model_version: string | null;
  ocr_engine: string | null;
}

export interface IngestResult {
  trip: TripView | null;
  queued: TripView | null;
  outcome: MatchOutcome;
  message: string;
}

export interface AnprStatus {
  enabled: boolean;
  source: string;
  last_read_at: string | null;
  last_plate: string | null;
  pending_reads: number;
}

export interface CaptureSettings {
  consent_mode: "confirm_required" | "fully_automatic";
  confidence_threshold: number;
  anpr_enabled: boolean;
  anpr_source: "simulator" | "http";
  anpr_service_url: string;
  discharge_confirmation_required: boolean;
}

// ---------------------------------------------------------------------------
// ANPR Engine Configuration (08-anpr-integration.md §5 / §6)
// ---------------------------------------------------------------------------

export type OcrEngine = "paddleocr" | "easyocr";

export interface AnprConfigView {
  active_ocr_engine: OcrEngine;
  confidence_threshold_paddleocr: number;
  confidence_threshold_easyocr: number;
  plate_vehicle_ratio_threshold: number;
  plate_format_rules: string | null;
  discharge_confirmation_required: boolean;
  save_recognition_images: boolean;
  retrain_candidate_threshold: number | null;
  is_capture_point: boolean;
}

export interface CameraSourceView {
  id: string;
  label: string;
  source_type: "rtsp" | "http" | "nvr_export" | "usb" | "video_file" | "live_test";
  connection_string: string;
  status: "active" | "inactive";
  last_connection_check_at: string | null;
  last_connection_check_result: string | null;
}

export interface ModelVersionView {
  id: string;
  version_label: string;
  component: string;
  validation_accuracy: number | null;
  is_live: boolean;
  deployed_by: string | null;
  deployed_at: string | null;
  rolled_back_from: string | null;
  created_at: string;
}

export interface TrainingCandidateView {
  id: string;
  source_trip_id: string | null;
  plate_number: string | null;
  frame_ref: string;
  reason: "low_confidence" | "human_corrected";
  used_in_model_version_id: string | null;
  created_at: string;
}

// ---------------------------------------------------------------------------
// Phase 4 — Sync & distribution (06-data-flow.md §5, 02 §3)
// ---------------------------------------------------------------------------

export interface TablePending {
  table: string;
  display: string;
  pending: number;
}

export interface PgSyncStateView {
  connected: boolean;
  adapter: string;
  tables: TablePending[];
  last_synced_at: string | null;
  /** Whether a connection string has been configured (real adapter). */
  configured: boolean;
  /** Most recent failure detail (None = no error). */
  last_error: string | null;
  /** Admin-set retention window for daily trip entries (days), cleared from
   *  local + Postgres; null = keep forever. Separate from sheet retention. */
  trip_retention_days: number | null;
}

export interface SheetsStateView {
  connected: boolean;
  adapter: string;
  pending: number;
  target_sheet_id: string | null;
  shared_group: string | null;
  frequency: "realtime" | "every_15_min";
  last_synced_at: string | null;
  status: string;
  /** Whether credentials + target sheet have been configured. */
  configured: boolean;
  /** Service-account email that owns the export (display only). */
  service_account_email: string | null;
  /** Most recent failure detail (None = no error). */
  last_error: string | null;
  /** Admin-set retention window for the sheet (days); null = no pruning. */
  retention_days: number | null;
}

export interface SyncRunResult {
  pushed: number;
  tables: TablePending[];
  last_run_at: string | null;
}

export interface SyncStatusView {
  online: boolean;
  pg: PgSyncStateView;
  sheets: SheetsStateView;
}

// ---------------------------------------------------------------------------
// Phase 5 — Reporting & oversight (05 §5, §6c, §6g)
// ---------------------------------------------------------------------------

export interface ReportFilters {
  from: string | null;
  to: string | null;
  company_id: string | null;
}

export interface PriorPeriodComparison {
  prior_trips: number;
  delta_trips: number;
  delta_percent: number | null;
}

export interface ReportSummary {
  total_trips: number;
  active_companies: number;
  avg_trips_per_day: number;
  prior_period: PriorPeriodComparison;
}

export interface DailyTripCount {
  date: string;
  count: number;
}

export interface CompanyTripCount {
  company_id: string | null;
  company_name: string;
  count: number;
}

export interface VehicleTripRow {
  plate_number: string;
  company_name: string | null;
  trip_count: number;
  total_capacity: number;
}

export interface ReportDashboard {
  summary: ReportSummary;
  trips_over_time: DailyTripCount[];
  top_companies: CompanyTripCount[];
  trips_by_vehicle: VehicleTripRow[];
  /** "postgres" = permanent archive; "local" = working-buffer fallback. */
  data_source: "postgres" | "local";
}

export interface ReportExportRow {
  id: string;
  plate: string;
  time_in: string;
  company: string;
  driver: string;
  capacity_at_trip: number | null;
  capacity_unit: string;
  receipt_no: string | null;
  capture_method: "auto" | "manual_entry";
  confidence_score: number | null;
}

export interface AuditEntry {
  id: string;
  actor_id: string | null;
  actor_name: string | null;
  action: string;
  target_id: string | null;
  details: unknown | null;
  timestamp: string;
}

export interface AuditFilters {
  from: string | null;
  to: string | null;
  actor_id: string | null;
  action: string | null;
}

export interface OfficerActivityView {
  officer_id: string;
  officer_name: string;
  trips_logged: number;
  queue_resolved: number;
  last_active_at: string | null;
}

// ---------------------------------------------------------------------------
// Phase 5 — System Monitor (05 §6h)
// ---------------------------------------------------------------------------

export interface HealthEventView {
  id: string;
  component: string;
  status: string;
  detail: string | null;
  detected_at: string;
  acknowledged_by: string | null;
  acknowledged_at: string | null;
  resolved_at: string | null;
}

export interface ComponentHealth {
  component: string;
  status: string;
  detail: string | null;
  last_detected_at: string | null;
  open_events: number;
}

export interface HealthDashboard {
  components: ComponentHealth[];
  open_alerts: HealthEventView[];
  recent_history: HealthEventView[];
  sync: SyncStatusView;
  anpr: AnprStatus;
}

export interface ConfidenceTrendPoint {
  date: string;
  avg_confidence: number | null;
  reads: number;
}

// ---------------------------------------------------------------------------
// Dynamic field definitions for the reference database
// ---------------------------------------------------------------------------

export interface FieldDefinition {
  id: string;
  entity_type: string;
  field_key: string;
  field_label: string;
  field_type: "text" | "number" | "boolean" | "mixed";
  is_required: boolean;
  /** Seeded built-in fields (plate, company, name…) back real columns. */
  is_standard: boolean;
  /** Hidden fields are excluded from forms/import/export. */
  is_hidden: boolean;
  /** For standard fields: the fixed internal column binding (e.g. "plate_number");
   *  the user-editable `field_key` may differ. Custom fields: null. */
  binding: string | null;
  sort_order: number;
  created_at: string;
  updated_at: string;
}

export type ReferenceEntityType = "company" | "driver" | "vehicle";
export type ReferenceFileFormat = "csv" | "xlsx";

/** User-editable display names for the three reference entities. */
export type EntityLabels = Record<ReferenceEntityType, string>;

export const DEFAULT_ENTITY_LABELS: EntityLabels = {
  vehicle: "Vehicles",
  company: "Companies",
  driver: "Drivers",
};

export interface ReferenceImportSummary {
  entity_type: ReferenceEntityType;
  created: number;
  updated: number;
  skipped: number;
  errors: string[];
}

export interface ReferenceExportResult {
  file_path: string | null;
}

// ---------------------------------------------------------------------------
// Combined reference import/export (single file, multiple entities)
// ---------------------------------------------------------------------------

/** How a spreadsheet header has been classified during import preview. */
export type ColumnKind = "standard" | "existing_custom" | "new_custom" | "ignore";

/** A recognized standard column of the reference database. */
export interface StandardColumn {
  kind: "standard";
  /** The raw header text from the spreadsheet. */
  header: string;
  /** Internal field key this header maps to (e.g. "plate_number"). */
  field_key: string;
  /** Up to 5 example values from the column, to help the admin decide. */
  sample_values: string[];
}

/** A custom field already defined in the database. */
export interface ExistingCustomColumn {
  kind: "existing_custom";
  header: string;
  field_key: string;
  field_type: "text" | "number" | "boolean" | "mixed";
  sample_values: string[];
}

/** A header that doesn't match anything yet and needs confirmation. */
export interface NewCustomColumn {
  kind: "new_custom";
  header: string;
  /** Proposed field key (derived from the header). */
  field_key: string;
  /** Field type chosen by the admin before import. */
  field_type: "text" | "number" | "boolean" | "mixed";
  /** Whether this column is required. */
  is_required: boolean;
  sample_values: string[];
}

export type ColumnInfo = StandardColumn | ExistingCustomColumn | NewCustomColumn;

/** One worksheet/sheet of the uploaded spreadsheet preview. */
export interface SheetPreview {
  sheet_name: string;
  entity_type: ReferenceEntityType | "unknown";
  columns: ColumnInfo[];
  row_count: number;
}

export interface ReferenceImportPreview {
  file_path: string;
  sheets: SheetPreview[];
}

/** A column mapping confirmed by the admin before applying the import. */
export interface ConfirmedColumn {
  header: string;
  /** "standard" | existing field_key | "new" */
  mapping: "ignore" | string;
  /** Required only when mapping === "new" */
  new_field_key?: string;
  new_field_type?: "text" | "number" | "boolean" | "mixed";
  new_is_required?: boolean;
}

export interface ConfirmedSheet {
  sheet_name: string;
  entity_type: ReferenceEntityType;
  columns: ConfirmedColumn[];
}

export interface ReferenceImportRequest {
  file_path: string;
  sheets: ConfirmedSheet[];
}

/** Full summary covering all three entity types in a combined import. */
export interface CombinedImportSummary {
  companies: ReferenceImportSummary;
  drivers: ReferenceImportSummary;
  vehicles: ReferenceImportSummary;
}
