"""WebVTT 전사본 파서 + 마크다운 변환기.

Microsoft Teams transcription 포맷 대응:
- `WEBVTT` 헤더
- NOTE/STYLE/REGION 블록 스킵
- 선택적 cue ID 라인
- 타임스탬프: HH:MM:SS.mmm --> HH:MM:SS.mmm
- 화자 표기: <v Speaker Name>text</v>  (voice span)
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import timedelta
from typing import Optional


_TIMESTAMP_RE = re.compile(
    r"(\d{2,}):(\d{2}):(\d{2})[.,](\d{3})\s*-->\s*"
    r"(\d{2,}):(\d{2}):(\d{2})[.,](\d{3})"
)
_VOICE_TAG_FULL_RE = re.compile(r"<v\s+([^>]+?)>(.*?)</v>", re.DOTALL)
_VOICE_TAG_OPEN_RE = re.compile(r"^\s*<v\s+([^>]+?)>(.*)$", re.DOTALL)
_TAG_STRIP_RE = re.compile(r"<[^>]+>")


@dataclass
class TranscriptCue:
    index: int
    start: timedelta
    end: timedelta
    speaker: Optional[str]
    text: str

    @property
    def start_hhmmss(self) -> str:
        return _fmt_hhmmss(self.start)

    @property
    def end_hhmmss(self) -> str:
        return _fmt_hhmmss(self.end)

    @property
    def duration_sec(self) -> float:
        return (self.end - self.start).total_seconds()


def _fmt_hhmmss(td: timedelta) -> str:
    total_ms = max(0, int(td.total_seconds() * 1000))
    h, rem = divmod(total_ms, 3_600_000)
    m, rem = divmod(rem, 60_000)
    s = rem // 1000
    return f"{h:02d}:{m:02d}:{s:02d}"


def _to_timedelta(h: str, m: str, s: str, ms: str) -> timedelta:
    return timedelta(hours=int(h), minutes=int(m), seconds=int(s), milliseconds=int(ms))


def _extract_voice(raw: str) -> tuple[Optional[str], str]:
    """voice span에서 (speaker, text)를 뽑는다. 태그가 없으면 speaker=None."""
    m = _VOICE_TAG_FULL_RE.search(raw)
    if m:
        speaker = m.group(1).strip()
        body = _VOICE_TAG_FULL_RE.sub(lambda x: x.group(2), raw)
        body = _TAG_STRIP_RE.sub("", body).strip()
        return speaker, body

    m2 = _VOICE_TAG_OPEN_RE.match(raw.strip())
    if m2:
        speaker = m2.group(1).strip()
        body = _TAG_STRIP_RE.sub("", m2.group(2)).strip()
        return speaker, body

    return None, _TAG_STRIP_RE.sub("", raw).strip()


def parse_vtt(text: str) -> list[TranscriptCue]:
    """WebVTT 텍스트를 TranscriptCue 리스트로 파싱."""
    if text.startswith("﻿"):
        text = text[1:]
    lines = text.splitlines()
    if not lines or not lines[0].strip().startswith("WEBVTT"):
        raise ValueError("Not a WEBVTT file (missing 'WEBVTT' header)")

    cues: list[TranscriptCue] = []
    idx = 0
    i = 1
    n = len(lines)

    while i < n:
        line = lines[i]
        stripped = line.strip()

        if not stripped:
            i += 1
            continue

        # NOTE/STYLE/REGION 블록은 빈 줄까지 스킵
        if re.match(r"^(NOTE|STYLE|REGION)(\s|$)", stripped):
            while i < n and lines[i].strip():
                i += 1
            continue

        ts_match = _TIMESTAMP_RE.search(stripped)
        if not ts_match:
            # cue ID 라인일 수 있음 — 다음 줄에서 타임스탬프 기대
            i += 1
            if i >= n:
                break
            next_stripped = lines[i].strip()
            ts_match = _TIMESTAMP_RE.search(next_stripped)
            if not ts_match:
                # 포맷 이상 — 다음으로
                continue

        start = _to_timedelta(*ts_match.group(1, 2, 3, 4))
        end = _to_timedelta(*ts_match.group(5, 6, 7, 8))
        i += 1

        text_lines: list[str] = []
        while i < n and lines[i].strip():
            text_lines.append(lines[i])
            i += 1
        raw_text = "\n".join(text_lines).strip()
        speaker, clean_text = _extract_voice(raw_text)

        idx += 1
        cues.append(
            TranscriptCue(index=idx, start=start, end=end, speaker=speaker, text=clean_text)
        )

    return cues


def render_markdown(
    cues: list[TranscriptCue],
    title: str = "회의 전사본",
    session_start: Optional[str] = None,
    merge_consecutive_same_speaker: bool = True,
) -> str:
    lines: list[str] = [f"# {title}", ""]
    if session_start:
        lines.append(f"- 시작: {session_start}")
    lines.append(f"- 발언 cue: {len(cues)}개")
    speakers = sorted({c.speaker for c in cues if c.speaker})
    if speakers:
        lines.append(f"- 화자: {', '.join(speakers)}")
    lines.append("")

    if not cues:
        lines.append("_(전사 내용 없음)_")
        return "\n".join(lines)

    if merge_consecutive_same_speaker:
        groups: list[list[TranscriptCue]] = []
        for cue in cues:
            if groups and groups[-1][-1].speaker == cue.speaker:
                groups[-1].append(cue)
            else:
                groups.append([cue])
    else:
        groups = [[c] for c in cues]

    for group in groups:
        speaker = group[0].speaker or "_(화자 미상)_"
        start_ts = group[0].start_hhmmss
        text = " ".join(c.text for c in group if c.text).strip()
        lines.append(f"## {start_ts} — {speaker}")
        lines.append("")
        if text:
            lines.append(text)
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def summarize(cues: list[TranscriptCue]) -> dict:
    """파싱 결과 요약 — CLI에서 한눈에 보기 위한 구조."""
    if not cues:
        return {"cue_count": 0, "speakers": [], "duration": "00:00:00"}
    speakers = sorted({c.speaker for c in cues if c.speaker})
    last_end = max(c.end for c in cues)
    return {
        "cue_count": len(cues),
        "speakers": speakers,
        "duration": _fmt_hhmmss(last_end),
        "first_start": cues[0].start_hhmmss,
        "last_end": _fmt_hhmmss(last_end),
    }
