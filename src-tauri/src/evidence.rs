//! Photo/frame evidence — every trip, always, retains multiple frames
//! (04-capture-pipeline.md §7.4). Frames from the ANPR service carry a base64
//! image payload which is written to the local filesystem; `photo_refs` on the
//! `trips` row stores the relative file paths plus per-frame metadata. The
//! simulator substitutes distinct placeholder images so the pipeline is
//! testable with zero camera hardware.

use std::path::{Path, PathBuf};

use base64::Engine;
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::models::{AnprFrame, FrameEvidence};

/// Returns the default frames directory (app_data_dir/frames).
/// Used by migration code that doesn't have access to AppState.
pub fn default_frames_dir() -> PathBuf {
    // Mirrors the logic in db::init_state: app_data_dir().join("frames")
    // On Windows: C:\Users\<user>\AppData\Roaming\com.truckflow.app\frames
    // On Linux: ~/.local/share/com.truckflow.app/frames
    if let Some(home) = std::env::var_os("APPDATA").map(PathBuf::from) {
        home.join("com.truckflow.app").join("frames")
    } else if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        home.join(".local").join("share").join("com.truckflow.app").join("frames")
    } else {
        PathBuf::from("frames")
    }
}

const PLACEHOLDER_PNGS: &[&str] = &[
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAAXNSR0IArs4c6QAAAARnQU1BAACxjwv8YQUAAAAJcEhZcwAADsMAAA7DAcdvqGQAAAANSURBVBhXY+CNXv8fAAOnAhfSo4paAAAAAElFTkSuQmCC",
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAAXNSR0IArs4c6QAAAARnQU1BAACxjwv8YQUAAAAJcEhZcwAADsMAAA7DAcdvqGQAAAANSURBVBhXY+ArmfAfAAO4AhKkJl85AAAAAElFTkSuQmCC",
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAAXNSR0IArs4c6QAAAARnQU1BAACxjwv8YQUAAAAJcEhZcwAADsMAAA7DAcdvqGQAAAANSURBVBhXYxBriPoPAAOQAfAuOHsIAAAAAElFTkSuQmCC",
];

/// Write every frame of a read to `frames_dir/<trip_id>/<kind>/` and return the
/// JSON `photo_refs` payload (array of {index, captured_at, kind, file}). Frames
/// with a base64 payload are decoded; simulator frames get a placeholder.
/// `kind` is "entry" or "exit" — the two sighting photo sets are stored and
/// referenced completely separately (09-anpr-page-complete-spec.md §6), never
/// merged.
pub fn persist_frames(frames_dir: &Path, trip_id: &str, frames: &[AnprFrame], kind: &str) -> Result<String, String> {
    if frames.is_empty() {
        return Ok("[]".to_string());
    }
    let trip_dir = frames_dir.join(trip_id).join(kind);
    std::fs::create_dir_all(&trip_dir).map_err(|e| format!("frame dir create failed: {e}"))?;

    let mut entries: Vec<Value> = Vec::with_capacity(frames.len());
    for frame in frames {
        let file = format!("frame_{}.png", frame.index);
        let bytes = match &frame.data {
            Some(raw) if !raw.is_empty() => base64::engine::general_purpose::STANDARD
                .decode(raw)
                .map_err(|_| format!("frame {} has invalid base64 payload", frame.index))?,
            _ => {
                let idx = (frame.index % PLACEHOLDER_PNGS.len() as u32) as usize;
                base64::engine::general_purpose::STANDARD
                    .decode(PLACEHOLDER_PNGS[idx])
                    .map_err(|e| format!("placeholder decode failed: {e}"))?
            }
        };
        std::fs::write(trip_dir.join(&file), &bytes).map_err(|e| format!("frame write failed: {e}"))?;
        entries.push(serde_json::json!({
            "index": frame.index,
            "captured_at": frame.captured_at,
            "kind": frame.kind,
            "file": trip_dir.join(&file).to_string_lossy().to_string(),
        }));
    }
    serde_json::to_string(&entries).map_err(|e| format!("photo_refs serialize failed: {e}"))
}

/// Load a trip's frames back as display payloads (base64) for the UI.
pub fn load_frames(frames_dir: &Path, photo_refs: &str) -> Result<Vec<FrameEvidence>, String> {
    let refs: Vec<Value> = serde_json::from_str(photo_refs).unwrap_or_default();
    let mut out = Vec::with_capacity(refs.len());
    for entry in refs {
        let index = entry.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let captured_at = entry.get("captured_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let file = entry.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let data_base64 = if file.is_empty() {
            None
        } else {
            read_file_base64(&frames_dir.join(file))
        };
        out.push(FrameEvidence { index, captured_at, kind, data_base64 });
    }
    Ok(out)
}

fn read_file_base64(path: &PathBuf) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() || bytes.len() > 3_000_000 {
        return None;
    }
    Some(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

/// Resolve a trip's photo refs (entry + exit, kept separate per §6) to its
/// evidence frames (used by commands). Entry frames come first, then exit
/// frames — each keeps its own `kind` label so the UI never merges them.
pub fn trip_evidence(conn: &Connection, frames_dir: &Path, trip_id: &str) -> Result<Vec<FrameEvidence>, String> {
    let (entry_refs, exit_refs): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT COALESCE(entry_photo_refs, photo_refs, '[]'), COALESCE(exit_photo_refs, '[]')
             FROM trips WHERE id = ?1",
            params![trip_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "Trip not found.".to_string())?;
    let mut out = load_frames(frames_dir, entry_refs.as_deref().unwrap_or("[]"))?;
    out.extend(load_frames(frames_dir, exit_refs.as_deref().unwrap_or("[]"))?);
    Ok(out)
}
