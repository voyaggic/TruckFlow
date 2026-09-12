import json
import urllib.request
import ssl

PAT = "sbp_fce6bae6f9011ba0e631b8e074b4dd8c8b9dc80a"
SVC_KEY = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InlucGRvY3RxZ3dyZWhjdmRic3l5Iiwicm9sZSI6InNlcnZpY2Vfcm9sZSIsImlhdCI6MTc4ODQ2MDA1NiwiZXhwIjoyMTA0MDM2MDU2fQ.YwbJTErt9Z0qqae1_CvtLDBTIQMuuEJLprc0cxGXANI"
PROJECT_REF = "ynpdoctqgwrehcvdbsyy"
ctx = ssl.create_default_context()

def push(table, rows):
    if not rows:
        return
    data = json.dumps(rows).encode()
    url = f"https://{PROJECT_REF}.supabase.co/rest/v1/{table}"
    req = urllib.request.Request(url, data=data, method="POST")
    req.add_header("apikey", SVC_KEY)
    req.add_header("Authorization", f"Bearer {SVC_KEY}")
    req.add_header("Content-Type", "application/json")
    req.add_header("Prefer", "return=minimal,resolution=merge-duplicates")
    try:
        resp = urllib.request.urlopen(req, timeout=60, context=ctx)
        print(f"  {table}: {resp.status} OK ({len(rows)} rows)")
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        print(f"  {table}: {e.code} {body[:500]}")
    except Exception as e:
        print(f"  {table}: ERROR {e}")

def push_filtered(table, rows, allowed_cols):
    filtered = []
    for row in rows:
        filtered.append({k: v for k, v in row.items() if k in allowed_cols})
    push(table, filtered)

import sqlite3
DB_PATH = r"C:\Users\voyya\AppData\Roaming\com.truckflow.app\truckflow.db"

def read_table(conn, table):
    try:
        cur = conn.execute(f"SELECT * FROM {table}")
        cols = [d[0] for d in cur.description]
        rows = []
        for row in cur.fetchall():
            obj = {}
            for i, col in enumerate(cols):
                val = row[i]
                if val is None:
                    obj[col] = None
                elif isinstance(val, bytes):
                    continue
                else:
                    obj[col] = val
            rows.append(obj)
        return rows
    except Exception as e:
        print(f"  {table}: read error {e}")
        return []

def main():
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = None

    # Users: only send columns that exist in Supabase users table
    users_cols = {"id", "name", "auth_type", "credential_hash", "status",
                  "revoked_by", "revoked_at", "profile_photo_ref", "phone_number",
                  "theme_mode", "theme_accent", "language_preference",
                  "created_at", "updated_at"}

    rows = read_table(conn, "users")
    print(f"users: {len(rows)} rows")
    push_filtered("users", rows, users_cols)

    # Trips: only send columns that exist in Supabase trips table
    trips_cols = {"id", "vehicle_id", "driver_id", "company_id", "capacity_at_trip",
                  "time_in", "receipt_no", "officer_id", "capture_method",
                  "confidence_score", "photo_refs", "status", "resolution_notes",
                  "pushed_to_sheets", "created_at", "updated_at", "synced",
                  "is_discharge_trip", "model_version", "ocr_engine",
                  "capacity_unit", "entry_time", "exit_time", "trip_status",
                  "entry_photo_refs", "exit_photo_refs", "sheet_row",
                  "sheet_exit_pushed"}

    rows = read_table(conn, "trips")
    print(f"trips: {len(rows)} rows")
    push_filtered("trips", rows, trips_cols)

    conn.close()

    # Verify
    print("\n=== Verifying ===")
    for table in ["companies", "drivers", "vehicles", "users", "trips"]:
        url = f"https://{PROJECT_REF}.supabase.co/rest/v1/{table}?select=*&limit=0"
        req = urllib.request.Request(url)
        req.add_header("apikey", SVC_KEY)
        req.add_header("Authorization", f"Bearer {SVC_KEY}")
        req.add_header("Prefer", "count=exact")
        try:
            resp = urllib.request.urlopen(req, timeout=10, context=ctx)
            count = resp.headers.get("content-range", "0")
            print(f"  {table}: {count}")
        except Exception as e:
            print(f"  {table}: {e}")

if __name__ == "__main__":
    main()
