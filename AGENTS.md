# TruckFlow Architecture Standards

## Non-Blocking Frontend Rule (MANDATORY)

**The frontend must NEVER freeze or become unresponsive under any circumstance.**

Every user action must follow this pattern:

### 1. Immediate Response (pure frontend, zero wait)
User clicks → UI immediately shows an honest **pending/in-progress state** on that specific item:
- A spinner on that button
- "Saving…", "Connecting…", "Syncing…" text
- The button disabled while that specific action is pending
- **NEVER show fake "success!" before the backend confirms it**

### 2. Backend Work (independent, however long it takes)
The actual work is sent to the backend and runs there — the frontend does NOT wait synchronously.

### 3. Result Delivery (event-driven)
When the backend finishes, it pushes the result to the frontend via Tauri events:
- `handle.emit("pg-configured", view)` — success
- `handle.emit("pg-config-error", json!({"error": e}))` — honest failure

### 4. UI Update (honest outcome)
The frontend updates with the **real confirmed result**:
- Success → checkmark, "Saved", new data visible
- Failure → error message, retry option, affected item does NOT show as saved
- The rest of the app stays fully interactive the entire time

### Key Principle
**Non-blocking ≠ presumptive.** The UI not freezing and the UI showing an unconfirmed result are two completely separate things. You can have one without the other. Both are required together:
- Instant responsiveness (nothing freezes)
- Truthful status reporting (nothing claims success before it's real)

## Backend Database Architecture

### Three Separate SQLite Connections
- `db` — Primary connection for all Tauri command handlers (UI actions)
- `sync_db` — Dedicated connection for the background sync poller (Postgres + Sheets)
- `anpr_db` — Dedicated connection for the background ANPR poller

### Why Separate Connections?
SQLite uses a global write lock. With one connection, the sync poller's network push (5-10 seconds) blocks ALL UI commands from reading or writing. Three connections eliminate this contention entirely.

### Lock Rules
- UI commands: `state.db.lock()` — safe, no contention with background threads
- Sync poller: `state.sync_db.try_lock()` — skip cycle if busy (non-critical background work)
- ANPR poller: `state.anpr_db.try_lock()` — skip cycle if busy (non-critical background work)
- Background threads spawned by commands: `state.db.lock()` — acceptable, quick DB writes

### WAL Mode
All connections use `PRAGMA journal_mode = WAL` which allows concurrent reads. Writes serialize at the OS level but are sub-ms for single-row operations.

## Adding New Features

When adding any new feature that involves:
1. Database writes
2. Network calls
3. File I/O
4. Any operation that could take > 10ms

**You MUST use the non-blocking pattern:**
- Backend: spawn work on a thread, return "processing" string, emit event on completion
- Frontend: use `useAsyncAction` hook, call `fire()` instead of `await invoke()`, show per-action pending state
- Never block the UI waiting for backend results

**Never reintroduce synchronous `await invoke()` for slow operations.**
