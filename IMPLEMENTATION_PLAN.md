# TruckFlow Implementation Plan

## Executive Summary

This plan transforms TruckFlow from a single-PC app to a **multi-PC, multi-company system** with PostgreSQL as the central hub. All changes follow industry standards for distributed systems.

---

## Phase 1: PostgreSQL Sync Reliability (Week 1)

### 1.1 Reduce Connection Timeout

**File:** `src-tauri/src/sync.rs`

```rust
// BEFORE
config.connect_timeout(std::time::Duration::from_secs(15));

// AFTER
config.connect_timeout(std::time::Duration::from_secs(3));
```

**Why:** 15s timeout hangs the sync for 47+ seconds per failed cycle. 3s fails fast.

### 1.2 Reduce Retry Attempts

**File:** `src-tauri/src/sync.rs`

```rust
// BEFORE
const MAX_ATTEMPTS: u32 = 3;

// AFTER
const MAX_ATTEMPTS: u32 = 1;
```

**Why:** 3 attempts × 15s = 47s wasted. 1 attempt × 3s = 3s.

### 1.3 Remove Exponential Backoff

**File:** `src-tauri/src/sync.rs`

```rust
// BEFORE
self.connect_backoff = (prev.max(5) * 2).min(30);

// AFTER
self.connect_backoff = 0;  // No backoff, retry on next signal
```

**Why:** Backoff grows 5→10→20→30s. Next signal should retry immediately.

### 1.4 Add Keepalive Pinger

**File:** `src-tauri/src/lib.rs`

```rust
fn spawn_keepalive_pinger(app: &AppHandle, state: &AppState) {
    let sync_notify = state.sync_notify.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let _ = sync_notify.try_send(());
        }
    });
}
```

**Why:** If no trips are created while offline, pending rows never sync. Keepalive ensures retry every 30s.

### 1.5 Reduce Push Timeout

**File:** `src-tauri/src/sync.rs`

```rust
// BEFORE
let result = match self.send_timeout(PgCommand::PushRows(...), rx, std::time::Duration::from_secs(120)) {

// AFTER
let result = match self.send_timeout(PgCommand::PushRows(...), rx, std::time::Duration::from_secs(15)) {
```

**Why:** 120s timeout is too long. 15s is enough for a batch push.

---

## Phase 2: Multi-PC Architecture (Week 2)

### 2.1 PostgreSQL Schema Updates

```sql
-- Users: unique username per company
CREATE TABLE users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    username TEXT NOT NULL,
    company_id UUID NOT NULL REFERENCES companies(id),
    password_hash TEXT,
    role TEXT NOT NULL DEFAULT 'gate_person',
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(username, company_id)
);

-- Machine status: track which PC is online
CREATE TABLE machine_status (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    machine_id UUID NOT NULL,
    user_id UUID REFERENCES users(id),
    company_id UUID NOT NULL REFERENCES companies(id),
    role TEXT NOT NULL,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    is_online BOOLEAN NOT NULL DEFAULT true,
    ip_address TEXT,
    pc_name TEXT,
    UNIQUE(machine_id)
);

-- Company config: shared across all PCs
CREATE TABLE company_config (
    company_id UUID PRIMARY KEY REFERENCES companies(id),
    pg_connection_string TEXT,
    sheets_id TEXT,
    sheets_frequency TEXT DEFAULT 'realtime',
    anpr_enabled BOOLEAN DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by UUID REFERENCES users(id)
);
```

### 2.2 Machine Heartbeat

**File:** `src-tauri/src/sync.rs`

```rust
fn update_machine_status(state: &AppState, user_id: &str) {
    let machine_id = get_machine_id();  // Hardware UUID or generated
    let pc_name = get_pc_name();
    
    if let Ok(conn) = state.sync_db.try_lock() {
        let _ = conn.execute(
            "INSERT INTO machine_status (machine_id, user_id, company_id, role, last_seen_at, is_online, pc_name)
             VALUES (?1, ?2, ?3, ?4, NOW(), true, ?5)
             ON CONFLICT (machine_id) DO UPDATE SET 
                 last_seen_at = NOW(), 
                 is_online = true,
                 user_id = EXCLUDED.user_id,
                 role = EXCLUDED.role",
            params![machine_id, user_id, get_company_id(&conn), get_role(&conn), pc_name],
        );
    }
}

// Called every 30 seconds
fn spawn_heartbeat(state: &AppState) {
    let sync_db = state.sync_db.clone();
    let running = state.running.clone();
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(30));
            if let Ok(conn) = sync_db.try_lock() {
                // Mark offline machines (not seen in 90s)
                let _ = conn.execute(
                    "UPDATE machine_status SET is_online = false 
                     WHERE last_seen_at < datetime('now', '-90 seconds')",
                    [],
                );
            }
        }
    });
}
```

### 2.3 Config Sync

**File:** `src-tauri/src/sync.rs`

```rust
fn pull_company_config(state: &AppState, company_id: &str) -> Result<(), String> {
    let pg = &*state.pg;
    if !pg.configured() || !pg.connected() {
        return Err("PostgreSQL not connected".to_string());
    }
    
    let rows = pg.query_rows(
        "SELECT * FROM company_config WHERE company_id = $1",
        &[company_id.to_string()],
    )?;
    
    if let Some(row) = rows.first() {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        
        // Save PG connection string (encrypted)
        if let Some(pg_str) = row.get("pg_connection_string").and_then(|v| v.as_str()) {
            db::set_setting(&conn, "pg_connection_string", pg_str)?;
        }
        
        // Save Sheets ID
        if let Some(sheets_id) = row.get("sheets_id").and_then(|v| v.as_str()) {
            db::set_setting(&conn, "sheets_id", sheets_id)?;
        }
        
        // Save other config
        if let Some(freq) = row.get("sheets_frequency").and_then(|v| v.as_str()) {
            db::set_setting(&conn, "sheets_frequency", freq)?;
        }
    }
    
    Ok(())
}
```

### 2.4 Auto-Pull on Login

**File:** `src-tauri/src/commands.rs`

```rust
pub fn login(state: State<AppState>, username: String, password: String, company_name: String) -> Result<LoginResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    
    // 1. Check local SQLite first
    let user = find_user_local(&conn, &username, &company_name)?;
    
    if user.is_none() {
        // 2. Pull from PostgreSQL
        let company_id = find_company_id(&conn, &company_name)?;
        pull_user_from_pg(&state, &username, &company_id)?;
        
        // 3. Try again
        let user = find_user_local(&conn, &username, &company_name)?
            .ok_or("Username not found in this company")?;
    }
    
    // 4. Verify password
    let user = user.unwrap();
    verify_password(&user.password_hash, &password)?;
    
    // 5. Pull config from PostgreSQL
    pull_company_config(&state, &user.company_id)?;
    
    // 6. Update machine status
    update_machine_status(&state, &user.id);
    
    Ok(LoginResult { user, session_token })
}
```

---

## Phase 3: User Management (Week 3)

### 3.1 Admin Creates Username Only

**File:** `src-tauri/src/commands.rs`

```rust
pub fn create_user(
    state: State<AppState>,
    actor_id: String,
    username: String,
    company_name: String,
    role: String,
) -> Result<User, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    
    // Check permission
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    
    // Check if username exists in this company
    let exists = conn.query_row(
        "SELECT COUNT(*) FROM users WHERE username = ?1 AND company_id = ?2",
        params![username, find_company_id(&conn, &company_name)?],
        |r| r.get::<_, i64>(0),
    )? > 0;
    
    if exists {
        return Err(format!("Username '{}' already exists in {}", username, company_name));
    }
    
    // Create user (no password yet)
    let user_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO users (id, username, company_id, role, is_active, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, true, datetime('now'), datetime('now'))",
        params![user_id, username, find_company_id(&conn, &company_name)?, role],
    )?;
    
    // Sync to PostgreSQL
    sync_user_to_pg(&state, &user_id)?;
    
    Ok(User { id: user_id, username, role, .. })
}
```

### 3.2 User Sets Password on First Login

**File:** `src-tauri/src/commands.rs`

```rust
pub fn set_initial_password(
    state: State<AppState>,
    username: String,
    company_name: String,
    password: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    
    // Find user
    let user = find_user_local(&conn, &username, &company_name)?
        .ok_or("Username not found")?;
    
    // Check if password already set
    if user.password_hash.is_some() {
        return Err("Password already set. Use login instead.");
    }
    
    // Validate password strength
    let strength = validate_password(&password);
    if !strength.valid {
        return Err(format!("Password too weak: {}", strength.message));
    }
    
    // Hash and save
    let hash = hash_password(&password)?;
    conn.execute(
        "UPDATE users SET password_hash = ?1, updated_at = datetime('now') WHERE id = ?2",
        params![hash, user.id],
    )?;
    
    // Sync to PostgreSQL
    sync_user_to_pg(&state, &user.id)?;
    
    Ok(())
}
```

### 3.3 Login Flow

**File:** `src-tauri/src/commands.rs`

```rust
pub fn login(
    state: State<AppState>,
    username: String,
    password: String,
    company_name: String,
) -> Result<LoginResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    
    // 1. Find user in local SQLite
    let company_id = find_company_id(&conn, &company_name)?;
    let user = conn.query_row(
        "SELECT * FROM users WHERE username = ?1 AND company_id = ?2 AND is_active = true",
        params![username, company_id],
        |r| User::from_row(r),
    ).optional()?;
    
    // 2. If not found locally, pull from PostgreSQL
    let user = if user.is_none() {
        pull_user_from_pg(&state, &username, &company_id)?;
        conn.query_row(
            "SELECT * FROM users WHERE username = ?1 AND company_id = ?2 AND is_active = true",
            params![username, company_id],
            |r| User::from_row(r),
        ).optional()?.ok_or("Username not found in this company")?
    } else {
        user.unwrap()
    };
    
    // 3. Check if password is set
    if user.password_hash.is_none() {
        return Err("PASSWORD_NOT_SET".to_string());  // Special error code
    }
    
    // 4. Verify password
    if !verify_password(&user.password_hash.unwrap(), &password)? {
        return Err("Invalid password".to_string());
    }
    
    // 5. Pull config from PostgreSQL
    pull_company_config(&state, &company_id)?;
    
    // 6. Update machine status
    update_machine_status(&state, &user.id);
    
    // 7. Create session
    let token = create_session(&conn, &user.id)?;
    
    Ok(LoginResult { user, session_token: token })
}
```

---

## Phase 4: Reporting (Week 4)

### 4.1 "Load from Archive" Button

**File:** `src-tauri/src/reporting.rs`

```rust
pub fn report_dashboard(
    state: State<AppState>,
    actor_id: String,
    filters: ReportFilters,
) -> Result<ReportDashboard, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, REPORT_PERM)?;
    
    // 1. Try PostgreSQL first (if connected)
    if state.pg.configured() && state.pg.connected() {
        match central_dashboard(&*state.pg, &filters) {
            Ok(Some(dashboard)) => {
                // Cache to SQLite for offline use
                cache_pg_data_to_sqlite(&state, &dashboard)?;
                return Ok(dashboard);
            }
            Ok(None) => {}  // PG returned nothing, fall through
            Err(_) => {}    // PG failed, fall through
        }
    }
    
    // 2. Fall back to local SQLite
    Ok(ReportDashboard {
        summary: report_summary(&conn, &filters)?,
        trips_over_time: trips_over_time(&conn, &filters)?,
        top_companies: top_companies(&conn, &filters, 10)?,
        trips_by_vehicle: trips_by_vehicle(&conn, &filters, 100)?,
        data_source: "local".to_string(),
    })
}
```

### 4.2 Date Range Options

**File:** `src-tauri/src/models.rs`

```rust
pub enum DateRange {
    Last30Days,
    Last90Days,
    Last6Months,
    Last1Year,
    Custom { start: String, end: String },
}

impl DateRange {
    pub fn to_sql(&self) -> (String, Vec<String>) {
        match self {
            DateRange::Last30Days => (
                "time_in >= datetime('now', '-30 days')".to_string(),
                vec![],
            ),
            DateRange::Last90Days => (
                "time_in >= datetime('now', '-90 days')".to_string(),
                vec![],
            ),
            DateRange::Last6Months => (
                "time_in >= datetime('now', '-6 months')".to_string(),
                vec![],
            ),
            DateRange::Last1Year => (
                "time_in >= datetime('now', '-1 year')".to_string(),
                vec![],
            ),
            DateRange::Custom { start, end } => (
                "time_in BETWEEN ?1 AND ?2".to_string(),
                vec![start.clone(), end.clone()],
            ),
        }
    }
}
```

### 4.3 Incremental Sync (No Duplicates)

**File:** `src-tauri/src/sync.rs`

```rust
pub fn pull_historical_data(
    state: &AppState,
    company_id: &str,
    date_range: &DateRange,
) -> Result<PullResult, String> {
    let pg = &*state.pg;
    if !pg.configured() || !pg.connected() {
        return Err("PostgreSQL not connected".to_string());
    }
    
    // 1. Build query based on date range
    let (where_clause, params) = date_range.to_sql();
    let sql = format!(
        "SELECT * FROM trips WHERE company_id = $1 AND {} ORDER BY time_in DESC",
        where_clause
    );
    
    // 2. Fetch from PostgreSQL
    let mut query_params = vec![company_id.to_string()];
    query_params.extend(params);
    let pg_rows = pg.query_rows(&sql, query_params)?;
    
    // 3. Filter out duplicates (already in SQLite)
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut new_rows = Vec::new();
    
    for row in &pg_rows {
        let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let exists = conn.query_row(
            "SELECT COUNT(*) FROM trips WHERE id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        ).unwrap_or(0) > 0;
        
        if !exists {
            new_rows.push(row.clone());
        }
    }
    
    // 4. Insert new rows to SQLite
    for row in &new_rows {
        insert_trip_local(&conn, row)?;
    }
    
    // 5. Update sync state
    update_last_synced(&conn, "trips", &date_range)?;
    
    Ok(PullResult {
        total_fetched: pg_rows.len(),
        new_inserted: new_rows.len(),
        duplicates_skipped: pg_rows.len() - new_rows.len(),
    })
}
```

### 4.4 UI Indicator

**File:** `src-tauri/src/commands.rs`

```rust
#[tauri::command]
pub fn get_sync_status(state: State<AppState>, actor_id: String) -> Result<SyncStatus, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    
    // Count local trips
    let local_trips = conn.query_row(
        "SELECT COUNT(*) FROM trips WHERE status = 'logged'",
        [],
        |r| r.get::<_, i64>(0),
    )?;
    
    // Count PG trips (if connected)
    let pg_trips = if state.pg.configured() && state.pg.connected() {
        let company_id = get_company_id(&conn, &actor_id)?;
        state.pg.query_rows(
            "SELECT COUNT(*) FROM trips WHERE company_id = $1 AND status = 'logged'",
            &[company_id],
        ).map(|rows| rows.len() as i64).unwrap_or(0)
    } else {
        0
    };
    
    // Get last synced time
    let last_synced = db::get_setting(&conn, "pg_last_synced_at").unwrap_or_default();
    
    Ok(SyncStatus {
        local_trips,
        pg_trips,
        last_synced,
        pg_connected: state.pg.connected(),
    })
}
```

---

## Phase 5: Admin Dashboard (Week 5)

### 5.1 Machine Status Display

**File:** `src-tauri/src/commands.rs`

```rust
#[tauri::command]
pub fn get_machine_status(state: State<AppState>, actor_id: String) -> Result<Vec<MachineStatus>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "view_machines")?;
    
    let company_id = get_company_id(&conn, &actor_id)?;
    
    let machines = conn.prepare(
        "SELECT ms.*, u.username 
         FROM machine_status ms
         LEFT JOIN users u ON ms.user_id = u.id
         WHERE ms.company_id = ?1
         ORDER BY ms.last_seen_at DESC"
    )?.query_map(params![company_id], |row| {
        Ok(MachineStatus {
            machine_id: row.get("machine_id")?,
            username: row.get("username")?,
            role: row.get("role")?,
            is_online: row.get("is_online")?,
            last_seen_at: row.get("last_seen_at")?,
            pc_name: row.get("pc_name")?,
        })
    })?.collect::<Result<Vec<_>, _>>()?;
    
    Ok(machines)
}
```

### 5.2 Role Assignment

**File:** `src-tauri/src/commands.rs`

```rust
#[tauri::command]
pub fn assign_machine_role(
    state: State<AppState>,
    actor_id: String,
    machine_id: String,
    new_role: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_machines")?;
    
    // Update local SQLite
    conn.execute(
        "UPDATE machine_status SET role = ?1 WHERE machine_id = ?2",
        params![new_role, machine_id],
    )?;
    
    // Sync to PostgreSQL
    if state.pg.configured() && state.pg.connected() {
        state.pg.query_rows(
            "UPDATE machine_status SET role = $1 WHERE machine_id = $2",
            &[new_role, machine_id],
        )?;
    }
    
    Ok(())
}
```

---

## Phase 6: Testing (Week 6)

### 6.1 Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_username_unique_per_company() {
        let conn = setup_test_db();
        
        // Create user in Company A
        create_user(&conn, "john", "company_a", "gate_person").unwrap();
        
        // Duplicate in Company A should fail
        assert!(create_user(&conn, "john", "company_a", "gate_person").is_err());
        
        // Same username in Company B should succeed
        assert!(create_user(&conn, "john", "company_b", "gate_person").is_ok());
    }
    
    #[test]
    fn test_user_sets_password() {
        let conn = setup_test_db();
        
        // Create user without password
        let user = create_user(&conn, "john", "company_a", "gate_person").unwrap();
        assert!(user.password_hash.is_none());
        
        // Set password
        set_initial_password(&conn, "john", "company_a", "StrongPass123!").unwrap();
        
        // Verify password is set
        let user = find_user(&conn, "john", "company_a").unwrap();
        assert!(user.password_hash.is_some());
    }
    
    #[test]
    fn test_incremental_sync_no_duplicates() {
        let state = setup_test_state();
        
        // Pull data
        let result = pull_historical_data(&state, "company_a", &DateRange::Last30Days).unwrap();
        assert_eq!(result.new_inserted, 10);
        
        // Pull again - should be 0 new
        let result = pull_historical_data(&state, "company_a", &DateRange::Last30Days).unwrap();
        assert_eq!(result.new_inserted, 0);
        assert_eq!(result.duplicates_skipped, 10);
    }
}
```

### 6.2 Integration Tests

```rust
#[test]
fn test_multi_pc_sync() {
    let state_a = setup_test_state();  // PC A
    let state_b = setup_test_state();  // PC B
    
    // Create trip on PC A
    create_trip(&state_a, "AAA111", "company_a").unwrap();
    
    // PC B should see it after sync
    pull_updates(&state_b, "company_a").unwrap();
    let trips = get_trips(&state_b, "company_a").unwrap();
    assert_eq!(trips.len(), 1);
}

#[test]
fn test_machine_status_tracking() {
    let state = setup_test_state();
    
    // Simulate PC heartbeat
    update_machine_status(&state, "user_1").unwrap();
    
    // Check status
    let status = get_machine_status(&state, "admin_1").unwrap();
    assert!(status.iter().any(|m| m.is_online));
}
```

### 6.3 Load Testing

```bash
# Simulate 100 concurrent users
for i in {1..100}; do
    cargo test --test load_test -- --nocapture &
done
wait

# Measure sync time
time cargo test --test sync_performance
```

---

## Implementation Order

| Week | Phase | Tasks | Deliverables |
|------|-------|-------|--------------|
| 1 | PG Sync | Timeout, retries, backoff, keepalive | Reliable PG sync |
| 2 | Multi-PC | Schema, heartbeat, config sync | Cross-PC data flow |
| 3 | Users | Username per company, password flow | User management |
| 4 | Reporting | Load from Archive, date ranges, dedup | Historical reports |
| 5 | Dashboard | Machine status, role assignment | Admin visibility |
| 6 | Testing | Unit, integration, load tests | Stress-tested system |

---

## Success Criteria

| Metric | Target |
|--------|--------|
| PG sync timeout | < 3 seconds on failure |
| Multi-PC sync delay | < 30 seconds |
| Duplicate prevention | 0 duplicates |
| Username uniqueness | Per company, enforced by DB |
| Load test | 100 concurrent users, no crashes |
| Uptime | 99.9% (no hangs) |

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| PG connection unstable | 3s timeout, 1 attempt, no backoff |
| Network partitions | SQLite cache, auto-reconnect |
| Duplicate data | UNIQUE constraints, dedup logic |
| Concurrent edits | Last-write-wins (simple) |
| Data loss | SQLite + PG dual storage |

---

## Files to Modify

| File | Changes |
|------|---------|
| `src-tauri/src/sync.rs` | Timeout, retries, backoff, keepalive, incremental sync |
| `src-tauri/src/commands.rs` | Login, user management, machine status |
| `src-tauri/src/reporting.rs` | Load from Archive, date ranges |
| `src-tauri/src/models.rs` | DateRange, SyncStatus, MachineStatus |
| `src-tauri/src/db.rs` | Schema updates, migrations |
| `src-tauri/src/lib.rs` | Keepalive pinger, heartbeat |
| `tests/` | New test files for each phase |

---

## Next Steps

1. **Review this plan** with the team
2. **Approve architecture** (PostgreSQL as hub, SQLite as cache)
3. **Start Phase 1** (PostgreSQL sync reliability)
4. **Iterate** based on testing results

---

*Plan version: 1.0*
*Last updated: 2026-09-02*
