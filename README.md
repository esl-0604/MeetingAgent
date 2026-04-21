# Meeting Agent

Teams 회의를 자동으로 아카이빙하는 Windows 스탠드얼론 앱.

회의 동안 **① 발표 자료(화면) ② 오디오 ③ 자막(OCR)** 을 자동 수집하고, 미팅 종료 시 지정 폴더에 정리 저장합니다. Slack 메시지 링크로 원격 제어도 가능합니다.

![platform](https://img.shields.io/badge/platform-Windows-blue) ![python](https://img.shields.io/badge/python-3.12-blue) ![license](https://img.shields.io/badge/license-MIT-green)

---

## 핵심 기능

| 기능 | 설명 |
|---|---|
| **Teams 창 자동 감지** | Win32 API로 "모임/통화/회의/Meeting/Call" 제목의 Teams 창을 폴링 |
| **슬라이드 캡처** | 1초 간격 스크린샷 → pHash 해밍 거리로 변화 감지 → WebP 저장 (중복 제거) |
| **오디오 녹음** | 시스템 루프백 + 마이크 믹싱 → OGG Vorbis (48kHz, 스테레오) |
| **자막 OCR** | 사용자 지정 영역을 EasyOCR로 실시간 스크래핑 (한국어+영어, 1.5초 간격) |
| **자막 영역 GUI** | 드래그로 절대 좌표 선택, 실시간 미리보기, JSON에 영속화 |
| **자동 시작/종료 감지** | 미팅 창 등장·소실 기반, 종료 후 저장 폴더 프롬프트 |
| **원격 트리거** | `meetingagent://start|stop|status|show` URL 프로토콜 (Slack 링크) |
| **완전 오프라인** | EasyOCR 모델을 exe 번들에 포함, 인터넷 불필요 |

---

## 저장 결과물

사용자가 지정한 폴더 아래에 세션별로 생성:

```
<저장 루트>/
└── meeting_2026-04-21_143012/
    ├── audio/
    │   └── audio.ogg               ← 시스템 + 마이크 믹스
    ├── transcript/
    │   └── captions.jsonl          ← OCR 자막 로그 (timestamp, text, conf)
    └── slides/
        ├── slide_0001_143012_520.webp
        ├── slide_0002_143045_311.webp
        └── metadata.json           ← 각 프레임의 타임스탬프·pHash·해밍거리
```

---

## 설치 (최종 사용자)

1. [Releases 페이지](https://github.com/esl-0604/MeetingAgent/releases/latest) 에서 **`MeetingAgent_installer.zip`** 다운로드
2. 원하는 경로에 **압축 해제**
3. 풀린 폴더 안의 **`install.bat`** 더블클릭
   - `%LOCALAPPDATA%\MeetingAgent\` 에 설치
   - `meetingagent://` URL 프로토콜 등록 (HKCU, 관리자 권한 불필요)
   - 바탕화면 + 시작 메뉴 바로가기 생성
4. Windows SmartScreen 경고 시 **"추가 정보 → 실행"** (최초 1회)

제거는 `%LOCALAPPDATA%\MeetingAgent\uninstall.bat` 실행.

---

## 사용법

1. **앱 실행** (바탕화면 아이콘 or 시작 메뉴)
2. **최초 1회 자막 영역 지정** — 상단 `[자막 영역 선택]` 버튼 → 화면 전체가 반투명 오버레이로 덮임 → Teams 자막이 뜨는 영역을 마우스 드래그 → 좌표가 자동 저장됨
3. **`[영역 미리보기]`** 로 실시간 캡처 확인 가능 (0.5초 간격 업데이트)
4. Teams 미팅 참가 + **자막(Live Captions) 켜기**
5. 앱이 자동 감지 → 3가지 워커 동시 시작
6. 미팅 나가면 자동 종료 감지 → **`[저장 폴더 선택]`** 버튼 등장 → 저장

---

## Slack 연동 (원격 제어)

Slack 메시지에 링크 넣고 클릭 → 사용자 PC의 앱이 실행됨.

```
📹 Meeting Agent
<https://esl-0604.github.io/MeetingAgent/launch.html?action=start|▶️ 녹화 시작>
<https://esl-0604.github.io/MeetingAgent/launch.html?action=stop|⏹️ 녹화 종료>
<https://esl-0604.github.io/MeetingAgent/launch.html?action=status|ℹ️ 상태>
```

지원하는 action:
| URL | 동작 |
|---|---|
| `meetingagent://show` | 창 전면으로 |
| `meetingagent://start` | 활성화 + Teams 감지 대기 |
| `meetingagent://stop` | 세션 강제 종료 + 저장 프롬프트 |
| `meetingagent://status` | 현재 상태 로그 |

배포 단계별 가이드는 [docs/SLACK_SETUP.md](docs/SLACK_SETUP.md) 참조.

첫 클릭 시 앱이 미설치면 랜딩 페이지가 자동으로 "Installer 다운로드"를 안내합니다. 설치 후부터는 원클릭으로 앱이 열립니다.

---

## 아키텍처

```
┌───────────────────────────────────────────────────────┐
│  MeetingAgent.exe (Tkinter GUI, 항상 위)                 │
│  ├─ MeetingDetector (Teams 창 폴링, 2s)                  │
│  ├─ SessionRunner (미팅 감지 시 아래 3개 스레드 시작)      │
│  │   ├─ ChangeCapture   (mss → pHash → WebP)            │
│  │   ├─ MixedAudioRecorder (soundcard loopback + mic → ogg)│
│  │   └─ CaptionOcrScraper  (mss 지정영역 → EasyOCR → jsonl) │
│  ├─ CommandWatcher (파일 IPC 폴링, 0.5s)                  │
│  ├─ RegionPicker / RegionPreviewWindow (자막 영역 UI)     │
│  └─ Singleton Mutex (Global\MeetingAgent_Singleton_v1)   │
└───────────────────────────────────────────────────────┘
         ▲                            │
         │ meetingagent:// (URL args) │ 파일 IPC
         │                            ▼
    [Windows 레지스트리]        %LOCALAPPDATA%\MeetingAgent\commands\*.cmd
    HKCU\Software\Classes\meetingagent
         ▲
         │ 프로토콜 트리거
         │
    [브라우저] ← [Slack 링크]
```

---

## 개발자 가이드

### 요구사항

- Windows 10/11
- Python 3.12 (3.11도 가능할 것으로 추정, 미검증)

### 소스로부터 실행

```bash
git clone https://github.com/esl-0604/MeetingAgent.git
cd MeetingAgent
python -m venv .venv
.venv\Scripts\pip install -r requirements.txt
.venv\Scripts\python meeting_agent_app.py
```

### .exe 빌드

EasyOCR 모델을 bundle에 포함하려면, 빌드 전에 한번 OCR을 돌려서 `~/.EasyOCR/model/` 에 모델이 캐시되어 있어야 합니다.

```bash
# 캐시 준비 (최초 1회, 모델 다운로드)
.venv\Scripts\python main.py caption-ocr --duration 5

# 빌드
build_exe.bat
```

빌드 결과: `dist/MeetingAgent/` (포터블 폴더, ~740MB) + `dist/MeetingAgent_portable.zip` (~335MB).

### 모듈 구성

| 파일 | 역할 |
|---|---|
| `meeting_agent_app.py` | GUI 앱 엔트리 + 라이프사이클 오케스트레이터 |
| `teams_window.py` | Win32 Teams 창 감지 |
| `capture.py` | 슬라이드 변화 감지·저장 |
| `audio_recorder.py` | 시스템+마이크 믹스 녹음 |
| `caption_ocr.py` | 자막 영역 OCR 스크래퍼 |
| `transcript.py` | WebVTT 파서 + 마크다운 렌더러 (Graph API 연동용) |
| `merge.py` | 슬라이드+자막 타임라인 병합 |
| `main.py` | CLI 엔트리 (개발자용, 개별 모듈 테스트) |
| `teams_uia_probe.py` | Teams UI Automation 트리 탐색기 (디버그) |
| `docs/launch.html` | GitHub Pages 랜딩 페이지 (Slack 프로토콜 런처) |
| `build_exe.bat` | PyInstaller 빌드 스크립트 |

---

## CLI 유틸 (개발자용)

`main.py` 에 여러 서브커맨드:

```bash
# Teams 창 감지 디버그
python main.py windows

# 슬라이드만 캡처
python main.py start --output captures --interval 1.0 --threshold 8

# 자막만 OCR
python main.py caption-ocr --duration 60 --output captions.jsonl

# 오디오 장치 목록
python main.py audio-devices

# 2초 오디오 녹음 테스트
python main.py audio-test --duration 2 --output test.ogg

# VTT → Markdown (Teams에서 수동 다운로드한 전사본 병합용)
python main.py transcript-render --input meeting.vtt --output meeting.md

# 캡처 + VTT 병합
python main.py merge --captures-dir captures/2026-04-21_143012 \
    --transcript meeting.vtt --skip-blank
```

---

## 알려진 제약

- **Teams 자막 UI 영역은 Chromium 내부에 렌더링** — Windows UI Automation으로는 추출 불가. 그래서 OCR 방식을 채택.
- **자막 영역 좌표는 PC별로 재지정 필요** — 다른 해상도나 Teams 위치에 맞춰야 함.
- **Graph API 전사본 자동 수집** — `OnlineMeetingTranscript.Read.All` 권한이 관리자 동의 필수라 delegated 권한만으로는 제한적. 당분간 OCR 경로 사용.
- **회의 중 Teams 창 이동** — 슬라이드 캡처는 창을 따라가지만, 자막 영역(절대 좌표)은 어긋나서 재지정 필요.

---

## 배포 크기 안내

| 항목 | 크기 | 비고 |
|---|---|---|
| Installer zip | 335 MB | PyTorch CPU + EasyOCR 모델 포함 |
| 설치 후 (`%LOCALAPPDATA%\MeetingAgent\`) | 740 MB | 압축 해제 상태 |

EasyOCR의 PyTorch 의존성 때문에 용량이 큽니다. 경량화는 Tesseract로 OCR 엔진 교체 시 가능하지만 한국어 정확도 하락 우려.

---

## 라이선스

MIT
