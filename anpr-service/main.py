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
  python main.py --port 9800                                  # idle (reads config.json)
  python main.py --source http://IP:8080/videofeed            # phone (MJPEG)
  python main.py --source rtsp://user:pass@cam:554/stream1    # real CCTV (RTSP)
  python main.py --source /path/to/video.mp4                  # video file
  python main.py --source usb:0                               # webcam
"""


from __future__ import annotations



# ---------------------------------------------------------------------------
# EasyOCR model path — must be set BEFORE easyocr is imported anywhere.
#
# When running as a PyInstaller-compiled executable, sys.frozen is True and
# sys.executable points to the .exe itself (not python.exe). We look for a
# pre-bundled easyocr_models/ directory alongside the exe and, if found, point
# EasyOCR at it via EASYOCR_MODULE_PATH.
#
# build_anpr.py creates this directory at build time by pre-downloading the
# English model weights. This means:
#   - No internet download on first run for end users.
#   - No silent hang waiting for a 200 MB download to complete.
#   - Works on air-gapped / offline machines.
#
# In dev (running plain `python main.py`), sys.frozen is not set, this block
# is a no-op, and EasyOCR uses its default ~/.EasyOCR/model/ path as normal.
# ---------------------------------------------------------------------------
import os
import sys

_self_dir = os.path.dirname(os.path.abspath(
    sys.executable if getattr(sys, "frozen", False) else __file__
))
_bundled_models = os.path.join(_self_dir, "easyocr_models")
if os.path.isdir(_bundled_models) and "EASYOCR_MODULE_PATH" not in os.environ:
    os.environ["EASYOCR_MODULE_PATH"] = _bundled_models
    print(f"[OCR] Using pre-bundled model weights: {_bundled_models}", flush=True)
elif not os.path.isdir(_bundled_models):
    print(
        "[OCR] No pre-bundled models found — EasyOCR will download models on first use "
        f"(expected at: {_bundled_models})",
        flush=True,
    )

import argparse
import base64
import io
import json
import math
import threading
import queue
import time
from datetime import datetime, timezone

import numpy as np

from sort import Sort

# ---------------------------------------------------------------------------
# OCR backend — real engines when available, deterministic mock otherwise
# ---------------------------------------------------------------------------

PLATE_FORMAT = "K{:03d}{}{:02d}"  # Kenyan-style: K123AB45


def _normalize_cloud_response(data: dict) -> list[tuple[str, float]] | None:
    """Normalize Roboflow workflow JSON → [(plate_text, confidence), ...].

    Handles:
      - { "outputs": [ { "results": [ {"text": "ABC123", "confidence": 0.92} ] } ] }
      - Empty / no results → None (triggers local fallback)

    Update this single function if Roboflow changes their response shape.
    """
    try:
        outputs = data.get("outputs", [])
        if not outputs:
            return None
        results = outputs[0].get("results", [])
        if not results:
            return None
        normalized = []
        for item in results:
            plate = "".join(ch for ch in (item.get("text", "") or "").upper() if ch.isalnum())
            conf = float(item.get("confidence", 0))
            if len(plate) >= 4 and conf >= 0.35:
                normalized.append((plate, conf))
        return normalized if normalized else None
    except Exception:
        return None


class OcrBackend:
    """Abstraction over plate OCR. Tries cloud (if configured) -> paddleocr -> easyocr -> fallback."""

    def __init__(self, prefer_cloud: bool = False, cloud_api_url: str = "", cloud_api_key: str = ""):
        self.name = "none"
        self.reader = None
        self.paddle = None
        self.model_version = None
        self.prefer_cloud = prefer_cloud
        self.cloud_api_url = cloud_api_url.rstrip("/")
        self.cloud_api_key = cloud_api_key
        self._models_loaded = False
        # Do NOT load models here — defer to first read() call
        # This keeps the HTTP server responsive during app startup
        print("[OCR] Engine configured (models will load on first frame)")

    def _ensure_models(self) -> None:
        """Load OCR models on first use (lazy initialization).

        Priority: PaddleOCR (primary) → EasyOCR (fallback) → mock mode.
        PaddleOCR is preferred: faster inference, smaller model footprint,
        no PyTorch dependency.
        """
        if self._models_loaded:
            return
        self._models_loaded = True

        # Try PaddleOCR first (primary engine — faster, better for plates)
        try:
            from paddleocr import PaddleOCR  # type: ignore

            self.paddle = PaddleOCR(use_angle_cls=False, lang="en", show_log=False)
            self.name = "paddleocr"
            self.model_version = "paddleocr-en"
            print("[OCR] PaddleOCR loaded (local)")
            return
        except Exception as e:
            print(f"[OCR] PaddleOCR not available: {e}")

        # Fallback to EasyOCR (only if easyocr is installed)
        try:
            import easyocr  # type: ignore

            self.reader = easyocr.Reader(["en"], gpu=False, verbose=False)
            self.name = "easyocr"
            self.model_version = "easyocr-en"
            print("[OCR] EasyOCR loaded (local, fallback)")
            return
        except Exception as e:
            print(f"[OCR] EasyOCR not available: {e}")

        print("[OCR] WARNING: No OCR engine available — running in mock mode")


    def read(self, frame: np.ndarray) -> list[tuple[str, float]]:
        """Return [(plate_text, confidence)] for the plates visible in frame.

        If cloud is preferred and the API is reachable, use cloud OCR.
        On any failure (timeout, network error, API error), fall back to local.
        Models are loaded lazily on first call.
        """
        self._ensure_models()
        if self.prefer_cloud and self.cloud_api_url and self.cloud_api_key:
            cloud_result = self._cloud_read(frame)
            if cloud_result is not None:
                return cloud_result
            # Cloud failed — fall through to local
        # Local fallback: always available
        if self.paddle is not None:
            return self._paddle_read(frame)
        if self.reader is not None:
            return self._easyocr_read(frame)
        return []

    def _cloud_read(self, frame: np.ndarray) -> list[tuple[str, float]] | None:
        """Try cloud OCR via Roboflow workflow. Returns None on any failure (triggers local fallback)."""
        try:
            import cv2
            _, buf = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 85])
            img_b64 = base64.b64encode(buf.tobytes()).decode("utf-8")

            import urllib.request
            import urllib.error

            # Roboflow Serverless workflow format
            payload = json.dumps({
                "api_key": self.cloud_api_key,
                "inputs": {
                    "image": {
                        "type": "base64",
                        "value": img_b64,
                    }
                },
            }).encode("utf-8")

            req = urllib.request.Request(
                self.cloud_api_url,
                data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=15) as resp:
                data = json.loads(resp.read().decode("utf-8"))

            # Normalize Roboflow response → internal format
            return _normalize_cloud_response(data)
        except Exception as e:
            print(f"[OCR] Cloud failed ({e}), using local fallback")
            return None

    def _easyocr_read(self, frame: np.ndarray) -> list[tuple[str, float]]:
        results = []
        try:
            for (_, text, conf) in self.reader.readtext(frame, detail=1):
                plate = "".join(ch for ch in text.upper() if ch.isalnum())
                if should_accept_plate(plate) and conf >= 0.35:
                    priority = plate_priority(plate)
                    adjusted_conf = float(conf) * (1.0 + priority * 0.1)
                    results.append((plate, adjusted_conf))
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
                    if should_accept_plate(plate) and conf >= 0.35:
                        # Boost confidence for Kenyan-format plates so they
                        # win over partial/garbage reads.
                        priority = plate_priority(plate)
                        adjusted_conf = float(conf) * (1.0 + priority * 0.1)
                        results.append((plate, adjusted_conf))
        except Exception:
            pass
        return results




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
# ── Kenyan plate format validation ─────────────────────────────────────
# Kenyan plates: 3 letters + 3 digits + 1 letter = 7 characters
# e.g. KBA 123A, KBT 456Z, SBA 789C
import re
_KENYAN_PLATE_RE = re.compile(r'^[A-Z]{3}\d{3}[A-Z]$')

def is_valid_plate(plate: str) -> bool:
    """Check if a plate string matches the Kenyan format.

    Kenyan standard: 3 uppercase letters + 3 digits + 1 uppercase letter.
    Also accepts the old 4-char minimum for backward compatibility with
    non-Kenyan plates (CCTV footage, test videos, etc.).
    """
    return bool(_KENYAN_PLATE_RE.match(plate))


# OCR plate mode — set from config.json, read at startup.
# 'universal' = accept any plate format (default)
# 'kenyan'    = only accept Kenyan format (3 letters + 3 digits + 1 letter)
_ocr_plate_mode = 'universal'

def set_ocr_plate_mode(mode: str) -> None:
    global _ocr_plate_mode
    _ocr_plate_mode = mode.lower().strip()
    print(f'[OCR] Plate mode set to: {_ocr_plate_mode}')

def should_accept_plate(plate: str) -> bool:
    """Filter OCR results based on the configured plate mode."""
    if _ocr_plate_mode == 'kenyan':
        return bool(_KENYAN_PLATE_RE.match(plate))
    # universal mode: accept any plate with 4+ chars
    return len(plate) >= 4


def plate_priority(plate: str) -> int:
    """Priority for plate readings — Kenyan format gets highest priority.

    2 = Kenyan format (3 letters + 3 digits + 1 letter)
    1 = Partial match (at least 5 chars, starts with letter)
    0 = Generic (any 4+ chars)
    """
    if _KENYAN_PLATE_RE.match(plate):
        return 2
    if len(plate) >= 5 and plate[0].isalpha():
        return 1
    return 0


# ── Plate detection ─────────────────────────────────────────────────────
# Priority: YOLO (fine-tuned model) → contour heuristics (fallback)

_yolo_model = None  # lazy-loaded singleton

# Detection method settings
_detection_method = 'contour'  # 'contour', 'paddleocr', or 'consecutive'
_consecutive_required = 3  # Number of matching reads required for consecutive method
_consecutive_matches = {}  # track_id -> list of (plate, count) for consecutive detection

def set_detection_method(method: str) -> None:
    """Set the plate detection method."""
    global _detection_method
    method = method.lower().strip()
    if method in ('contour', 'paddleocr', 'consecutive'):
        _detection_method = method
        print(f'[DETECT] Detection method set to: {_detection_method}')
    else:
        print(f'[DETECT] Unknown detection method: {method}')

def get_detection_method() -> str:
    return _detection_method

def _load_yolo():
    """Load the fine-tuned YOLO license plate detector (lazy, once)."""
    global _yolo_model
    if _yolo_model is not None:
        return _yolo_model
    # Search multiple locations for the model
    candidates = [
        os.path.join(_self_dir, 'models', 'license_plate_detector.pt'),
        os.path.join(os.path.dirname(_self_dir), 'anpr-service', 'models', 'license_plate_detector.pt'),
    ]
    model_path = next((p for p in candidates if os.path.exists(p)), None)
    if model_path is None:
        print('[DETECT] YOLO model not found — using contour fallback')
        return None
    try:
        import torch
        # PyTorch 2.6+ requires weights_only=True by default; our model
        # uses ultralytics DetectionModel which needs unpickling.
        _orig_load = torch.load
        def _patched_load(*a, **kw):
            kw.setdefault('weights_only', False)
            return _orig_load(*a, **kw)
        torch.load = _patched_load
        try:
            from ultralytics import YOLO
            _yolo_model = YOLO(model_path)
            print(f'[DETECT] YOLO model loaded: {model_path}')
        finally:
            torch.load = _orig_load
        return _yolo_model
    except Exception as e:
        print(f'[DETECT] YOLO load failed ({e}) — using contour fallback')
        return None


# ── Detection Methods ───────────────────────────────────────────────────

def detect_contour(frame: np.ndarray) -> list[tuple[int, int, int, int]]:
    """Contour-based detection - fast, works at CCTV distances."""
    try:
        import cv2  # type: ignore
    except Exception:
        h, w = frame.shape[:2]
        return [(int(w * 0.15), int(h * 0.45), int(w * 0.85), int(h * 0.95))]

    gray = cv2.cvtColor(frame, cv2.COLOR_BGR2GRAY) if frame.ndim == 3 else frame
    clahe = cv2.createCLAHE(clipLimit=3.0, tileGridSize=(8, 8))
    enhanced = clahe.apply(gray)
    enhanced = cv2.GaussianBlur(enhanced, (3, 3), 0)
    _, thresh_otsu = cv2.threshold(enhanced, 0, 255, cv2.THRESH_BINARY + cv2.THRESH_OTSU)
    thresh_adaptive = cv2.adaptiveThreshold(
        enhanced, 255, cv2.ADAPTIVE_THRESH_GAUSSIAN_C,
        cv2.THRESH_BINARY, 31, 10
    )
    contours_otsu, _ = cv2.findContours(thresh_otsu, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    contours_adaptive, _ = cv2.findContours(thresh_adaptive, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    tagged = [(c, 'otsu') for c in contours_otsu] + [(c, 'adaptive') for c in contours_adaptive]
    boxes: list[tuple[int, int, int, int]] = []
    h, w = gray.shape[:2]
    frame_area = w * h
    for c, source in tagged:
        x, y, bw, bh = cv2.boundingRect(c)
        area = bw * bh
        max_pct = 0.60 if source == 'adaptive' else 0.40
        if area < frame_area * 0.0008 or area > frame_area * max_pct:
            continue
        aspect = bw / max(1, bh)
        if 1.4 < aspect < 7.0 and 8 < bh < h * 0.70:
            boxes.append((x, y, x + bw, y + bh))
    return boxes


def detect_paddleocr(frame: np.ndarray) -> list[tuple[int, int, int, int]]:
    """PaddleOCR-based detection - uses AI to find plate regions."""
    try:
        from paddleocr import PaddleOCR
    except ImportError:
        print('[DETECT] PaddleOCR not available, falling back to contour')
        return detect_contour(frame)

    try:
        import cv2
        result = PaddleOCR(use_angle_cls=False, lang='en', show_log=False).ocr(frame, cls=False)
        boxes = []
        if result and result[0]:
            for line in result[0]:
                if line and len(line) >= 2:
                    bbox = line[0]
                    if bbox:
                        x1 = int(min(p[0] for p in bbox))
                        y1 = int(min(p[1] for p in bbox))
                        x2 = int(max(p[0] for p in bbox))
                        y2 = int(max(p[1] for p in bbox))
                        boxes.append((x1, y1, x2, y2))
        return boxes
    except Exception as e:
        print(f'[DETECT] PaddleOCR detection failed: {e}, falling back to contour')
        return detect_contour(frame)


def reset_consecutive_state():
    """Reset consecutive detection state (call when starting new video/session)."""
    global _consecutive_matches
    _consecutive_matches = {}


def detect_consecutive(frame: np.ndarray, track_id: int = None, plate_read: str = None) -> tuple[list[tuple[int, int, int, int]], bool]:
    """Consecutive reads detection - requires multiple matching reads.

    Returns (boxes, accepted) where accepted is True if this read matches the pattern.
    For use with tracking - returns current boxes and whether to accept this reading.
    """
    global _consecutive_matches

    # First pass: use contour to find candidates
    boxes = detect_contour(frame)

    # If no plate_read provided, just return boxes (initial detection)
    if plate_read is None or track_id is None:
        return boxes, False

    # Check if this read matches consecutive pattern
    if track_id not in _consecutive_matches:
        _consecutive_matches[track_id] = []

    # Add current read
    _consecutive_matches[track_id].append(plate_read)

    # Keep only recent reads for this track
    max_stored = _consecutive_required * 3
    _consecutive_matches[track_id] = _consecutive_matches[track_id][-max_stored:]

    # Check if we have enough consecutive matching reads
    reads = _consecutive_matches[track_id]
    if len(reads) >= _consecutive_required:
        # Count consecutive matches
        consecutive_count = 1
        for i in range(len(reads) - 1, 0, -1):
            if reads[i] == reads[i-1]:
                consecutive_count += 1
            else:
                break

        if consecutive_count >= _consecutive_required:
            return boxes, True

    return boxes, False


def detect_plate_boxes(frame: np.ndarray) -> list[tuple[int, int, int, int]]:
    """Return (x1, y1, x2, y2) candidate plate boxes.

    Dispatches to the configured detection method:
    - 'contour': Fast, works at CCTV distances (default)
    - 'paddleocr': Uses PaddleOCR's AI detection
    - 'consecutive': Uses contour + requires matching reads
    """
    if _detection_method == 'paddleocr':
        return detect_paddleocr(frame)
    else:
        # contour and consecutive both use contour detection initially
        return detect_contour(frame)


# Legacy function kept for compatibility
def detect_plate_boxes_contour(frame: np.ndarray) -> list[tuple[int, int, int, int]]:
    """Legacy contour detection - use detect_contour() directly instead."""
    return detect_contour(frame)


# ---------------------------------------------------------------------------
# The tracking pipeline
# ---------------------------------------------------------------------------

class TrackedReading:
    """One best-reading slot per SORT track id (09 §4.1 — never more).

    Updated from the OCR worker thread, so mutations are guarded by
    Pipeline.lock (held by the worker when writing, by the capture
    thread when reading)."""

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


# ---------------------------------------------------------------------------
# Custom HTTP MJPEG frame reader — works with IP Webcam and similar servers
# ---------------------------------------------------------------------------

class MjpegFrameReader:
    """Reads JPEG frames from an HTTP multipart MJPEG stream using raw sockets.
    
    IP Webcam sends:
      --boundary\r\nContent-Type: image/jpeg\r\nContent-Length: NNN\r\n\r\n<NNN bytes JPEG>\r\n
    This reader reads the boundary line, parses Content-Length, then reads
    exactly N bytes of JPEG data. Much more reliable than scanning for
    boundary markers in binary data.
    """

    def __init__(self, url: str, timeout: int = 10):
        self.url = url
        self.timeout = timeout
        self._conn = None
        self._resp = None
        self._buf = b''  # leftover bytes from previous reads

    def _connect(self):
        """Open HTTP connection and start reading the MJPEG stream."""
        import urllib.parse
        import http.client

        parsed = urllib.parse.urlparse(self.url)
        host = parsed.hostname
        port = parsed.port or (443 if parsed.scheme == 'https' else 80)
        path = parsed.path or '/'
        if parsed.query:
            path += '?' + parsed.query

        if parsed.scheme == 'https':
            import ssl
            ctx = ssl.create_default_context()
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
            conn = http.client.HTTPSConnection(host, port, timeout=self.timeout, context=ctx)
        else:
            conn = http.client.HTTPConnection(host, port, timeout=self.timeout)

        conn.request('GET', path, headers={
            'User-Agent': 'TruckFlow-ANPR/1.0',
            'Connection': 'keep-alive',
        })
        resp = conn.getresponse()
        if resp.status != 200:
            conn.close()
            raise RuntimeError(f'HTTP {resp.status} from {self.url}')

        content_type = resp.getheader('Content-Type', '')
        if 'multipart' not in content_type and 'jpeg' not in content_type:
            conn.close()
            raise RuntimeError(f'Not an MJPEG stream (Content-Type: {content_type})')

        self._conn = conn
        self._resp = resp
        self._buf = b''
        # Extract boundary from Content-Type
        self._boundary = None
        if 'boundary=' in content_type:
            raw = content_type.split('boundary=')[1].strip().strip('"')
            # IP Webcam sends boundary=--Ba4oTvQMY8ew04N8dcnM (with dashes)
            self._boundary = raw

    def _recv(self, n: int) -> bytes:
        """Read exactly n bytes from the response, using internal buffer."""
        while len(self._buf) < n:
            chunk = self._resp.read(n - len(self._buf))
            if not chunk:
                return self._buf
            self._buf += chunk
        result = self._buf[:n]
        self._buf = self._buf[n:]
        return result

    def _readline(self) -> bytes:
        """Read one line (up to \r\n) from the response."""
        while b'\r\n' not in self._buf:
            chunk = self._resp.read(512)
            if not chunk:
                line = self._buf
                self._buf = b''
                return line
            self._buf += chunk
        idx = self._buf.index(b'\r\n')
        line = self._buf[:idx]
        self._buf = self._buf[idx + 2:]
        return line

    def read_frame(self) -> tuple[bool, np.ndarray | None]:
        """Read one JPEG frame from the stream. Returns (success, frame)."""
        try:
            if self._resp is None:
                self._connect()

            jpeg_data = self._read_multipart_frame()
            if jpeg_data is None or len(jpeg_data) < 100:
                return False, None

            # Decode JPEG bytes to numpy array
            try:
                import cv2  # type: ignore
                arr = np.frombuffer(jpeg_data, dtype=np.uint8)
                frame = cv2.imdecode(arr, cv2.IMREAD_COLOR)
                if frame is None or frame.size == 0:
                    return False, None
                return True, frame
            except Exception:
                return False, None

        except Exception:
            self._close()
            return False, None

    def _read_multipart_frame(self) -> bytes | None:
        """Read one JPEG frame from a multipart MJPEG stream.

        IP Webcam stream format (body starts after HTTP response headers):
          \r\n--boundary\r\n\r\nContent-Type: image/jpeg\r\nContent-Length: NNN\r\n\r\n<NNN bytes JPEG>\r\n
        Strategy: scan buffer for '--boundary', then parse Content-Length
        header, then read exactly N bytes of JPEG data.
        """
        boundary = self._boundary.encode() if self._boundary else b''

        # 1) Fill buffer until we find the boundary marker
        timeout = time.time() + 15
        while time.time() < timeout:
            if boundary in self._buf:
                break
            try:
                chunk = self._resp.read(8192)
            except Exception:
                return None
            if not chunk:
                return None
            self._buf += chunk

        if boundary not in self._buf:
            return None

        # 2) Find boundary position and skip past it
        idx = self._buf.find(boundary)
        rest = self._buf[idx + len(boundary):]

        # 3) Find end of part headers (\r\n\r\n)
        hdr_end = rest.find(b'\r\n\r\n')
        while hdr_end < 0 and time.time() < timeout:
            try:
                chunk = self._resp.read(4096)
            except Exception:
                return None
            if not chunk:
                return None
            rest += chunk
            hdr_end = rest.find(b'\r\n\r\n')

        if hdr_end < 0:
            return None

        # 4) Parse Content-Length from part headers
        headers_raw = rest[:hdr_end].decode('latin-1', errors='replace')
        content_length = None
        for line in headers_raw.split('\r\n'):
            line = line.strip()
            if line.lower().startswith('content-length:'):
                try:
                    content_length = int(line.split(':', 1)[1].strip())
                except ValueError:
                    pass

        # JPEG data starts after the \r\n\r\n separator
        data_start = hdr_end + 4

        # 5) Read exactly content_length bytes of JPEG data
        if content_length and content_length > 0:
            # Ensure we have enough data in the buffer
            while len(rest) - data_start < content_length and time.time() < timeout:
                try:
                    chunk = self._resp.read(content_length - (len(rest) - data_start))
                except Exception:
                    return None
                if not chunk:
                    return None
                rest += chunk

            jpeg_data = rest[data_start:data_start + content_length]
            # Leave everything after the JPEG in the buffer (trailing \r\n + next boundary)
            consumed = idx + len(boundary) + data_start + content_length
            self._buf = self._buf[consumed:]
            return jpeg_data

        # Fallback: no Content-Length — scan for JPEG end marker (FFD9)
        while time.time() < timeout:
            end = rest.find(b'\xff\xd9', data_start)
            if end >= 0:
                jpeg_data = rest[data_start:end + 2]
                consumed = idx + len(boundary) + end + 2
                self._buf = self._buf[consumed:]
                return jpeg_data
            try:
                chunk = self._resp.read(8192)
            except Exception:
                return None
            if not chunk:
                return None
            rest += chunk
        return None

    def _close(self):
        try:
            if self._conn:
                self._conn.close()
        except Exception:
            pass
        self._conn = None
        self._resp = None
        self._buf = b''

    def release(self):
        self._close()


class Pipeline:
    """Owns the capture thread, SORT tracker and the finalized sighting list.

    CAPTURE → DETECTION → TRACKING runs on the capture thread (fast, <5 ms).
    OCR runs on a SEPARATE background worker thread so it never blocks frame
    capture.  This eliminates the ~0.5–1 s stutter that PaddleOCR/EasyOCR
    per-plate latency used to cause in the live preview and ANPR feed.
    """

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
        # ── OCR worker thread (decoupled from capture) ──
        # Jobs are (track_id, crop_ndarray_copy, frame_num).  The capture thread
        # enqueues; the worker dequeues, runs OCR (~0.5–1 s), and updates the
        # TrackedReading under self.lock.  Bounded queue prevents memory
        # blow-up if OCR falls behind.
        self._ocr_queue: queue.Queue = queue.Queue(maxsize=30)
        self._ocr_worker_running = True
        self._ocr_thread = threading.Thread(target=self._ocr_worker, daemon=True, name="ocr-worker")
        self._ocr_thread.start()
        self._frame_key = 0

    # -- capture ------------------------------------------------------------

    def capture_frames(self) -> None:
        """Pull frames from the configured source and feed the tracker.

        For HTTP/HTTPS sources, uses a custom MJPEG reader that parses
        multipart streams directly (OpenCV VideoCapture can't handle these
        on Windows). For other sources (video files, USB, RTSP), uses OpenCV.
        Live sources retry on stream drops with exponential backoff.
        Video files stop cleanly at EOF — no retry loop, no resource drain.
        """
        # Bail immediately if no source configured
        if not self.source or not self.source.strip():
            print("[ANPR] No source configured — capture thread exiting")
            return

        # nvr_export is an NVR video export file — identical semantics to video_file
        is_video_file = self.source_type in ("video_file", "nvr_export")
        max_backoff = 30  # seconds
        attempt = 0
        # Use custom MJPEG reader for HTTP/HTTPS sources
        use_mjpeg = self.source_type == "http" and (
            self.source.startswith("http://") or self.source.startswith("https://")
        )
        while not _shutdown_event.is_set():
            if use_mjpeg:
                ok, frame = self._run_mjpeg_capture(attempt)
            else:
                ok, frame = self._run_opencv_capture()
            if ok:
                break  # EOF / normal exit
            if _shutdown_event.is_set():
                break
            # Video files: EOF reached — finalize tracks and stop cleanly.
            # Do NOT retry: the file is finite, reconnecting would just loop
            # forever, consuming CPU/memory and potentially starving the WebView.
            if is_video_file:
                print("[ANPR] Video file ended — finalizing all tracks and stopping")
                self.finalize_all()
                self.running = False
                break
            # Live source — retry with backoff
            self.running = False
            attempt += 1
            wait = min(max_backoff, 2 ** attempt)
            print(f"[ANPR] Reconnecting in {wait}s (attempt {attempt})...")
            # Sleep in small increments so we can respond to shutdown quickly
            for _ in range(int(wait * 2)):
                if _shutdown_event.is_set():
                    return
                time.sleep(0.5)
            self.sort = Sort(max_age=12, min_hits=2)
            self.readings.clear()

        # Drain any pending OCR jobs and stop the worker thread.
        self._stop_ocr_worker()

    def _run_mjpeg_capture(self, attempt: int) -> tuple[bool, bool]:
        """Run the MJPEG frame reader loop. Returns (eof, error)."""
        try:
            reader = MjpegFrameReader(self.url_for_mjpeg())
            self.running = True
            consecutive_failures = 0
            print(f"[ANPR] Camera connected (MJPEG): {self.source}")
            while not _shutdown_event.is_set():
                ok, frame = reader.read_frame()
                if not ok or frame is None:
                    consecutive_failures += 1
                    if consecutive_failures >= 20:
                        print("[ANPR] MJPEG stream dropped, reconnecting...")
                        reader.release()
                        return False, False
                    time.sleep(0.05)
                    continue
                consecutive_failures = 0
                self.frames_processed += 1
                self.frame_num += 1
                with self.lock:
                    self.current_frame = frame
                self.tick(frame)
                time.sleep(0.05)
        except Exception as e:
            print(f"[ANPR] MJPEG capture error: {e}")
            return False, True
        finally:
            try:
                reader.release()
            except Exception:
                pass
        return True, False

    def _run_opencv_capture(self) -> tuple[bool, bool]:
        """Run the OpenCV capture loop. Returns (eof, error).

        For video files, a single failed read means EOF — return immediately
        so the caller can stop cleanly. For live sources, tolerate a few
        consecutive failures before reporting a dropped stream.
        """
        cap = self._open_capture()
        if cap is None:
            return False, True
        is_video_file = self.source_type in ("video_file", "nvr_export")
        try:
            self.running = True
            consecutive_failures = 0
            print(f"[ANPR] Camera connected: {self.source}")
            while not _shutdown_event.is_set():
                ok, frame = cap.read()
                if not ok:
                    consecutive_failures += 1
                    # Video files: EOF is immediate — one failed read = done.
                    # Live sources: tolerate transient failures before giving up.
                    threshold = 1 if is_video_file else 15
                    if consecutive_failures >= threshold:
                        if is_video_file:
                            # LOOP the video instead of stopping — a finite
                            # file would otherwise freeze on its last frame /
                            # go black after a single play-through.
                            print("[ANPR] Video file reached EOF — looping back to start")
                            cap.set(cv2.CAP_PROP_POS_FRAMES, 0)
                            consecutive_failures = 0
                            time.sleep(0.05)
                            continue
                        print("[ANPR] Stream dropped, reconnecting...")
                        return False, False
                    time.sleep(0.05)
                    continue
                consecutive_failures = 0
                self.frames_processed += 1
                self.frame_num += 1
                with self.lock:
                    self.current_frame = frame
                self.tick(frame)
                time.sleep(0.05)
        except Exception as e:
            print(f"[ANPR] Capture error: {e}")
            return False, True
        finally:
            try:
                cap.release()
            except Exception:
                pass
        return True, False

    def url_for_mjpeg(self) -> str:
        """Return the URL for the MJPEG reader, stripping /videofeed if needed
        and trying HTTPS → HTTP fallback."""
        url = self.source
        if url.startswith("https://"):
            # Try HTTPS first (some IP Webcam servers require it)
            return url
        return url

    def _open_capture(self):
        try:
            import cv2  # type: ignore
        except Exception:
            if self.source_type in ("http", "rtsp", "usb", "video_file", "nvr_export"):
                print("OpenCV (cv2) is required for this source. Install: pip install opencv-python")
                return None
            return None
        # For HTTPS sources with self-signed certs, try FFmpeg first
        if self.source_type == "http" and self.source.startswith("https://"):
            # Try with FFmpeg backend which handles HTTPS better
            os.environ["OPENCV_FFMPEG_CAPTURE_OPTIONS"] = "protocol_whitelist;file,tcp,http,https,rtsp|rtp|udp"
            cap = cv2.VideoCapture(self.source, cv2.CAP_FFMPEG)
            if cap.isOpened():
                cap.set(cv2.CAP_PROP_BUFFERSIZE, 3)
                return cap
            # Fallback: try plain HTTP (some cameras serve both)
            http_url = self.source.replace("https://", "http://")
            print(f"[ANPR] HTTPS failed, trying HTTP: {http_url}")
            cap = cv2.VideoCapture(http_url)
            if cap.isOpened():
                cap.set(cv2.CAP_PROP_BUFFERSIZE, 3)
            return cap
        if self.source_type in ("video_file", "nvr_export"):
            return cv2.VideoCapture(self.source)
        if self.source_type == "usb":
            # CAP_DSHOW matches the detection script and _test_source.py.
            # The default backend (MSMF on Windows) enumerates devices in a
            # different order and often fails on virtual cameras, causing the
            # detected index to open a DIFFERENT physical device at runtime.
            # Accept both bare index ("1") and prefixed ("usb:1").
            idx = int(self.source.removeprefix("usb:"))
            cap = cv2.VideoCapture(idx, cv2.CAP_DSHOW)
            if not cap.isOpened():
                cap.release()
                cap = cv2.VideoCapture(idx)  # fallback to default backend
            if cap.isOpened():
                cap.set(cv2.CAP_PROP_BUFFERSIZE, 3)
            return cap
        if self.source_type in ("http", "rtsp", "live_test"):
            # Set FFmpeg options for network streams
            if self.source.startswith("rtsp://"):
                os.environ["OPENCV_FFMPEG_CAPTURE_OPTIONS"] = "rtsp_transport;tcp"
            cap = cv2.VideoCapture(self.source)
            cap.set(cv2.CAP_PROP_BUFFERSIZE, 3)
            cap.set(cv2.CAP_PROP_OPEN_TIMEOUT_MSEC, 10000)
            cap.set(cv2.CAP_PROP_READ_TIMEOUT_MSEC, 10000)
            return cap
        return None

    # -- one frame ----------------------------------------------------------

    # ── OCR worker thread (runs in background, never blocks capture) ──────

    def _ocr_worker(self) -> None:
        """Background thread that runs OCR on crops enqueued by tick().

        PaddleOCR / EasyOCR takes ~0.5–1 s per plate on CPU.  By running
        on a separate thread, the capture loop continues grabbing frames
        at full camera FPS — eliminating the preview stutter.
        """
        while self._ocr_worker_running or not self._ocr_queue.empty():
            try:
                tid, crop, fnum = self._ocr_queue.get(timeout=0.2)
            except queue.Empty:
                continue
            try:
                reads = self.ocr.read(crop)
                with self.lock:
                    slot = self.readings.get(tid)
                    if slot is not None:
                        for plate, conf in reads:
                            slot.consider(plate, conf, crop)
            except Exception as e:
                print(f"[ANPR] OCR worker error (track {tid}): {e}")
            finally:
                self._ocr_queue.task_done()

    def _stop_ocr_worker(self) -> None:
        """Signal the OCR worker to finish and wait for it."""
        self._ocr_worker_running = False
        try:
            self._ocr_thread.join(timeout=3.0)
        except Exception:
            pass

    # ── capture-thread fast path (detection + tracking, < 5 ms) ──────────

    def tick(self, frame: np.ndarray) -> None:
        """Detect plate candidates, run SORT tracker, enqueue OCR jobs.

        This method runs on the capture thread and MUST stay fast (< 5 ms)
        so frames keep flowing to the preview.  OCR is offloaded to
        _ocr_worker via the bounded queue.
        """
        boxes = detect_plate_boxes(frame)
        dets = np.array(boxes, dtype=float).reshape(-1, 4) if boxes else np.empty((0, 4))
        tracked = self.sort.update(dets)

        # Enqueue OCR jobs for tracks that are due — capture thread never
        # blocks on OCR; the worker handles it in the background.
        for row in tracked:
            x1, y1, x2, y2, tid = (int(v) for v in row)
            if x2 <= x1 or y2 <= y1:
                continue
            with self.lock:
                slot = self.readings.setdefault(tid, TrackedReading(tid))
                if self.frame_num - slot.last_ocr_frame < OCR_INTERVAL:
                    continue
                slot.last_ocr_frame = self.frame_num
            # Pad the crop 50% on each side so OCR sees the FULL plate,
            # not just the contour-detected portion.  A contour might find
            # only the numbers ("878S") while the letters ("KBW") are
            # adjacent — padding gives PaddleOCR the context to read both.
            bw, bh = x2 - x1, y2 - y1
            pad_x, pad_y = bw // 2, bh // 2
            crop = frame[
                max(0, y1 - pad_y):min(frame.shape[0], y2 + pad_y),
                max(0, x1 - pad_x):min(frame.shape[1], x2 + pad_x)
            ]
            if crop.size == 0:
                continue
            # COPY the crop — the frame buffer will be overwritten on next read.
            crop_copy = downscale_crop(crop, max_width=320).copy()
            try:
                self._ocr_queue.put_nowait((tid, crop_copy, self.frame_num))
            except queue.Full:
                pass  # drop this OCR attempt; next frame will retry

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
        # Drain any pending OCR jobs so their results land in readings
        # before we finalize — otherwise the last plate read per track
        # would be lost.
        self._ocr_queue.join()
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
        with self.lock:
            has_frame = self.current_frame is not None
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
            "camera_connected": has_frame,
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
        # Snapshot tracker state under lock to avoid race with capture thread
        with self.lock:
            trackers_snapshot = [(row.id, tuple(int(v) for v in row.bbox)) for row in self.sort.trackers]
            readings_snapshot = dict(self.readings)
        for tid, slot in readings_snapshot.items():
            # Green box if plate detected, yellow if tracking only
            color = (0, 255, 0) if slot.plate else (0, 200, 255)
            thickness = 3 if slot.plate else 2
            for t_id, (x1, y1, x2, y2) in trackers_snapshot:
                if t_id == tid:
                    cv2.rectangle(out, (x1, y1), (x2, y2), color, thickness)
                    # Label with plate and confidence
                    if slot.plate:
                        label = f"{slot.plate} ({slot.confidence:.0%})"
                    else:
                        label = f"Tracking #{tid}..."
                    # Draw label background for readability
                    (tw, th), _ = cv2.getTextSize(label, cv2.FONT_HERSHEY_SIMPLEX, 0.6, 2)
                    cv2.rectangle(out, (x1, y1 - th - 10), (x1 + tw + 4, y1), color, -1)
                    cv2.putText(out, label, (x1 + 2, y1 - 5), cv2.FONT_HERSHEY_SIMPLEX, 0.6, (0, 0, 0), 2)
                    break
        # FPS counter
        fps = self.frames_processed / max(1.0, time.time() - self.start_time)
        cv2.putText(out, f"FPS: {fps:.1f}  Frames: {self.frames_processed}", (10, 30),
                    cv2.FONT_HERSHEY_SIMPLEX, 0.8, (255, 255, 255), 2)
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
            elif path == "/shutdown":
                self._json({"shutting_down": True})
                _shutdown_event.set()
                # Schedule server shutdown from a separate thread to avoid deadlock
                threading.Thread(target=lambda: (time.sleep(0.5), os._exit(0)), daemon=True).start()
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
            elif path.startswith("/camera_preview"):
                # Capture a single frame from a USB camera by index.
                # Used by the Detect Cameras panel to show live thumbnails.
                import urllib.parse
                qs = urllib.parse.urlparse(self.path).query
                params = urllib.parse.parse_qs(qs)
                idx = int(params.get("index", ["0"])[0])
                try:
                    import cv2 as _cv2
                    cap = _cv2.VideoCapture(idx, _cv2.CAP_DSHOW)
                    if not cap.isOpened():
                        self.send_response(503)
                        self.send_header("Content-Type", "application/json")
                        self.end_headers()
                        self.wfile.write(b'{"error": "camera not available"}')
                        return
                    ret, frame = cap.read()
                    cap.release()
                    if not ret or frame is None:
                        self.send_response(503)
                        self.send_header("Content-Type", "application/json")
                        self.end_headers()
                        self.wfile.write(b'{"error": "cannot read frame"}')
                        return
                    data = _jpeg(frame)
                    if not data:
                        self.send_response(500)
                        self.end_headers()
                        return
                    self.send_response(200)
                    self.send_header("Content-Type", "image/jpeg")
                    self.send_header("Content-Length", str(len(data)))
                    self.send_header("Cache-Control", "no-cache")
                    self.end_headers()
                    try:
                        self.wfile.write(data)
                    except (BrokenPipeError, ConnectionResetError, OSError):
                        pass
                except Exception as e:
                    self.send_response(500)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(f'{{"error": "{e}"}}'.encode())
            elif path == "/preview_frame":
                # Support multi-camera: ?camera=N selects a specific camera
                import urllib.parse as _up
                _qs = _up.urlparse(self.path).query
                _qp = _up.parse_qs(_qs)
                cam_idx = int(_qp.get("camera", ["0"])[0])
                frame = None
                if isinstance(pipeline, MultiPipeline):
                    if 0 <= cam_idx < len(pipeline.pipelines):
                        with pipeline.pipelines[cam_idx].lock:
                            frame = pipeline.pipelines[cam_idx].current_frame
                else:
                    with pipeline.lock:
                        frame = pipeline.current_frame
                if frame is None:
                    self.send_response(503)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(b'{"error": "no frame yet"}')
                    return
                data = _jpeg(frame)
                if not data:
                    self.send_response(500)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "image/jpeg")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass
            elif path == "/preview":
                self._stream_mjpeg()
            else:
                self._json({"error": "not found"}, 404)

        def do_POST(self):
            """Handle configuration updates via POST."""
            path = self.path.split("?")[0]
            if path != "/config":
                self._json({"error": "not found"}, 404)
                return

            # Read request body
            content_length = int(self.headers.get('Content-Length', 0))
            body = self.rfile.read(content_length) if content_length > 0 else b''

            try:
                config = json.loads(body.decode()) if body else {}
            except json.JSONDecodeError:
                self._json({"error": "invalid JSON"}, 400)
                return

            # Apply configuration changes
            changes = []

            if 'detection_method' in config:
                method = config['detection_method'].lower().strip()
                if method in ('contour', 'paddleocr', 'consecutive'):
                    set_detection_method(method)
                    changes.append(f"detection_method={method}")
                else:
                    self._json({"error": f"unknown detection method: {method}"}, 400)
                    return

            if 'ocr_plate_mode' in config:
                mode = config['ocr_plate_mode'].lower().strip()
                if mode in ('universal', 'kenyan'):
                    set_ocr_plate_mode(mode)
                    changes.append(f"ocr_plate_mode={mode}")
                else:
                    self._json({"error": f"unknown plate mode: {mode}"}, 400)
                    return

            self._json({"status": "ok", "changes": changes})

        def _stream_mjpeg(self):
            # Support per-camera MJPEG: /preview?camera=N streams that camera's
            # annotated feed. Works for both single Pipeline and MultiPipeline
            # (the old code accessed pipeline.current_frame/annotate, which only
            # exist on single Pipeline — /preview was broken in multi-camera mode).
            import urllib.parse as _up
            _qp = _up.parse_qs(_up.urlparse(self.path).query)
            cam_idx: int | None = None
            if "camera" in _qp:
                try:
                    cam_idx = int(_qp["camera"][0])
                except (ValueError, IndexError):
                    cam_idx = None

            def _get_frame_and_annotator():
                if isinstance(pipeline, MultiPipeline):
                    if not pipeline.pipelines:
                        return None, None
                    if cam_idx is not None:
                        if not (0 <= cam_idx < len(pipeline.pipelines)):
                            return None, None
                        p = pipeline.pipelines[cam_idx]
                    else:
                        # No camera specified: first pipeline that has a frame
                        p = next((pp for pp in pipeline.pipelines if pp.current_frame is not None), pipeline.pipelines[0])
                    with p.lock:
                        return p.current_frame, p
                with pipeline.lock:
                    return pipeline.current_frame, pipeline

            self.send_response(200)
            self.send_header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
            self.send_header("Cache-Control", "no-cache, no-store, must-revalidate")
            self.send_header("Pragma", "no-cache")
            self.send_header("Expires", "0")
            self.end_headers()
            try:
                while True:
                    frame, annotator = _get_frame_and_annotator()
                    if frame is not None and annotator is not None:
                        data = _jpeg(annotator.annotate(frame))
                        if data:
                            self.wfile.write(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n")
                            self.wfile.write(data)
                            self.wfile.write(b"\r\n")
                            self.wfile.flush()
                    time.sleep(0.1)
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass

    # Refuse to start if another LIVE instance already serves this port.
    # ThreadingHTTPServer sets SO_REUSEADDR, which on Windows PERMITS a second
    # bind of an in-use port — two servers then share it and requests are
    # answered by whichever won the race (observed: a stale instance serving
    # black frames from a camera that is no longer configured).
    import socket as _socket
    _probe = _socket.socket()
    _probe.settimeout(0.7)
    _port_taken = False
    try:
        _probe.connect(("127.0.0.1", port))
        _port_taken = True
    except OSError:
        pass
    finally:
        try:
            _probe.close()
        except Exception:
            pass
    if _port_taken:
        print(f"FATAL: port {port} is already served by another ANPR instance — not starting a duplicate.", flush=True)
        sys.exit(1)

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"ANPR service listening on http://127.0.0.1:{port}")
    server.serve_forever()


# ---------------------------------------------------------------------------

def infer_type(source: str) -> str:
    if source.startswith("rtsp://"):
        return "rtsp"
    if source.startswith("http://") or source.startswith("https://"):
        return "http"
    if source.startswith("usb:"):
        return "usb"
    return "video_file"


# ---------------------------------------------------------------------------
# Config file — the Tauri app writes this when camera sources change.
# ---------------------------------------------------------------------------

CONFIG_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "config.json")

# Global shutdown event for graceful stop
_shutdown_event = threading.Event()


def load_config() -> dict:
    """Load config from config.json if it exists."""
    if os.path.exists(CONFIG_PATH):
        try:
            with open(CONFIG_PATH) as f:
                return json.load(f)
        except Exception:
            pass
    return {}


def save_config(cfg: dict) -> None:
    """Save config to config.json."""
    with open(CONFIG_PATH, "w") as f:
        json.dump(cfg, f, indent=2)


def get_source_from_config() -> tuple[str, str]:
    """Read the camera source from config.json.
    Returns (source_url, source_type)."""
    cfg = load_config()
    source = cfg.get("source", "")
    source_type = cfg.get("source_type") or (infer_type(source) if source else "")
    return source, source_type


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="TruckFlow ANPR service (SORT-tracked)")
    p.add_argument("--source", default=None, help="rtsp://…, http://…, video file path, or usb:N (overrides config.json)")
    p.add_argument("--type", default=None, choices=["rtsp", "http", "video_file", "usb", "live_test"], help="override source type inference")
    p.add_argument("--port", type=int, default=9800)
    return p.parse_args(argv)


class MultiPipeline:
    """Wraps multiple Pipeline instances for multi-camera support.

    Each camera source gets its own Pipeline with its own capture thread,
    SORT tracker, and readings.  Sightings from all cameras are merged
    into a single list and /latest returns the most recent from any camera.
    """

    def __init__(self, ocr: OcrBackend, port: int):
        self.ocr = ocr
        self.port = port
        self.pipelines: list[Pipeline] = []
        self.lock = threading.Lock()
        self.running = False
        self.start_time = time.time()

    def add_source(self, source: str, source_type: str) -> None:
        """Add a camera source and start its capture thread."""
        if not source or not source.strip():
            return
        p = Pipeline(source, source_type, self.ocr, self.port)
        self.pipelines.append(p)
        t = threading.Thread(target=p.capture_frames, daemon=True)
        t.start()
        print(f"[ANPR] Camera added: {source} ({source_type})")

    def stop(self) -> None:
        """Stop all pipelines."""
        self.running = False
        for p in self.pipelines:
            p.running = False

    # -- Merged status / sightings ----------------------------------------

    @property
    def sightings(self) -> list[dict]:
        """Merged sightings from all cameras, sorted by timestamp."""
        all_s = []
        for p in self.pipelines:
            with p.lock:
                all_s.extend(p.sightings)
        all_s.sort(key=lambda s: s.get("timestamp", ""))
        return all_s

    def latest_payload(self) -> dict | None:
        """Return the most recent sighting from any camera."""
        best = None
        best_ts = ""
        for p in self.pipelines:
            payload = p.latest_payload()
            if payload and payload.get("timestamp", "") > best_ts:
                best = payload
                best_ts = payload.get("timestamp", "")
        return best

    def status(self) -> dict:
        """Aggregated status from all cameras."""
        total_frames = 0
        total_plates = 0
        any_running = False
        any_frame = False
        cameras = []
        for i, p in enumerate(self.pipelines):
            st = p.status()
            total_frames += st["frames_processed"]
            total_plates += st["plates_detected"]
            if st["running"]:
                any_running = True
            if st["camera_connected"]:
                any_frame = True
            cameras.append({
                "index": i,
                "source": st["source_url"],
                "source_type": st["source_type"],
                "running": st["running"],
                "connected": st["camera_connected"],
                "frames": st["frames_processed"],
                "fps": st["fps"],
            })
        uptime = int(time.time() - self.start_time)
        fps = total_frames / max(1.0, uptime)
        return {
            "running": any_running,
            "source_type": "multi" if len(self.pipelines) > 1 else (self.pipelines[0].source_type if self.pipelines else ""),
            "source_url": f"{len(self.pipelines)} cameras",
            "models_loaded": self.ocr.name != "mock",
            "plates_detected": total_plates,
            "frames_processed": total_frames,
            "fps": round(fps, 1),
            "last_plate_time": None,
            "uptime_seconds": uptime,
            "ocr_engine": self.ocr.name,
            "camera_connected": any_frame,
            "cameras": cameras,
        }

    def latest_payload_dedup(self) -> dict | None:
        """Return the latest payload exactly once (dedup across cameras)."""
        best = None
        best_ts = ""
        for p in self.pipelines:
            with p.lock:
                if p.last_emitted is not p.latest and p.latest:
                    ts = p.latest.get("timestamp", "")
                    if ts > best_ts:
                        best = p.latest
                        best_ts = ts
                        p.last_emitted = p.latest
        return best

    def annotate(self, frame):
        """No-op for multi pipeline (each sub-pipeline annotates its own)."""
        return frame

    def _classify(self, plate: str) -> str:
        """Classify across all cameras' sightings."""
        with self.lock:
            open_plates = {s["plate"] for s in self.sightings if s.get("entry_exit") == "entry"}
        return "exit" if plate in open_plates else "entry"


def main() -> None:
    import signal

    args = parse_args()
    cfg = load_config()
    # Priority: CLI --source > config.json > default
    if args.source:
        source = args.source
        source_type = args.type or infer_type(source)
    else:
        source = cfg.get("source", "")
        source_type = cfg.get("source_type") or infer_type(source) if source else ""
        if args.type:
            source_type = args.type

    # If no source configured, start in idle mode (HTTP server only, no capture)
    has_source = bool(source and source.strip())
    if has_source:
        print(f"Camera source: {source} ({source_type})")
    else:
        print("[ANPR] No camera source configured — starting in idle mode (HTTP server only)")

    # Cloud OCR preference — read from config.json (written by Rust backend)
    prefer_cloud = cfg.get("prefer_cloud", False)
    cloud_api_url = cfg.get("cloud_api_url", "")
    cloud_api_key = cfg.get("cloud_api_key", "")
    if prefer_cloud and cloud_api_url:
        print(f"[OCR] Cloud preferred — will try {cloud_api_url} first, local fallback on failure")
    else:
        print("[OCR] Local-only mode (cloud not preferred)")

    ocr = OcrBackend(prefer_cloud=prefer_cloud, cloud_api_url=cloud_api_url, cloud_api_key=cloud_api_key)

    # OCR plate mode — 'universal' (any plate) or 'kenyan' (3 letters + 3 digits + 1 letter)
    ocr_plate_mode = cfg.get('ocr_plate_mode', 'universal')
    set_ocr_plate_mode(ocr_plate_mode)

    # --- Multi-camera: read "sources" array from config.json --------------
    sources_list = cfg.get("sources", [])
    if args.source:
        # CLI --source overrides everything
        sources_list = [{"source": args.source, "source_type": source_type}]

    if len(sources_list) > 1:
        # Multi-camera mode: one Pipeline per source, merged sightings
        multi = MultiPipeline(ocr, args.port)
        for s in sources_list:
            s_src = s.get("source", "")
            s_type = s.get("source_type", "") or infer_type(s_src) if s_src else ""
            multi.add_source(s_src, s_type)
        multi.running = True
        pipeline = multi
        has_any = any(p.source for p in multi.pipelines)
        print(f"[ANPR] Multi-camera mode: {len(multi.pipelines)} source(s)")
    else:
        # Single-camera mode (backward compat)
        if sources_list:
            source = sources_list[0].get("source", source)
            source_type = sources_list[0].get("source_type", source_type) or infer_type(source) if source else source_type
        has_source = bool(source and source.strip())
        pipeline = Pipeline(source or "", source_type or "", ocr, args.port)
        has_any = has_source
        if has_source:
            print(f"Camera source: {source} ({source_type})")

    # Graceful shutdown on SIGTERM/SIGINT (Windows and Unix)
    def _shutdown(signum, frame):
        print("[ANPR] Shutdown signal received")
        _shutdown_event.set()
        pipeline.running = False
        if isinstance(pipeline, MultiPipeline):
            pipeline.stop()
        threading.Thread(target=lambda: os._exit(0), daemon=True).start()

    signal.signal(signal.SIGTERM, _shutdown)
    signal.signal(signal.SIGINT, _shutdown)

    if has_any:
        if isinstance(pipeline, MultiPipeline):
            # Capture threads already started in add_source()
            pass
        else:
            capture_thread = threading.Thread(target=pipeline.capture_frames, daemon=True)
            capture_thread.start()
    else:
        print("[ANPR] No source — capture thread not started")
    serve(pipeline, args.port)


if __name__ == "__main__":
    main()
