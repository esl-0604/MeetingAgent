"""pHash 기반 화면 변화 감지 + WebP 저장 루프."""
from __future__ import annotations

import json
import time
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path
from typing import Optional

import imagehash
import mss
from PIL import Image

from teams_window import find_teams_window


@dataclass
class FrameRecord:
    index: int
    timestamp: str
    filename: str
    phash: str
    hamming_from_prev: Optional[int]
    window_title: str


class ChangeCapture:
    def __init__(
        self,
        output_root: Path,
        interval: float = 1.0,
        phash_threshold: int = 8,
        webp_quality: int = 85,
    ) -> None:
        self.interval = interval
        self.threshold = phash_threshold
        self.webp_quality = webp_quality
        self.session_start = datetime.now()
        session_name = self.session_start.strftime("%Y-%m-%d_%H%M%S")
        self.session_dir = output_root / session_name
        self.session_dir.mkdir(parents=True, exist_ok=True)
        self.metadata_path = self.session_dir / "metadata.json"
        self.frames: list[FrameRecord] = []
        self._stop = False

    def request_stop(self) -> None:
        self._stop = True

    def run(self) -> None:
        prev_hash: Optional[imagehash.ImageHash] = None
        frame_idx = 0
        missing_notice_at = 0.0
        print(f"[capture] session dir: {self.session_dir}")
        print(f"[capture] interval={self.interval}s  threshold={self.threshold}  quality={self.webp_quality}")
        print("[capture] Ctrl+C 로 종료")

        with mss.mss() as sct:
            while not self._stop:
                tick = time.monotonic()
                win = find_teams_window()
                if win is None:
                    now = time.monotonic()
                    if now - missing_notice_at > 10:
                        print("[capture] Teams 창을 찾지 못함. 대기 중...")
                        missing_notice_at = now
                    self._sleep_remainder(tick)
                    continue

                try:
                    raw = sct.grab(win.mss_region)
                except Exception as e:
                    print(f"[capture] grab 실패: {e}  (region={win.mss_region})")
                    self._sleep_remainder(tick)
                    continue

                img = Image.frombytes("RGB", raw.size, raw.rgb)
                cur_hash = imagehash.phash(img)

                if prev_hash is None:
                    dist: Optional[int] = None
                    should_save = True
                    reason = "첫 프레임"
                else:
                    dist = int(cur_hash - prev_hash)  # imagehash returns numpy.int64
                    should_save = dist > self.threshold
                    reason = f"변화 감지 hamming={dist}" if should_save else ""

                if should_save:
                    frame_idx += 1
                    now_dt = datetime.now()
                    stamp = now_dt.strftime("%H%M%S_") + f"{now_dt.microsecond // 1000:03d}"
                    fname = f"slide_{frame_idx:04d}_{stamp}.webp"
                    fpath = self.session_dir / fname
                    img.save(fpath, format="WEBP", quality=self.webp_quality, method=6)
                    record = FrameRecord(
                        index=frame_idx,
                        timestamp=now_dt.isoformat(timespec="milliseconds"),
                        filename=fname,
                        phash=str(cur_hash),
                        hamming_from_prev=dist,
                        window_title=win.title,
                    )
                    self.frames.append(record)
                    self._flush_metadata()
                    print(f"[capture] {reason} -> {fname}  ({img.size[0]}x{img.size[1]})")
                    prev_hash = cur_hash

                self._sleep_remainder(tick)

        self._flush_metadata()
        print(f"[capture] 종료. {len(self.frames)}장 저장됨.")
        print(f"[capture] metadata: {self.metadata_path}")

    def _sleep_remainder(self, start_monotonic: float) -> None:
        elapsed = time.monotonic() - start_monotonic
        remaining = self.interval - elapsed
        if remaining > 0:
            time.sleep(remaining)

    def _flush_metadata(self) -> None:
        payload = {
            "session_start": self.session_start.isoformat(timespec="seconds"),
            "interval_sec": self.interval,
            "phash_threshold": self.threshold,
            "webp_quality": self.webp_quality,
            "frames": [asdict(f) for f in self.frames],
        }
        self.metadata_path.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
