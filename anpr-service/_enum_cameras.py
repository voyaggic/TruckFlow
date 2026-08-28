#!/usr/bin/env python3
"""Enumerate available cameras via OpenCV + DirectShow.

Usage:
    python _enum_cameras.py           # full probe (reads a few frames per camera)
    python _enum_cameras.py --fast    # quick probe (open + read one frame only)

Outputs a JSON array to stdout:
    [{"index": 0, "name": "Integrated Camera", "width": 640, "height": 480,
      "fps": 30.0, "backend": "dshow", "status": "ok", "is_live": true,
      "device_type": "usb", "avg_frame_diff": 0.0, "brightness": 128.0}, ...]
"""
import sys
import json
import time
import numpy as np

try:
    import cv2
except ImportError:
    print("[]")
    sys.exit(0)


def _get_dshow_name(index):
    """Try to get the DirectShow device name for a given index via pygrabber."""
    try:
        from pygrabber.dshow_graph import FilterGraph
        fg = FilterGraph()
        devices = fg.get_input_devices()
        if 0 <= index < len(devices):
            return devices[index]
    except Exception:
        pass
    return ""


def _classify_status(frames):
    """Classify camera status from a list of grabbed frames (BGR or None)."""
    valid = [f for f in frames if f is not None]
    if not valid:
        return "error", False, 0.0, 0.0

    # Check for pure black
    mean_val = float(np.mean(valid[-1]))
    if mean_val < 5.0:
        return "black", False, 0.0, mean_val

    # Check for static (test pattern / frozen frame) by comparing consecutive frames
    if len(valid) >= 2:
        diffs = []
        for a, b in zip(valid[:-1], valid[1:]):
            d = float(np.mean(np.abs(a.astype(np.float32) - b.astype(np.float32))))
            diffs.append(d)
        avg_diff = sum(diffs) / len(diffs) if diffs else 0.0
        is_live = avg_diff > 1.5  # threshold: moving content has diff > 1.5
        status = "ok" if is_live else "static"
        return status, is_live, avg_diff, mean_val
    else:
        # Single frame — can't determine motion, assume ok if not black
        return "ok", True, 0.0, mean_val


def _probe_camera(index, fast=False):
    """Try to open camera at `index`. Returns dict or None."""
    cap = cv2.VideoCapture(index, cv2.CAP_DSHOW)
    if not cap.isOpened():
        cap = cv2.VideoCapture(index)
        if not cap.isOpened():
            return None

    # Read 1 frame to confirm it works
    ret, frame = cap.read()
    if not ret or frame is None:
        cap.release()
        return None

    width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
    height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
    fps = cap.get(cv2.CAP_PROP_FPS) or 0.0

    frames = [frame]
    if not fast:
        # Read a few more frames to detect static/live
        for _ in range(4):
            time.sleep(0.08)
            ret2, f2 = cap.read()
            if ret2 and f2 is not None:
                frames.append(f2)
        cap.release()
    else:
        cap.release()

    status, is_live, avg_diff, brightness = _classify_status(frames)
    dshow_name = _get_dshow_name(index)
    name = dshow_name if dshow_name else f"Camera {index}"

    return {
        "index": index,
        "name": name,
        "width": width,
        "height": height,
        "fps": round(fps, 1),
        "backend": "dshow",
        "status": status,
        "is_live": is_live,
        "device_type": "usb",
        "avg_frame_diff": round(avg_diff, 2),
        "brightness": round(brightness, 1),
    }


def main():
    fast = "--fast" in sys.argv
    # Probe indices 0-9 (covers most USB hubs / virtual cameras)
    cameras = []
    for i in range(10):
        try:
            result = _probe_camera(i, fast=fast)
            if result is not None:
                cameras.append(result)
        except Exception:
            pass
    print(json.dumps(cameras))


if __name__ == "__main__":
    main()
