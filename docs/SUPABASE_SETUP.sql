-- TruckFlow Supabase Setup Script
-- Run this in Supabase Dashboard → SQL Editor → New Query
-- This creates all the tables needed for sync to work

-- Companies
CREATE TABLE IF NOT EXISTS public.companies (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    extra_fields TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Drivers
CREATE TABLE IF NOT EXISTS public.drivers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    extra_fields TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
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
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Trips
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
    sheet_exit_pushed INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_trips_time_in ON public.trips(time_in);
CREATE INDEX IF NOT EXISTS idx_trips_status ON public.trips(status);

-- Permissions
CREATE TABLE IF NOT EXISTS public.permissions (
    id TEXT PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    min_auth_level TEXT NOT NULL,
    description TEXT
);

-- User Permissions
CREATE TABLE IF NOT EXISTS public.user_permissions (
    user_id TEXT REFERENCES public.users(id),
    permission_id TEXT REFERENCES public.permissions(id),
    granted_by TEXT REFERENCES public.users(id),
    granted_at TEXT NOT NULL,
    PRIMARY KEY (user_id, permission_id)
);

-- Role Presets
CREATE TABLE IF NOT EXISTS public.role_presets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    permission_ids TEXT NOT NULL
);

-- Audit Log
CREATE TABLE IF NOT EXISTS public.audit_log (
    id TEXT PRIMARY KEY,
    actor_id TEXT REFERENCES public.users(id),
    action TEXT NOT NULL,
    target_id TEXT,
    details TEXT,
    created_at TEXT NOT NULL
);

-- Integrations
CREATE TABLE IF NOT EXISTS public.integrations (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    config TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- App Settings
CREATE TABLE IF NOT EXISTS public.app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ANPR Config
CREATE TABLE IF NOT EXISTS public.anpr_config (
    id TEXT PRIMARY KEY,
    camera_id TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    min_confidence REAL DEFAULT 0.7,
    cooldown_seconds INTEGER DEFAULT 30,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Field Definitions
CREATE TABLE IF NOT EXISTS public.field_definitions (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    field_key TEXT NOT NULL,
    display_label TEXT NOT NULL,
    field_type TEXT NOT NULL,
    required INTEGER NOT NULL DEFAULT 0,
    options TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

-- Drop existing FK constraints and recreate as DEFERRABLE
-- This allows syncing child records before parents without FK errors
DO $$
BEGIN
  -- Drop existing FK constraints
  ALTER TABLE public.trips DROP CONSTRAINT IF EXISTS trips_vehicle_id_fkey;
  ALTER TABLE public.trips DROP CONSTRAINT IF EXISTS trips_driver_id_fkey;
  ALTER TABLE public.trips DROP CONSTRAINT IF EXISTS trips_company_id_fkey;
  ALTER TABLE public.trips DROP CONSTRAINT IF EXISTS trips_officer_id_fkey;
  ALTER TABLE public.vehicles DROP CONSTRAINT IF EXISTS vehicles_company_id_fkey;
  ALTER TABLE public.vehicles DROP CONSTRAINT IF EXISTS vehicles_default_driver_id_fkey;
  ALTER TABLE public.user_permissions DROP CONSTRAINT IF EXISTS user_permissions_user_id_fkey;
  ALTER TABLE public.user_permissions DROP CONSTRAINT IF EXISTS user_permissions_permission_id_fkey;

  -- Recreate as DEFERRABLE
  ALTER TABLE public.trips ADD CONSTRAINT trips_vehicle_id_fkey FOREIGN KEY (vehicle_id) REFERENCES public.vehicles(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.trips ADD CONSTRAINT trips_driver_id_fkey FOREIGN KEY (driver_id) REFERENCES public.drivers(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.trips ADD CONSTRAINT trips_company_id_fkey FOREIGN KEY (company_id) REFERENCES public.companies(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.trips ADD CONSTRAINT trips_officer_id_fkey FOREIGN KEY (officer_id) REFERENCES public.users(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.vehicles ADD CONSTRAINT vehicles_company_id_fkey FOREIGN KEY (company_id) REFERENCES public.companies(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.vehicles ADD CONSTRAINT vehicles_default_driver_id_fkey FOREIGN KEY (default_driver_id) REFERENCES public.drivers(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.user_permissions ADD CONSTRAINT user_permissions_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public.user_permissions ADD CONSTRAINT user_permissions_permission_id_fkey FOREIGN KEY (permission_id) REFERENCES public.permissions(id) DEFERRABLE INITIALLY DEFERRED;
END $$;

-- Grant access to authenticated and anon roles
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

-- Done! You should see "Success. No rows returned" in Supabase
