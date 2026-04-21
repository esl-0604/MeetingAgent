"""Teams 창의 UI Automation 트리를 덤프/관찰해서 자막·전사 패널 텍스트 요소를 찾는다.

검증 단계 전용. 실제 운영용 scraper는 이 결과를 기반으로 작성한다.

사용법:
    python main.py uia-probe dump   # 한 번 스냅샷 덤프
    python main.py uia-probe watch  # 5초 간격으로 텍스트 변화 추적
"""
from __future__ import annotations

import hashlib
import time
from dataclasses import dataclass
from typing import Iterator, Optional

import uiautomation as auto

from teams_window import find_teams_window


@dataclass
class UiaNode:
    path: str
    control_type: str
    name: str
    value: str
    automation_id: str
    class_name: str

    @property
    def text(self) -> str:
        parts = [self.name, self.value]
        return " | ".join(p for p in parts if p)

    @property
    def has_meaningful_text(self) -> bool:
        t = self.text.strip()
        return len(t) >= 3 and not t.isspace()


def _walk(
    element: auto.Control, path: str = "", max_depth: int = 20, depth: int = 0
) -> Iterator[UiaNode]:
    if depth > max_depth:
        return
    try:
        ct = element.ControlTypeName
        name = (element.Name or "").strip()
        value = ""
        try:
            pattern = element.GetValuePattern()
            if pattern is not None:
                value = (pattern.Value or "").strip()
        except Exception:
            pass
        aid = (element.AutomationId or "").strip()
        cls = (element.ClassName or "").strip()
    except Exception:
        return

    segment = f"{ct}[{name[:24]}]" if name else ct
    cur_path = f"{path}/{segment}" if path else segment
    yield UiaNode(
        path=cur_path,
        control_type=ct,
        name=name,
        value=value,
        automation_id=aid,
        class_name=cls,
    )

    try:
        child = element.GetFirstChildControl()
    except Exception:
        child = None
    while child is not None:
        yield from _walk(child, cur_path, max_depth, depth + 1)
        try:
            child = child.GetNextSiblingControl()
        except Exception:
            break


def _get_teams_control() -> Optional[auto.Control]:
    win = find_teams_window()
    if win is None:
        return None
    try:
        return auto.ControlFromHandle(win.hwnd)
    except Exception:
        return None


def dump(max_depth: int = 20, min_text_len: int = 3) -> list[UiaNode]:
    root = _get_teams_control()
    if root is None:
        return []
    nodes: list[UiaNode] = []
    for node in _walk(root, max_depth=max_depth):
        if len(node.text) >= min_text_len:
            nodes.append(node)
    return nodes


def print_dump(max_depth: int = 20) -> int:
    """한 번 UIA 트리 덤프해 텍스트 있는 노드만 출력."""
    root = _get_teams_control()
    if root is None:
        print("Teams 창을 찾지 못했습니다. Teams 회의 창을 연 상태로 다시 실행하세요.")
        return 1
    print(f"[uia] root: {root.ControlTypeName}  name={root.Name!r}")
    total = 0
    text_nodes = 0
    for node in _walk(root, max_depth=max_depth):
        total += 1
        if node.has_meaningful_text:
            text_nodes += 1
            summary = node.text.replace("\n", " ⏎ ")
            if len(summary) > 160:
                summary = summary[:157] + "..."
            hint = []
            if node.automation_id:
                hint.append(f"aid={node.automation_id}")
            if node.class_name:
                hint.append(f"cls={node.class_name}")
            hint_str = f"  ({', '.join(hint)})" if hint else ""
            print(f"  [{node.control_type}] {summary}{hint_str}")
    print()
    print(f"[uia] 총 노드 {total}, 텍스트 노드 {text_nodes}")
    return 0


def _fingerprint(text: str) -> str:
    return hashlib.md5(text.encode("utf-8", "ignore")).hexdigest()[:8]


def watch(interval: float = 3.0, max_iterations: int = 120, max_depth: int = 20) -> int:
    """interval 초 간격으로 UIA 트리를 스냅샷해서 새로 나타나거나 바뀐 텍스트 노드를 찾는다.
    Live caption / transcript 후보를 식별하기 위함.
    """
    root = _get_teams_control()
    if root is None:
        print("Teams 창을 찾지 못했습니다.")
        return 1

    print(f"[uia-watch] {interval}s 간격으로 UIA 트리 감시. Ctrl+C로 종료.")
    seen: dict[str, str] = {}  # path -> fingerprint of text
    it = 0
    try:
        while it < max_iterations:
            it += 1
            tick = time.monotonic()
            changes_new: list[UiaNode] = []
            changes_updated: list[UiaNode] = []
            current_paths: set[str] = set()
            for node in _walk(root, max_depth=max_depth):
                if not node.has_meaningful_text:
                    continue
                current_paths.add(node.path)
                fp = _fingerprint(node.text)
                prev = seen.get(node.path)
                if prev is None:
                    changes_new.append(node)
                elif prev != fp:
                    changes_updated.append(node)
                seen[node.path] = fp

            disappeared = set(seen.keys()) - current_paths
            for p in disappeared:
                seen.pop(p, None)

            if changes_new or changes_updated:
                print(f"--- iter {it} (+{len(changes_new)} ~{len(changes_updated)}) ---")
                for n in changes_new:
                    print(f"  + [{n.control_type}] {n.text[:160]}  (aid={n.automation_id!r}, cls={n.class_name!r})")
                for n in changes_updated:
                    print(f"  ~ [{n.control_type}] {n.text[:160]}  (aid={n.automation_id!r}, cls={n.class_name!r})")

            elapsed = time.monotonic() - tick
            time.sleep(max(0.0, interval - elapsed))
    except KeyboardInterrupt:
        print("[uia-watch] 중단됨.")
    return 0
