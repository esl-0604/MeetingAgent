//! Meeting state detection.
//!
//! We poll the UIA tree of every visible top-level Teams window every
//! `detect.poll_interval_ms` looking for "in a meeting" markers — things like a
//! Leave/Hang-up button on the call control bar. When we see a marker for the
//! first time we emit `MeetingEvent::Started`; when it disappears for two
//! consecutive polls we emit `MeetingEvent::Stopped`.
//!
//! Two-poll debounce avoids false-stop blips while Teams re-renders the bar
//! (mute toggle, view switch, etc).

mod presenter;

pub use presenter::{detect_active_share, detect_active_share_global, ShareState};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};
use windows::core::PWSTR;
use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{IUIAutomation, IUIAutomationElement};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::config::Config;
use crate::uia::{self, name_of, walk_descendants, WalkAction};

/// Newtype around a Win32 HWND so we can pass it across `Send` boundaries —
/// HWND itself is `!Send` in the windows crate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HwndHandle(pub isize);
unsafe impl Send for HwndHandle {}
unsafe impl Sync for HwndHandle {}

#[derive(Debug, Clone)]
pub enum MeetingEvent {
    Started {
        teams_hwnd: HwndHandle,
        teams_pid: u32,
        title: Option<String>,
    },
    Stopped,
}

const MEETING_NAME_NEEDLES: &[&str] = &[
    "leave", "hang up", "hang-up", "end call",
    "나가기", "끊기", "통화 종료",
];

pub async fn run_detector(
    cfg: Arc<Config>,
    tx: mpsc::Sender<MeetingEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    info!("meeting detector starting (poll {}ms)", cfg.detect.poll_interval_ms);

    let interval = Duration::from_millis(cfg.detect.poll_interval_ms.max(500));
    let cfg2 = cfg.clone();
    let shutdown_inner = shutdown.resubscribe();

    let h = tokio::task::spawn_blocking(move || {
        if let Err(e) = poll_loop(cfg2, tx, interval, shutdown_inner) {
            warn!("detector poll loop exited: {e:#}");
        }
    });

    let _ = shutdown.recv().await;
    let _ = h.await;
    info!("meeting detector stopped");
    Ok(())
}

fn poll_loop(
    _cfg: Arc<Config>,
    tx: mpsc::Sender<MeetingEvent>,
    interval: Duration,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    uia::com_init_thread();
    let auto = uia::create_automation()?;

    let mut in_meeting: Option<(HwndHandle, u32, Option<String>)> = None;
    let mut miss_streak: u32 = 0;

    loop {
        if tx.is_closed() {
            break;
        }
        if matches!(
            shutdown.try_recv(),
            Ok(()) | Err(broadcast::error::TryRecvError::Closed)
        ) {
            break;
        }

        let teams_windows = enumerate_teams_windows();
        // Collect every Teams window that currently has a hangup-button, then
        // pick the best one — preferring the main meeting view over the
        // mini-meeting-view / sharing-control-bar popups that Teams briefly
        // opens during a screen share. Picking a popup hwnd as the session
        // anchor causes `ElementFromHandle` to start failing the moment the
        // popup closes (EVENT_E_NO_SUBSCRIBERS / 0x80040201).
        let mut candidates: Vec<(HwndHandle, u32, Option<String>)> = Vec::new();
        for tw in &teams_windows {
            let elem = match uia::element_from_hwnd(&auto, tw.hwnd) {
                Ok(e) => e,
                Err(e) => {
                    debug!("ElementFromHandle({:#x}): {e}", tw.hwnd);
                    continue;
                }
            };
            if window_has_meeting_marker(&auto, &elem) {
                candidates.push((HwndHandle(tw.hwnd), tw.pid, tw.title.clone()));
            }
        }
        let found = candidates
            .iter()
            .find(|c| !is_popup_title(c.2.as_deref()))
            .cloned()
            .or_else(|| candidates.first().cloned());

        match (&in_meeting, &found) {
            (None, Some(f)) => {
                in_meeting = Some(f.clone());
                miss_streak = 0;
                let _ = tx.blocking_send(MeetingEvent::Started {
                    teams_hwnd: f.0,
                    teams_pid: f.1,
                    title: f.2.clone(),
                });
            }
            (Some(_), None) => {
                miss_streak += 1;
                // Grace period: when the user starts a screen share, Teams
                // transiently rearranges its windows and the hangup button
                // can disappear from the UIA tree for several seconds. Wait
                // ~15 s of continuous misses (10 * 1.5 s poll) before
                // declaring the meeting truly over.
                if miss_streak >= 10 {
                    in_meeting = None;
                    miss_streak = 0;
                    let _ = tx.blocking_send(MeetingEvent::Stopped);
                }
            }
            (Some(_), Some(_)) | (None, None) => {
                miss_streak = 0;
            }
        }

        std::thread::sleep(interval);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct TeamsWindow {
    pub hwnd: isize,
    pub pid: u32,
    #[allow(dead_code)]
    pub title: Option<String>,
}

/// Public wrapper around the private `enumerate_teams_windows` so sibling
/// modules (e.g. `presenter`) can iterate all Teams windows without reaching
/// into private state.
pub(crate) fn enumerate_teams_windows_pub() -> Vec<TeamsWindow> {
    enumerate_teams_windows()
}

fn enumerate_teams_windows() -> Vec<TeamsWindow> {
    let mut out: Vec<TeamsWindow> = Vec::new();
    let out_ptr = &mut out as *mut Vec<TeamsWindow> as isize;
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(out_ptr));
    }
    out
}

extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let out = &mut *(lparam.0 as *mut Vec<TeamsWindow>);
        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }
        let mut pid: u32 = 0;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return true.into();
        }
        let exe = process_image_basename(pid).unwrap_or_default();
        let exe_lower = exe.to_lowercase();
        if !(exe_lower.contains("ms-teams") || exe_lower.contains("teams.exe")) {
            return true.into();
        }
        let title = window_title(hwnd);
        out.push(TeamsWindow { hwnd: hwnd.0 as isize, pid, title });
    }
    true.into()
}

fn window_title(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

fn process_image_basename(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let res = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(h);
        if res.is_err() || len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
    }
}

/// Is this window title one of Teams' transient popups (mini meeting view,
/// floating sharing control bar, etc) rather than the main meeting window?
fn is_popup_title(title: Option<&str>) -> bool {
    let Some(t) = title else { return false };
    let lower = t.to_lowercase();
    // Korean
    if t.contains("간단히 보기") || t.contains("컨트롤 막대") {
        return true;
    }
    // English
    if lower.contains("compact view")
        || lower.contains("mini view")
        || lower.contains("control bar")
        || lower.contains("sharing toolbar")
    {
        return true;
    }
    false
}

fn window_has_meeting_marker(auto: &IUIAutomation, root: &IUIAutomationElement) -> bool {
    let mut hit = false;
    // The hangup button sits inside the WebView2 DOM at depth ~16 in current
    // New Teams (verified 2026-04 against version 26072.x). Use 20 for
    // headroom across UI updates. We also disable the offscreen filter — the
    // mini-meeting-view window can mark intermediate panels offscreen even
    // when the call control bar is fully visible.
    let _ = walk_descendants(auto, root, 20, false, |e, _| {
        let n = name_of(e).to_lowercase();
        if MEETING_NAME_NEEDLES.iter().any(|nd| n.contains(&nd.to_lowercase())) {
            // Accept Button (50000) and SplitButton (50019) by control type,
            // OR an explicit AutomationId of "hangup-button" which Teams uses
            // even when the localised name varies.
            let ct = uia::control_type_of(e);
            let aid = uia::automation_id_of(e).to_lowercase();
            if ct == 50000 || ct == 50019 || aid == "hangup-button" {
                hit = true;
                return WalkAction::Stop;
            }
        }
        WalkAction::Continue
    });
    hit
}

/// `--diagnose` entrypoint. Lists every visible Teams window, dumps its UIA
/// tree, and prints any element whose Name matches a meeting-marker needle —
/// so we can see WHY meeting detection isn't firing.
pub fn diagnose(max_depth: u32) -> Result<()> {
    uia::com_init_thread();
    let auto = uia::create_automation()?;
    let windows = enumerate_teams_windows();

    println!("\n=== Teams windows enumerated ({}) ===", windows.len());
    if windows.is_empty() {
        println!("(none — process_image_basename couldn't match 'ms-teams' / 'teams.exe')");
        println!("Verify with: powershell \"Get-Process ms-teams\"");
        return Ok(());
    }
    for (i, tw) in windows.iter().enumerate() {
        println!(
            "  #{i}  pid={} hwnd={:#x} title={:?}",
            tw.pid, tw.hwnd, tw.title
        );
    }

    for (i, tw) in windows.iter().enumerate() {
        println!(
            "\n=== Window #{i} (hwnd={:#x}) — meeting-marker candidates ===",
            tw.hwnd
        );
        let elem = match uia::element_from_hwnd(&auto, tw.hwnd) {
            Ok(e) => e,
            Err(e) => {
                println!("  ElementFromHandle failed: {e}");
                continue;
            }
        };
        let mut hits = 0usize;
        let _ = walk_descendants(&auto, &elem, max_depth, false, |e, depth| {
            let n_raw = name_of(e);
            let n = n_raw.to_lowercase();
            if MEETING_NAME_NEEDLES.iter().any(|nd| n.contains(&nd.to_lowercase())) {
                hits += 1;
                let pad = "  ".repeat(depth as usize);
                println!(
                    "{pad}HIT depth={} ct={} loc={:?} name={:?} aid={:?}",
                    depth,
                    uia::control_type_of(e),
                    uia::localized_type_of(e),
                    n_raw,
                    uia::automation_id_of(e),
                );
            }
            WalkAction::Continue
        });
        if hits == 0 {
            println!("  (no element with name matching {:?})", MEETING_NAME_NEEDLES);
            println!("  → Either no meeting in this window, or our needle list misses the actual button label.");
            println!("  Consider raising --diagnose-depth (current {max_depth}) or check inspect.exe.");
        } else {
            println!("  ({hits} hit(s) total. Detector requires control type 50000=Button or 50019=SplitButton.)");
        }
    }

    println!("\n=== UIA tree dump (first window, depth {max_depth}) ===");
    if let Some(tw) = windows.first() {
        if let Ok(elem) = uia::element_from_hwnd(&auto, tw.hwnd) {
            let _ = uia::dump_tree(&auto, &elem, max_depth);
        }
    }

    Ok(())
}

/// Find every Teams root UIA element. Used by the caption worker when no hwnd
/// was supplied (e.g. forced-start mode without a detected meeting).
pub fn find_teams_elements(
    auto: &IUIAutomation,
    _desktop: &IUIAutomationElement,
) -> Result<Vec<IUIAutomationElement>> {
    let mut out = Vec::new();
    for tw in enumerate_teams_windows() {
        if let Ok(e) = uia::element_from_hwnd(auto, tw.hwnd) {
            out.push(e);
        }
    }
    Ok(out)
}
