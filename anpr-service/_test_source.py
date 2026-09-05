#!/usr/bin/env python3
"""Capture test: open a camera source with OpenCV and grab a frame.
Usage: python _test_source.py <source> <source_type> <timeout_seconds>
Output: JSON {"ok": true/false, "message": "..."}
"""
import sys, json, time

def main():
    if len(sys.argv) < 4:
        print(json.dumps({"ok": False, "message": "Usage: _test_source.py <source> <source_type> <timeout>"}))
        sys.exit(1)
    
    source = sys.argv[1]
    source_type = sys.argv[2]
    timeout = int(sys.argv[3])
    
    import cv2
    
    # Map source types to OpenCV backend flags
    cap = None
    if source_type == "usb":
        try:
            idx = int(source)
        except ValueError:
            idx = 0
        cap = cv2.VideoCapture(idx, cv2.CAP_DSHOW)
    elif source_type in ("http", "rtsp"):
        cap = cv2.VideoCapture(source, cv2.CAP_FFMPEG)
    elif source_type == "video_file":
        cap = cv2.VideoCapture(source)
    else:
        cap = cv2.VideoCapture(source)
    
    if not cap or not cap.isOpened():
        print(json.dumps({"ok": False, "message": f"Failed to open source: {source}"}))
        sys.exit(1)
    
    cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)
    start = time.time()
    frames_read = 0
    while time.time() - start < timeout:
        ret, frame = cap.read()
        if ret and frame is not None:
            frames_read += 1
            if frames_read >= 2:
                cap.release()
                h, w = frame.shape[:2]
                print(json.dumps({"ok": True, "message": f"Frame captured: {w}x{h}"}))
                sys.exit(0)
        time.sleep(0.1)
    
    cap.release()
    print(json.dumps({"ok": False, "message": f"No frames received in {timeout}s"}))
    sys.exit(1)

if __name__ == "__main__":
    main()
