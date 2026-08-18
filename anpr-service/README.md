# TruckFlow ANPR service

The recognition engine behind the ANPR page. It pulls frames from a camera
source, tracks vehicles with **SORT** (one tracking ID per physical vehicle in
view), reads plates, and exposes the pipeline over HTTP for the desktop app
(which polls `127.0.0.1:9800`).

This implements the tracking rules of `09-anpr-page-complete-spec.md` §4:

- **One tracking ID per vehicle-in-view** — never per frame. The earlier
  "2,555 plates from a 1-minute video" bug cannot happen: every track keeps
  exactly one *best reading* slot, and only the highest-confidence OCR read
  ever overwrites it.
- **A sighting finalizes when the vehicle leaves frame** (track loss). A
  vehicle stationary for 30 minutes still produces exactly one sighting.
- **Video files stop cleanly at EOF** — no auto-loop. Restarting the service
  is always a fresh tracking session.

## Install

```bash
pip install -r requirements.txt
```

`numpy` + `opencv-python` are required. For real plate reading install an OCR
engine (recommended for this project: **PaddleOCR** or **EasyOCR**). With no
OCR engine installed the service runs in deterministic **mock mode** — the
whole pipeline (tracking, finalization, `/latest`, evidence frames) still
works, but plates are synthetic.

## Run

```bash
# Phone as CCTV (IP Webcam app) — the recommended faithful test is RTSP mode:
python main.py --source rtsp://192.168.1.5:8554/h264_pcm.sdp

# Phone MJPEG (IP Webcam default) — quick preview only:
python main.py --source http://192.168.1.5:8080/videofeed

# Real CCTV / NVR (RTSP):
python main.py --source "rtsp://user:password@camera-ip:554/stream1"

# USB webcam:
python main.py --source usb:0

# Video file (dev/testing):
python main.py --source C:/Users/you/Downloads/test_video.mp4

# Force mock OCR (no downloads, deterministic plates):
python main.py --source C:/path/video.mp4 --mock
```

## Endpoints

| Endpoint | Purpose |
|---|---|
| `GET /status` | pipeline status: running, fps, plates detected, frames, uptime, OCR engine |
| `GET /latest` | most recent finalized sighting (exactly once per sighting — the app dedupes by timestamp) |
| `GET /sightings` | full list of finalized sightings with track ids and entry/exit hints |
| `GET /preview` | MJPEG stream of the annotated feed (boxes + track ids + best plates) |
| `GET /preview_frame` | single annotated JPEG frame |
| `GET /health` | `{"ok": true}` |

## Notes

- The desktop app's **Diagnostics** tab reports whether this service is
  reachable, whether ffmpeg/OpenCV are installed, and storage usage.
- The **Live Preview** tab shows `/preview` (the annotated feed) plus per-camera
  status; the **Settings** tab tests each camera source's connection.
- Entry/exit *matching* against the trips table is done by the desktop app
  (09 §4.3); the service only hints `entry`/`exit` per plate in `/sightings`.
