-- TruckFlow Cloud Setup Script
-- Run in Supabase Dashboard → SQL Editor → New Query
-- Creates the full cloud schema mirroring the local SQLite.
-- All sync-able tables carry `synced` and `updated_at` columns.

-- ============================================================
-- CORE REFERENCE DATA (must exist before trips/vehicles)
-- ============================================================

-- Organizations (the account owner; one per project. NOT a client company)
CREATE TABLE IF NOT EXISTS public.organizations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);

-- Companies (CLIENT companies only -- trucks that discharge trips)
CREATE TABLE IF NOT EXISTS public.companies (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    extra_fields TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);

-- Drivers
CREATE TABLE IF NOT EXISTS public.drivers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    extra_fields TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);

-- Vehicles
CREATE TABLE IF NOT EXISTS public.vehicles (
    id TEXT PRIMARY KEY,
    plate_number TEXT NOT NULL,
    company_id TEXT REFERENCES public.companies(id),
    registered_capacity REAL,
    default_driver_id TEXT REFERENCES public.drivers(id),
    status TEXT NOT NULL DEFAULT 'active',
    extra_fields TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0,
    capacity_unit TEXT NOT NULL DEFAULT 'litres'
);
CREATE INDEX IF NOT EXISTS idx_vehicles_plate ON public.vehicles(plate_number);

-- Users
CREATE TABLE IF NOT EXISTS public.users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    auth_type TEXT NOT NULL,
    credential_hash TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    revoked_by TEXT REFERENCES public.users(id),
    revoked_at TEXT,
    profile_photo_ref TEXT,
    phone_number TEXT,
    theme_mode TEXT DEFAULT 'light',
    theme_accent TEXT,
    language_preference TEXT,
    notification_sound INTEGER NOT NULL DEFAULT 1,
    must_change_password INTEGER NOT NULL DEFAULT 0,
    company_id TEXT REFERENCES public.companies(id),
    organization_id TEXT REFERENCES public.organizations(id),
    failed_login_attempts INTEGER NOT NULL DEFAULT 0,
    locked_until TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);

-- Permissions
CREATE TABLE IF NOT EXISTS public.permissions (
    id TEXT PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    min_auth_level TEXT NOT NULL,
    description TEXT,
    synced INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT now()
);

-- User Permissions
CREATE TABLE IF NOT EXISTS public.user_permissions (
    user_id TEXT REFERENCES public.users(id),
    permission_id TEXT REFERENCES public.permissions(id),
    granted_by TEXT REFERENCES public.users(id),
    granted_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, permission_id)
);

-- Role Presets
CREATE TABLE IF NOT EXISTS public.role_presets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    permission_ids TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT now()
);

-- ============================================================
-- TRIPS
-- ============================================================

CREATE TABLE IF NOT EXISTS public.trips (
    id TEXT PRIMARY KEY,
    vehicle_id TEXT REFERENCES public.vehicles(id),
    driver_id TEXT REFERENCES public.drivers(id),
    company_id TEXT,
    capacity_at_trip REAL,
    time_in TEXT NOT NULL,
    receipt_no TEXT,
    officer_id TEXT REFERENCES public.users(id),
    capture_method TEXT NOT NULL DEFAULT 'auto',
    confidence_score REAL,
    photo_refs TEXT,
    status TEXT NOT NULL DEFAULT 'logged',
    resolution_notes TEXT,
    pushed_to_sheets INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0,
    is_discharge_trip INTEGER,
    model_version TEXT,
    ocr_engine TEXT,
    capacity_unit TEXT NOT NULL DEFAULT 'litres',
    entry_time TEXT,
    exit_time TEXT,
    trip_status TEXT DEFAULT 'complete',
    entry_photo_refs TEXT,
    exit_photo_refs TEXT,
    sheet_row INTEGER,
    sheet_exit_pushed INTEGER DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_trips_time_in ON public.trips(time_in);
CREATE INDEX IF NOT EXISTS idx_trips_status ON public.trips(status);
CREATE INDEX IF NOT EXISTS idx_trips_entry_time ON public.trips(entry_time);
CREATE INDEX IF NOT EXISTS idx_trips_trip_status ON public.trips(trip_status);

-- ============================================================
-- REFERENCE / CONFIG TABLES (org-specific, synced across PCs)
-- ============================================================

-- Field Definitions — the org's schema config. Different orgs have
-- different field names, types, labels. This is the single source of
-- truth for what fields exist on each entity type.
CREATE TABLE IF NOT EXISTS public.field_definitions (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    field_key TEXT NOT NULL,
    field_label TEXT NOT NULL,
    field_type TEXT NOT NULL DEFAULT 'text',
    is_required INTEGER NOT NULL DEFAULT 0,
    sort_order INTEGER NOT NULL DEFAULT 0,
    is_standard INTEGER NOT NULL DEFAULT 0,
    is_hidden INTEGER NOT NULL DEFAULT 0,
    binding TEXT,
    field_unit TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_field_definitions_unique
    ON public.field_definitions(entity_type, field_key);

-- Audit Log
CREATE TABLE IF NOT EXISTS public.audit_log (
    id TEXT PRIMARY KEY,
    actor_id TEXT REFERENCES public.users(id),
    action TEXT NOT NULL,
    target_id TEXT,
    details TEXT,
    timestamp TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT now()
);

-- System Health Events
CREATE TABLE IF NOT EXISTS public.system_health_events (
    id TEXT PRIMARY KEY,
    component TEXT NOT NULL,
    status TEXT NOT NULL,
    detected_at TEXT NOT NULL,
    acknowledged_by TEXT REFERENCES public.users(id),
    resolved_at TEXT,
    detail TEXT,
    acknowledged_at TEXT,
    synced INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT now()
);

-- Integrations (Google Sheets, etc.)
CREATE TABLE IF NOT EXISTS public.integrations (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    connected_by TEXT REFERENCES public.users(id),
    oauth_token_ref TEXT,
    target_sheet_id TEXT,
    shared_group TEXT,
    sync_frequency TEXT NOT NULL DEFAULT 'realtime',
    last_synced_at TEXT,
    status TEXT NOT NULL DEFAULT 'disconnected',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);

-- ANPR Config
CREATE TABLE IF NOT EXISTS public.anpr_config (
    id TEXT PRIMARY KEY,
    active_ocr_engine TEXT NOT NULL DEFAULT 'paddleocr',
    confidence_threshold_paddleocr REAL NOT NULL DEFAULT 0.7,
    confidence_threshold_easyocr REAL NOT NULL DEFAULT 0.7,
    plate_vehicle_ratio_threshold REAL NOT NULL DEFAULT 0.05,
    discharge_confirmation_required INTEGER NOT NULL DEFAULT 1,
    plate_format_rules TEXT,
    save_recognition_images INTEGER NOT NULL DEFAULT 1,
    retrain_candidate_threshold INTEGER,
    updated_by TEXT REFERENCES public.users(id),
    extra_fields TEXT,
    detection_method TEXT NOT NULL DEFAULT 'contour',
    prefer_cloud INTEGER NOT NULL DEFAULT 0,
    is_capture_point INTEGER NOT NULL DEFAULT 0,
    max_pending_duration_hours REAL,
    designated_machine_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);

-- Camera Sources
CREATE TABLE IF NOT EXISTS public.camera_sources (
    id TEXT PRIMARY KEY,
    label TEXT NOT NULL,
    source_type TEXT NOT NULL,
    connection_string TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    last_connection_check_at TEXT,
    last_connection_check_result TEXT,
    extra_fields TEXT,
    camera_role TEXT,
    designated_machine_id TEXT,
    is_capture_point INTEGER NOT NULL DEFAULT 0,
    max_pending_duration_hours REAL,
    tracked INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);

-- Model Versions
CREATE TABLE IF NOT EXISTS public.model_versions (
    id TEXT PRIMARY KEY,
    version_label TEXT NOT NULL,
    component TEXT NOT NULL,
    validation_accuracy REAL,
    is_live INTEGER NOT NULL DEFAULT 0,
    deployed_by TEXT REFERENCES public.users(id),
    deployed_at TEXT,
    rolled_back_from TEXT REFERENCES public.model_versions(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_model_versions_component ON public.model_versions(component);

-- Training Candidates
CREATE TABLE IF NOT EXISTS public.training_candidates (
    id TEXT PRIMARY KEY,
    source_trip_id TEXT REFERENCES public.trips(id),
    frame_ref TEXT NOT NULL,
    reason TEXT NOT NULL,
    plate_number TEXT,
    confidence REAL,
    ocr_engine TEXT,
    captured_at TEXT,
    capture_method TEXT,
    used_in_model_version_id TEXT REFERENCES public.model_versions(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0
);

-- ============================================================
-- APP SETTINGS (per-device key-value, NOT synced)
-- ============================================================

CREATE TABLE IF NOT EXISTS public.app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ============================================================
-- TWO-WAY SYNC INFRASTRUCTURE
-- ============================================================

-- Sync Log: every create/update/delete on a synced table is logged here.
-- This is the change-tracking backbone for two-way sync, offline replay,
-- and conflict detection across PCs.
CREATE TABLE IF NOT EXISTS public.sync_log (
    id TEXT PRIMARY KEY,
    table_name TEXT NOT NULL,
    record_id TEXT NOT NULL,
    operation TEXT NOT NULL,
    payload TEXT,
    synced INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_sync_log_table ON public.sync_log(table_name);
CREATE INDEX IF NOT EXISTS idx_sync_log_record ON public.sync_log(table_name, record_id);
CREATE INDEX IF NOT EXISTS idx_sync_log_pending ON public.sync_log(synced, created_at);

-- PC Identity: each PC registers itself so we know who made what change.
CREATE TABLE IF NOT EXISTS public.pc_identity (
    pc_id TEXT PRIMARY KEY,
    hostname TEXT,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
);

-- Offline Queue: local changes that couldn't be pushed yet.
-- Replayed when connectivity returns. Also lives locally in SQLite.
CREATE TABLE IF NOT EXISTS public.offline_queue (
    id TEXT PRIMARY KEY,
    sync_log_id TEXT NOT NULL REFERENCES public.sync_log(id),
    operation TEXT NOT NULL,
    table_name TEXT NOT NULL,
    record_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    status TEXT NOT NULL DEFAULT 'pending'
);
CREATE INDEX IF NOT EXISTS idx_offline_queue_status ON public.offline_queue(status, created_at);

-- ============================================================
-- FK CONSTRAINTS — DEFERRABLE for sync
-- ============================================================

DO $$
BEGIN
  -- Drop and recreate FK constraints as DEFERRABLE so child records
  -- can be synced before parents without FK errors.
  BEGIN ALTER TABLE public.trips DROP CONSTRAINT IF EXISTS trips_vehicle_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public.trips DROP CONSTRAINT IF EXISTS trips_driver_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public.trips DROP CONSTRAINT IF EXISTS trips_officer_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public.vehicles DROP CONSTRAINT IF EXISTS vehicles_company_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public.vehicles DROP CONSTRAINT IF EXISTS vehicles_default_driver_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public.user_permissions DROP CONSTRAINT IF EXISTS user_permissions_user_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public.user_permissions DROP CONSTRAINT IF EXISTS user_permissions_permission_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;

  -- Recreate as DEFERRABLE
  ALTER TABLE public.trips ADD CONSTRAINT trips_vehicle_id_fkey FOREIGN KEY (vehicle_id) REFERENCES public.vehicles(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.trips ADD CONSTRAINT trips_driver_id_fkey FOREIGN KEY (driver_id) REFERENCES public.drivers(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.trips ADD CONSTRAINT trips_officer_id_fkey FOREIGN KEY (officer_id) REFERENCES public.users(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.vehicles ADD CONSTRAINT vehicles_company_id_fkey FOREIGN KEY (company_id) REFERENCES public.companies(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.vehicles ADD CONSTRAINT vehicles_default_driver_id_fkey FOREIGN KEY (default_driver_id) REFERENCES public.drivers(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.user_permissions ADD CONSTRAINT user_permissions_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.user_permissions ADD CONSTRAINT user_permissions_permission_id_fkey FOREIGN KEY (permission_id) REFERENCES public.permissions(id) DEFERRABLE INITIALLY DEFERRED;
END $$;

-- ============================================================
-- ACCESS & SCHEMA RELOAD
-- ============================================================

GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO anon;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO authenticated;

-- Notify PostgREST to reload schema cache (required after DDL via Management API)
CREATE OR REPLACE FUNCTION public.notify_pgrst_cache_needs_refresh()
RETURNS void LANGUAGE plpgsql SECURITY DEFINER AS $$
BEGIN
  NOTIFY pgrst, 'reload schema cache';
END;
$$;

GRANT EXECUTE ON FUNCTION public.notify_pgrst_cache_needs_refresh() TO anon, authenticated;

-- Done!
