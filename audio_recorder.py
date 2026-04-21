"""시스템 출력(loopback) + 마이크를 동시에 녹음해 OGG Vorbis로 저장한다."""
from __future__ import annotations

import queue
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import numpy as np
import soundcard as sc
import soundfile as sf


@dataclass
class AudioConfig:
    samplerate: int = 48000
    channels: int = 2
    blocksize: int = 1024  # frames per block
    mic_gain: float = 1.0
    loopback_gain: float = 1.0


class MixedAudioRecorder:
    """두 스트림(시스템 loopback + 마이크)을 믹싱해 하나의 .ogg 로 저장.

    - 기본: 시스템 기본 스피커의 loopback + 시스템 기본 마이크
    - 시작/정지 가능, 중단돼도 지금까지 녹음된 부분은 유효한 OGG로 남음 (블록 단위 write)
    """

    def __init__(self, output_path: Path, cfg: Optional[AudioConfig] = None) -> None:
        self.output_path = output_path
        self.cfg = cfg or AudioConfig()
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._write_queue: "queue.Queue[Optional[np.ndarray]]" = queue.Queue(maxsize=256)
        self._writer_thread: Optional[threading.Thread] = None
        self._error: Optional[BaseException] = None
        self._started_at: Optional[float] = None
        self._frames_written = 0

    def start(self) -> None:
        if self._thread is not None:
            raise RuntimeError("이미 실행 중")
        self.output_path.parent.mkdir(parents=True, exist_ok=True)
        self._stop.clear()
        self._error = None
        self._frames_written = 0
        self._started_at = time.time()
        self._writer_thread = threading.Thread(target=self._writer_loop, name="audio-writer", daemon=True)
        self._writer_thread.start()
        self._thread = threading.Thread(target=self._capture_loop, name="audio-capture", daemon=True)
        self._thread.start()

    def stop(self, timeout: float = 5.0) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=timeout)
        self._write_queue.put(None)  # sentinel
        if self._writer_thread is not None:
            self._writer_thread.join(timeout=timeout)
        if self._error is not None:
            raise self._error

    @property
    def duration_sec(self) -> float:
        return self._frames_written / float(self.cfg.samplerate) if self.cfg.samplerate else 0.0

    def _capture_loop(self) -> None:
        cfg = self.cfg
        try:
            default_speaker = sc.default_speaker()
            default_mic = sc.default_microphone()
            # 시스템 출력 loopback: soundcard 에서는 get_microphone(..., include_loopback=True) 로 획득
            loopback_mic = sc.get_microphone(id=str(default_speaker.name), include_loopback=True)
        except Exception as e:
            self._error = RuntimeError(f"audio device 초기화 실패: {e}")
            return

        try:
            with (
                loopback_mic.recorder(samplerate=cfg.samplerate, channels=cfg.channels, blocksize=cfg.blocksize) as rec_sys,
                default_mic.recorder(samplerate=cfg.samplerate, channels=cfg.channels, blocksize=cfg.blocksize) as rec_mic,
            ):
                while not self._stop.is_set():
                    sys_block = rec_sys.record(numframes=cfg.blocksize)
                    mic_block = rec_mic.record(numframes=cfg.blocksize)
                    # 채널 수가 다를 수 있으니 맞춰준다
                    sys_block = _force_channels(sys_block, cfg.channels)
                    mic_block = _force_channels(mic_block, cfg.channels)
                    mixed = (sys_block * cfg.loopback_gain + mic_block * cfg.mic_gain).astype(np.float32)
                    # clipping 방지 (soft clip)
                    np.clip(mixed, -1.0, 1.0, out=mixed)
                    try:
                        self._write_queue.put(mixed, timeout=2.0)
                    except queue.Full:
                        # writer가 밀리면 블록 드롭 (녹음 지연 방지)
                        continue
        except Exception as e:
            self._error = e

    def _writer_loop(self) -> None:
        cfg = self.cfg
        try:
            with sf.SoundFile(
                str(self.output_path),
                mode="w",
                samplerate=cfg.samplerate,
                channels=cfg.channels,
                format="OGG",
                subtype="VORBIS",
            ) as f:
                while True:
                    item = self._write_queue.get()
                    if item is None:
                        break
                    f.write(item)
                    self._frames_written += item.shape[0]
        except Exception as e:
            if self._error is None:
                self._error = e


def _force_channels(block: np.ndarray, target_channels: int) -> np.ndarray:
    if block.ndim == 1:
        block = block.reshape(-1, 1)
    cur = block.shape[1]
    if cur == target_channels:
        return block
    if cur == 1 and target_channels == 2:
        return np.repeat(block, 2, axis=1)
    if cur == 2 and target_channels == 1:
        return block.mean(axis=1, keepdims=True)
    # 일반화: 앞의 target_channels 채널만 사용, 부족하면 0 패딩
    if cur > target_channels:
        return block[:, :target_channels]
    pad = np.zeros((block.shape[0], target_channels - cur), dtype=block.dtype)
    return np.concatenate([block, pad], axis=1)


def list_audio_devices() -> dict:
    """사용 가능한 오디오 장치 목록."""
    speakers = [s.name for s in sc.all_speakers()]
    mics = [m.name for m in sc.all_microphones(include_loopback=False)]
    loopbacks = [m.name for m in sc.all_microphones(include_loopback=True) if m.isloopback]
    return {
        "default_speaker": sc.default_speaker().name,
        "default_microphone": sc.default_microphone().name,
        "speakers": speakers,
        "microphones": mics,
        "loopbacks": loopbacks,
    }
