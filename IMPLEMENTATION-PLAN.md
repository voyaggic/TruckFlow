# TruckFlow — Implementation Plan (v2)

> Master build plan for the TruckFlow desktop app. Written against the **eight** source specs in `D:\Exhauster project\` (`00-project-overview.md` … `08-anpr-integration.md`). Every phase maps back to specific sections of those documents, and **no phase is "done" until its exit criteria pass**. Open items from the specs are marked **[OPEN]** and use clearly-labeled placeholders — never invented values.
>
> **Doc hierarchy (important):** `08-anpr-integration.md` **supersedes and extends** the ANPR-related sections of `02-architecture.md` §4 and `04-capture-pipeline.md`. Where they disagree on engine choice, packaging, duplicate-timing, thresholds, or the confirmation/declined flows, `08` wins — this is called out again in §0 and §1 below.

---

## 0. Environment & conventions (apply to every phase)

- **Machine:** Windows only (confirmed by client). Development machine: Windows, Node `v24.18.0`, Rust `1.97.1`, Git `2.53.0` — verified present. ANPR service is Python-based (see Phase 2); the installed Python version must be pinned and verified before building.
- **Stack (`02-architecture.md`, `08-anpr-integration.md`):** Tauri 2.x (latest stable) + React (latest stable, TypeScript) + Rust core, SQLite local, PostgreSQL central, Recharts + Lodash. ANPR = self-hosted open-source **YOLOv8 (detection) + PaddleOCR (primary OCR) / EasyOCR (manually-switchable alternative)** as a separate local Python service (`08` §1). Use the **latest stable version of every dependency at the time each phase is actually built** — re-verify, don't assume.
- **Shell:** Tauri bundling; clean packaged app, no visible console window (`02` §1). The ANPR service ships and auto-starts as a **managed background process** of the app — one install, one icon, no separate program the user launches (`08` §2).
- **Database rules (`01-database-schema.md`):** UUID PKs generated client-side (never auto-increment), no hard deletes (status fields, incl. the `declined` trip status which is purgeable *only* by explicit action), every table has `created_at`/`updated_at`/`synced`, `extra_fields` JSON on core entity tables, point-in-time data **copied** onto `trips` (never live-referenced). `pushed_to_sheets` is independent from `synced`.
- **Capture provenance (`01-database-schema.md` `trips`):** `model_version` + `ocr_engine` are recorded on every auto / manual-confirm read and never omitted; confidence scores are **never** compared across different OCR engines or model versions as if equivalent (`08` §3, `01` schema notes).
- **Code conventions:** no hardcoded role branching anywhere — every UI section gated by its own permission check (`03` §1, `05` §0). Modular components, not whole-page role templates. The ANPR Engine Configuration page is its own permission-gated section (`manage_anpr_config`), **not** nested inside general admin access (`08` §5).
- **Dependency/independence invariant (`06-data-flow.md`):** only capture → process → save-locally is the non-negotiable core. Every downstream step must be independently failable without breaking anything upstream. A crash/hang in the ANPR service must never take down trip logging (Manual Entry stays functional).
- **No time-based duplicate blocking, ever (`08` §8):** the earlier fixed-window duplicate-trip rule is **removed**, not deprioritized. Suspicious repeat patterns are a matter of human review via retained photo evidence, not an automated real-time block. Do not re-introduce a timing check.
- **Testing-before-live principle (`08` §7):** no engine swap, threshold change, or model deployment is applied to live operation without first being run through the testing sandbox. This applies every time, for the life of the system.
- **Out of scope, always:** money/billing (`00` §5), driver biometrics, Finance accounting software integration, multi-site. Do not add speculatively.

---

## 1. Where we are now (read this first)

**Phases 1–3 are already built** (schema v1, auth + permissions, app shell, capture pipeline, reference DB, Gate Officer main view, verification queue, frame evidence) and their prior test suites pass (32 backend tests green). **The refined spec set — notably the new `08-anpr-integration.md` — changes several things inside those built phases.** Before starting Phase 4, the deltas below must be reconciled. Phase boundaries in §2.1–§2.3 therefore list both the original build scope (reference) and the **reconciliation tasks** that apply the refined spec to the existing codebase.

**Reconciliation checklist (delta between built code and refined specs):**

| # | Delta | Built today | Required by refined spec | Where |
|---|---|---|---|---|
| R1 | `trips.status` enum | `(logged, queued, resolved, discarded)` | `(logged, queued, resolved, discarded, declined)` — `declined` = officer rejected in confirm-mode; saved locally, excluded from trip counting/analytics, purgeable by officer/admin (`01` `trips`, `08` §9) | Phase 2/3 |
| R2 | `trips` columns | has `pushed_to_sheets` | also `is_discharge_trip` (nullable boolean), `model_version` (text, nullable), `ocr_engine` (`paddleocr`/`easyocr`/`manual`, nullable) (`01` `trips`) | Phase 1/2 |
| R3 | New tables | `anpr_config`, `anpr_credentials`, `camera_sources`, `model_versions`, `training_candidates` missing | all five present (`01` schema; `camera_sources` source types incl. `video_file`/`live_test`; `anpr_config` incl. per-engine thresholds + `plate_vehicle_ratio_threshold` + `plate_format_rules` + `discharge_confirmation_required`) | Phase 1 |
| R4 | Duplicate/timing check | implemented (10-min window on close re-reads) | **removed entirely** (`08` §8) — delete the timing logic and its tests, not just disable it | Phase 2 |
| R5 | ANPR engine/strategy | adapter trait + simulator; "real adapter" left generic (Plate-Recognizer-style) | self-hosted **YOLOv8 + PaddleOCR/EasyOCR Python service**, local HTTP `{plate, confidence, timestamp, frames}`; **auto-started as managed subprocess**; engine swap is **manual** with per-engine thresholds (`08` §1–3) | Phase 2 |
| R6 | ANPR Engine Configuration page | absent | dedicated section gated by new permission `manage_anpr_config`: engine status/swap, model & version management (validation-before-deploy), camera sources, testing sandbox, thresholds & tuning, credentials, training/continuous-learning, storage/resource, diagnostics (`08` §5) | Phase 2 |
| R7 | Discharge classification | absent | Yes/No discharge step with **two-step confirm** (each path shows its own confirm/cancel popup) whenever `discharge_confirmation_required` is on; `is_discharge_trip` null until classified; non-discharge entries excluded from analytics but retained (`08` §9, `01` `trips`) | Phase 2 |
| R8 | Training candidates | absent | auto-flag frames from low-confidence reads and human-corrected queue items into `training_candidates`; admin-triggered retrain → new non-live version → held-out validation → explicit deploy; `retrain_candidate_threshold` notification (`08` §6, §5) | Phase 3 |
| R9 | Fixed-pixel skip threshold | N/A (not built) | `plate_vehicle_ratio_threshold` (bounding-box-ratio) replaces the old fixed-pixel/distance rule; camera-position independent (`08` §8) | Phase 2 (config page) |
| R10 | Users table | already has `revoked_by`, `revoked_at`, `profile_photo_ref`, `phone_number`, `theme_mode`, `theme_accent`, `language_preference` | matches — no delta | — |
| R11 | Permissions seed | keys from earlier spec | add `manage_anpr_config` (min auth `password`) and `manage_integrations` (`password`); confirm every key in `01` §permissions is seeded; Admin preset bundle gains `manage_anpr_config` (`08` §5); System Monitor preset gains acknowledge + history recommendations (`03` §11) | Phase 1 |

> **Ordering note:** R2/R3 (schema) gate everything else and belong to the Phase 1 reconciliation. R1/R4/R5/R6/R7 belong to Phase 2. R8 belongs to Phase 3. Phases 4–7 below are new build work.

---

## 2.1 Phase 1 — Core foundation (built — reconcile deltas)

*References: `01-database-schema.md` (all), `02-architecture.md` §1–2, `03-auth-permissions.md` (all)*

### Build scope (complete — reference only)
Project scaffold (Tauri 2 + React + TS + Vite, no console); SQLite schema v1 for `companies`, `drivers`, `vehicles`, `trips`, `users`, `permissions`, `user_permissions`, `role_presets`, `audit_log`, `system_health_events`, `integrations`, plus internal `app_settings`; client-side UUID v4; seed `permissions` + default `role_presets` (only when empty); auth (PIN/password, Argon2, auth-type derived from permissions, auto-upgrade flow); first-run "create first Admin"; permission-driven app shell with neutral login, manual-session model, revocation rules.

### Reconciliation tasks (apply against existing code)
1. **Schema migration to v2** — add `is_discharge_trip`, `model_version`, `ocr_engine` to `trips`; create `anpr_config`, `anpr_credentials`, `camera_sources`, `model_versions`, `training_candidates` exactly per `01-database-schema.md`. Keep `users` as-is (already compliant). All changes via the existing embedded migration runner; do not delete/recreate existing data.
2. **Seed additions** — add `manage_anpr_config` and confirm `manage_integrations` are seeded as `permissions` rows with `min_auth_level = password`; extend Admin preset to include `manage_anpr_config`; extend System Monitor preset with the `05-ui-screens.md` §6h recommended additions (acknowledge alerts, health history).
3. **`declined` support in code** — extend the trips status type/validation everywhere (SQLite column is already TEXT; this is semantic + code-level). Ensure `declined` rows are excluded from every normal query/count/analytics path and surfaced only in the confirmation/decline and admin purge contexts.
4. **Auth/upgrade tests unaffected** — re-run Phase 1 exit criteria after the migration to prove no regression.

### Exit criteria (Phase 1)
- All checklists in `01-database-schema.md` §"Testing checklist" — incl. capacity snapshot test, orphaned-reference test, auth-upgrade-flag test, and the model-version/engine no-mixing query test.
- All checklists in `03-auth-permissions.md` §12.
- Zero hardcoded role branching in the codebase; sections assembled purely from `user_permissions`.

---

## 2.2 Phase 2 — Core capture pipeline (built — reconcile deltas)

*References: `04-capture-pipeline.md` (all, where not superseded), `08-anpr-integration.md` (all — supersedes ANPR specifics), `05-ui-screens.md` §2, `02-architecture.md` §4 (pattern only)*

### Build scope (complete — reference only)
Reference DB management (CRUD + deactivate, never delete; confirmation dialogs on capacity/company edits); ANPR service integration (separate local service, HTTP JSON `{plate, confidence, timestamp, frames}`, multi-frame + vehicle-presence detection, dev **simulator** as the testable default behind a swappable driver trait); cross-reference logic (exact, partial-narrowing to one/multiple/zero, confidence thresholding); trip creation (auto-fill + copy of `company_id`/`capacity_at_trip`, `time_in` = capture moment, `officer_id`, `capture_method`, `confidence_score`, multi-frame `photo_refs`, consent-mode toggle); Gate Officer main view (header, shift summary, Current Entry, Manual Entry always visible, queue panel, search, recent feed, sync indicator, action flash); manual entry fallback.

### Reconciliation tasks (apply against existing code)
5. **Remove the duplicate/impossible-timing check (R4).** Delete the timing-window logic and its tests entirely — the close-repeat read no longer queues. Update the ingest/quoting tests that assert duplicate-timing behavior (they currently expect a 10-min window to flag; that expectation is obsolete per `08` §8).
6. **ANPR service repoint (R5).** Change the real-service driver behind the existing trait/interface from a generic/Plate-Recognizer-style adapter to the **YOLOv8 + PaddleOCR/EasyOCR Python service**:
   - Detection = YOLOv8 fine-tuned for plates; OCR = PaddleOCR primary, EasyOCR manually selectable alternative; Tesseract rejected (`08` §1).
   - Local HTTP API, same JSON contract `{plate, confidence, timestamp, frames}` as the simulator, so app-side logic is unchanged (`02` §4 pattern).
   - **Managed subprocess:** Tauri auto-starts the service on app launch, monitors its process/health, surfaces failure in the UI immediately, and never blocks Manual Entry when it is down (`08` §2). No user-visible second program.
   - **Packaging:** single installer bundling the Python runtime, PyTorch, OCR engines, and model weights; if full bundling is impractical for a given dependency, fall back to a documented one-time provisioning script run once at machine setup (never a recurring/user-facing burden) (`08` §2, `07` Phase 7).
   - Keep the simulator as the offline/CI default driver; the live service and simulator both implement the same trait.
6a. **ANPR Engine Configuration page (R6/R9).** New permission-gated section (`manage_anpr_config`), fully independent of general admin access. Full feature set per `08` §5:
   - **Engine selection & status:** active-engine indicator, manual swap with confirmation dialog, every swap audit-logged, detected hardware (GPU present/absent), live average read-time indicator.
   - **Model & version management** (`model_versions`): current live version per component (detection + per-OCR-engine); full version history (deployed by/when/validation accuracy); **deploy never automatic** — a candidate model must pass validation against a fixed held-out test set, with accuracy compared against the current live model and shown to admin before explicit confirm; one-click rollback; rollback provenance recorded.
   - **Camera/input source configuration** (`camera_sources`): add/edit/remove sources of types `rtsp` / `nvr_export` / `usb` / `video_file` / `live_test`; per-source connection test with last-check result; multiple simultaneous sources.
   - **Testing sandbox:** single-image upload, video upload, and live-test mode — full pipeline output (detection, OCR text, confidence, reference-match result) with **zero writes to the live `trips` table** (`08` §4, §5). Used by every testing-before-live change (`08` §7).
   - **Thresholds & tuning** (`anpr_config`): confidence threshold tuned **independently per OCR engine** (never one shared number); `plate_vehicle_ratio_threshold` (bounding-box-ratio, replacing any fixed-pixel/distance rule) with a live preview against a test frame; `plate_format_rules` regex/pattern validation — a structurally impossible plate is auto-flagged regardless of engine confidence; `discharge_confirmation_required` toggle.
   - **Credentials** (`anpr_credentials`): API/license keys stored securely, never plain text in the table, always masked in UI, rotation tracked (who/when).
   - **Training / continuous learning** (`training_candidates`): view candidate pool; trigger retraining → output is a new **non-live** version that must pass validation + explicit deploy; `retrain_candidate_threshold` notification.
   - **Storage & resource management:** disk usage for saved frames / candidates / model versions with pre-capacity warning; guidance on concurrent streams vs. detected capability.
   - **Diagnostics:** ANPR-specific error log (recognition-level, distinct from System Monitor health); rolling confidence trend (a sustained drop surfaces to System Monitor); dependency/runtime health check surfaced proactively.
7. **Trip creation additions (R1/R7).**
   - Record `model_version` + `ocr_engine` on every auto / manual-confirm trip; never omit them for those capture methods (`01` `trips`).
   - **Discharge Yes/No classification:** when `discharge_confirmation_required` is enabled, approving a trip (auto or confirm-mode) requires the "Was this a discharge trip?" step. **Neither answer commits on a single tap** — each triggers its own confirm/cancel popup before finalizing. `is_discharge_trip` stays null until classified; non-discharge entries excluded from trip analytics but retained for record-keeping (`08` §9).
   - **`declined` path (R1):** in confirm-mode, the officer can decline a read; the record is saved with `status = declined`, excluded from the main trip count, retained for reference, purgeable by officer/admin (`08` §9). Confirm-mode UI must present decline as a first-class action.
8. **Gate Officer main view** — add the discharge classification step and declined action to the confirm flow; ANPR-service health reflected in the sync/status indicators (`08` §2).

### Exit criteria (Phase 2)
- All checklists in `04-capture-pipeline.md` §9, **as corrected by `08` §8** (the duplicate-timing item is superseded/removed; all others apply), plus the `08` §10 items that land in Phase 2: service auto-start with no manual step; crash/hang detected and surfaced without blocking Manual Entry; engine switch requires confirmation and is audit-logged; confidence thresholds stored/applied independently per engine; sandbox image/video/live-test never writes live `trips`; plate-ratio threshold correct across two simulated camera positions/angles without per-camera recalibration; discharge two-step confirm on both paths.
- Gate Officer checks in `05-ui-screens.md` §8.
- End-to-end capture via the simulator, and independently via Manual Entry with the ANPR service stopped.

---

## 2.3 Phase 3 — Exception handling (built — reconcile deltas)

*References: `04-capture-pipeline.md` §6, `05-ui-screens.md` §3, `08-anpr-integration.md` §6, §9*

### Build scope (complete — reference only)
Verification Queue screen (multiple frames, reason flag, best-guess + confidence, selectable matches, inline edit of auto-filled fields, register-new with duplicate-plate warning, Discard / Skip, capture-vs-resolution timestamps, resolver attribution); photo/frame evidence capture/storage/retrieval for every trip.

### Reconciliation tasks (apply against existing code)
9. **Reason-flag set update** — queue reason flags now reflect the refined rule set: `multiple matches` / `no match / possible new vehicle` / `low raw confidence` / structurally-invalid plate format (`plate_format_rules`). **No duplicate-timing reason flag** (R4).
10. **`declined` entries list + purge (R1)** — a view (officer + admin) of locally-saved `declined` records with an explicit purge action, per `08` §9. Purging is the only place `declined` rows are physically deleted, and only by an authorized user; confirm dialogs apply.
11. **Training-candidate auto-flagging (R8)** — frames from low-confidence reads and human-corrected queue items automatically populate `training_candidates` (reason `low_confidence` / `queue_corrected`); `source_trip_id` linked; candidates consumed only by the Phase 2 retraining flow (`08` §6). Manually-uploaded sandbox images are **not** auto-flagged (`08` §4).

### Exit criteria (Phase 3)
- Every queue resolution path (confirm / edit / discard / skip / register-new) produces the correct `trips.status` and preserves original `time_in` unchanged (`04` §9, `05` §8).
- A `declined` entry is saved locally, excluded from the main trips database/count, and purgeable (`08` §10 final check).
- Low-confidence and queue-corrected frames land in `training_candidates` with the correct reason and source link.

---

## 2.4 Phase 4 — Sync & distribution (build — pending)

*References: `02-architecture.md` §3, `06-data-flow.md` (all), `05-ui-screens.md` §6e, §6f, `01-database-schema.md` (`integrations`, `trips` sync flags)*

### Tasks
12. **PostgreSQL sync** — one-way local→central for `trips`; reference/user/audit data synced on the same UUID + `synced`-flag pattern. Flag flips to `true` only on confirmed receipt; background retry loop with no manual "send" action; rows that fail mid-sync simply remain `false` (no loss, no duplicates — UUID-keyed). `pushed_to_sheets` handled by a fully separate process so one target failing never affects the other (`02` §3, `06` Step 5). **Dev target is a mock sync adapter** (in-memory store / JSON log implementing the central interface) so all logic is testable offline; a real Rust PostgreSQL driver is the swappable adapter behind the same trait — no central DB exists during early phases.
13. **Google Sheets integration** — OAuth connect flow, target sheet selection (existing or create), Google Group sharing field, sync frequency (realtime / every 15 min), last-synced indicator, manual "Sync now", disconnect; backed by the `integrations` table; gated by the `manage_integrations` permission (`05` §6f). Built against a mock/stub provider in dev; real Google client as swappable driver.
14. **Connectivity/sync status** — local indicator on the Gate Officer screen ("Online — synced" vs "Offline — N pending", non-actionable); overall sync status + pending record counts + Postgres/sheets health in System Settings (`05` §2, §6e).

### Exit criteria (Phase 4)
- All checklists in `06-data-flow.md` §"Testing checklist" — simulate each failure in the left column one at a time and confirm the corresponding "still works" cell.
- Extended-offline-then-reconnect (simulate 24–72 h offline) → zero data loss, zero duplicate `trips` rows.
- Independent-failure test for each sync target: Sheets sync failing (e.g. revoked OAuth token) leaves PostgreSQL sync and local capture fully unaffected, and vice versa.

---

## 2.5 Phase 5 — Reporting & oversight (build — pending)

*References: `05-ui-screens.md` §5, §6, `02-architecture.md` §2*

### Tasks
15. **Reporting Dashboard** — strictly read-only; date-range filters (presets + custom range) + company filter; summary stats (total trips, active companies, avg/day, prior-period comparison); trips-over-time line chart; top-companies bar chart; trips-by-vehicle table; **required drill-down** into the underlying individual `trips` records with photo/frame evidence reachable; Excel export; Sheets sync status/link (`05` §5). Aggregation via Lodash, charts via Recharts. Queries **PostgreSQL only**, never SQLite (`02` §3, `06` Step 6). Zero monetary/billing data anywhere.
16. **Admin oversight/activity view** — aggregate per-officer activity (trips logged, queue items resolved); aggregate/historical only, never access to a live session (`05` §6c).
17. **Audit log** — chronological, searchable/filterable `audit_log` view (who/what/when/to-what) (`05` §6g).
18. **System Monitor section** — per-component health from `system_health_events` (`camera`, `anpr_service`, `sync`, `database`); camera connection status + last frame received; ANPR service status + last successful read; sync status + pending + failures; local DB health + recent errors; clear alerts for degraded/offline; **acknowledge alert** action; basic incident history (`05` §6h). Include the confidence-trend signal from the ANPR diagnostics (`08` §5) as an `anpr_service`-level input.

### Exit criteria (Phase 5)
- A Reporting-type user can view aggregated data and drill down to a specific underlying trip record with its photo evidence.
- Zero monetary/billing data reachable from any screen; no write/delete action reachable from the dashboard under any permission combination.

---

## 2.6 Phase 6 — Polish & operational readiness (build — pending)

*References: `05-ui-screens.md` §4, §7, `02-architecture.md` §6, `03-auth-permissions.md` §4–5*

### Tasks
19. **Settings screen** — theme mode (light/dark/system) **and** independent accent palette; self-service PIN/password change (current credential first, live strength checklist updating as the user types); notification preference (sound on/off); profile photo upload/change/remove; contact phone; language-preference placeholder **[OPEN]**: English/Swahili undecided (`05` §4); About/app version.
20. **Error/empty states** — intentional on every screen (`05` §7): brand-new officer's empty recent list, admin's empty user list, ANPR unreachable, camera feed lost, sync repeatedly failing — surfaced clearly, never silent.
21. **Confirmation dialogs** — required before every destructive/high-impact action: deactivate user, edit vehicle capacity/company, discard queue item, purge `declined` entries, switch OCR engine, deploy a model, disconnect an integration (`05` §7, `08` §5).
22. **Auto-updater** — Tauri updater, semantic versioning, rollback-capable; publish previous known-good version through the same mechanism; staged rollout on a dev/admin machine before the live pilot site (`02` §6).

### Exit criteria (Phase 6)
- Every destructive action requires confirmation; every screen has an intentional empty state; a simulated update publishes and auto-installs on a test build (`07` Phase 6).

---

## 2.7 Phase 7 — Pilot deployment prep (build — pending)

*References: `07-build-plan.md` Phase 7, `00-project-overview.md` §8, `08-anpr-integration.md` §2*

### Tasks
23. **On-site checklist docs** — actual camera feed access method **[OPEN]**: RTSP? NVR export? which system?; terminal/hardware placement and provisioning **[OPEN]**, incl. one-installer ANPR dependency bundling or the documented one-time provisioning script fallback (`08` §2); seed tooling for the real ~60-exhauster reference data **[OPEN]**: placeholder/test data until provided; install/run instructions.
24. **Data-retention confirmation** — confirm with client/legal whether frame-evidence retention (which incidentally captures people) has any obligation under Kenya's Data Protection Act **[OPEN]** (`08` §9, `00` §8). Do not assume resolved.
25. **Training material** — this is the first system these officers will log into; a real training session plan, not a handoff (`07` Phase 7).
26. **Pilot parameters** — 4–6 weeks, single gate; success markers per `07` (majority of trips auto-captured without queue intervention, the logbook-to-Excel retyping step observably stops, positive officer adoption after 1–2 weeks, zero data loss in any simulated/real interruption). Draft — confirm with client before Phase 7.

### Exit criteria (Phase 7)
- Site-ready package: installer, seeding path, on-site guide, training runbook, pilot success-tracking template.

---

## Standing reminders (apply to every phase)
- Latest-stable dependency versions at the moment of building — re-verify, don't assume.
- Nothing is fixed: `extra_fields`, composable permissions, modular sections, and the `08` config/tables are the escape hatches for all future extension (engine swaps, new input types, threshold retunes, model versions).
- No money/billing unless the client directly asks.
- **`07-build-plan.md` general instruction governs every phase:** complete and pass the prior phase's testing checklists before starting the next; if a test fails, fix it in-phase; if a requirement is ambiguous or contradictory, **stop and ask** — never guess.
- `08-anpr-integration.md` supersedes `02` §4 and `04` §3/§4/§7/§9 on all ANPR-specific points. When in doubt between documents, follow `08`, then `01`, then `06`, then `07`.
