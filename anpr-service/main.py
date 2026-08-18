"""TruckFlow ANPR service — SORT-tracked plate recognition.

A small, self-contained service that pulls frames from a camera source, tracks
vehicles with SORT, reads plates, and exposes the pipeline over HTTP for the
TruckFlow desktop app (which polls `127.0.0.1:9800`).

Design follows 09-anpr-page-complete-spec.md §4:
  - One tracking ID per physical vehicle-in-view (SORT).
  - Multiple OCR attempts on the same track never create multiple records:
    each track keeps exactly one *best reading* slot (highest confidence wins,
    lower-confidence reads are discarded).
  - A sighting is finalized only when the tracked vehicle leaves frame (track
    loss). One vehicle in frame for 5 seconds or 30 minutes still produces
    exactly one sighting.
  - Video files stop cleanly at EOF (no auto-loop); a manual restart is a
    fully fresh tracking session.

Endpoints (all JSON unless noted):
  GET /status          pipeline status (running, fps, counts, uptime…)
  GET /latest          most recent finalized sighting (AnprRead shape, deduped
                       by timestamp so the app's poll sees it exactly once)
  GET /sightings       full list of finalized sightings (track id, entry/exit)
  GET /preview         MJPEG stream of the annotated live feed
  GET /preview_frame   single annotated JPEG frame
  GET /health          {"ok": true}

Run:
  python main.py --source http://192.168.1.5:8080/videofeed   # phone (MJPEG)
  python main.py --source rtsp://user:pass@cam:554/stream1    # real CCTV (RTSP)
  python main.py --source C:/path/to/video.mp4                # dev video file
  python main.py --source usb:0                               # webcam
  python main.py --source file --mock                         # no OCR deps, dev
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import math
import os
import sys
import threading
import time
from datetime import datetime, timezone

import numpy as np

from sort import Sort

# ---------------------------------------------------------------------------
# OCR backend — real engines when available, deterministic mock otherwise
# ---------------------------------------------------------------------------

PLATE_FORMAT = "K{:03d}{}{:02d}"  # Kenyan-style: K123AB45


class OcrBackend:
    """Abstraction over plate OCR. Tries easyocr -> paddleocr -> mock."""

    def __init__(self, force_mock: bool = False):
        self.name = "mock"
        self.reader = None
        self.paddle = None
        self.model_version = None
        if force_mock:
            return
        try:
            import easyocr  # type: ignore

            self.reader = easyocr.Reader(["en"], gpu=False, verbose=False)
            self.name = "easyocr"
            self.model_version = "easyocr-en"
            return
        except Exception:
            pass
        try:
            from paddleocr import PaddleOCR  # type: ignore

            self.paddle = PaddleOCR(use_angle_cls=False, lang="en", show_log=False)
            self.name = "paddleocr"
            self.model_version = "paddleocr-en"
            return
        except Exception:
            pass
        self.name = "mock"

    def read(self, frame: np.ndarray) -> list[tuple[str, float]]:
        """Return [(plate_text, confidence)] for the plates visible in frame."""
        if self.name == "mock":
            return self._mock_read(frame)
        if self.reader is not None:
            return self._easyocr_read(frame)
        return self._paddle_read(frame)

    def _easyocr_read(self, frame: np.ndarray) -> list[tuple[str, float]]:
        results = []
        try:
            for (_, text, conf) in self.reader.readtext(frame, detail=1):
                plate = "".join(ch for ch in text.upper() if ch.isalnum())
                if len(plate) >= 4 and conf >= 0.35:
                    results.append((plate, float(conf)))
        except Exception:
            pass
        return results

    def _paddle_read(self, frame: np.ndarray) -> list[tuple[str, float]]:
        results = []
        try:
            out = self.paddle.ocr(frame, cls=False)
            for line in out or []:
                for item in line or []:
                    box, (text, conf) = item[0], item[1]
                    plate = "".join(ch for ch in str(text).upper() if ch.isalnum())
                    if len(plate) >= 4 and conf >= 0.35:
                        results.append((plate, float(conf)))
        except Exception:
            pass
        return results

    # A deterministic synthetic plate so the whole pipeline (tracking,
    # finalization, /latest, evidence frames) is runnable with zero OCR
    # dependencies. The plate is derived from the frame hash, so the same
    # region in the same video yields a stable reading — enough to demo
    # tracking and dedup end to end.
    def _mock_read(self, frame: np.ndarray) -> list[tuple[str, float]]:
        gray = frame.mean(axis=2) if frame.ndim == 3 else frame
        small = gray[::8, ::8]
        h = int(hash(small.tobytes()) % 0xFFFFFFF)
        plate = PLATE_FORMAT.format(h % 1000, chr(65 + (h // 1000) % 26), h % 100)
        # One "vehicle" per frame region with varied confidence.
        conf = 0.55 + (h % 30) / 100.0
        return [(plate, conf)]


# ---------------------------------------------------------------------------
# OCR throttling + crop sizing
# ---------------------------------------------------------------------------

# Frames between OCR attempts on the same track. The tracker runs every frame
# (fast); OCR runs every N frames per track (slow on CPU). 5 keeps ~5 reads/s
# per vehicle — plenty to converge on a best reading.
OCR_INTERVAL = 5


def downscale_crop(crop: np.ndarray, max_width: int = 320) -> np.ndarray:
    """Shrink an OCR crop to a max width, preserving aspect ratio. EasyOCR
    reads small plates fine; feeding it a 1080p crop wastes seconds per call."""
    h, w = crop.shape[:2]
    if w <= max_width or h == 0:
        return crop
    scale = max_width / w
    try:
        import cv2  # type: ignore

        return cv2.resize(crop, (max_width, max(1, int(h * scale))))
    except Exception:
        return crop


# ---------------------------------------------------------------------------
# Plate detection — contour heuristics over the frame (boxes for the tracker)
# ---------------------------------------------------------------------------

def detect_plate_boxes(frame: np.ndarray) -> list[tuple[int, int, int, int]]:
    """Return (x1, y1, x2, y2) candidate plate boxes.

    Uses high-contrast rectangular blobs. This is intentionally simple — real
    deployments swap in the detector that ships with the chosen OCR engine
    (the boxes only feed the tracker; the OCR backend reads the crop).
    """
    try:
        import cv2  # type: ignore
    except Exception:
        # No OpenCV: treat the whole lower-middle band as one "vehicle" so the
        # tracker still demonstrates correctly in mock mode.
        h, w = frame.shape[:2]
        return [(int(w * 0.15), int(h * 0.45), int(w * 0.85), int(h * 0.95))]

    gray = cv2.cvtColor(frame, cv2.COLOR_BGR2GRAY) if frame.ndim == 3 else frame
    gray = cv2.GaussianBlur(gray, (3, 3), 0)
    _, thresh = cv2.threshold(gray, 0, 255, cv2.THRESH_BINARY + cv2.THRESH_OTSU)
    contours, _ = cv2.findContours(thresh, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    boxes: list[tuple[int, int, int, int]] = []
    h, w = gray.shape[:2]
    for c in contours:
        x, y, bw, bh = cv2.boundingRect(c)
        area = bw * bh
        if area < (w * h) * 0.0015 or area > (w * h) * 0.25:
            continue
        aspect = bw / max(1, bh)
        if 1.6 < aspect < 6.5 and 12 < bh < h * 0.4:
            boxes.append((x, y, x + bw, y + bh))
    return boxes


# ---------------------------------------------------------------------------
# The tracking pipeline
# ---------------------------------------------------------------------------

class TrackedReading:
    """One best-reading slot per SORT track id (09 §4.1 — never more)."""

    def __init__(self, track_id: int):
        self.track_id = track_id
        self.plate: str | None = None
        self.confidence: float = 0.0
        self.first_seen = time.time()
        self.last_seen = self.first_seen
        self.best_frame: np.ndarray | None = None  # annotated crop at best read
        self.last_ocr_frame = -100  # throttle: frame number of the last OCR attempt

    def consider(self, plate: str, confidence: float, frame: np.ndarray) -> None:
        self.last_seen = time.time()
        if confidence > self.confidence:  # highest-confidence read wins
            self.plate = plate
            self.confidence = confidence
            self.best_frame = frame


class Pipeline:
    """Owns the capture thread, SORT tracker and the finalized sighting list."""

    def __init__(self, source: str, source_type: str, ocr: OcrBackend, port: int):
        self.source = source
        self.source_type = source_type
        self.ocr = ocr
        self.port = port
        self.sort = Sort(max_age=12, min_hits=2)
        self.frame_num = 0
        self.readings: dict[int, TrackedReading] = {}
        self.sightings: list[dict] = []
        self.last_emitted: dict | None = None  # last /latest payload (dedup by ts)
        self.latest: dict | None = None
        self.start_time = time.time()
        self.frames_processed = 0
        self.fps = 0.0
        self.running = False
        self.lock = threading.Lock()
        self.current_frame: np.ndarray | None = None
        self._frame_key = 0

    # -- capture ------------------------------------------------------------

    def capture_frames(self) -> None:
        """Pull frames from the configured source and feed the tracker."""
        cap = self._open_capture()
        if cap is None:
            return
        try:
            self.running = True
            loop_start = time.time()
            while True:
                ok, frame = cap.read()
                if not ok:
                    break
                self.frames_processed += 1
                self.frame_num += 1
                self.current_frame = frame
                self.tick(frame)
                # Throttle to ~10 fps for stability (mock/demo friendly).
                time.sleep(0.05)
            cap.release()
        except Exception:
            pass
        finally:
            self.running = False
            # EOF reached: every remaining track leaves frame -> finalize all.
            self.finalize_all()

    def _open_capture(self):
        try:
            import cv2  # type: ignore
        except Exception:
            if self.source_type in ("http", "rtsp", "usb", "video_file"):
                print("OpenCV (cv2) is required for this source. Install: pip install opencv-python")
                return None
            return None
        if self.source_type == "video_file":
            return cv2.VideoCapture(self.source)
        if self.source_type == "usb":
            return cv2.VideoCapture(int(self.source))
        if self.source_type in ("http", "rtsp", "live_test"):
            cap = cv2.VideoCapture(self.source)
            return cap
        return None

    # -- one frame ----------------------------------------------------------

    def tick(self, frame: np.ndarray) -> None:
        boxes = detect_plate_boxes(frame)
        dets = np.array(boxes, dtype=float).reshape(-1, 4) if boxes else np.empty((0, 4))
        tracked = self.sort.update(dets)

        # Collect best reading for every tracked box by cropping + OCR.
        # OCR is throttled per track: a track only needs its highest-confidence
        # read, so re-reading every frame just burns CPU (EasyOCR on CPU is
        # ~1s/call). Every OCR_INTERVAL frames per track is plenty and keeps
        # the live pipeline fast.
        for row in tracked:
            x1, y1, x2, y2, tid = (int(v) for v in row)
            if x2 <= x1 or y2 <= y1:
                continue
            slot = self.readings.setdefault(tid, TrackedReading(tid))
            if self.frame_num - slot.last_ocr_frame < OCR_INTERVAL:
                continue
            slot.last_ocr_frame = self.frame_num
            crop = frame[max(0, y1):min(frame.shape[0], y2), max(0, x1):min(frame.shape[1], x2)]
            if crop.size == 0:
                continue
            crop = downscale_crop(crop, max_width=320)
            reads = self.ocr.read(crop)
            for plate, conf in reads:
                slot.consider(plate, conf, crop)

        # Tracks the tracker dropped (vehicle left frame) -> finalize sightings.
        live_ids = {int(r[4]) for r in tracked}
        for tid in list(self.readings.keys()):
            if tid not in live_ids:
                self.finalize_track(tid)

    def finalize_track(self, tid: int) -> None:
        slot = self.readings.pop(tid, None)
        if slot is None or not slot.plate:
            return
        ts = datetime.now(timezone.utc).isoformat()
        sighting = {
            "track_id": tid,
            "plate": slot.plate,
            "confidence": round(slot.confidence, 3),
            "timestamp": ts,
            "entry_exit": self._classify(slot.plate),
        }
        with self.lock:
            self.sightings.append(sighting)
            self.latest = self._to_read(sighting, slot)
        # A new finalized reading becomes the next /latest payload.
        self.last_emitted = None

    def finalize_all(self) -> None:
        for tid in list(self.readings.keys()):
            self.finalize_track(tid)

    # Entry/exit classification (09 §4.3): the app's trips table is the
    # authority, but the service hints entry/exit by alternating per-plate open
    # state, so the preview and /sightings are meaningful standalone.
    def _classify(self, plate: str) -> str:
        with self.lock:
            open_plates = {s["plate"] for s in self.sightings if s.get("entry_exit") == "entry"}
        if plate in open_plates:
            return "exit"
        return "entry"

    def _to_read(self, sighting: dict, slot: TrackedReading) -> dict:
        frame_b64 = None
        if slot.best_frame is not None:
            try:
                import cv2  # type: ignore

                ok, buf = cv2.imencode(".jpg", slot.best_frame)
                if ok:
                    frame_b64 = base64.b64encode(buf.tobytes()).decode()
            except Exception:
                pass
        return {
            "plate": sighting["plate"],
            "confidence": sighting["confidence"],
            "timestamp": sighting["timestamp"],
            "frames": [
                {
                    "index": 0,
                    "captured_at": sighting["timestamp"],
                    "kind": "entry" if sighting["entry_exit"] == "entry" else "exit",
                    "data": frame_b64,
                }
            ],
            "model_version": self.ocr.model_version,
            "ocr_engine": self.ocr.name,
        }

    # -- status / preview ----------------------------------------------------

    def status(self) -> dict:
        fps = self.frames_processed / max(1.0, time.time() - self.start_time)
        return {
            "running": self.running,
            "source_type": self.source_type,
            "source_url": self.source,
            "models_loaded": self.ocr.name != "mock",
            "plates_detected": len(self.sightings),
            "frames_processed": self.frames_processed,
            "fps": round(fps, 1),
            "last_plate_time": self.latest["timestamp"] if self.latest else None,
            "uptime_seconds": int(time.time() - self.start_time),
            "ocr_engine": self.ocr.name,
        }

    def latest_payload(self) -> dict | None:
        """Return the latest finalized reading exactly once per sighting."""
        with self.lock:
            if self.last_emitted is self.latest:
                return None
            self.last_emitted = self.latest
            return self.latest

    def annotate(self, frame: np.ndarray) -> np.ndarray:
        """Draw track boxes + best plates on a copy for the preview feed."""
        try:
            import cv2  # type: ignore
        except Exception:
            return frame
        out = frame.copy()
        for tid, slot in self.readings.items():
            color = (80, 220, 120) if slot.plate else (220, 180, 60)
            # Re-derive box from the tracker for drawing.
            for row in self.sort.trackers:
                if row.id == tid:
                    x1, y1, x2, y2 = (int(v) for v in row.bbox)
                    cv2.rectangle(out, (x1, y1), (x2, y2), color, 2)
                    label = f"#{tid} {slot.plate or '…'}"
                    cv2.putText(out, label, (x1, max(12, y1 - 6)), cv2.FONT_HERSHEY_SIMPLEX, 0.5, color, 2)
                    break
        return out


# ---------------------------------------------------------------------------
# HTTP server (stdlib — no framework dependency)
# ---------------------------------------------------------------------------

def _jpeg(frame: np.ndarray) -> bytes:
    import cv2  # type: ignore

    ok, buf = cv2.imencode(".jpg", frame, [int(cv2.IMWRITE_JPEG_QUALITY), 80])
    if not ok:
        return b""
    return buf.tobytes()


def serve(pipeline: Pipeline, port: int) -> None:
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):  # keep the console quiet
            pass

        def _json(self, obj: dict | list, code: int = 200):
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            path = self.path.split("?")[0]
            if path == "/status":
                self._json(pipeline.status())
            elif path == "/latest":
                payload = pipeline.latest_payload()
                if payload is None:
                    self._json({"plate": "", "confidence": 0, "timestamp": "", "frames": [], "model_version": None, "ocr_engine": None})
                else:
                    self._json(payload)
            elif path == "/sightings":
                with pipeline.lock:
                    self._json(pipeline.sightings)
            elif path == "/health":
                self._json({"ok": True})
            elif path == "/preview_frame":
                frame = pipeline.current_frame
                if frame is None:
                    self.send_response(503)
                    self.end_headers()
                    return
                data = _jpeg(pipeline.annotate(frame))
                self.send_response(200)
                self.send_header("Content-Type", "image/jpeg")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            elif path == "/preview":
                self._stream_mjpeg()
            else:
                self._json({"error": "not found"}, 404)

        def _stream_mjpeg(self):
            self.send_response(200)
            self.send_header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
            self.end_headers()
            try:
                while True:
                    frame = pipeline.current_frame
                    if frame is not None:
                        data = _jpeg(pipeline.annotate(frame))
                        self.wfile.write(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n")
                        self.wfile.write(data)
                        self.wfile.write(b"\r\n")
                    time.sleep(0.1)
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"ANPR service listening on http://127.0.0.1:{port}")
    server.serve_forever()


# ---------------------------------------------------------------------------

def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="TruckFlow ANPR service (SORT-tracked)")
    p.add_argument("--source", default="http://127.0.0.1:8080/videofeed", help="rtsp://…, http://…, video file path, or usb:N")
    p.add_argument("--type", default=None, choices=["rtsp", "http", "video_file", "usb", "live_test"], help="override source type inference")
    p.add_argument("--port", type=int, default=9800)
    p.add_argument("--mock", action="store_true", help="force the deterministic mock OCR (no model downloads)")
    return p.parse_args(argv)


def infer_type(source: str) -> str:
    if source.startswith("rtsp://"):
        return "rtsp"
    if source.startswith("http://") or source.startswith("https://"):
        return "http"
    if source.startswith("usb:"):
        return "usb"
    return "video_file"


def main() -> None:
    args = parse_args()
    source_type = args.type or infer_type(args.source)
    ocr = OcrBackend(force_mock=args.mock)
    print(f"OCR backend: {ocr.name}" + ("  (real engines not installed — mock readings)" if ocr.name == "mock" else ""))
    pipeline = Pipeline(args.source, source_type, ocr, args.port)

    capture_thread = threading.Thread(target=pipeline.capture_frames, daemon=True)
    capture_thread.start()
    serve(pipeline, args.port)


if __name__ == "__main__":
    main()
