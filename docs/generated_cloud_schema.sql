-- TruckFlow Cloud Schema (auto-generated from local SQLite)
-- Run in Supabase Dashboard → SQL Editor → New Query

CREATE TABLE IF NOT EXISTS public."anpr_config" (
    "id" TEXT NOT NULL,
    "active_ocr_engine" TEXT NOT NULL DEFAULT 'paddleocr',
    "confidence_threshold_paddleocr" DOUBLE PRECISION NOT NULL DEFAULT 0.7,
    "confidence_threshold_easyocr" DOUBLE PRECISION NOT NULL DEFAULT 0.7,
    "plate_vehicle_ratio_threshold" DOUBLE PRECISION NOT NULL DEFAULT 0.05,
    "discharge_confirmation_required" INTEGER NOT NULL DEFAULT 1,
    "plate_format_rules" TEXT,
    "save_recognition_images" INTEGER NOT NULL DEFAULT 1,
    "retrain_candidate_threshold" BIGINT,
    "updated_by" TEXT,
    "extra_fields" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    "is_capture_point" INTEGER NOT NULL DEFAULT 0,
    "max_pending_duration_hours" DOUBLE PRECISION,
    "designated_machine_id" TEXT,
    "prefer_cloud" INTEGER NOT NULL DEFAULT 0,
    "detection_method" TEXT NOT NULL DEFAULT 'contour',
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_anpr_config_1 ON public."anpr_config" ("id");
CREATE TABLE IF NOT EXISTS public."app_settings" (
    "key" TEXT NOT NULL,
    "value" TEXT NOT NULL,
    PRIMARY KEY ("key")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_app_settings_1 ON public."app_settings" ("key");
CREATE TABLE IF NOT EXISTS public."audit_log" (
    "id" TEXT NOT NULL,
    "actor_id" TEXT,
    "action" TEXT NOT NULL,
    "target_id" TEXT,
    "details" TEXT,
    "timestamp" TEXT NOT NULL,
    "synced" INTEGER NOT NULL DEFAULT 0,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_audit_log_1 ON public."audit_log" ("id");
CREATE TABLE IF NOT EXISTS public."camera_sources" (
    "id" TEXT NOT NULL,
    "label" TEXT NOT NULL,
    "source_type" TEXT NOT NULL,
    "connection_string" TEXT NOT NULL,
    "status" TEXT NOT NULL DEFAULT 'active',
    "last_connection_check_at" TEXT,
    "last_connection_check_result" TEXT,
    "extra_fields" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    "camera_role" TEXT,
    "redundant_of_camera_id" TEXT,
    "tracked" INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_camera_sources_1 ON public."camera_sources" ("id");
CREATE TABLE IF NOT EXISTS public."companies" (
    "id" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "status" TEXT NOT NULL DEFAULT 'active',
    "extra_fields" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_companies_1 ON public."companies" ("id");
CREATE TABLE IF NOT EXISTS public."drivers" (
    "id" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "status" TEXT NOT NULL DEFAULT 'active',
    "extra_fields" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_drivers_1 ON public."drivers" ("id");
CREATE TABLE IF NOT EXISTS public."field_definitions" (
    "id" TEXT NOT NULL,
    "entity_type" TEXT NOT NULL,
    "field_key" TEXT NOT NULL,
    "field_label" TEXT NOT NULL,
    "field_type" TEXT NOT NULL DEFAULT 'text',
    "is_required" BIGINT NOT NULL DEFAULT 0,
    "sort_order" BIGINT NOT NULL DEFAULT 0,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "is_standard" BIGINT NOT NULL DEFAULT 0,
    "is_hidden" BIGINT NOT NULL DEFAULT 0,
    "binding" TEXT,
    "field_unit" TEXT,
    "synced" INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_field_definitions_unique ON public."field_definitions" ("entity_type", "field_key");
CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_field_definitions_1 ON public."field_definitions" ("id");
CREATE TABLE IF NOT EXISTS public."integrations" (
    "id" TEXT NOT NULL,
    "type" TEXT NOT NULL,
    "connected_by" TEXT,
    "oauth_token_ref" TEXT,
    "target_sheet_id" TEXT,
    "shared_group" TEXT,
    "sync_frequency" TEXT NOT NULL DEFAULT 'realtime',
    "last_synced_at" TEXT,
    "status" TEXT NOT NULL DEFAULT 'disconnected',
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_integrations_1 ON public."integrations" ("id");
CREATE TABLE IF NOT EXISTS public."model_versions" (
    "id" TEXT NOT NULL,
    "version_label" TEXT NOT NULL,
    "component" TEXT NOT NULL,
    "validation_accuracy" DOUBLE PRECISION,
    "is_live" INTEGER NOT NULL DEFAULT 0,
    "deployed_by" TEXT,
    "deployed_at" TEXT,
    "rolled_back_from" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_model_versions_one_live ON public."model_versions" ("component");
CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_model_versions_1 ON public."model_versions" ("id");
CREATE TABLE IF NOT EXISTS public."offline_queue" (
    "id" TEXT NOT NULL,
    "sync_log_id" TEXT NOT NULL,
    "operation" TEXT NOT NULL,
    "table_name" TEXT NOT NULL,
    "record_id" TEXT NOT NULL,
    "payload" TEXT NOT NULL,
    "created_at" TEXT NOT NULL,
    "retry_count" BIGINT NOT NULL DEFAULT 0,
    "last_error" TEXT,
    "status" TEXT NOT NULL DEFAULT 'pending',
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_offline_queue_1 ON public."offline_queue" ("id");
CREATE TABLE IF NOT EXISTS public."pc_identity" (
    "pc_id" TEXT NOT NULL,
    "hostname" TEXT,
    "first_seen_at" TEXT NOT NULL,
    "last_seen_at" TEXT NOT NULL,
    PRIMARY KEY ("pc_id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_pc_identity_1 ON public."pc_identity" ("pc_id");
CREATE TABLE IF NOT EXISTS public."permissions" (
    "id" TEXT NOT NULL,
    "key" TEXT NOT NULL,
    "min_auth_level" TEXT NOT NULL,
    "description" TEXT,
    "synced" INTEGER NOT NULL DEFAULT 0,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_permissions_2 ON public."permissions" ("key");
CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_permissions_1 ON public."permissions" ("id");
CREATE TABLE IF NOT EXISTS public."role_presets" (
    "id" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "permission_ids" TEXT NOT NULL,
    "synced" INTEGER NOT NULL DEFAULT 0,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_role_presets_1 ON public."role_presets" ("id");
CREATE TABLE IF NOT EXISTS public."sync_log" (
    "id" TEXT NOT NULL,
    "table_name" TEXT NOT NULL,
    "record_id" TEXT NOT NULL,
    "operation" TEXT NOT NULL,
    "payload" TEXT,
    "pc_id" TEXT NOT NULL,
    "created_at" TEXT NOT NULL,
    "synced_to_cloud" BIGINT NOT NULL DEFAULT 0,
    "synced_to_local" BIGINT NOT NULL DEFAULT 0,
    "conflict_resolved" BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_sync_log_1 ON public."sync_log" ("id");
CREATE TABLE IF NOT EXISTS public."system_health_events" (
    "id" TEXT NOT NULL,
    "component" TEXT NOT NULL,
    "status" TEXT NOT NULL,
    "detected_at" TEXT NOT NULL,
    "acknowledged_by" TEXT,
    "resolved_at" TEXT,
    "detail" TEXT,
    "acknowledged_at" TEXT,
    "synced" INTEGER NOT NULL DEFAULT 0,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_system_health_events_1 ON public."system_health_events" ("id");
CREATE TABLE IF NOT EXISTS public."training_candidates" (
    "id" TEXT NOT NULL,
    "source_trip_id" TEXT,
    "frame_ref" TEXT NOT NULL,
    "reason" TEXT NOT NULL,
    "used_in_model_version_id" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_training_candidates_1 ON public."training_candidates" ("id");
CREATE TABLE IF NOT EXISTS public."trips" (
    "id" TEXT NOT NULL,
    "vehicle_id" TEXT,
    "driver_id" TEXT,
    "company_id" TEXT,
    "capacity_at_trip" DOUBLE PRECISION,
    "time_in" TEXT NOT NULL,
    "receipt_no" TEXT,
    "officer_id" TEXT,
    "capture_method" TEXT NOT NULL DEFAULT 'auto',
    "confidence_score" DOUBLE PRECISION,
    "photo_refs" TEXT,
    "status" TEXT NOT NULL DEFAULT 'logged',
    "resolution_notes" TEXT,
    "pushed_to_sheets" INTEGER NOT NULL DEFAULT 0,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    "is_discharge_trip" INTEGER,
    "model_version" TEXT,
    "ocr_engine" TEXT,
    "capacity_unit" TEXT NOT NULL DEFAULT 'litres',
    "archived" INTEGER NOT NULL DEFAULT 0,
    "entry_time" TEXT,
    "exit_time" TEXT,
    "trip_status" TEXT NOT NULL DEFAULT 'complete',
    "entry_photo_refs" TEXT,
    "exit_photo_refs" TEXT,
    "sheet_row" BIGINT,
    "sheet_exit_pushed" INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_trips_1 ON public."trips" ("id");
CREATE TABLE IF NOT EXISTS public."user_permissions" (
    "user_id" TEXT NOT NULL,
    "permission_id" TEXT NOT NULL,
    "granted_by" TEXT,
    "granted_at" TEXT NOT NULL,
    "synced" INTEGER NOT NULL DEFAULT 0,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    PRIMARY KEY ("user_id", "permission_id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_user_permissions_1 ON public."user_permissions" ("user_id", "permission_id");
CREATE TABLE IF NOT EXISTS public."users" (
    "id" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "auth_type" TEXT NOT NULL,
    "credential_hash" TEXT NOT NULL,
    "status" TEXT NOT NULL DEFAULT 'active',
    "revoked_by" TEXT,
    "revoked_at" TEXT,
    "profile_photo_ref" TEXT,
    "phone_number" TEXT,
    "theme_mode" TEXT DEFAULT 'light',
    "theme_accent" TEXT,
    "language_preference" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    "notification_sound" INTEGER NOT NULL DEFAULT 1,
    "must_change_password" INTEGER NOT NULL DEFAULT 0,
    "failed_login_attempts" BIGINT NOT NULL DEFAULT 0,
    "locked_until" TEXT,
    "company_id" TEXT,
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_username_company ON public."users" ("name", "company_id");
CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_users_2 ON public."users" ("name");
CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_users_1 ON public."users" ("id");
CREATE TABLE IF NOT EXISTS public."vehicles" (
    "id" TEXT NOT NULL,
    "plate_number" TEXT NOT NULL,
    "company_id" TEXT,
    "registered_capacity" DOUBLE PRECISION,
    "default_driver_id" TEXT,
    "status" TEXT NOT NULL DEFAULT 'active',
    "extra_fields" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL DEFAULT (now()),
    "synced" INTEGER NOT NULL DEFAULT 0,
    "capacity_unit" TEXT NOT NULL DEFAULT 'litres',
    PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX IF NOT EXISTS sqlite_autoindex_vehicles_1 ON public."vehicles" ("id");

-- ============================================================
-- FK CONSTRAINTS (DEFERRABLE for sync ordering)
-- ============================================================

DO $$
BEGIN
  BEGIN ALTER TABLE public."anpr_config" DROP CONSTRAINT IF EXISTS anpr_config_updated_by_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."audit_log" DROP CONSTRAINT IF EXISTS audit_log_actor_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."integrations" DROP CONSTRAINT IF EXISTS integrations_connected_by_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."model_versions" DROP CONSTRAINT IF EXISTS model_versions_rolled_back_from_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."model_versions" DROP CONSTRAINT IF EXISTS model_versions_deployed_by_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."offline_queue" DROP CONSTRAINT IF EXISTS offline_queue_sync_log_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."system_health_events" DROP CONSTRAINT IF EXISTS system_health_events_acknowledged_by_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."training_candidates" DROP CONSTRAINT IF EXISTS training_candidates_used_in_model_version_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."training_candidates" DROP CONSTRAINT IF EXISTS training_candidates_source_trip_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."trips" DROP CONSTRAINT IF EXISTS trips_officer_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."trips" DROP CONSTRAINT IF EXISTS trips_driver_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."trips" DROP CONSTRAINT IF EXISTS trips_vehicle_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."user_permissions" DROP CONSTRAINT IF EXISTS user_permissions_granted_by_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."user_permissions" DROP CONSTRAINT IF EXISTS user_permissions_permission_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."user_permissions" DROP CONSTRAINT IF EXISTS user_permissions_user_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."users" DROP CONSTRAINT IF EXISTS users_company_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."users" DROP CONSTRAINT IF EXISTS users_revoked_by_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."vehicles" DROP CONSTRAINT IF EXISTS vehicles_default_driver_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;
  BEGIN ALTER TABLE public."vehicles" DROP CONSTRAINT IF EXISTS vehicles_company_id_fkey; EXCEPTION WHEN OTHERS THEN NULL; END;

  ALTER TABLE public."anpr_config" ADD CONSTRAINT anpr_config_updated_by_fkey FOREIGN KEY ("updated_by") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."audit_log" ADD CONSTRAINT audit_log_actor_id_fkey FOREIGN KEY ("actor_id") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."integrations" ADD CONSTRAINT integrations_connected_by_fkey FOREIGN KEY ("connected_by") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."model_versions" ADD CONSTRAINT model_versions_rolled_back_from_fkey FOREIGN KEY ("rolled_back_from") REFERENCES public."model_versions" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."model_versions" ADD CONSTRAINT model_versions_deployed_by_fkey FOREIGN KEY ("deployed_by") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."offline_queue" ADD CONSTRAINT offline_queue_sync_log_id_fkey FOREIGN KEY ("sync_log_id") REFERENCES public."sync_log" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."system_health_events" ADD CONSTRAINT system_health_events_acknowledged_by_fkey FOREIGN KEY ("acknowledged_by") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."training_candidates" ADD CONSTRAINT training_candidates_used_in_model_version_id_fkey FOREIGN KEY ("used_in_model_version_id") REFERENCES public."model_versions" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."training_candidates" ADD CONSTRAINT training_candidates_source_trip_id_fkey FOREIGN KEY ("source_trip_id") REFERENCES public."trips" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."trips" ADD CONSTRAINT trips_officer_id_fkey FOREIGN KEY ("officer_id") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."trips" ADD CONSTRAINT trips_driver_id_fkey FOREIGN KEY ("driver_id") REFERENCES public."drivers" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."trips" ADD CONSTRAINT trips_vehicle_id_fkey FOREIGN KEY ("vehicle_id") REFERENCES public."vehicles" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."user_permissions" ADD CONSTRAINT user_permissions_granted_by_fkey FOREIGN KEY ("granted_by") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."user_permissions" ADD CONSTRAINT user_permissions_permission_id_fkey FOREIGN KEY ("permission_id") REFERENCES public."permissions" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."user_permissions" ADD CONSTRAINT user_permissions_user_id_fkey FOREIGN KEY ("user_id") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."users" ADD CONSTRAINT users_company_id_fkey FOREIGN KEY ("company_id") REFERENCES public."companies" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."users" ADD CONSTRAINT users_revoked_by_fkey FOREIGN KEY ("revoked_by") REFERENCES public."users" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."vehicles" ADD CONSTRAINT vehicles_default_driver_id_fkey FOREIGN KEY ("default_driver_id") REFERENCES public."drivers" ("id") DEFERRABLE INITIALLY DEFERRED;
  ALTER TABLE public."vehicles" ADD CONSTRAINT vehicles_company_id_fkey FOREIGN KEY ("company_id") REFERENCES public."companies" ("id") DEFERRABLE INITIALLY DEFERRED;
END $$;

-- ============================================================
-- INDEXES
-- ============================================================

CREATE INDEX IF NOT EXISTS idx_vehicles_plate ON public."vehicles" ("plate_number");
CREATE INDEX IF NOT EXISTS idx_trips_time_in ON public."trips" ("time_in");
CREATE INDEX IF NOT EXISTS idx_trips_status ON public."trips" ("status");
CREATE INDEX IF NOT EXISTS idx_trips_entry_time ON public."trips" ("entry_time");
CREATE INDEX IF NOT EXISTS idx_trips_trip_status ON public."trips" ("trip_status");
CREATE INDEX IF NOT EXISTS idx_model_versions_component ON public."model_versions" ("component");
CREATE INDEX IF NOT EXISTS idx_sync_log_table ON public."sync_log" ("table_name");
CREATE INDEX IF NOT EXISTS idx_training_candidates_trip ON public."training_candidates" ("source_trip_id");

-- ============================================================
-- ACCESS & SCHEMA RELOAD
-- ============================================================

GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO anon;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO authenticated;

CREATE OR REPLACE FUNCTION public.notify_pgrst_cache_needs_refresh()
RETURNS void LANGUAGE plpgsql SECURITY DEFINER AS $$
BEGIN
  NOTIFY pgrst, 'reload schema cache';
END;
$$;

GRANT EXECUTE ON FUNCTION public.notify_pgrst_cache_needs_refresh() TO anon, authenticated;

-- ============================================================
-- USER PROFILES VIEW (read-only, service_role only — not granted to anon)
-- ============================================================

CREATE OR REPLACE VIEW public.user_profiles AS
SELECT
  u.id AS user_id,
  u.name AS user_name,
  u.status,
  u.auth_type,
  u.company_id,
  c.name AS company_name,
  u.must_change_password,
  u.created_at,
  rp.name AS role_name,
  (
    SELECT json_agg(json_build_object(
      'key', p.key,
      'description', p.description,
      'min_auth_level', p.min_auth_level,
      'granted_at', up.granted_at,
      'granted_by', up.granted_by
    ) ORDER BY p.key)
    FROM public.user_permissions up
    LEFT JOIN public.permissions p ON p.id = up.permission_id
    WHERE up.user_id = u.id
  ) AS permissions
FROM public.users u
LEFT JOIN public.companies c ON c.id = u.company_id
LEFT JOIN public.role_presets rp ON (
  -- Set equality regardless of key order: normalize both sides
  SELECT COALESCE(string_agg(DISTINCT trim(k.key), ',' ORDER BY trim(k.key)), '')
  FROM unnest(string_to_array(rp.permission_ids, ',')) AS k(key)
) = (
  SELECT COALESCE(string_agg(DISTINCT p3.key, ',' ORDER BY p3.key), '')
  FROM public.user_permissions up3
  JOIN public.permissions p3 ON p3.id = up3.permission_id
  WHERE up3.user_id = u.id
);



-- ============================================================
-- ROW LEVEL SECURITY (lockdown)
-- The app uses the service_role key, which bypasses RLS.
-- Enabling RLS with no policies denies the public `anon` key
-- so your data is not readable by anyone holding the project URL.
-- ============================================================
DO $$
DECLARE t text;
BEGIN
  FOR t IN SELECT tablename FROM pg_tables WHERE schemaname = 'public'
  LOOP
    EXECUTE format('ALTER TABLE public.%I ENABLE ROW LEVEL SECURITY;', t);
  END LOOP;
END $$;
