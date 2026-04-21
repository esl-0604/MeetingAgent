"""Meeting Agent 스탠드얼론 GUI.

실행:
    .venv/Scripts/python.exe meeting_agent_app.py
    또는 PyInstaller로 빌드한 MeetingAgent.exe 를 실행

동작:
1. 작은 플로팅 창(항상 위)이 뜨고 Teams 미팅을 감지할 때까지 대기
2. 미팅 감지 시 자동으로 3개 워커 시작
   - 오디오 녹음 (시스템 loopback + 마이크 믹스)
   - 자막 OCR 스크래핑 (사용자 지정 영역)
   - 발표 자료(화면) pHash 변화 캡처
3. 미팅 종료 시 저장 폴더를 물어보고 audio/ transcript/ slides/ 3개 하위 폴더로 이동
4. "자막 영역 선택" 버튼으로 드래그-투-셀렉트 절대 좌표 영역을 저장/재사용
"""
from __future__ import annotations

import ctypes
import json
import os
import queue
import shutil
import sys
import threading
import time
import tkinter as tk
import urllib.parse
from datetime import datetime
from pathlib import Path
from tkinter import filedialog, messagebox, scrolledtext, ttk
from typing import Optional

import mss
from PIL import Image, ImageTk

from audio_recorder import AudioConfig, MixedAudioRecorder
from caption_ocr import CaptionOcrConfig, CaptionOcrScraper
from capture import ChangeCapture
from teams_window import WindowRect, find_teams_window, list_teams_windows


# ---------------------------------------------------------------------------
# 단일 인스턴스 + URL 프로토콜 (meetingagent://) IPC
# ---------------------------------------------------------------------------

_MUTEX_NAME = "Global\\MeetingAgent_Singleton_v1"


def _ipc_cmd_dir() -> Path:
    base = os.environ.get("LOCALAPPDATA") or str(Path.home())
    d = Path(base) / "MeetingAgent" / "commands"
    d.mkdir(parents=True, exist_ok=True)
    return d


def parse_protocol_url(url: str) -> Optional[str]:
    """meetingagent://<action> 또는 meetingagent://<action>/ → 'start', 'stop', 'status', 'show'."""
    if not url or not url.startswith("meetingagent://"):
        return None
    try:
        parsed = urllib.parse.urlparse(url)
        host = (parsed.hostname or "").strip().lower()
        if host:
            return host
        path = parsed.path.strip("/").lower()
        return path or None
    except Exception:
        return None


def acquire_singleton_mutex():
    """이미 실행 중이면 None 반환. 점유 성공 시 mutex 핸들 반환(유지 필요)."""
    try:
        import win32api
        import win32event
        import winerror
    except Exception:
        return "no-pywin32"  # 단일 인스턴스 체크 불가 — 진행 허용
    try:
        mutex = win32event.CreateMutex(None, True, _MUTEX_NAME)
        last = win32api.GetLastError()
        if last == winerror.ERROR_ALREADY_EXISTS:
            return None
        return mutex
    except Exception:
        return "no-pywin32"


def send_command_to_running_instance(action: str) -> bool:
    """다른 인스턴스의 명령 큐(파일)에 action 기록."""
    try:
        cmd_file = _ipc_cmd_dir() / f"{int(time.time() * 1000)}_{action}.cmd"
        cmd_file.write_text(
            json.dumps({"action": action, "ts": datetime.now().isoformat()}, ensure_ascii=False),
            encoding="utf-8",
        )
        return True
    except Exception:
        return False


# ---------------------------------------------------------------------------
# 설정 파일 (자막 영역 등) — 실행 파일 옆에 저장
# ---------------------------------------------------------------------------


def _config_path() -> Path:
    if getattr(sys, "frozen", False):
        base = Path(sys.executable).parent
    else:
        base = Path(__file__).parent
    return base / "meeting_agent_config.json"


def load_config() -> dict:
    p = _config_path()
    if p.exists():
        try:
            return json.loads(p.read_text(encoding="utf-8"))
        except Exception:
            return {}
    return {}


def save_config(cfg: dict) -> None:
    p = _config_path()
    try:
        p.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Teams 미팅 창 판별 — UI Automation 기반
#
# Teams 미팅 창의 UIA 트리에는 아래 AutomationId 를 가진 툴바가 있다:
#   - "horizontalMiddleEnd"  = 모임 컨트롤 (Meeting controls)
#   - "horizontalEnd"        = 통화 컨트롤 (Call controls)
#   - "indicators"           = 통화 표시기 (Call indicators)
# 이 중 하나라도 트리에서 발견되면 확실히 미팅 창.
# 제목 키워드에 의존하지 않아 스케줄/Meet Now/미팅 이름 상관없이 동작.
# ---------------------------------------------------------------------------

_MEETING_UIA_IDS = frozenset({"horizontalMiddleEnd", "horizontalEnd", "indicators"})
# 제목 기반 fallback (UIA 실패 시 최후 수단)
_NON_MEETING_HINT = ("calendar", "일정", "채팅", "chat")
_MEETING_HINT = ("모임", "통화", "회의", "meeting", "call")


def _walk_uia(control, max_depth: int = 25, depth: int = 0):
    yield control
    if depth >= max_depth:
        return
    try:
        child = control.GetFirstChildControl()
    except Exception:
        return
    while child is not None:
        yield from _walk_uia(child, max_depth, depth + 1)
        try:
            child = child.GetNextSiblingControl()
        except Exception:
            break


def _has_meeting_uia_markers(hwnd: int, max_depth: int = 25) -> bool:
    """창 hwnd의 UIA 트리에서 미팅 툴바 AutomationId를 찾으면 True."""
    try:
        import uiautomation as auto
    except Exception:
        return False
    try:
        root = auto.ControlFromHandle(hwnd)
        if root is None:
            return False
        for node in _walk_uia(root, max_depth=max_depth):
            try:
                aid = (node.AutomationId or "").strip()
            except Exception:
                continue
            if aid in _MEETING_UIA_IDS:
                return True
        return False
    except Exception:
        return False


def _title_hint_meeting(title: str) -> bool:
    """UIA fallback용 제목 기반 휴리스틱."""
    if "Microsoft Teams" not in title:
        return False
    low = title.lower()
    if any(kw in low for kw in _NON_MEETING_HINT):
        return False
    if any(kw in title or kw.lower() in low for kw in _MEETING_HINT):
        return True
    return False


def find_meeting_window() -> Optional[WindowRect]:
    """UIA로 미팅 툴바 존재 여부를 확인해 미팅 창을 고른다.

    UIA 확인 실패/예외 시 제목 기반 휴리스틱으로 폴백.
    """
    wins = list_teams_windows()
    if not wins:
        return None

    # 1) UIA 기반 — 미팅 UI 컨트롤이 있는 창
    uia_matches = []
    for w in wins:
        try:
            if _has_meeting_uia_markers(w.hwnd):
                uia_matches.append(w)
        except Exception:
            continue
    if uia_matches:
        return max(uia_matches, key=lambda w: w.width * w.height)

    # 2) Fallback — 제목 키워드
    title_matches = [w for w in wins if _title_hint_meeting(w.title)]
    if title_matches:
        return max(title_matches, key=lambda w: w.width * w.height)

    return None


# ---------------------------------------------------------------------------
# 미팅 감지 스레드
# ---------------------------------------------------------------------------


class MeetingDetector(threading.Thread):
    def __init__(self, on_detected, on_ended, poll_interval: float = 2.0) -> None:
        super().__init__(daemon=True, name="meeting-detector")
        self.on_detected = on_detected
        self.on_ended = on_ended
        self.poll_interval = poll_interval
        self._stop = threading.Event()
        self._current: Optional[WindowRect] = None
        self._missing_count = 0
        self._end_confirm_cycles = 3

    def run(self) -> None:
        while not self._stop.is_set():
            win = find_meeting_window()
            if win is not None:
                self._missing_count = 0
                if self._current is None:
                    self._current = win
                    try:
                        self.on_detected(win)
                    except Exception:
                        pass
            else:
                if self._current is not None:
                    self._missing_count += 1
                    if self._missing_count >= self._end_confirm_cycles:
                        self._current = None
                        self._missing_count = 0
                        try:
                            self.on_ended()
                        except Exception:
                            pass
            self._stop.wait(self.poll_interval)

    def stop(self) -> None:
        self._stop.set()


# ---------------------------------------------------------------------------
# 세션 워커 러너
# ---------------------------------------------------------------------------


class SessionRunner:
    def __init__(
        self,
        work_dir: Path,
        logger,
        caption_region: Optional[dict] = None,
    ) -> None:
        self.work_dir = work_dir
        self.work_dir.mkdir(parents=True, exist_ok=True)
        self.slides_dir = work_dir / "slides"
        self.audio_dir = work_dir / "audio"
        self.transcript_dir = work_dir / "transcript"
        for d in (self.slides_dir, self.audio_dir, self.transcript_dir):
            d.mkdir(parents=True, exist_ok=True)
        self.audio_path = self.audio_dir / "audio.ogg"
        self.captions_path = self.transcript_dir / "captions.jsonl"
        self._slides_staging = work_dir / "_slides_staging"
        self.log = logger
        self.caption_region = caption_region

        self.capture: Optional[ChangeCapture] = None
        self.capture_thread: Optional[threading.Thread] = None
        self.audio: Optional[MixedAudioRecorder] = None
        self.caption: Optional[CaptionOcrScraper] = None
        self.started_at: Optional[datetime] = None

    def start(self) -> None:
        self.started_at = datetime.now()

        self.log("[1/3] 발표 자료(화면) 감지 시작")
        self.capture = ChangeCapture(self._slides_staging, interval=1.0, phash_threshold=8)
        self.capture_thread = threading.Thread(target=self._run_capture, daemon=True, name="capture-runner")
        self.capture_thread.start()

        self.log("[2/3] 오디오 녹음 시작 (시스템 + 마이크 믹스)")
        try:
            self.audio = MixedAudioRecorder(self.audio_path, AudioConfig(samplerate=48000, channels=2))
            self.audio.start()
        except Exception as e:
            self.log(f"      오디오 시작 실패: {e}")
            self.audio = None

        self.log("[3/3] 자막 OCR 시작 (모델 로드 중, 수십 초 소요 가능)")
        try:
            cfg = CaptionOcrConfig(interval=1.5, absolute_region=self.caption_region)
            self.caption = CaptionOcrScraper(self.captions_path, cfg)
            self.caption.start()
            if self.caption_region:
                self.log(f"      자막 OCR 준비 완료 (지정 영역: {self.caption_region})")
            else:
                self.log("      자막 OCR 준비 완료 (기본 영역: 창 하단 비율)")
        except Exception as e:
            self.log(f"      자막 OCR 실패: {e}")
            self.caption = None

    def _run_capture(self) -> None:
        try:
            self.capture.run()  # type: ignore[union-attr]
        except Exception as e:
            self.log(f"[capture] 실행 오류: {e}")

    def stop(self) -> None:
        if self.capture is not None:
            self.capture.request_stop()
        if self.audio is not None:
            try:
                self.audio.stop(timeout=5.0)
            except Exception as e:
                self.log(f"[audio] 정지 오류: {e}")
        if self.caption is not None:
            try:
                self.caption.stop(timeout=5.0)
            except Exception as e:
                self.log(f"[caption] 정지 오류: {e}")
        if self.capture_thread is not None:
            self.capture_thread.join(timeout=5.0)

        # 캡처가 만든 datetime 서브폴더 평탄화
        try:
            if self._slides_staging.exists():
                for sess in self._slides_staging.iterdir():
                    if sess.is_dir():
                        for f in sess.iterdir():
                            target = self.slides_dir / f.name
                            shutil.move(str(f), str(target))
                        try:
                            sess.rmdir()
                        except Exception:
                            pass
                try:
                    self._slides_staging.rmdir()
                except Exception:
                    pass
        except Exception as e:
            self.log(f"[slides] 정리 오류: {e}")

    def summary_counts(self) -> dict:
        slides = len(list(self.slides_dir.glob("slide_*.webp"))) if self.slides_dir.exists() else 0
        audio = self.audio_path.stat().st_size if self.audio_path.exists() else 0
        caps = 0
        if self.captions_path.exists():
            with self.captions_path.open("r", encoding="utf-8") as f:
                caps = sum(1 for _ in f)
        return {"slides": slides, "audio_bytes": audio, "captions_lines": caps}


# ---------------------------------------------------------------------------
# 드래그로 자막 영역 선택 (절대 화면 좌표)
# ---------------------------------------------------------------------------


def _virtual_screen_bounds() -> tuple[int, int, int, int]:
    """여러 모니터를 포함한 가상 스크린 좌표 (left, top, width, height)."""
    try:
        u32 = ctypes.windll.user32
        vx = u32.GetSystemMetrics(76)  # SM_XVIRTUALSCREEN
        vy = u32.GetSystemMetrics(77)  # SM_YVIRTUALSCREEN
        vw = u32.GetSystemMetrics(78)  # SM_CXVIRTUALSCREEN
        vh = u32.GetSystemMetrics(79)  # SM_CYVIRTUALSCREEN
        return vx, vy, vw, vh
    except Exception:
        # fallback: 주 모니터
        return 0, 0, 1920, 1080


class RegionPicker:
    """전체 화면 위에 반투명 오버레이를 띄우고 드래그로 영역 선택."""

    def __init__(self, master: tk.Tk) -> None:
        self.result: Optional[dict] = None
        vx, vy, vw, vh = _virtual_screen_bounds()
        self._vx, self._vy = vx, vy

        self.top = tk.Toplevel(master)
        self.top.overrideredirect(True)
        try:
            self.top.attributes("-topmost", True)
        except tk.TclError:
            pass
        try:
            self.top.attributes("-alpha", 0.30)
        except tk.TclError:
            pass
        self.top.geometry(f"{vw}x{vh}+{vx}+{vy}")
        self.top.configure(bg="gray30")

        self.canvas = tk.Canvas(self.top, cursor="crosshair", bg="gray30", highlightthickness=0)
        self.canvas.pack(fill=tk.BOTH, expand=True)

        self.canvas.create_text(
            vw // 2,
            40,
            text="자막이 뜨는 영역을 드래그로 선택 (ESC=취소)",
            fill="white",
            font=("맑은 고딕", 16, "bold"),
        )

        self._start_xy: Optional[tuple[int, int]] = None  # canvas local
        self._rect_id: Optional[int] = None
        self.canvas.bind("<Button-1>", self._on_press)
        self.canvas.bind("<B1-Motion>", self._on_drag)
        self.canvas.bind("<ButtonRelease-1>", self._on_release)
        self.top.bind("<Escape>", lambda _e: self._cancel())
        self.top.focus_set()

    def _on_press(self, e: tk.Event) -> None:
        self._start_xy = (e.x, e.y)
        if self._rect_id is not None:
            self.canvas.delete(self._rect_id)
        self._rect_id = self.canvas.create_rectangle(
            e.x, e.y, e.x, e.y, outline="red", width=3
        )

    def _on_drag(self, e: tk.Event) -> None:
        if self._start_xy is None or self._rect_id is None:
            return
        sx, sy = self._start_xy
        self.canvas.coords(self._rect_id, sx, sy, e.x, e.y)

    def _on_release(self, e: tk.Event) -> None:
        if self._start_xy is None:
            return
        sx, sy = self._start_xy
        ex, ey = e.x, e.y
        left_local, right_local = sorted([sx, ex])
        top_local, bottom_local = sorted([sy, ey])
        # 캔버스 로컬 좌표 → 가상 스크린 절대 좌표
        left = self._vx + left_local
        top = self._vy + top_local
        w = right_local - left_local
        h = bottom_local - top_local
        if w < 20 or h < 10:
            self.result = None
        else:
            self.result = {"left": int(left), "top": int(top), "width": int(w), "height": int(h)}
        self.top.destroy()

    def _cancel(self) -> None:
        self.result = None
        self.top.destroy()

    def show_modal(self) -> Optional[dict]:
        self.top.grab_set()
        self.top.wait_window()
        return self.result


# ---------------------------------------------------------------------------
# 자막 영역 실시간 미리보기 창
# ---------------------------------------------------------------------------


_FALLBACK_RATIOS = (0.10, 0.70, 0.90, 0.96)  # left, top, right, bottom


def compute_effective_region(caption_region: Optional[dict]) -> Optional[dict]:
    if caption_region and caption_region.get("width", 0) >= 40 and caption_region.get("height", 0) >= 20:
        return {
            "left": int(caption_region["left"]),
            "top": int(caption_region["top"]),
            "width": int(caption_region["width"]),
            "height": int(caption_region["height"]),
        }
    win = find_teams_window()
    if win is None:
        return None
    lr, tr, rr, br = _FALLBACK_RATIOS
    left = win.left + int(win.width * lr)
    right = win.left + int(win.width * rr)
    top = win.top + int(win.height * tr)
    bottom = win.top + int(win.height * br)
    w, h = right - left, bottom - top
    if w < 40 or h < 20:
        return None
    return {"left": left, "top": top, "width": w, "height": h}


class RegionPreviewWindow:
    """자막 영역을 500ms 간격으로 라이브 스크린샷하여 표시."""

    MAX_W = 520
    MAX_H = 200

    def __init__(self, master: tk.Tk, get_region) -> None:
        self.master = master
        self.get_region = get_region
        self._stop = False
        self._photo: Optional[ImageTk.PhotoImage] = None
        self._sct = mss.mss()

        self.top = tk.Toplevel(master)
        self.top.title("자막 영역 미리보기")
        self.top.geometry(f"{self.MAX_W + 20}x{self.MAX_H + 70}+80+540")
        try:
            self.top.attributes("-topmost", True)
        except tk.TclError:
            pass
        self.top.protocol("WM_DELETE_WINDOW", self._on_close)

        self.status_var = tk.StringVar(value="(초기화 중)")
        ttk.Label(self.top, textvariable=self.status_var, font=("맑은 고딕", 9)).pack(pady=(6, 2))
        self.canvas = tk.Canvas(self.top, width=self.MAX_W, height=self.MAX_H, bg="black", highlightthickness=1, highlightbackground="#888")
        self.canvas.pack(padx=8, pady=6)
        self._img_id = self.canvas.create_text(self.MAX_W // 2, self.MAX_H // 2, text="(캡처 대기)", fill="white")

        self.top.after(100, self._tick)

    def _tick(self) -> None:
        if self._stop:
            return
        region = self.get_region() or compute_effective_region(self.get_region())
        # get_region이 None 돌려주면 fallback
        if region is None:
            region = compute_effective_region(None)

        if region is None:
            self.status_var.set("영역 계산 불가 — Teams 창 없음 + 영역 미지정")
            self.canvas.delete("all")
            self._img_id = self.canvas.create_text(self.MAX_W // 2, self.MAX_H // 2, text="(영역 없음)", fill="white")
        else:
            try:
                raw = self._sct.grab(region)
                img = Image.frombytes("RGB", raw.size, raw.rgb)
                src_w, src_h = img.size
                img.thumbnail((self.MAX_W, self.MAX_H), Image.LANCZOS)
                dst_w, dst_h = img.size
                self._photo = ImageTk.PhotoImage(img)
                self.canvas.delete("all")
                # 중앙 정렬
                x = (self.MAX_W - dst_w) // 2
                y = (self.MAX_H - dst_h) // 2
                self.canvas.create_image(x, y, anchor=tk.NW, image=self._photo)
                self.status_var.set(
                    f"({region['left']},{region['top']})  {src_w}x{src_h}  →  표시 {dst_w}x{dst_h}"
                )
            except Exception as e:
                self.status_var.set(f"캡처 오류: {e}")
        self.top.after(500, self._tick)

    def _on_close(self) -> None:
        self._stop = True
        try:
            self._sct.close()
        except Exception:
            pass
        self.top.destroy()


# ---------------------------------------------------------------------------
# Tkinter UI
# ---------------------------------------------------------------------------


class CommandWatcher(threading.Thread):
    """파일 기반 IPC 큐를 폴링해 원격 명령을 UI로 라우팅."""

    def __init__(self, app: "AppUI", poll: float = 0.5) -> None:
        super().__init__(daemon=True, name="command-watcher")
        self.app = app
        self.poll = poll
        self._stop = threading.Event()
        self.cmd_dir = _ipc_cmd_dir()

    def run(self) -> None:
        while not self._stop.is_set():
            try:
                for f in sorted(self.cmd_dir.glob("*.cmd")):
                    try:
                        data = json.loads(f.read_text(encoding="utf-8"))
                        action = (data.get("action") or "").lower()
                        if action:
                            self.app.dispatch_remote_command(action)
                    except Exception:
                        pass
                    finally:
                        try:
                            f.unlink()
                        except Exception:
                            pass
            except Exception:
                pass
            self._stop.wait(self.poll)

    def stop(self) -> None:
        self._stop.set()


class AppUI:
    def __init__(self, root: tk.Tk) -> None:
        self.root = root
        root.title("Meeting Agent")
        root.geometry("440x480+60+60")
        try:
            root.attributes("-topmost", True)
        except tk.TclError:
            pass

        self.config = load_config()
        self.caption_region: Optional[dict] = self.config.get("caption_region")

        # 헤더
        frame_top = ttk.Frame(root, padding=6)
        frame_top.pack(fill=tk.X)
        self.status_var = tk.StringVar(value="[대기] Teams 미팅을 기다리는 중...")
        ttk.Label(frame_top, textvariable=self.status_var, font=("맑은 고딕", 11, "bold")).pack(anchor=tk.W)

        self.region_var = tk.StringVar()
        ttk.Label(frame_top, textvariable=self.region_var, font=("맑은 고딕", 9), foreground="#444").pack(anchor=tk.W, pady=(3, 0))
        self._refresh_region_label()

        # 버튼 바 (상단)
        frame_ctrl = ttk.Frame(root, padding=(6, 0))
        frame_ctrl.pack(fill=tk.X)
        ttk.Button(frame_ctrl, text="자막 영역 선택", command=self.on_pick_region).pack(side=tk.LEFT)
        ttk.Button(frame_ctrl, text="영역 초기화", command=self.on_clear_region).pack(side=tk.LEFT, padx=4)
        ttk.Button(frame_ctrl, text="영역 미리보기", command=self.on_preview_region).pack(side=tk.LEFT, padx=4)
        self._preview_win: Optional[RegionPreviewWindow] = None

        # 로그
        self.log_area = scrolledtext.ScrolledText(root, height=18, font=("Consolas", 9), wrap=tk.WORD)
        self.log_area.pack(fill=tk.BOTH, expand=True, padx=6, pady=6)
        self.log_area.configure(state="disabled")

        # 버튼 바 (하단)
        frame_btn = ttk.Frame(root, padding=6)
        frame_btn.pack(fill=tk.X)
        self.btn_save = ttk.Button(frame_btn, text="저장 폴더 선택", command=self.on_save_clicked)
        # 미팅 끝났을 때만 pack
        ttk.Button(frame_btn, text="종료", command=self.on_quit).pack(side=tk.RIGHT)

        self._msg_queue: queue.Queue = queue.Queue()
        self.root.after(100, self._drain_queue)

        self.runner: Optional[SessionRunner] = None
        self.detector = MeetingDetector(
            on_detected=self._detected_cb,
            on_ended=self._ended_cb,
        )

        self.log("Meeting Agent 시작. Teams 미팅이 시작되면 자동 감지합니다.")
        if self.caption_region:
            self.log(f"저장된 자막 영역 로드: {self.caption_region}")
        else:
            self.log("자막 영역 미지정 — 상단 '자막 영역 선택' 권장 (미지정 시 창 하단 자동 추정)")
        self.detector.start()

        self.cmd_watcher = CommandWatcher(self)
        self.cmd_watcher.start()

    def _refresh_region_label(self) -> None:
        if self.caption_region:
            r = self.caption_region
            self.region_var.set(f"자막 영역: ({r['left']},{r['top']}) {r['width']}x{r['height']}")
        else:
            self.region_var.set("자막 영역: (미지정 — 창 하단 자동)")

    def log(self, msg: str) -> None:
        ts = datetime.now().strftime("%H:%M:%S")
        self._msg_queue.put(("log", f"[{ts}] {msg}"))

    def set_status(self, text: str) -> None:
        self._msg_queue.put(("status", text))

    def _drain_queue(self) -> None:
        try:
            while True:
                kind, payload = self._msg_queue.get_nowait()
                if kind == "status":
                    self.status_var.set(payload)
                elif kind == "log":
                    self.log_area.configure(state="normal")
                    self.log_area.insert(tk.END, payload + "\n")
                    self.log_area.see(tk.END)
                    self.log_area.configure(state="disabled")
                elif kind == "show_save":
                    self.btn_save.pack(side=tk.LEFT)
                elif kind == "hide_save":
                    self.btn_save.pack_forget()
        except queue.Empty:
            pass
        self.root.after(150, self._drain_queue)

    # 영역 선택
    def on_pick_region(self) -> None:
        if self.runner is not None:
            messagebox.showwarning("알림", "세션이 진행 중에는 영역을 바꿀 수 없습니다. 미팅 종료 후에 다시 시도하세요.")
            return
        self.log("자막 영역 선택 모드 — 화면 전체에 반투명 오버레이가 뜹니다.")
        try:
            self.root.attributes("-topmost", False)
        except tk.TclError:
            pass
        picker = RegionPicker(self.root)
        result = picker.show_modal()
        try:
            self.root.attributes("-topmost", True)
        except tk.TclError:
            pass
        if result:
            self.caption_region = result
            self.config["caption_region"] = result
            save_config(self.config)
            self.log(f"자막 영역 저장: {result}")
            self._refresh_region_label()
        else:
            self.log("영역 선택이 취소되었습니다.")

    def on_clear_region(self) -> None:
        if self.caption_region is None:
            self.log("이미 영역이 미지정 상태입니다.")
            return
        self.caption_region = None
        self.config.pop("caption_region", None)
        save_config(self.config)
        self.log("자막 영역을 미지정으로 초기화 (창 하단 비율로 자동 추정).")
        self._refresh_region_label()

    def on_preview_region(self) -> None:
        existing = self._preview_win
        if existing is not None:
            try:
                if existing.top.winfo_exists():
                    existing.top.lift()
                    existing.top.focus_set()
                    return
            except Exception:
                pass
        self._preview_win = RegionPreviewWindow(self.root, lambda: self.caption_region)
        self.log("자막 영역 미리보기 창 열림 (0.5초 간격 라이브 업데이트)")

    # 감지 콜백
    def _detected_cb(self, win: WindowRect) -> None:
        self.log(f"Teams 미팅이 감지되었습니다: {win.title!r}")
        self.log("다음 3가지를 진행하겠습니다:")
        self.log("  1. 오디오 녹음")
        self.log("  2. 전사/자막 감지 (OCR)")
        self.log("  3. 발표 자료(화면) 감지")
        self.set_status("[녹화중] 세션 진행 중...")

        work = Path.cwd() / "_active_session" / datetime.now().strftime("%Y-%m-%d_%H%M%S")
        self.runner = SessionRunner(work, self.log, caption_region=self.caption_region)
        threading.Thread(target=self.runner.start, daemon=True, name="session-starter").start()

    def _ended_cb(self) -> None:
        self.log("Teams 미팅이 종료되었습니다. 감지된 내용을 정리합니다.")
        self.set_status("[정리중] 워커 정지 중...")
        threading.Thread(target=self._finalize_and_prompt, daemon=True, name="finalizer").start()

    def _finalize_and_prompt(self) -> None:
        if self.runner is not None:
            self.runner.stop()
            info = self.runner.summary_counts()
            self.log(
                f"세션 정리: 슬라이드 {info['slides']}장, "
                f"오디오 {info['audio_bytes']/1024:.1f}KB, 자막 {info['captions_lines']}줄"
            )
        self.set_status("[대기] 저장 폴더를 지정해주세요")
        self.log("저장할 폴더를 선택해주세요 ('저장 폴더 선택' 버튼).")
        self._msg_queue.put(("show_save", None))

    def on_save_clicked(self) -> None:
        if self.runner is None:
            messagebox.showwarning("알림", "정리된 세션이 없습니다.")
            return
        folder = filedialog.askdirectory(title="회의록을 저장할 루트 폴더를 선택하세요")
        if not folder:
            return
        stamp = (self.runner.started_at or datetime.now()).strftime("%Y-%m-%d_%H%M%S")
        target = Path(folder) / f"meeting_{stamp}"
        target.mkdir(parents=True, exist_ok=True)
        src = self.runner.work_dir
        try:
            for sub in ("audio", "transcript", "slides"):
                src_sub = src / sub
                tgt_sub = target / sub
                if src_sub.exists():
                    if tgt_sub.exists():
                        shutil.rmtree(tgt_sub)
                    shutil.move(str(src_sub), str(tgt_sub))
            shutil.rmtree(src, ignore_errors=True)
            self.log(f"저장 완료: {target}")
            messagebox.showinfo("완료", f"저장 완료:\n{target}")
        except Exception as e:
            self.log(f"저장 오류: {e}")
            messagebox.showerror("저장 오류", str(e))
            return

        self._msg_queue.put(("hide_save", None))
        self.set_status("[대기] Teams 미팅을 기다리는 중...")
        self.runner = None

    # 원격 명령 라우팅 (CommandWatcher → 메인 스레드)
    def dispatch_remote_command(self, action: str) -> None:
        self.root.after(0, lambda: self._handle_remote_action(action))

    def _handle_remote_action(self, action: str) -> None:
        self.log(f"[remote] 명령 수신: {action!r}")
        if action in ("show", "focus", "open"):
            self._bring_to_front()
        elif action == "start":
            self._bring_to_front()
            self.log("[remote] 'start' — Teams 미팅 자동 감지 대기 중 (미팅 시작 시 자동 녹화)")
        elif action == "stop":
            if self.runner is not None:
                self.log("[remote] 'stop' — 현재 세션을 종료 처리합니다.")
                self._ended_cb()
            else:
                self.log("[remote] 'stop' — 활성 세션이 없습니다.")
        elif action == "status":
            self._bring_to_front()
            state = "세션 진행 중" if self.runner is not None else "대기 중"
            self.log(f"[remote] 현재 상태: {state}")
        else:
            self.log(f"[remote] 알 수 없는 action '{action}' — 무시")

    def _bring_to_front(self) -> None:
        try:
            self.root.deiconify()
            self.root.lift()
            self.root.focus_force()
            self.root.attributes("-topmost", True)
            self.root.after(200, lambda: self.root.attributes("-topmost", True))
        except Exception:
            pass

    def on_quit(self) -> None:
        if self.runner is not None:
            try:
                self.runner.stop()
            except Exception:
                pass
        try:
            self.cmd_watcher.stop()
        except Exception:
            pass
        self.detector.stop()
        self.root.destroy()


def _set_dpi_aware() -> None:
    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(2)
        return
    except Exception:
        pass
    try:
        ctypes.windll.user32.SetProcessDPIAware()
    except Exception:
        pass


def main() -> int:
    _set_dpi_aware()

    # argv 에 meetingagent://<action> 이 있으면 파싱
    protocol_action: Optional[str] = None
    for a in sys.argv[1:]:
        act = parse_protocol_url(a)
        if act:
            protocol_action = act
            break

    # 단일 인스턴스 체크
    mutex = acquire_singleton_mutex()
    if mutex is None:
        # 이미 실행 중 — 명령만 전달하고 즉시 종료
        if protocol_action:
            send_command_to_running_instance(protocol_action)
        # 사용자가 그냥 더블클릭한 경우: 기존 창 띄우기 요청
        else:
            send_command_to_running_instance("show")
        return 0

    root = tk.Tk()
    app = AppUI(root)
    root.protocol("WM_DELETE_WINDOW", app.on_quit)

    # 내가 첫 인스턴스인데 프로토콜 action 들어왔으면 부트 직후 실행
    if protocol_action:
        root.after(500, lambda: app.dispatch_remote_command(protocol_action))

    root.mainloop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
