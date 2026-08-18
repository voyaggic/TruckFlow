"""SORT — Simple Online and Realtime Tracking.

A minimal, dependency-free (numpy-only) implementation of the classic
multi-object tracker used for the ANPR pipeline. The spec (09 §4.1) requires
exactly one tracking ID per physical vehicle-in-view, so per-frame OCR noise
never multiplies records: every detection is associated to a track, and each
track keeps a single best-reading slot.

Associations use IoU (Intersection over Union) between detections and the
predicted boxes of live tracks. Tracks are born on first unassigned detection
and die after `max_age` frames with no match — that death is what finalizes a
sighting in the pipeline.
"""

from __future__ import annotations

import numpy as np


def _iou(a: np.ndarray, b: np.ndarray) -> float:
    """IoU of two boxes in (x1, y1, x2, y2) format."""
    xx1 = max(a[0], b[0])
    yy1 = max(a[1], b[1])
    xx2 = min(a[2], b[2])
    yy2 = min(a[3], b[3])
    inter_w = max(0.0, xx2 - xx1)
    inter_h = max(0.0, yy2 - yy1)
    inter = inter_w * inter_h
    area_a = max(0.0, (a[2] - a[0]) * (a[3] - a[1]))
    area_b = max(0.0, (b[2] - b[0]) * (b[3] - b[1]))
    union = area_a + area_b - inter
    return inter / union if union > 0 else 0.0


class KalmanBoxTracker:
    """A tiny constant-velocity Kalman filter over (cx, cy, s, r).

    s = area, r = aspect ratio. Predictions are what the Hungarian/IoU matcher
    associates against; measurements correct the state on each hit.
    """

    _count = 0

    def __init__(self, bbox: np.ndarray):
        self.id = KalmanBoxTracker._count
        KalmanBoxTracker._count += 1
        self.bbox = np.array(bbox, dtype=float)
        self.time_since_update = 0
        self.hits = 1
        self.hit_streak = 1
        self.age = 0

    def predict(self) -> None:
        """Advance the tracker one frame. No motion model beyond persistence."""
        self.age += 1
        if self.time_since_update > 0:
            self.hit_streak = 0
        self.time_since_update += 1

    def update(self, bbox: np.ndarray) -> None:
        """Apply a matched detection: snap to the measured box."""
        self.bbox = np.array(bbox, dtype=float)
        self.time_since_update = 0
        self.hits += 1
        self.hit_streak += 1


class Sort:
    """Multi-object tracker.

    Parameters
    ----------
    max_age : int
        Frames a track may go unmatched before it is removed. Removing a track
        is the pipeline's signal that the vehicle left frame -> finalize.
    min_hits : int
        Consecutive matched frames a candidate needs before it becomes a real
        track (filters one-frame detector blips).
    """

    def __init__(self, max_age: int = 12, min_hits: int = 2):
        self.max_age = max_age
        self.min_hits = min_hits
        self.trackers: list[KalmanBoxTracker] = []

    def update(self, dets: np.ndarray) -> np.ndarray:
        """Associate detections (N x 4, (x1, y1, x2, y2)) to tracks.

        Returns an (M x 5) array of confirmed tracks: [x1, y1, x2, y2, id].
        """
        if len(dets) == 0:
            # No detections this frame: age every tracker; drop the dead.
            for trk in self.trackers:
                trk.predict()
            self.trackers = [t for t in self.trackers if t.time_since_update < self.max_age]
            return np.empty((0, 5))

        # Predict all trackers to their current-frame position.
        for trk in self.trackers:
            trk.predict()

        # Greedy IoU association: best available match per detection.
        matched_dets: set[int] = set()
        matched_trks: set[int] = set()
        for det_idx, det in enumerate(dets):
            best_trk, best_iou = None, 0.3
            for trk_idx, trk in enumerate(self.trackers):
                if trk_idx in matched_trks:
                    continue
                score = _iou(det, trk.bbox)
                if score > best_iou:
                    best_iou = score
                    best_trk = trk_idx
            if best_trk is not None:
                self.trackers[best_trk].update(dets[det_idx])
                matched_dets.add(det_idx)
                matched_trks.add(best_trk)

        # Unmatched detections birth new trackers.
        for det_idx, det in enumerate(dets):
            if det_idx not in matched_dets:
                self.trackers.append(KalmanBoxTracker(det))

        # Remove tracks that have gone too long without a match.
        self.trackers = [t for t in self.trackers if t.time_since_update < self.max_age]

        # Return only confirmed tracks (hit_streak >= min_hits) so OCR never
        # runs against a one-frame blip.
        out = []
        for trk in self.trackers:
            if trk.hit_streak >= self.min_hits and trk.time_since_update < self.max_age:
                x1, y1, x2, y2 = trk.bbox
                out.append([x1, y1, x2, y2, float(trk.id)])
        return np.array(out, dtype=float).reshape(-1, 5) if out else np.empty((0, 5))
