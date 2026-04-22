"""Teams 창 하단 자막 영역을 주기적으로 OCR 해서 JSONL 로 기록."""
from __future__ import annotations

import json
import sys
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Optional

import mss
import numpy as np
from PIL import Image

from teams_window import find_teams_window


def _bundled_easyocr_model_dir() -> Optional[str]:
    """PyInstaller 번들 안의 모델 경로. 비번들 환경에선 None 반환(기본 경로 사용)."""
    base = getattr(sys, "_MEIPASS", None)
    if not base:
        return None
    candidate = Path(base) / "easyocr_models"
    if candidate.exists() and any(candidate.iterdir()):
        return str(candidate)
    return None


@dataclass
class CaptionOcrConfig:
    interval: float = 1.5
    # 사용자가 직접 지정한 절대 화면 좌표 영역 (우선순위 1)
    absolute_region: Optional[dict] = None  # {"left":..., "top":..., "width":..., "height":...}
    # 미지정 시 Teams 창 기준 비율 영역 (우선순위 2)
    region_top_ratio: float = 0.70
    region_bottom_ratio: float = 0.96
    region_left_ratio: float = 0.10
    region_right_ratio: float = 0.90
    languages: list[str] = field(default_factory=lambda: ["ko", "en"])
    min_confidence: float = 0.30
    # 이전과 동일한 텍스트면 재기록 안 함 (단순 dedupe)
    dedupe: bool = True


class CaptionOcrScraper:
    def __init__(self, output_jsonl: Path, cfg: Optional[CaptionOcrConfig] = None) -> None:
        self.out = output_jsonl
        self.cfg = cfg or CaptionOcrConfig()
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._reader = None
        self._error: Optional[BaseException] = None
        self._lines_written = 0
        self._last_text = ""

    def start(self) -> None:
        if self._thread is not None:
            raise RuntimeError("이미 실행 중")
        import easyocr  # lazy import — 초기화 시간이 김

        model_dir = _bundled_easyocr_model_dir()
        if model_dir:
            self._reader = easyocr.Reader(
                self.cfg.languages,
                gpu=False,
                verbose=False,
                model_storage_directory=model_dir,
                user_network_directory=model_dir,
                download_enabled=False,
            )
        else:
            self._reader = easyocr.Reader(self.cfg.languages, gpu=False, verbose=False)
        self.out.parent.mkdir(parents=True, exist_ok=True)
        self._stop.clear()
        self._error = None
        self._lines_written = 0
        self._last_text = ""
        self._thread = threading.Thread(target=self._loop, name="caption-ocr", daemon=True)
        self._thread.start()

    def stop(self, timeout: float = 5.0) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=timeout)
        if self._error is not None:
            raise self._error

    @property
    def lines_written(self) -> int:
        return self._lines_written

    def _compute_region(self) -> Optional[dict]:
        # 1) 사용자 지정 절대 좌표 우선
        ar = self.cfg.absolute_region
        if ar and ar.get("width", 0) >= 40 and ar.get("height", 0) >= 20:
            return {
                "left": int(ar["left"]),
                "top": int(ar["top"]),
                "width": int(ar["width"]),
                "height": int(ar["height"]),
            }
        # 2) Teams 창 기준 비율 영역
        win = find_teams_window()
        if win is None:
            return None
        left = win.left + int(win.width * self.cfg.region_left_ratio)
        right = win.left + int(win.width * self.cfg.region_right_ratio)
        top = win.top + int(win.height * self.cfg.region_top_ratio)
        bottom = win.top + int(win.height * self.cfg.region_bottom_ratio)
        w = right - left
        h = bottom - top
        if w < 40 or h < 20:
            return None
        return {"left": left, "top": top, "width": w, "height": h}

    def _loop(self) -> None:
        try:
            with mss.mss() as sct, self.out.open("a", encoding="utf-8") as sink:
                while not self._stop.is_set():
                    tick = time.monotonic()
                    region = self._compute_region()
                    if region is None:
                        self._sleep_remainder(tick)
                        continue
                    try:
                        raw = sct.grab(region)
                    except Exception:
                        self._sleep_remainder(tick)
                        continue

                    img = Image.frombytes("RGB", raw.size, raw.rgb)
                    arr = np.array(img)
                    try:
                        results = self._reader.readtext(arr)
                    except Exception as e:
                        self._error = e
                        return

                    lines = [
                        (conf, txt.strip())
                        for bbox, txt, conf in results
                        if conf >= self.cfg.min_confidence and txt.strip()
                    ]
                    joined = "\n".join(t for _, t in lines).strip()
                    if joined and (not self.cfg.dedupe or joined != self._last_text):
                        record = {
                            "timestamp": datetime.now().isoformat(timespec="milliseconds"),
                            "text": joined,
                            "lines": [{"conf": round(c, 2), "text": t} for c, t in lines],
                            "region": region,
                        }
                        sink.write(json.dumps(record, ensure_ascii=False) + "\n")
                        sink.flush()
                        self._lines_written += 1
                        self._last_text = joined

                    self._sleep_remainder(tick)
        except Exception as e:
            if self._error is None:
                self._error = e

    def _sleep_remainder(self, start_monotonic: float) -> None:
        elapsed = time.monotonic() - start_monotonic
        remaining = self.cfg.interval - elapsed
        if remaining > 0:
            self._stop.wait(timeout=remaining)
