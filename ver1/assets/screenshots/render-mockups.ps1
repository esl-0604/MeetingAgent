# Render screenshots of the welcome window and popup mockups using
# Edge headless. Avoids having to launch the live app.

# Don't set $ErrorActionPreference = "Stop" — Edge writes its progress lines
# to stderr which PowerShell wraps as ErrorRecords on Windows PowerShell 5.1
# and would abort the script. We just check $LASTEXITCODE / file presence
# after each Render call.

$root = Resolve-Path "$PSScriptRoot\..\.."
$gui = Join-Path $root "src\gui"
$out = $PSScriptRoot
$temp = Join-Path $out ".tmp"
New-Item -Path $temp -ItemType Directory -Force | Out-Null

$edge = "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
if (-not (Test-Path $edge)) {
    Write-Error "msedge.exe not found at $edge"
    exit 1
}

# 128x128 mascot, base64.
$iconBytes = [System.IO.File]::ReadAllBytes((Join-Path $root "assets\icon-128.png"))
$iconB64 = [Convert]::ToBase64String($iconBytes)
$iconUri = "data:image/png;base64,$iconB64"

# 32x32 mascot for popup head.
$smallBytes = [System.IO.File]::ReadAllBytes((Join-Path $root "assets\icon-32.png"))
$smallUri = "data:image/png;base64," + [Convert]::ToBase64String($smallBytes)

$callCount = 0
function Render([string]$html, [string]$name, [int]$w, [int]$h) {
    $script:callCount++
    Write-Host "[render call $($script:callCount)] $name w=$w h=$h html-len=$($html.Length)"
    $tmpPath = Join-Path $temp $name
    $tmpPath = $tmpPath -replace "\.png$", ".html"
    [System.IO.File]::WriteAllText($tmpPath, $html, [System.Text.Encoding]::UTF8)
    $url = "file:///" + ($tmpPath -replace "\\", "/")
    $outPath = Join-Path $out $name
    Remove-Item $outPath -ErrorAction SilentlyContinue
    # Per-call user-data-dir so concurrent Edge processes don't fight over
    # the same profile lock — overlapping headless instances were what made
    # the second-third Render calls produce 0-byte / missing PNGs.
    $userData = Join-Path $temp "ud-$callCount"
    & $edge --headless=new --disable-gpu --no-sandbox `
        "--user-data-dir=$userData" `
        "--window-size=$w,$h" `
        "--virtual-time-budget=2000" `
        "--screenshot=$outPath" `
        "$url" | Out-Null
    Start-Sleep -Milliseconds 500
    if ((Test-Path $outPath) -and ((Get-Item $outPath).Length -gt 1000)) {
        Write-Host "rendered $outPath ($((Get-Item $outPath).Length) bytes)"
    } else {
        Write-Warning "failed/blank: $name"
    }
}

# 1) Welcome window mockup.
$welcome = [System.IO.File]::ReadAllText((Join-Path $gui "welcome.html"), [System.Text.Encoding]::UTF8)
$welcome = $welcome -replace [regex]::Escape("{{ICON_DATA_URI}}"), $iconUri
Render $welcome "10-welcome.png" 540 440

# 2) Popup mockups — fill the template with sample content.
$popup = [System.IO.File]::ReadAllText((Join-Path $gui "popup.html"), [System.Text.Encoding]::UTF8)
$popup = $popup -replace [regex]::Escape("{{ICON_DATA_URI}}"), $smallUri

function PopupSample([string]$head, [string]$title, [string]$body, [string]$actions, [string]$bodyAction) {
    $h = $popup -replace [regex]::Escape("{{HEAD}}"), $head
    $h = $h -replace [regex]::Escape("{{TITLE}}"), $title
    $h = $h -replace [regex]::Escape("{{BODY}}"), $body
    $h = $h -replace [regex]::Escape("{{ACTIONS}}"), $actions
    $h = $h -replace [regex]::Escape("{{BODY_ACTION}}"), $bodyAction
    # Disable the slide-in animation so the screenshot catches a settled
    # frame, not a mid-flight one.
    $h = $h -replace "transform: translateX\(120%\);", "transform: translateX(0);"
    $h = $h -replace "animation: slideIn[^;]+;", "animation: none;"
    # Lift overflow: hidden on .body so long text wraps instead of clipping
    # under the action buttons in the screenshot.
    $h = $h -replace "(\.body\s*\{[^}]*?)overflow:\s*hidden;", "`$1overflow: visible;"
    $h
}

# 2a) Meeting detected prompt.
$btns11 = '<button class="secondary">무시</button><button class="primary">녹화</button>'
$prompt = PopupSample "미팅 감지됨" "녹화하시겠습니까?" "Teams 미팅이 감지되었습니다: 주간 동기화 회의" $btns11 "_body"
Render $prompt "11-popup-meeting-prompt.png" 500 260

# 2b) Meeting ended confirmation.
$btns12 = '<button class="primary">녹화 중지 + 폴더 열기</button>'
$ended = PopupSample "미팅 종료 감지" "미팅이 종료된 것 같습니다." "10초 안에 응답하지 않으면 자동으로 마무리합니다." $btns12 "stop_open"
Render $ended "12-popup-meeting-ended.png" 500 260

# 2c) Event notification (caption detected).
$event = PopupSample "이벤트" "자막 감지됨" "Teams 라이브 캡션을 transcript.txt에 기록합니다." "" "_body"
Render $event "13-popup-event-caption.png" 500 180

# 2d) Event notification (share started).
$share = PopupSample "이벤트" "화면 공유 시작" "내 화면을 공유 중입니다 — 캡처 대상이 공유 창으로 자동 전환됩니다." "" "_body"
Render $share "14-popup-event-share.png" 500 180

# 2e) Post-finalize.
$btns15 = '<button class="primary">폴더 열기</button>'
$bodyText15 = "클릭해서 폴더 열기:<br>C:\Users\you\Documents\MeetingAgent\sessions\2026-04-28-153045-MeetingSession"
$post = PopupSample "녹화 완료" "세션이 저장되었습니다." $bodyText15 $btns15 "open"
Render $post "15-popup-post-finalize.png" 500 260

Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
Write-Host "done"
