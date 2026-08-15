import { invoke } from "@tauri-apps/api/core";
import type {
  AnprConfigView,
  AnprRead,
  AnprStatus,
  AppStatus,
  AuditEntry,
  AuditFilters,
  CameraSourceView,
  CaptureSettings,
  CompanyView,
  ConfidenceTrendPoint,
  DriverView,
  FieldDefinition,
  FrameEvidence,
  HealthDashboard,
  HealthEventView,
  IngestResult,
  ListPermissionItem,
  LoginResult,
  ModelVersionView,
  OfficerActivityView,
  PasswordResetRequestView,
  PasswordStrength,
  PendingUpgradeInfo,
  PermissionChangeResult,
  RecoveryCodeInfo,
  ReportDashboard,
  ReportExportRow,
  ReportFilters,
  RolePresetView,
  SessionUser,
  PgSyncStateView,
  SheetsStateView,
  SyncRunResult,
  SyncStatusView,
  TrainingCandidateView,
  TripView,
  UserView,
  VehicleView,
} from "./types";

export const api = {
  appStatus: () => invoke<AppStatus>("app_status"),

  createFirstAdmin: (name: string, password: string) =>
    invoke<LoginResult>("create_first_admin", { name, password }),

  loginPassword: (username: string, password: string) =>
    invoke<LoginResult>("login_password", { username, password }),

  logout: () => invoke<void>("logout"),

  getCurrentUser: () => invoke<SessionUser | null>("get_current_user"),

  getPendingUpgrade: (userId: string) =>
    invoke<PendingUpgradeInfo | null>("get_pending_upgrade", { userId }),

  listPermissions: (userId?: string) =>
    invoke<ListPermissionItem[]>("list_permissions", { userId: userId ?? null }),

  listRolePresets: () => invoke<RolePresetView[]>("list_role_presets"),

  listUsers: () => invoke<UserView[]>("list_users"),

  createUser: (actorId: string, name: string, permissionKeys: string[], initialPassword: string) =>
    invoke<UserView>("create_user", { actorId, name, permissionKeys, initialPassword }),

  setUserPermissions: (actorId: string, userId: string, permissionKeys: string[], actorCredential: string) =>
    invoke<PermissionChangeResult>("set_user_permissions", { actorId, userId, permissionKeys, actorCredential }),

  completeAuthUpgrade: (userId: string, currentCredential: string) =>
    invoke<PermissionChangeResult>("complete_auth_upgrade", { userId, currentCredential }),

  changeOwnCredential: (userId: string, currentCredential: string, newCredential: string) =>
    invoke<void>("change_own_credential", { userId, currentCredential, newCredential }),

  setUserTheme: (userId: string, themeMode: string, themeAccent: string) =>
    invoke<void>("set_user_theme", { userId, themeMode, themeAccent }),

  setUserStatus: (actorId: string, userId: string, status: "active" | "disabled") =>
    invoke<void>("set_user_status", { actorId, userId, status }),

  deleteUser: (actorId: string, userId: string, actorCredential: string) =>
    invoke<void>("delete_user", { actorId, userId, actorCredential }),

  restoreUser: (actorId: string, userId: string) =>
    invoke<void>("restore_user", { actorId, userId }),

  purgeUser: (actorId: string, userId: string, actorCredential: string) =>
    invoke<void>("purge_user", { actorId, userId, actorCredential }),

  resetUserPassword: (actorId: string, userId: string, tempPassword: string, actorCredential: string) =>
    invoke<void>("reset_user_password", { actorId, userId, tempPassword, actorCredential }),

  recoverAdminPassword: (username: string, recoveryCode: string, newPassword: string) =>
    invoke<LoginResult>("recover_admin_password", { username, recoveryCode, newPassword }),

  checkRecoveryCode: (username: string, recoveryCode: string) =>
    invoke<void>("check_recovery_code", { username, recoveryCode }),

  createPasswordResetRequest: (username: string) =>
    invoke<void>("create_password_reset_request", { username }),

  listPasswordResetRequests: (actorId: string) =>
    invoke<PasswordResetRequestView[]>("list_password_reset_requests", { actorId }),

  dismissPasswordResetRequest: (actorId: string, requestId: string) =>
    invoke<void>("dismiss_password_reset_request", { actorId, requestId }),

  getRecoveryCode: (actorId: string) =>
    invoke<RecoveryCodeInfo>("get_recovery_code", { actorId }),

  regenerateRecoveryCode: (actorId: string) =>
    invoke<RecoveryCodeInfo>("regenerate_recovery_code", { actorId }),

  validatePasswordStrength: (password: string) =>
    invoke<PasswordStrength>("validate_password_strength", { password }),

  // --- Reference database (companies / drivers / vehicles) ---

  listCompanies: (search?: string) =>
    invoke<CompanyView[]>("list_companies", { search: search ?? null }),

  createCompany: (actorId: string, name: string, extraFields?: Record<string, unknown>) =>
    invoke<CompanyView>("create_company", {
      actorId,
      name,
      extraFields: extraFields ? JSON.stringify(extraFields) : null,
    }),

  updateCompany: (actorId: string, companyId: string, name: string, extraFields?: Record<string, unknown>) =>
    invoke<void>("update_company", {
      actorId,
      companyId,
      name,
      extraFields: extraFields ? JSON.stringify(extraFields) : null,
    }),

  setCompanyStatus: (actorId: string, companyId: string, status: "active" | "inactive") =>
    invoke<void>("set_company_status", { actorId, companyId, status }),

  listDrivers: (search?: string) =>
    invoke<DriverView[]>("list_drivers", { search: search ?? null }),

  createDriver: (actorId: string, name: string, extraFields?: Record<string, unknown>) =>
    invoke<DriverView>("create_driver", {
      actorId,
      name,
      extraFields: extraFields ? JSON.stringify(extraFields) : null,
    }),

  updateDriver: (actorId: string, driverId: string, name: string, extraFields?: Record<string, unknown>) =>
    invoke<void>("update_driver", {
      actorId,
      driverId,
      name,
      extraFields: extraFields ? JSON.stringify(extraFields) : null,
    }),

  setDriverStatus: (actorId: string, driverId: string, status: "active" | "inactive") =>
    invoke<void>("set_driver_status", { actorId, driverId, status }),

  listVehicles: (search?: string) =>
    invoke<VehicleView[]>("list_vehicles", { search: search ?? null }),

  createVehicle: (
    actorId: string,
    plateNumber: string,
    companyId: string | null,
    registeredCapacity: number | null,
    capacityUnit: string,
    defaultDriverId: string | null,
    extraFields?: Record<string, unknown>,
  ) =>
    invoke<VehicleView>("create_vehicle", {
      actorId,
      plateNumber,
      companyId,
      registeredCapacity,
      capacityUnit,
      defaultDriverId,
      extraFields: extraFields ? JSON.stringify(extraFields) : null,
    }),

  updateVehicle: (
    actorId: string,
    vehicleId: string,
    plateNumber: string,
    companyId: string | null,
    registeredCapacity: number | null,
    capacityUnit: string,
    defaultDriverId: string | null,
    extraFields?: Record<string, unknown>,
  ) =>
    invoke<void>("update_vehicle", {
      actorId,
      vehicleId,
      plateNumber,
      companyId,
      registeredCapacity,
      capacityUnit,
      defaultDriverId,
      extraFields: extraFields ? JSON.stringify(extraFields) : null,
    }),

  setVehicleStatus: (actorId: string, vehicleId: string, status: "active" | "inactive") =>
    invoke<void>("set_vehicle_status", { actorId, vehicleId, status }),

  // --- Dynamic field definitions ---

  listFieldDefinitions: (entityType: string) =>
    invoke<FieldDefinition[]>("list_field_definitions", { entityType }),

  createFieldDefinition: (
    actorId: string,
    entityType: string,
    fieldKey: string,
    fieldLabel: string,
    fieldType: string,
    isRequired: boolean,
    sortOrder?: number,
  ) =>
    invoke<FieldDefinition>("create_field_definition", {
      actorId,
      entityType,
      fieldKey,
      fieldLabel,
      fieldType,
      isRequired,
      sortOrder: sortOrder ?? null,
    }),

  updateFieldDefinition: (
    actorId: string,
    fieldId: string,
    changes: Partial<Pick<FieldDefinition, "field_label" | "field_type" | "is_required" | "sort_order">>,
  ) =>
    invoke<void>("update_field_definition", {
      actorId,
      fieldId,
      fieldLabel: changes.field_label ?? null,
      fieldType: changes.field_type ?? null,
      isRequired: changes.is_required ?? null,
      sortOrder: changes.sort_order ?? null,
    }),

  deleteFieldDefinition: (actorId: string, fieldId: string) =>
    invoke<void>("delete_field_definition", { actorId, fieldId }),

  // --- Capture pipeline ---

  simulateRead: (plate: string, confidence: number) =>
    invoke<IngestResult>("simulate_read", { plate, confidence }),

  manualEntry: (plate: string, officerId: string) =>
    invoke<IngestResult>("manual_entry", { plate, officerId }),

  approveTrip: (tripId: string, officerId: string) =>
    invoke<TripView>("approve_trip", { tripId, officerId }),

  updateTripFields: (
    tripId: string,
    officerId: string,
    companyId: string | null,
    driverId: string | null,
    capacityAtTrip: number | null,
    receiptNo: string | null,
  ) =>
    invoke<TripView>("update_trip_fields", {
      tripId,
      officerId,
      companyId,
      driverId,
      capacityAtTrip,
      receiptNo,
    }),

  listTodayTrips: () => invoke<TripView[]>("list_today_trips"),

  searchTrips: (query: string) => invoke<TripView[]>("search_trips", { query }),

  listQueued: () => invoke<TripView[]>("list_queued"),

  getCaptureSettings: () => invoke<CaptureSettings>("get_capture_settings"),

  setCaptureSettings: (
    actorId: string,
    settings: Partial<
      Pick<CaptureSettings, "consent_mode" | "confidence_threshold" | "anpr_enabled" | "anpr_source">
    >,
  ) =>
    invoke<void>("set_capture_settings", {
      actorId,
      consentMode: settings.consent_mode ?? null,
      confidenceThreshold: settings.confidence_threshold ?? null,
      anprEnabled: settings.anpr_enabled ?? null,
      anprSource: settings.anpr_source ?? null,
    }),

  anprStatus: () => invoke<AnprStatus>("anpr_status"),

  simulatorPushReads: (reads: AnprRead[]) =>
    invoke<number>("simulator_push_reads", { reads }),

  // --- Verification-queue resolution (Phase 3) ---

  resolveQueuedExisting: (
    tripId: string,
    officerId: string,
    vehicleId: string,
    companyId: string | null,
    driverId: string | null,
    capacityAtTrip: number | null,
    capacityUnit: string,
    receiptNo: string | null,
  ) =>
    invoke<TripView>("resolve_queued_existing", {
      tripId,
      officerId,
      vehicleId,
      companyId,
      driverId,
      capacityAtTrip,
      capacityUnit,
      receiptNo,
    }),

  resolveQueuedNew: (
    tripId: string,
    officerId: string,
    plateNumber: string,
    companyId: string | null,
    registeredCapacity: number | null,
    capacityUnit: string,
    defaultDriverId: string | null,
    confirmDuplicatePlate: boolean,
  ) =>
    invoke<TripView>("resolve_queued_new", {
      tripId,
      officerId,
      plateNumber,
      companyId,
      registeredCapacity,
      capacityUnit,
      defaultDriverId,
      confirmDuplicatePlate,
    }),

  discardTrip: (tripId: string, officerId: string) =>
    invoke<TripView>("discard_trip", { tripId, officerId }),

  declineTrip: (tripId: string, officerId: string) =>
    invoke<TripView>("decline_trip", { tripId, officerId }),

  listDeclined: () => invoke<TripView[]>("list_declined"),

  purgeDeclined: (tripId: string, actorId: string) =>
    invoke<void>("purge_declined", { tripId, actorId }),

  classifyDischarge: (tripId: string, officerId: string, isDischarge: boolean) =>
    invoke<TripView>("classify_discharge", { tripId, officerId, isDischarge }),

  tripFrames: (tripId: string) => invoke<FrameEvidence[]>("trip_frames", { tripId }),

  // --- ANPR Engine Configuration (08 §5 / §6, gated on manage_anpr_config) ---

  getAnprConfig: () => invoke<AnprConfigView>("get_anpr_config"),

  updateAnprConfig: (
    actorId: string,
    changes: Partial<
      Pick<
        AnprConfigView,
        | "active_ocr_engine"
        | "confidence_threshold_paddleocr"
        | "confidence_threshold_easyocr"
        | "plate_vehicle_ratio_threshold"
        | "plate_format_rules"
        | "discharge_confirmation_required"
        | "save_recognition_images"
        | "retrain_candidate_threshold"
        | "is_capture_point"
      >
    >,
  ) =>
    invoke<AnprConfigView>("update_anpr_config", {
      actorId,
      activeOcrEngine: changes.active_ocr_engine ?? null,
      confidenceThresholdPaddleocr: changes.confidence_threshold_paddleocr ?? null,
      confidenceThresholdEasyocr: changes.confidence_threshold_easyocr ?? null,
      plateVehicleRatioThreshold: changes.plate_vehicle_ratio_threshold ?? null,
      plateFormatRules: changes.plate_format_rules ?? null,
      dischargeConfirmationRequired: changes.discharge_confirmation_required ?? null,
      saveRecognitionImages: changes.save_recognition_images ?? null,
      retrainCandidateThreshold: changes.retrain_candidate_threshold ?? null,
      isCapturePoint: changes.is_capture_point ?? null,
    }),

  listCameraSources: () => invoke<CameraSourceView[]>("list_camera_sources"),

  addCameraSource: (actorId: string, label: string, sourceType: string, connectionString: string) =>
    invoke<CameraSourceView>("add_camera_source", { actorId, label, sourceType, connectionString }),

  updateCameraSource: (
    actorId: string,
    sourceId: string,
    label: string | null,
    connectionString: string | null,
  ) => invoke<CameraSourceView>("update_camera_source", { actorId, sourceId, label, connectionString }),

  setCameraSourceStatus: (actorId: string, sourceId: string, status: "active" | "inactive") =>
    invoke<CameraSourceView>("set_camera_source_status", { actorId, sourceId, status }),

  listModelVersions: () => invoke<ModelVersionView[]>("list_model_versions"),

  registerModelVersion: (
    actorId: string,
    versionLabel: string,
    component: string,
    validationAccuracy: number | null,
  ) =>
    invoke<ModelVersionView>("register_model_version", { actorId, versionLabel, component, validationAccuracy }),

  deployModelVersion: (actorId: string, versionId: string) =>
    invoke<ModelVersionView>("deploy_model_version", { actorId, versionId }),

  rollbackModelVersion: (actorId: string, versionId: string) =>
    invoke<ModelVersionView>("rollback_model_version", { actorId, versionId }),

  listTrainingCandidates: () => invoke<TrainingCandidateView[]>("list_training_candidates"),

  // --- Sync & integrations (Phase 4, gated on manage_integrations) ---

  syncStatus: () => invoke<SyncStatusView>("sync_status"),

  syncNowPg: (actorId: string) => invoke<SyncRunResult>("sync_now_pg", { actorId }),

  connectGoogleSheets: (
    actorId: string,
    targetSheetId: string | null,
    sharedGroup: string | null,
    syncFrequency: string,
  ) =>
    invoke<SheetsStateView>("connect_google_sheets", {
      actorId,
      targetSheetId,
      sharedGroup,
      syncFrequency,
    }),

  disconnectGoogleSheets: (actorId: string) =>
    invoke<SheetsStateView>("disconnect_google_sheets", { actorId }),

  setGoogleSheetsFrequency: (actorId: string, syncFrequency: string) =>
    invoke<SheetsStateView>("set_google_sheets_frequency", { actorId, syncFrequency }),

  syncNowSheets: (actorId: string) => invoke<SyncRunResult>("sync_now_sheets", { actorId }),

  setSheetsRetention: (actorId: string, days: number | null) =>
    invoke<SheetsStateView>("set_sheets_retention", { actorId, days }),

  setTripRetention: (actorId: string, days: number | null) =>
    invoke<PgSyncStateView>("set_trip_retention", { actorId, days }),

  clearExportedTrips: (actorId: string) =>
    invoke<SheetsStateView>("clear_exported_trips", { actorId }),

  simulateConnectivity: (postgresOnline: boolean, sheetsOnline: boolean) =>
    invoke<void>("simulate_connectivity", { postgresOnline, sheetsOnline }),

  configurePostgres: (actorId: string, connectionString: string) =>
    invoke<SyncStatusView["pg"]>("configure_postgres", { actorId, connectionString }),

  disconnectPostgres: (actorId: string) =>
    invoke<SyncStatusView["pg"]>("disconnect_postgres", { actorId }),

  configureGoogleSheets: (
    actorId: string,
    serviceAccountJson: string,
    targetSheetId: string,
    sharedGroup: string | null,
    syncFrequency: string,
  ) =>
    invoke<SheetsStateView>("configure_google_sheets", {
      actorId,
      serviceAccountJson,
      targetSheetId,
      sharedGroup,
      syncFrequency,
    }),

  // --- Phase 5: Reporting & oversight ---

  reportDashboard: (actorId: string, filters: ReportFilters) =>
    invoke<ReportDashboard>("report_dashboard", { actorId, filters }),

  reportTripsDrill: (actorId: string, filters: ReportFilters, limit: number) =>
    invoke<TripView[]>("report_trips_drill", { actorId, filters, limit }),

  reportExport: (actorId: string, filters: ReportFilters) =>
    invoke<ReportExportRow[]>("report_export", { actorId, filters }),

  reportExportCsv: (actorId: string, filters: ReportFilters, targetPath?: string) =>
    invoke<string>("report_export_csv", { actorId, filters, targetPath: targetPath ?? null }),

  reportExportXlsx: (actorId: string, filters: ReportFilters, targetPath?: string) =>
    invoke<string>("report_export_xlsx", { actorId, filters, targetPath: targetPath ?? null }),

  listAuditLog: (actorId: string, filters: AuditFilters) =>
    invoke<AuditEntry[]>("list_audit_log", { actorId, filters }),

  listAuditActions: (actorId: string) => invoke<string[]>("list_audit_actions_command", { actorId }),

  officerActivity: (actorId: string, from: string | null, to: string | null) =>
    invoke<OfficerActivityView[]>("officer_activity", { actorId, from, to }),

  deleteAuditEntries: (actorId: string, entryIds: string[]) =>
    invoke<number>("delete_audit_entries", { actorId, entryIds }),

  // --- Trip archive management (admin, password-protected) ---

  listRecentTrips: (actorId: string, limit: number) =>
    invoke<TripView[]>("list_recent_trips", { actorId, limit }),

  listArchivedTrips: (actorId: string, filters: ReportFilters) =>
    invoke<TripView[]>("list_archived_trips", { actorId, filters }),

  softDeleteTrips: (actorId: string, tripIds: string[], actorCredential: string) =>
    invoke<number>("soft_delete_trips", { actorId, tripIds, actorCredential }),

  restoreTrips: (actorId: string, tripIds: string[]) =>
    invoke<number>("restore_trips", { actorId, tripIds }),

  hardDeleteTrips: (actorId: string, tripIds: string[], actorCredential: string) =>
    invoke<number>("hard_delete_trips", { actorId, tripIds, actorCredential }),

  purgeLocalTrips: (actorId: string, actorCredential: string) =>
    invoke<number>("purge_local_trips", { actorId, actorCredential }),

  // --- Phase 5: System Monitor ---

  healthDashboard: (actorId: string) => invoke<HealthDashboard>("health_dashboard", { actorId }),

  acknowledgeHealthEvent: (actorId: string, eventId: string) =>
    invoke<HealthEventView>("acknowledge_health_event", { actorId, eventId }),

  deleteHealthEvents: (actorId: string, eventIds: string[]) =>
    invoke<number>("delete_health_events", { actorId, eventIds }),

  // --- Phase 6: Settings / profile / monitor trend ---

  updateOwnProfile: (userId: string, phoneNumber: string | null, languagePreference: string | null, notificationSound: boolean) =>
    invoke<void>("update_own_profile", { userId, phoneNumber, languagePreference, notificationSound }),

  setProfilePhoto: (userId: string, imageBase64: string | null) =>
    invoke<void>("set_profile_photo", { userId, imageBase64 }),

  getProfilePhoto: (userId: string) => invoke<string | null>("get_profile_photo", { userId }),

  anprConfidenceTrend: (actorId: string, from: string | null, to: string | null) =>
    invoke<ConfidenceTrendPoint[]>("anpr_confidence_trend", { actorId, from, to }),
};
