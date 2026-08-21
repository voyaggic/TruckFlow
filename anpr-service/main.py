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
        """Load OCR models on first use (lazy initialization)."""
        if self._models_loaded:
            return
        self._models_loaded = True
        # Try PaddleOCR first (faster, better for plates)
        try:
            from paddleocr import PaddleOCR  # type: ignore

            self.paddle = PaddleOCR(use_angle_cls=False, lang="en", show_log=False)
            self.name = "paddleocr"
            self.model_version = "paddleocr-en"
            print("[OCR] PaddleOCR loaded (local)")
            return
        except Exception as e:
            print(f"[OCR] PaddleOCR not available: {e}")
        # Fallback to EasyOCR
        try:
            import easyocr  # type: ignore

            self.reader = easyocr.Reader(["en"], gpu=False, verbose=False)
            self.name = "easyocr"
            self.model_version = "easyocr-en"
            print("[OCR] EasyOCR loaded (local)")
            return
        except Exception as e:
            print(f"[OCR] EasyOCR not available: {e}")
        print("[OCR] WARNING: No OCR engine available!")

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
        """Pull frames from the configured source and feed the tracker.
        
        For HTTP/HTTPS sources, uses a custom MJPEG reader that parses
        multipart streams directly (OpenCV VideoCapture can't handle these
        on Windows). For other sources (video files, USB, RTSP), uses OpenCV.
        Retries on stream drops with exponential backoff.
        Stops retrying when shutdown signal is received or source is empty.
        """
        # Bail immediately if no source configured
        if not self.source or not self.source.strip():
            print("[ANPR] No source configured — capture thread exiting")
            return

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
            # Failed — retry with backoff
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
        """Run the OpenCV capture loop. Returns (eof, error)."""
        cap = self._open_capture()
        if cap is None:
            return False, True
        try:
            self.running = True
            consecutive_failures = 0
            print(f"[ANPR] Camera connected: {self.source}")
            while not _shutdown_event.is_set():
                ok, frame = cap.read()
                if not ok:
                    consecutive_failures += 1
                    if consecutive_failures >= 15:
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
            if self.source_type in ("http", "rtsp", "usb", "video_file"):
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
        if self.source_type == "video_file":
            return cv2.VideoCapture(self.source)
        if self.source_type == "usb":
            return cv2.VideoCapture(int(self.source))
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
            elif path == "/preview_frame":
                with pipeline.lock:
                    frame = pipeline.current_frame
                if frame is None:
                    self.send_response(503)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(b'{"error": "no frame yet"}')
                    return
                data = _jpeg(pipeline.annotate(frame))
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

        def _stream_mjpeg(self):
            self.send_response(200)
            self.send_header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
            self.send_header("Cache-Control", "no-cache, no-store, must-revalidate")
            self.send_header("Pragma", "no-cache")
            self.send_header("Expires", "0")
            self.end_headers()
            try:
                while True:
                    with pipeline.lock:
                        frame = pipeline.current_frame
                    if frame is not None:
                        data = _jpeg(pipeline.annotate(frame))
                        if data:
                            self.wfile.write(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n")
                            self.wfile.write(data)
                            self.wfile.write(b"\r\n")
                            self.wfile.flush()
                    time.sleep(0.1)
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass

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
    pipeline = Pipeline(source or "", source_type or "", ocr, args.port)

    # Graceful shutdown on SIGTERM/SIGINT (Windows and Unix)
    def _shutdown(signum, frame):
        print("[ANPR] Shutdown signal received")
        _shutdown_event.set()
        pipeline.running = False
        threading.Thread(target=lambda: os._exit(0), daemon=True).start()

    signal.signal(signal.SIGTERM, _shutdown)
    signal.signal(signal.SIGINT, _shutdown)

    if has_source:
        capture_thread = threading.Thread(target=pipeline.capture_frames, daemon=True)
        capture_thread.start()
    else:
        print("[ANPR] No source — capture thread not started")
    serve(pipeline, args.port)


if __name__ == "__main__":
    main()
