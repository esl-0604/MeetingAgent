"""Meeting Archiver 프로토타입 CLI.

사용법:
    python main.py start [--output captures] [--interval 1.0] [--threshold 8]
    python main.py windows   # 현재 감지되는 Teams 창 목록 (디버그)
"""
from __future__ import annotations

import argparse
import signal
import sys
from pathlib import Path


def _set_dpi_aware() -> None:
    """좌표 DPI 스케일 불일치 방지."""
    try:
        import ctypes

        try:
            ctypes.windll.shcore.SetProcessDpiAwareness(2)  # per-monitor v1
            return
        except Exception:
            pass
        try:
            ctypes.windll.user32.SetProcessDPIAware()
        except Exception:
            pass
    except Exception:
        pass


def _force_utf8_stdio() -> None:
    for stream_name in ("stdout", "stderr"):
        stream = getattr(sys, stream_name, None)
        reconf = getattr(stream, "reconfigure", None)
        if reconf is not None:
            try:
                reconf(encoding="utf-8")
            except Exception:
                pass


def _cmd_start(args: argparse.Namespace) -> int:
    from capture import ChangeCapture

    cap = ChangeCapture(
        output_root=args.output,
        interval=args.interval,
        phash_threshold=args.threshold,
        webp_quality=args.quality,
    )

    def _handler(*_):
        print("\n[main] 종료 요청 수신")
        cap.request_stop()

    signal.signal(signal.SIGINT, _handler)
    try:
        signal.signal(signal.SIGTERM, _handler)
    except (AttributeError, ValueError):
        pass

    cap.run()
    return 0


def _cmd_windows(_args: argparse.Namespace) -> int:
    from teams_window import find_teams_window, list_teams_windows

    all_wins = list_teams_windows()
    if not all_wins:
        print("Teams 창이 감지되지 않았습니다.")
        return 1
    print(f"감지된 Teams 창 {len(all_wins)}개:")
    for w in all_wins:
        print(f"  hwnd={w.hwnd}  {w.width}x{w.height}  @({w.left},{w.top})  title={w.title!r}")
    picked = find_teams_window()
    if picked:
        print(f"\n선택될 창: hwnd={picked.hwnd}  title={picked.title!r}")
    return 0


def _cmd_transcript_parse(args: argparse.Namespace) -> int:
    from transcript import parse_vtt, summarize

    text = Path(args.input).read_text(encoding="utf-8")
    cues = parse_vtt(text)
    info = summarize(cues)
    print(f"cue 수      : {info['cue_count']}")
    print(f"총 길이     : {info.get('duration', '—')}")
    print(f"화자        : {', '.join(info['speakers']) if info['speakers'] else '(없음)'}")
    if cues:
        print(f"첫 cue      : [{cues[0].start_hhmmss}] {cues[0].speaker or '미상'}  {cues[0].text[:60]}")
        print(f"마지막 cue  : [{cues[-1].start_hhmmss}] {cues[-1].speaker or '미상'}  {cues[-1].text[:60]}")
    return 0


def _cmd_uia_probe(args: argparse.Namespace) -> int:
    from teams_uia_probe import print_dump, watch

    if args.mode == "dump":
        return print_dump(max_depth=args.max_depth)
    elif args.mode == "watch":
        return watch(interval=args.interval, max_iterations=args.iterations, max_depth=args.max_depth)
    return 2


def _cmd_audio_devices(_args: argparse.Namespace) -> int:
    from audio_recorder import list_audio_devices

    info = list_audio_devices()
    print(f"기본 스피커     : {info['default_speaker']}")
    print(f"기본 마이크     : {info['default_microphone']}")
    print(f"전체 스피커     : {len(info['speakers'])}개")
    for s in info["speakers"]:
        print(f"    - {s}")
    print(f"전체 마이크     : {len(info['microphones'])}개")
    for m in info["microphones"]:
        print(f"    - {m}")
    print(f"Loopback 가능   : {len(info['loopbacks'])}개")
    for lb in info["loopbacks"]:
        print(f"    - {lb}")
    return 0


def _cmd_audio_test(args: argparse.Namespace) -> int:
    import time as _t

    from audio_recorder import AudioConfig, MixedAudioRecorder

    out = args.output
    out.parent.mkdir(parents=True, exist_ok=True)
    rec = MixedAudioRecorder(out, AudioConfig(samplerate=args.samplerate, channels=args.channels))
    print(f"[audio-test] {args.duration}s 녹음 시작 → {out}")
    rec.start()
    _t.sleep(args.duration)
    rec.stop()
    size = out.stat().st_size if out.exists() else 0
    print(f"[audio-test] 종료. duration≈{rec.duration_sec:.2f}s, file={size} bytes")
    return 0


def _cmd_caption_ocr(args: argparse.Namespace) -> int:
    import time as _t

    from caption_ocr import CaptionOcrConfig, CaptionOcrScraper

    cfg = CaptionOcrConfig(
        interval=args.interval,
        region_top_ratio=args.region_top,
        region_bottom_ratio=args.region_bottom,
        region_left_ratio=args.region_left,
        region_right_ratio=args.region_right,
        min_confidence=args.min_conf,
    )
    out = args.output
    scraper = CaptionOcrScraper(out, cfg)
    print(f"[caption-ocr] EasyOCR 모델 로드 중... (처음엔 수십 초 걸릴 수 있음)")
    scraper.start()
    print(f"[caption-ocr] 시작. out={out}  interval={cfg.interval}s  region={cfg.region_left_ratio:.2f}~{cfg.region_right_ratio:.2f} x {cfg.region_top_ratio:.2f}~{cfg.region_bottom_ratio:.2f}")
    print(f"[caption-ocr] Ctrl+C 또는 --duration 경과까지 유지")
    try:
        if args.duration:
            _t.sleep(args.duration)
        else:
            while True:
                _t.sleep(1)
    except KeyboardInterrupt:
        pass
    scraper.stop()
    print(f"[caption-ocr] 종료. {scraper.lines_written}개 라인 기록됨 → {out}")
    return 0


def _cmd_merge(args: argparse.Namespace) -> int:
    import os
    from datetime import datetime

    from merge import (
        cues_to_speech_events,
        is_blank_hash,
        load_capture_session,
        render_notes,
    )
    from transcript import parse_vtt

    slides = []
    session_start = None
    if args.captures_dir:
        session_start, slides = load_capture_session(args.captures_dir)

    cues = []
    if args.transcript:
        cues = parse_vtt(args.transcript.read_text(encoding="utf-8"))

    if args.meeting_start:
        meeting_start = datetime.fromisoformat(args.meeting_start)
    elif session_start is not None:
        meeting_start = session_start
    else:
        print("ERROR: --meeting-start 또는 --captures-dir 중 하나는 필수입니다.")
        return 2

    if args.skip_blank:
        slides = [s for s in slides if not is_blank_hash(s.phash)]

    speech_events = cues_to_speech_events(cues, meeting_start)
    events = [*speech_events, *slides]

    if args.output:
        out_path = args.output
    elif args.captures_dir:
        out_path = args.captures_dir / "meeting_notes.md"
    else:
        out_path = Path("meeting_notes.md")

    # 이미지 상대 경로는 md 저장 위치와 captures-dir 의 관계로 자동 결정
    if args.captures_dir:
        try:
            rel = os.path.relpath(args.captures_dir.resolve(), out_path.resolve().parent)
        except ValueError:
            rel = str(args.captures_dir.resolve())
        rel_posix = rel.replace(os.sep, "/")
        slide_prefix = "./" if rel_posix in (".", "") else rel_posix.rstrip("/") + "/"
    else:
        slide_prefix = "./"

    title = args.title or f"회의록 ({meeting_start.strftime('%Y-%m-%d %H:%M')})"
    md = render_notes(
        events,
        title=title,
        meeting_start=meeting_start,
        slide_relative_prefix=slide_prefix,
    )

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(md, encoding="utf-8")
    print(f"[merge] {len(slides)}장 슬라이드 + {len(cues)}개 cue → {out_path}  (slide prefix: {slide_prefix})")
    return 0


def _cmd_transcript_render(args: argparse.Namespace) -> int:
    from transcript import parse_vtt, render_markdown

    src = Path(args.input)
    text = src.read_text(encoding="utf-8")
    cues = parse_vtt(text)
    md = render_markdown(
        cues,
        title=args.title or f"회의 전사본 ({src.stem})",
        session_start=args.session_start,
        merge_consecutive_same_speaker=not args.no_merge,
    )
    out = args.output if args.output else src.with_suffix(".md")
    out.write_text(md, encoding="utf-8")
    print(f"[transcript] {len(cues)}개 cue → {out}")
    return 0


def main() -> int:
    _force_utf8_stdio()
    _set_dpi_aware()

    parser = argparse.ArgumentParser(description="Teams 회의 슬라이드 변화 감지 캡처 (프로토타입)")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_start = sub.add_parser("start", help="캡처 시작 (Ctrl+C로 종료)")
    p_start.add_argument("--output", type=Path, default=Path("captures"), help="저장 루트 경로")
    p_start.add_argument("--interval", type=float, default=1.0, help="캡처 간격(초)")
    p_start.add_argument("--threshold", type=int, default=8, help="pHash 해밍 거리 임계치")
    p_start.add_argument("--quality", type=int, default=85, help="WebP 품질 0-100")
    p_start.set_defaults(func=_cmd_start)

    p_win = sub.add_parser("windows", help="현재 감지되는 Teams 창 목록 출력")
    p_win.set_defaults(func=_cmd_windows)

    p_tp = sub.add_parser("transcript-parse", help="WebVTT 파일 파싱 요약 출력")
    p_tp.add_argument("--input", required=True, type=Path, help="VTT 파일 경로")
    p_tp.set_defaults(func=_cmd_transcript_parse)

    p_tr = sub.add_parser("transcript-render", help="WebVTT → Markdown 변환")
    p_tr.add_argument("--input", required=True, type=Path, help="VTT 파일 경로")
    p_tr.add_argument("--output", type=Path, default=None, help="출력 .md 경로 (기본: 입력과 동일 stem)")
    p_tr.add_argument("--title", type=str, default=None, help="문서 제목")
    p_tr.add_argument("--session-start", type=str, default=None, help="회의 시작 표기 (ISO 등)")
    p_tr.add_argument("--no-merge", action="store_true", help="연속 같은 화자 병합 비활성화")
    p_tr.set_defaults(func=_cmd_transcript_render)

    p_m = sub.add_parser("merge", help="캡처 세션 + VTT 전사본을 병합한 회의록 생성")
    p_m.add_argument("--captures-dir", type=Path, default=None, help="capture 세션 폴더 (metadata.json 포함)")
    p_m.add_argument("--transcript", type=Path, default=None, help="VTT 전사 파일 경로")
    p_m.add_argument("--meeting-start", type=str, default=None, help="회의 시작 ISO 시각 (예: 2026-04-21T10:00:00). 미지정 시 capture 세션 시작 사용")
    p_m.add_argument("--output", type=Path, default=None, help="출력 .md 경로")
    p_m.add_argument("--title", type=str, default=None, help="문서 제목")
    p_m.add_argument("--skip-blank", action="store_true", help="phash가 전부 0인 단색 프레임 제외")
    p_m.set_defaults(func=_cmd_merge)

    p_uia = sub.add_parser("uia-probe", help="Teams 창 UIA 트리 탐색(검증용)")
    p_uia.add_argument("mode", choices=["dump", "watch"], help="dump=한번 덤프, watch=변화 감시")
    p_uia.add_argument("--max-depth", type=int, default=20)
    p_uia.add_argument("--interval", type=float, default=3.0, help="watch 모드 간격(초)")
    p_uia.add_argument("--iterations", type=int, default=120, help="watch 모드 최대 반복")
    p_uia.set_defaults(func=_cmd_uia_probe)

    p_ad = sub.add_parser("audio-devices", help="오디오 장치 목록")
    p_ad.set_defaults(func=_cmd_audio_devices)

    p_at = sub.add_parser("audio-test", help="오디오 녹음 짧은 스모크 테스트")
    p_at.add_argument("--duration", type=float, default=2.0, help="녹음 길이(초)")
    p_at.add_argument("--output", type=Path, default=Path("_audio_test.ogg"))
    p_at.add_argument("--samplerate", type=int, default=48000)
    p_at.add_argument("--channels", type=int, default=2)
    p_at.set_defaults(func=_cmd_audio_test)

    p_co = sub.add_parser("caption-ocr", help="Teams 하단 자막 영역 OCR 실시간 수집(JSONL)")
    p_co.add_argument("--output", type=Path, default=Path("_caption.jsonl"))
    p_co.add_argument("--duration", type=float, default=0.0, help="0이면 Ctrl+C까지 계속")
    p_co.add_argument("--interval", type=float, default=1.5)
    p_co.add_argument("--region-top", type=float, default=0.70)
    p_co.add_argument("--region-bottom", type=float, default=0.96)
    p_co.add_argument("--region-left", type=float, default=0.10)
    p_co.add_argument("--region-right", type=float, default=0.90)
    p_co.add_argument("--min-conf", type=float, default=0.30)
    p_co.set_defaults(func=_cmd_caption_ocr)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
