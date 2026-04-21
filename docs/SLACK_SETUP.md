# Slack → Meeting Agent 연동 배포 가이드

Slack 메시지의 링크/버튼 클릭으로 사용자 PC의 Meeting Agent를 실행시키는 방법.

## 전체 흐름

```
Slack 메시지 링크 클릭
    ↓ (브라우저 이동)
https://<org>.github.io/meeting-agent/launch.html?action=start
    ↓ (meetingagent://start 프로토콜 시도)
[설치됨]  → Windows "Meeting Agent에서 열기?" → 앱 실행 (+ 원격 명령 전달)
[미설치]  → 2.5초 후 "Installer 다운로드" UI 표시 → install.bat 1회 실행 → 이후 원클릭
```

## 배포 체크리스트

### 1. GitHub Repository 준비

- Repository 생성 (예: `esl-0604/MeetingAgent`)
- 다음 구조로 정리
```
repo/
├── docs/
│   ├── launch.html              ← GitHub Pages로 서빙
│   ├── slack_message_template.json
│   └── SLACK_SETUP.md (이 파일)
└── releases/
    └── MeetingAgent_installer.zip   ← Releases 에 업로드
```

### 2. `launch.html` 의 `DOWNLOAD_URL` 수정

`docs/launch.html` 내부 JS 상단 상수를 본인 Repository 경로로 바꿉니다.

```js
const DOWNLOAD_URL = 'https://github.com/esl-0604/MeetingAgent/releases/latest/download/MeetingAgent_installer.zip';
```

### 3. GitHub Pages 활성화

Repository Settings → Pages → Source: `Deploy from a branch` → Branch: `main` / `/docs` → Save.

몇 분 뒤 `https://esl-0604.github.io/MeetingAgent/launch.html` 로 접근 가능.

### 4. Installer zip 을 GitHub Releases 에 업로드

Repository → Releases → Draft a new release → Tag `v0.1.0` → Title "Initial release" → 파일 첨부 `MeetingAgent_installer.zip` (이 프로젝트의 `dist/MeetingAgent_portable.zip` 을 이름만 변경해서 올리면 됨) → Publish.

`https://github.com/esl-0604/MeetingAgent/releases/latest/download/MeetingAgent_installer.zip` 이 최신 버전을 가리키는 고정 URL로 사용됩니다.

### 5. Slack 메시지 생성

#### 간단한 방법: Slack Workflow Builder

1. Slack 좌측 상단 워크스페이스명 → **도구 → Workflow Builder**
2. 새 워크플로우 → 원하는 트리거 (예: 바로가기, 지정 채널 메시지)
3. 단계 추가: **"메시지 보내기"**
4. 메시지 편집 → 우측 상단 **"메시지 블록 편집"** 을 JSON 모드로 전환
5. `docs/slack_message_template.json` 의 `blocks` 배열 내용을 복사해 붙여넣기
6. URL의 `esl-0604` 를 GitHub 조직명으로 수정
7. 게시

#### 더 간단: 그냥 메시지에 링크 붙이기

```
📹 Meeting Agent
<https://esl-0604.github.io/MeetingAgent/launch.html?action=start|▶️ 녹화 준비>
<https://esl-0604.github.io/MeetingAgent/launch.html?action=stop|⏹️ 녹화 종료>
<https://esl-0604.github.io/MeetingAgent/launch.html?action=status|ℹ️ 상태>
```

## 지원하는 Action

| URL | 동작 |
|---|---|
| `meetingagent://show` (기본값) | 앱 창을 전면으로 |
| `meetingagent://start` | 앱을 활성 상태로 유지 (Teams 감지 대기) + 전면 표시 |
| `meetingagent://stop` | 활성 세션 강제 종료 → 저장 프롬프트 |
| `meetingagent://status` | 현재 상태를 앱 로그에 출력 + 전면 표시 |

## 보안 관련

- URL 프로토콜은 **HKCU(현재 사용자)** 에만 등록되어 관리자 권한 없이 설치 가능
- 브라우저는 **"Meeting Agent 를 열까요?"** 로 한 번은 사용자 확인 요구 (표준 보안 동작)
- **"항상 허용"** 체크 시 이후 클릭은 자동 실행
- 링크를 누른 사용자의 PC에서만 실행됨. 다른 사용자의 PC에는 영향 없음

## 트러블슈팅

- **링크 눌러도 아무 반응 없음**: 브라우저에 따라 프로토콜 차단 설정이 있을 수 있음. 설정에서 `meetingagent://` 허용 필요.
- **설치했는데도 미설치로 인식**: 브라우저 재시작 후 재시도. 레지스트리 등록은 완료됐지만 브라우저가 캐시 중.
- **"이 앱은 Windows의 보호를 받고 있습니다" (SmartScreen)**: installer를 받은 직후 한 번만 나오는 경고. "추가 정보 → 실행".
