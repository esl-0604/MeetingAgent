# Bake assets/readme-template.html + assets/screenshots/*.png into a
# self-contained README.html at ver2/MeetingAgent-v0.2.0/README.html.

$root = Resolve-Path "$PSScriptRoot\.."
$templatePath = Join-Path $root "assets\readme-template.html"
$shotsDir = Join-Path $root "assets\screenshots"
$outPath = Resolve-Path (Join-Path $root "..\ver2\MeetingAgent-v0.2.0")
$outFile = Join-Path $outPath "README.html"

function B64Url([string]$path) {
    if (-not (Test-Path $path)) { return $null }
    $bytes = [System.IO.File]::ReadAllBytes($path)
    $b64 = [Convert]::ToBase64String($bytes)
    return "data:image/png;base64,$b64"
}

$html = [System.IO.File]::ReadAllText($templatePath, [System.Text.Encoding]::UTF8)

$icon = B64Url (Join-Path $root "assets\icon-128.png")

$shots = @{
    "{{ICON_LARGE}}" = $icon
    "{{SHOT_WELCOME}}" = (B64Url (Join-Path $shotsDir "20-welcome-real.png"))
    "{{SHOT_FOLDER_PICKER}}" = (B64Url (Join-Path $shotsDir "21-folder-picker-real.png"))
    "{{SHOT_TRAY_AREA}}" = (B64Url (Join-Path $shotsDir "22-tray-area.png"))
    "{{SHOT_TRAY_MENU}}" = (B64Url (Join-Path $shotsDir "23-tray-menu.png"))
    "{{SHOT_STARTUP}}" = (B64Url (Join-Path $shotsDir "24-popup-startup.png"))
    "{{SHOT_MEETING_PROMPT}}" = (B64Url (Join-Path $shotsDir "25-popup-meeting-prompt.png"))
    "{{SHOT_EVENT_CAPTION}}" = (B64Url (Join-Path $shotsDir "26-popup-caption.png"))
    "{{SHOT_EVENT_SHARE}}" = (B64Url (Join-Path $shotsDir "27-popup-share-start.png"))
    "{{SHOT_SOURCE_SWITCH}}" = (B64Url (Join-Path $shotsDir "28-popup-source-switch.png"))
    "{{SHOT_MEETING_ENDED}}" = (B64Url (Join-Path $shotsDir "29-popup-meeting-ended.png"))
    "{{SHOT_POST_FINALIZE}}" = (B64Url (Join-Path $shotsDir "30-popup-post-finalize.png"))
}

foreach ($key in $shots.Keys) {
    $val = $shots[$key]
    if (-not $val) {
        Write-Warning "missing image for $key — leaving placeholder"
        continue
    }
    $html = $html -replace [regex]::Escape($key), $val
}

# Write with UTF-8 BOM so non-Edge browsers treat it correctly when opened
# from a USB drive without an HTTP Content-Type.
$bom = [System.Text.Encoding]::UTF8.GetPreamble()
$bytes = [System.Text.Encoding]::UTF8.GetBytes($html)
[System.IO.File]::WriteAllBytes($outFile, $bom + $bytes)

$size = (Get-Item $outFile).Length
Write-Host "wrote $outFile ($([math]::Round($size/1KB, 1)) KB)"
