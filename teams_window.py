"""Win32 API로 Teams 창을 찾아 캡처 영역(bounding rect)을 구한다."""
from __future__ import annotations

from dataclasses import dataclass

import win32gui


_TEAMS_TITLE_KEYWORDS = ("Microsoft Teams",)
_MEETING_HINTS = ("meeting", "call", "회의", "통화")
_MIN_WIN_SIZE = 200


@dataclass
class WindowRect:
    left: int
    top: int
    right: int
    bottom: int
    title: str
    hwnd: int

    @property
    def width(self) -> int:
        return self.right - self.left

    @property
    def height(self) -> int:
        return self.bottom - self.top

    @property
    def mss_region(self) -> dict:
        return {
            "left": self.left,
            "top": self.top,
            "width": self.width,
            "height": self.height,
        }


def find_teams_window() -> WindowRect | None:
    """Teams 창 중 가장 유력한 하나를 반환. 없으면 None."""
    candidates: list[WindowRect] = []

    def _enum_cb(hwnd: int, _extra: object) -> bool:
        if not win32gui.IsWindowVisible(hwnd):
            return True
        if win32gui.IsIconic(hwnd):
            return True
        title = win32gui.GetWindowText(hwnd)
        if not title:
            return True
        if not any(kw in title for kw in _TEAMS_TITLE_KEYWORDS):
            return True
        left, top, right, bottom = win32gui.GetWindowRect(hwnd)
        w = right - left
        h = bottom - top
        if w < _MIN_WIN_SIZE or h < _MIN_WIN_SIZE:
            return True
        candidates.append(
            WindowRect(left=left, top=top, right=right, bottom=bottom, title=title, hwnd=hwnd)
        )
        return True

    win32gui.EnumWindows(_enum_cb, None)
    if not candidates:
        return None

    for w in candidates:
        lowered = w.title.lower()
        if any(hint in lowered for hint in _MEETING_HINTS):
            return w

    return max(candidates, key=lambda w: w.width * w.height)


def list_teams_windows() -> list[WindowRect]:
    """디버그용: 감지된 Teams 창 전부."""
    found: list[WindowRect] = []

    def _cb(hwnd: int, _extra: object) -> bool:
        if not win32gui.IsWindowVisible(hwnd) or win32gui.IsIconic(hwnd):
            return True
        title = win32gui.GetWindowText(hwnd)
        if not title or not any(kw in title for kw in _TEAMS_TITLE_KEYWORDS):
            return True
        left, top, right, bottom = win32gui.GetWindowRect(hwnd)
        found.append(
            WindowRect(left=left, top=top, right=right, bottom=bottom, title=title, hwnd=hwnd)
        )
        return True

    win32gui.EnumWindows(_cb, None)
    return found
