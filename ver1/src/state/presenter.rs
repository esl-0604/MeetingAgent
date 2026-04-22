//! Detect active screen-share and the presenter's name.
//!
//! Teams renders one of these accessibility strings while sharing is active:
//!   - `"<Name> is presenting"` (en-US)
//!   - `"<Name> is sharing screen"` (en-US, older)
//!   - `"<이름>님이 발표 중입니다"` (ko-KR)
//!   - `"You are presenting"` / `"내가 발표 중"` when *we* are sharing.
//!
//! We sweep the Teams UIA tree for any element whose Name matches and parse
//! the presenter out of the matched string.

use crate::uia::{self, automation_id_of, name_of, walk_descendants, WalkAction};
use anyhow::Result;
use windows::Win32::UI::Accessibility::{IUIAutomation, IUIAutomationElement};

#[derive(Debug, Clone)]
pub struct ShareState {
    pub presenter: Option<String>,
}

const PATTERNS: &[&str] = &[
    " is presenting",
    " is sharing screen",
    " is sharing their screen",
    "님이 발표 중",
    "님이 화면을 공유",
];
const SELF_PATTERNS: &[&str] = &[
    "you are presenting",
    "you are sharing",
    "내가 발표 중",
    "내가 화면을 공유",
];

pub fn detect_active_share(
    auto: &IUIAutomation,
    teams_root: &IUIAutomationElement,
) -> Result<Option<ShareState>> {
    let mut hit: Option<ShareState> = None;
    // Don't skip offscreen — the "X is presenting" banner can be transient
    // or partially clipped by other panels.
    walk_descendants(auto, teams_root, 20, false, |e, _| {
        let raw = name_of(e);
        let n = raw.to_lowercase();
        if SELF_PATTERNS.iter().any(|p| n.contains(p)) {
            hit = Some(ShareState { presenter: Some("(me)".into()) });
            return WalkAction::Stop;
        }
        for p in PATTERNS {
            if let Some(idx) = n.find(p) {
                let presenter = raw[..idx].trim().trim_end_matches(',').trim().to_string();
                let presenter = if presenter.is_empty() { None } else { Some(presenter) };
                hit = Some(ShareState { presenter });
                return WalkAction::Stop;
            }
        }
        // New: the call control bar's "Share" button flips its label to
        // "Stop sharing" / "공유 중지" while WE are sharing. AutomationId
        // stays "share-button" in both states — checking the name
        // distinguishes active-share from idle.
        if automation_id_of(e).to_lowercase() == "share-button" {
            if n.contains("중지") || n.contains("stop sharing") || n.contains("stop presenting") {
                hit = Some(ShareState { presenter: Some("(me)".into()) });
                return WalkAction::Stop;
            }
        }
        WalkAction::Continue
    })?;
    Ok(hit)
}

/// Scan every visible Teams window and return a `ShareState` if any of them
/// show share-active markers. Needed because the "Stop sharing" button lives
/// in the mini-meeting-view window (a distinct hwnd from the main meeting
/// window), so a per-root scan on the detector's chosen hwnd can miss it.
pub fn detect_active_share_global(auto: &IUIAutomation) -> Option<ShareState> {
    for tw in super::enumerate_teams_windows_pub() {
        let elem = match uia::element_from_hwnd(auto, tw.hwnd) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if let Ok(Some(s)) = detect_active_share(auto, &elem) {
            return Some(s);
        }
    }
    None
}

#[allow(dead_code)]
pub fn detect_share_for_hwnd(hwnd: isize) -> Result<Option<ShareState>> {
    uia::com_init_thread();
    let auto = uia::create_automation()?;
    let elem = uia::element_from_hwnd(&auto, hwnd)?;
    detect_active_share(&auto, &elem)
}
