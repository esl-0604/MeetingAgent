"""슬라이드 캡처(1단계) + 전사(2단계) → 타임스탬프 기준 병합 회의록."""
from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Optional, Union

from transcript import TranscriptCue


@dataclass
class SlideRef:
    absolute_time: datetime
    filename: str
    phash: str
    hamming_from_prev: Optional[int]


@dataclass
class SpeechEvent:
    absolute_time: datetime
    speaker: Optional[str]
    text: str


Event = Union[SlideRef, SpeechEvent]


def load_capture_session(session_dir: Path) -> tuple[datetime, list[SlideRef]]:
    """capture 세션 폴더의 metadata.json에서 세션 시작 시각 + 슬라이드 프레임 목록을 로드."""
    metadata_path = session_dir / "metadata.json"
    data = json.loads(metadata_path.read_text(encoding="utf-8"))
    session_start = datetime.fromisoformat(data["session_start"])
    slides: list[SlideRef] = []
    for f in data.get("frames", []):
        slides.append(
            SlideRef(
                absolute_time=datetime.fromisoformat(f["timestamp"]),
                filename=f["filename"],
                phash=f["phash"],
                hamming_from_prev=f.get("hamming_from_prev"),
            )
        )
    return session_start, slides


def cues_to_speech_events(
    cues: list[TranscriptCue], meeting_start: datetime
) -> list[SpeechEvent]:
    return [
        SpeechEvent(
            absolute_time=meeting_start + c.start,
            speaker=c.speaker,
            text=c.text,
        )
        for c in cues
    ]


def is_blank_hash(phash: str) -> bool:
    """의미 없는 단색 프레임(전부 0) 필터용."""
    s = phash.strip().lower()
    return bool(s) and all(ch == "0" for ch in s)


def render_notes(
    events: list[Event],
    *,
    title: str,
    meeting_start: datetime,
    slide_relative_prefix: str = "./",
) -> str:
    events_sorted = sorted(events, key=lambda e: e.absolute_time)

    slides_count = sum(1 for e in events_sorted if isinstance(e, SlideRef))
    cues_count = sum(1 for e in events_sorted if isinstance(e, SpeechEvent))
    speakers = sorted(
        {
            e.speaker
            for e in events_sorted
            if isinstance(e, SpeechEvent) and e.speaker
        }
    )

    lines: list[str] = [f"# {title}", ""]
    lines.append(f"- 회의 시작: {meeting_start.isoformat(timespec='seconds')}")
    lines.append(f"- 슬라이드: {slides_count}장   발언 cue: {cues_count}건")
    if speakers:
        lines.append(f"- 화자: {', '.join(speakers)}")
    lines.append("")

    section_open = False
    current_speaker: Optional[str] = None
    paragraph: list[str] = []

    def flush_paragraph() -> None:
        if paragraph:
            text = " ".join(paragraph).strip()
            if text:
                lines.append(text)
                lines.append("")
            paragraph.clear()

    def clock(dt: datetime) -> str:
        return dt.strftime("%H:%M:%S")

    for event in events_sorted:
        if isinstance(event, SpeechEvent):
            speaker_label = event.speaker or "_(화자 미상)_"
            if not section_open or speaker_label != current_speaker:
                flush_paragraph()
                current_speaker = speaker_label
                section_open = True
                lines.append(f"## {clock(event.absolute_time)} — {current_speaker}")
                lines.append("")
            if event.text:
                paragraph.append(event.text)

        elif isinstance(event, SlideRef):
            flush_paragraph()
            if not section_open:
                lines.append(f"## {clock(event.absolute_time)} — _(슬라이드 전환)_")
                lines.append("")
            rel = slide_relative_prefix + event.filename
            lines.append(f"![slide {clock(event.absolute_time)}]({rel})")
            lines.append("")

    flush_paragraph()
    return "\n".join(lines).rstrip() + "\n"
