=== Meeting Agent ===

[Install]
1. Double-click install.bat
   - Copies files to %LOCALAPPDATA%\MeetingAgent\
   - Registers meetingagent:// URL protocol (per-user, no admin)
   - Creates Desktop + Start Menu shortcuts

[Run]
- Double-click the Desktop 'Meeting Agent' shortcut, OR
- Double-click Run.bat in this folder, OR
- Click a Slack meetingagent:// link

[How it works]
1. On launch, a small floating window appears and waits for a Teams meeting.
2. First time only: click [Select caption region] and drag over where Teams
   captions appear. [Preview region] shows a live crop to verify.
3. Join a Teams meeting with Live Captions ON. The app auto-detects and
   starts 3 workers:
     - Slide change capture (pHash WebP)
     - Audio recording (system loopback + mic mix, OGG)
     - Caption OCR (EasyOCR on the selected region, JSONL)
4. When you leave the meeting, the app prompts for a save folder.

[Slack remote control]
  meetingagent://start   - Bring app to front, keep detecting
  meetingagent://stop    - Force-finalize current session (save prompt)
  meetingagent://status  - Log current state
  meetingagent://show    - Bring window to front

[Uninstall]
Run %LOCALAPPDATA%\MeetingAgent\uninstall.bat

[Offline]
EasyOCR models are bundled. No internet required.
