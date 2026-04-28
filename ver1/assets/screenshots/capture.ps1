# Best-effort screenshot capture for the README.html user guide.
# Launches meeting-agent.exe, captures stages of the UI, then exits.
#
# Usage: pwsh -File capture.ps1
# Output: capture-*.png in this folder.

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$exe = "c:\LEE Dev\Agent\MeetingAgent\ver2\MeetingAgent-v0.2.0\meeting-agent.exe"
$outDir = $PSScriptRoot

# Make sure no instance is running.
Get-Process meeting-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

# Reset state so welcome shows fresh.
Remove-Item "$env:APPDATA\MeetingAgent\state.json" -ErrorAction SilentlyContinue

function Take-Screenshot([string]$name) {
    $bounds = [Windows.Forms.SystemInformation]::VirtualScreen
    $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $g = [System.Drawing.Graphics]::FromImage($bitmap)
    $g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    $path = Join-Path $outDir $name
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bitmap.Dispose()
    Write-Host "saved $path"
}

# --- launch ---
Start-Process -FilePath $exe
Start-Sleep -Seconds 3   # wait for WebView2 init + welcome window

# 01: welcome window (fresh)
Take-Screenshot "01-welcome.png"

# Acknowledge welcome with Enter
[System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
Start-Sleep -Seconds 2

# 02: folder picker
Take-Screenshot "02-folder-picker.png"

# Cancel folder picker (Esc) so the existing default sticks.
[System.Windows.Forms.SendKeys]::SendWait("{ESC}")
Start-Sleep -Seconds 2

# 03: full screen with tray icon (the small startup popup may also be visible)
Take-Screenshot "03-tray-and-popup.png"

# Wait for the startup popup to slide out.
Start-Sleep -Seconds 6
Take-Screenshot "04-tray-only.png"

# Quit gracefully via process stop (no public CLI for tray Quit).
Get-Process meeting-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host "done"
