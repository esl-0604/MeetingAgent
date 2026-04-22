# Meeting Agent — ver1 (Rust, L2 capture)

Background process that runs alongside Microsoft Teams (New Teams, Windows 11)
and, while a meeting is active, captures three streams in parallel without
joining as a bot and without using Teams' native recording:

| Stream | Source | Mechanism |
|---|---|---|
| Live captions (with speaker) | Teams WebView2 DOM | UI Automation tree walk |
| Audio | `ms-teams.exe` PCM stream | WASAPI **process loopback** + microphone |
| Shared-screen keyframes | Teams window composition | `Windows.Graphics.Capture` + perceptual-hash dedup |

All three are stitched onto a single QPC monotonic timeline and rendered as
`events.jsonl` + `summary.md` per session.

## Architecture

```
                    ┌─────────────────────────────┐
                    │ state/  meeting detector    │
                    │ (UIA "Leave" button probe)  │
                    └────────────┬────────────────┘
                                 │ MeetingEvent::Started{hwnd, pid}
       ┌─────────────────────────┼─────────────────────────┐
       ▼                         ▼                         ▼
┌──────────────┐        ┌────────────────┐       ┌──────────────────┐
│ uia/captions │        │ audio/loopback │       │ screen/wgc       │
│ poll @ 400ms │        │ + audio/mic    │       │ + screen/phash   │
│ → Caption ev │        │ → AudioSegment │       │ → Slide ev       │
└──────┬───────┘        └────────┬───────┘       └────────┬─────────┘
       │                         │                        │
       └──────────── timeline ───┴────────────────────────┘
                          │
                  events.jsonl  →  summary.md (on session end)
```

## Module map

```
src/
├── main.rs                 CLI + tokio runtime + session orchestration
├── clock.rs                QPC monotonic timestamps
├── config.rs               JSON config (loaded from %APPDATA%/MeetingAgent/config.json)
├── output.rs               session-{timestamp}/ directory layout
├── state/
│   ├── mod.rs              Teams window enumeration + meeting marker probe
│   └── presenter.rs        "X is presenting" detector
├── uia/
│   ├── mod.rs              COM init, IUIAutomation helpers, tree walker
│   └── captions.rs         caption container discovery + delta extraction
├── audio/
│   ├── mod.rs              audio worker entry
│   ├── loopback.rs         WASAPI process loopback (+ default fallback)
│   ├── mic.rs              microphone capture
│   └── wav.rs              float32 WAV writer
├── screen/
│   ├── mod.rs              capture loop + share gating
│   ├── wgc.rs              Windows.Graphics.Capture wrapper (D3D11 interop)
│   └── phash.rs            8x8 average hash + PNG save
└── timeline/
    ├── mod.rs              JSONL writer task
    ├── event.rs            tagged event enum
    └── render.rs           summary.md renderer
```

## Why each technical choice

- **Rust** — `windows-rs` provides first-class bindings to every API we need
  (WASAPI, WGC, UIA, COM/WinRT interop). Native single binary, no GIL, easy to
  ship.
- **WASAPI process loopback** (Windows 10 2004+) — captures only audio rendered
  by `ms-teams.exe` and its child processes, so YouTube playing in another tab
  won't contaminate the recording. Falls back to default-device loopback if
  per-process activation fails.
- **`Windows.Graphics.Capture`** (Win 10 1803+) — captures the Teams window's
  composited frames directly from the GPU, works even when Teams is occluded
  by other windows. Cursor is suppressed and (on Win 11 22H2+) the capture
  border is hidden, so saved slides look clean.
- **UI Automation for captions** — New Teams is a WebView2 host, so its DOM is
  reflected verbatim into the UIA tree. We are reading the same string Teams'
  web app rendered. This is the deepest data layer available without process
  injection or TLS MITM (both of which violate Teams' EULA and would be broken
  by the next auto-update).
- **QPC clock** — wall-clock can drift mid-meeting (NTP, DST). All three
  pipelines stamp events with `QueryPerformanceCounter`-derived ms so the
  timeline is internally consistent.

## Build

### Prerequisites

1. **Rust** (1.75+):
   ```powershell
   winget install --id Rustlang.Rustup -e
   ```

2. **A linker** — pick one:

   **Option A — MSVC (recommended for production):** install Visual Studio
   Build Tools with the *Desktop development with C++* workload.
   ```powershell
   winget install Microsoft.VisualStudio.2022.BuildTools `
     --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --quiet --wait --norestart"
   ```
   Then ensure MSVC toolchain is selected:
   ```powershell
   rustup default stable-x86_64-pc-windows-msvc
   ```

   **Option B — GNU/MinGW (lighter, ~300 MB):** install MSYS2 and the
   mingw-w64 toolchain.
   ```powershell
   winget install MSYS2.MSYS2
   # in MSYS2 MinGW64 shell:
   pacman -S --noconfirm mingw-w64-x86_64-toolchain
   ```
   Then add `C:\msys64\mingw64\bin` to your PATH and switch toolchain:
   ```powershell
   rustup default stable-x86_64-pc-windows-gnu
   ```
   Edit `rust-toolchain.toml` and change `channel = "stable"` to
   `"stable-x86_64-pc-windows-gnu"`.

### Verify and build

```powershell
# Type-check only (fast, no link required)
cargo check

# Production build
cargo build --release
```

The binary lands at `target/release/meeting-agent.exe` (~5 MB stripped).

## Run

```powershell
# Auto-detect Teams meetings (default)
.\target\release\meeting-agent.exe

# Force-start a session immediately for testing without Teams
.\target\release\meeting-agent.exe --force-start --output .\sessions

# Verbose logs for debugging caption discovery
.\target\release\meeting-agent.exe --log meeting_agent=debug,info
```

Press **Ctrl-C** to shut down. The current session is finalised cleanly
(WAV files closed, `summary.md` rendered) before exit.

## Output layout

```
sessions/
└── session-2026-04-22_104503/
    ├── audio/
    │   ├── teams_loopback.wav      ms-teams.exe-only mix (32-bit float, 48 kHz)
    │   └── microphone.wav          your mic (system mix format)
    ├── slides/
    │   ├── slide_000001_Smith_xxxx.png
    │   ├── slide_000002_Smith_xxxx.png
    │   └── ...
    ├── transcript/                  reserved for offline STT pass
    ├── events.jsonl                 one JSON event per line, append-only
    └── summary.md                   chronological human-readable digest
```

## Configuration

Optional JSON at `%APPDATA%/MeetingAgent/config.json`. Defaults are sensible:

```json
{
  "output_root": "C:/Users/you/Documents/MeetingAgent/sessions",
  "audio": {
    "capture_teams_loopback": true,
    "capture_microphone": true,
    "teams_process_name": "ms-teams.exe",
    "fallback_to_default_loopback": true
  },
  "screen": {
    "enabled": true,
    "min_frame_interval_ms": 500,
    "phash_threshold": 8,
    "only_during_share": true
  },
  "caption": {
    "enabled": true,
    "poll_interval_ms": 400
  },
  "detect": {
    "poll_interval_ms": 1500,
    "use_log_tail": false
  }
}
```

## Runtime requirements

- **Windows 11** (latest) — process loopback API + WGC border-suppression need
  recent builds.
- **New Teams** (WebView2 host) — caption extraction relies on the WebView2
  accessibility tree; classic Teams (Electron/AngularJS) had a different
  layout and is not supported.
- The Teams **Live Captions panel must be open** while you want captions
  captured. Click `…` → `Language and speech` → `Turn on live captions` once.
  This agent does NOT toggle it for you (Teams owns that state).
- For the slide capture to be useful, keep the Teams window large enough that
  the shared screen is rendered at decent resolution.

## Known friction / things to verify on first run

The hardest pieces to get right without iterating against live Teams are
listed here. Expect to tune.

1. **UIA caption container discovery.** New Teams ships UI updates roughly
   monthly and may rename internal elements. The discovery uses three
   heuristics (name contains "Captions"/"캡션", AutomationId contains
   "captions" patterns, list-type control). If captures show *no* caption
   events but Teams clearly displays them, run with `--log
   meeting_agent::uia=debug` and look for the "caption container located"
   message; if it never appears, dump the tree to find the right selector.

2. **Process loopback PROPVARIANT.** Activation params are passed to
   `ActivateAudioInterfaceAsync` via a hand-rolled C-layout struct cast to
   `*const PROPVARIANT` (because the `windows-rs` PROPVARIANT type owns its
   memory and would try to free our stack pointer on Drop). If a future
   `windows` crate version changes the wire layout of `PROPVARIANT`, update
   the `PropvariantBlob` repr in `audio/loopback.rs`.

3. **Caption speaker parsing.** We split on the first `:` to separate
   `Speaker: text`. Some Teams locales use other delimiters; if speakers come
   out blank, adjust `split_speaker_text` in `uia/captions.rs`.

4. **Presenter detection** uses substring matches on names like
   `" is presenting"` / `"님이 발표 중"`. If Teams renames the marker, slides
   will be saved with `unknown` presenter (still functional).

5. **Process loopback** sometimes returns silent buffers for the first
   ~100 ms while the audio engine warms up. Normal — first slide of audio is
   discardable.

6. **First-time Defender prompt.** Process loopback registers a transient
   audio endpoint; some Defender Real-Time Protection profiles flag the
   `mmdevapi.dll` activation. Whitelist the binary if it gets quarantined.

## Differences from ver0

- ver0 was a Python tkinter app doing screen-region OCR for captions and
  full-screen scraping for slides. It worked but was fragile (OCR misses,
  occlusion breaks slide capture, GIL caps real-time perf).
- ver1 reads captions from the actual UIA/DOM (no OCR), captures audio
  per-process (no system-mix contamination), and pulls slide frames via
  WGC (works under occlusion). Single Rust binary, no Python runtime.

## Legal / ethical

- You are a meeting participant on your own PC — Korean law generally treats
  participant-side recording as legal. **Check your company policy** before
  use; many enterprises forbid all unofficial recording regardless.
- Teams' "Recording" banner does NOT fire from this tool. Your participants
  will not be notified. Disclose proactively if your organisation expects it.
