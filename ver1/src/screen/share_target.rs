//! Heuristic for "what is the user actually sharing right now?"
//!
//! Teams gives us no public API to ask "which HWND/HMONITOR did the user
//! pick from the share picker?", so we infer it from the foreground window
//! at the moment self-share starts:
//!
//!   * Foreground is a non-Teams window → user almost certainly chose
//!     "Share window" on this hwnd. Capture it directly so the recording
//!     mirrors exactly what the audience sees, and nothing else from the
//!     surrounding desktop leaks in.
//!   * Foreground is a Teams window (or absent) → fall back to the monitor
//!     containing that window (the cursor's monitor as last resort). This
//!     covers "Share screen" mode where Teams briefly returns to focus
//!     before hiding itself.
//!
//! We snapshot once at share-start and stick with the choice for the whole
//! self-share — chasing foreground changes mid-share would cause frequent
//! WGC session re-creates and visible frame gaps.

use anyhow::Result;
use tracing::{debug, info};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetCursorPos, GetForegroundWindow, GetWindowTextW, IsWindowVisible,
};
use windows::Win32::System::Threading::GetCurrentProcessId;

use crate::state;
use crate::uia::{self, class_of, help_text_of, name_of, walk_descendants, WalkAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareTarget {
    /// Capture a specific window — best fidelity for "Share window" mode.
    Window(isize),
    /// Capture an entire monitor — used for "Share screen" mode and as a
    /// fallback when the foreground window can't be trusted.
    Monitor(isize),
}

/// Identify what to capture during self-share. `teams_pid` lets us ignore
/// Teams' own foreground.
///
/// Strategy:
///   1. Wait ~300 ms for the UI to settle (the share picker closes, the
///      shared window pops to foreground).
///   2. Probe Teams's share-control-bar UIA tree for a mode hint: it
///      surfaces `"창을 공유하고 있습니다"` (window mode) or
///      `"화면을 공유하고 있습니다"` (screen mode). The actual HWND is NOT
///      exposed by Teams — confirmed by `--diagnose-share` — so we still
///      need a heuristic, but the mode hint lets us pick the right one.
///   3. Window mode → foreground HWND (filtering Teams). Screen mode →
///      monitor of foreground (or cursor) window. Unknown mode → try
///      window heuristic first, then monitor fallback.
pub fn pick(teams_pid: u32) -> ShareTarget {
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mode = detect_share_mode_via_uia();
    info!("share target: detected mode = {:?}", mode);

    let teams_pids = teams_process_pids();
    let is_teams = |pid: u32| {
        pid == teams_pid
            || teams_pids.contains(&pid)
            || pid == unsafe { GetCurrentProcessId() }
    };

    let fg = unsafe { GetForegroundWindow() };
    let fg_pid = if !fg.is_invalid() { window_pid(fg) } else { 0 };
    let fg_is_teams = !fg.is_invalid() && is_teams(fg_pid);

    match mode {
        Some(ShareMode::Window) => {
            if !fg.is_invalid() && !fg_is_teams {
                info!(
                    "share target: window mode → foreground hwnd={:#x} pid={}",
                    fg.0 as isize, fg_pid
                );
                return ShareTarget::Window(fg.0 as isize);
            }
            // Window mode but foreground is Teams — pick the most-recent
            // non-Teams visible top-level window as a best-effort guess.
            if let Some(w) = most_recent_non_teams_window(&teams_pids, teams_pid) {
                info!(
                    "share target: window mode (foreground was Teams) → guess hwnd={:#x} title={:?}",
                    w.hwnd, w.title
                );
                return ShareTarget::Window(w.hwnd);
            }
            // Last-ditch fallback to monitor.
            return monitor_fallback(fg);
        }
        Some(ShareMode::Screen) => {
            // Screen mode — use the monitor that contains the foreground
            // window (likely on the shared screen) or the cursor.
            return monitor_fallback(fg);
        }
        None => {
            // Mode hint missing — fall through to the original heuristic.
            if !fg.is_invalid() && !fg_is_teams {
                info!(
                    "share target: unknown mode → foreground hwnd={:#x} pid={}",
                    fg.0 as isize, fg_pid
                );
                return ShareTarget::Window(fg.0 as isize);
            }
            return monitor_fallback(fg);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ShareMode {
    Window,
    Screen,
}

/// Look for Teams's "창을 공유하고 있습니다" / "화면을 공유하고 있습니다" /
/// English equivalents. These live in the share-control-bar window
/// (a separate Teams hwnd) — we just walk every Teams window since the
/// scan is cheap and the bar's hwnd identity isn't stable.
fn detect_share_mode_via_uia() -> Option<ShareMode> {
    let auto = uia::create_automation().ok()?;
    for tw in state::enumerate_teams_windows_pub() {
        let elem = match uia::element_from_hwnd(&auto, tw.hwnd) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut hit: Option<ShareMode> = None;
        let _ = walk_descendants(&auto, &elem, 25, false, |e, _| {
            let n = name_of(e).to_lowercase();
            // Korean
            if n.contains("창을 공유") {
                hit = Some(ShareMode::Window);
                return WalkAction::Stop;
            }
            if n.contains("화면을 공유") {
                hit = Some(ShareMode::Screen);
                return WalkAction::Stop;
            }
            // English (best-effort guesses based on Teams localization)
            if n.contains("sharing a window") || n.contains("sharing this window") {
                hit = Some(ShareMode::Window);
                return WalkAction::Stop;
            }
            if n.contains("sharing your screen") || n.contains("sharing screen") {
                hit = Some(ShareMode::Screen);
                return WalkAction::Stop;
            }
            // Class names are present but unhelpful for this purpose; we
            // only rely on Name to keep the matcher localizable.
            let _ = class_of(e);
            let _ = help_text_of(e);
            WalkAction::Continue
        });
        if hit.is_some() {
            return hit;
        }
    }
    None
}

fn monitor_fallback(fg: HWND) -> ShareTarget {
    if !fg.is_invalid() {
        let hmon = unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) };
        if !hmon.is_invalid() {
            debug!("share target: monitor of foreground window = {:#x}", hmon.0 as isize);
            return ShareTarget::Monitor(hmon.0 as isize);
        }
    }
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_ok() {
        let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
        if !hmon.is_invalid() {
            debug!("share target: cursor monitor = {:#x}", hmon.0 as isize);
            return ShareTarget::Monitor(hmon.0 as isize);
        }
    }
    let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    debug!("share target: primary monitor fallback = {:#x}", hmon.0 as isize);
    ShareTarget::Monitor(hmon.0 as isize)
}

/// Best-effort: of all visible top-level windows, pick the highest-Z (most
/// recently active) one that is NOT a Teams renderer and NOT this agent.
/// EnumWindows iterates in Z-order top-down, so the first matching candidate
/// is the most recently active.
fn most_recent_non_teams_window(teams_pids: &[u32], teams_pid: u32) -> Option<VisibleWindow> {
    let me = unsafe { GetCurrentProcessId() };
    enumerate_visible_windows()
        .into_iter()
        .find(|w| {
            w.pid != me
                && w.pid != teams_pid
                && !teams_pids.contains(&w.pid)
                && !w.title.trim().is_empty()
        })
}

fn window_pid(hwnd: HWND) -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Every PID that hosts a visible Teams window. New Teams spawns multiple
/// renderers (`ms-teams.exe`, `msteams.exe`, `msteamswebview.exe`), and any
/// of them can be the foreground when the user clicks the share toolbar.
fn teams_process_pids() -> Vec<u32> {
    state::enumerate_teams_windows_pub()
        .into_iter()
        .map(|w| w.pid)
        .collect()
}

/// `--diagnose-share` entrypoint. Walks each Teams window's UIA tree
/// looking for elements whose Name or AutomationId hints at the active
/// share target. Prints them so we can see exactly what Teams exposes
/// and how to parse it. Run this WHILE you are actively sharing.
pub fn diagnose(max_depth: u32) -> Result<()> {
    uia::com_init_thread();
    let auto = uia::create_automation()?;

    // Keywords (en/ko) that typically appear on share-related UIA nodes.
    const NEEDLES: &[&str] = &[
        "sharing", "you're sharing", "you are sharing", "presenting",
        "stop sharing", "stop presenting",
        "공유", "발표", "내 화면", "중지",
    ];

    let windows = state::enumerate_teams_windows_pub();
    println!("\n=== Teams windows ({}) ===", windows.len());
    for (i, w) in windows.iter().enumerate() {
        println!(
            "  #{i} pid={} hwnd={:#x} title={:?}",
            w.pid, w.hwnd, w.title
        );
    }

    for (i, w) in windows.iter().enumerate() {
        let elem = match uia::element_from_hwnd(&auto, w.hwnd) {
            Ok(e) => e,
            Err(e) => {
                println!("\n=== Window #{i} (hwnd={:#x}) — ElementFromHandle failed: {e} ===", w.hwnd);
                continue;
            }
        };
        println!(
            "\n=== Window #{i} (hwnd={:#x}) — share-marker hits (Name + HelpText) ===",
            w.hwnd
        );
        let mut hits = 0usize;
        let _ = walk_descendants(&auto, &elem, max_depth, false, |e, depth| {
            let raw = name_of(e);
            let help = help_text_of(e);
            let n = raw.to_lowercase();
            let h = help.to_lowercase();
            let matched = NEEDLES.iter().any(|nd| {
                let nd_lower = nd.to_lowercase();
                n.contains(&nd_lower) || h.contains(&nd_lower)
            });
            if matched {
                hits += 1;
                let pad = "  ".repeat(depth as usize);
                println!(
                    "{pad}HIT depth={} ct={} loc={:?} name={:?} aid={:?} class={:?} help={:?}",
                    depth,
                    uia::control_type_of(e),
                    uia::localized_type_of(e),
                    raw,
                    uia::automation_id_of(e),
                    class_of(e),
                    help,
                );
            }
            WalkAction::Continue
        });
        if hits == 0 {
            println!("  (no element with share-related name/helptext in this window)");
        }

        // For the share-control-bar window, dump the FULL subtree (every
        // descendant, not just hits) — its title contains "공유" / "share"
        // and we want to discover any sibling/parent that names the actual
        // shared window.
        let title_lower = w.title.as_deref().unwrap_or("").to_lowercase();
        if title_lower.contains("공유") || title_lower.contains("share") {
            println!(
                "\n=== FULL DESCENDANT DUMP of share-control window #{i} (hwnd={:#x}) ===",
                w.hwnd
            );
            let _ = walk_descendants(&auto, &elem, max_depth, false, |e, depth| {
                let pad = "  ".repeat(depth as usize);
                let nm = name_of(e);
                let help = help_text_of(e);
                let aid = uia::automation_id_of(e);
                let cls = class_of(e);
                let ct = uia::control_type_of(e);
                let interesting = !nm.is_empty()
                    || !help.is_empty()
                    || !aid.is_empty()
                    || (!cls.is_empty() && cls != "Group");
                if interesting {
                    println!(
                        "{pad}d{} ct={} loc={:?} name={:?} aid={:?} class={:?} help={:?}",
                        depth,
                        ct,
                        uia::localized_type_of(e),
                        nm,
                        aid,
                        cls,
                        help,
                    );
                }
                WalkAction::Continue
            });
        }
    }

    println!(
        "\n=== Top-level visible windows (for window-title matching reference) ==="
    );
    for w in enumerate_visible_windows() {
        println!("  hwnd={:#x} pid={} title={:?}", w.hwnd, w.pid, w.title);
    }

    println!("\n=== Monitors (for screen-index matching reference) ===");
    for (idx, m) in enumerate_monitors().iter().enumerate() {
        println!(
            "  display#{} hmon={:#x} rect=({}, {}, {}, {})",
            idx + 1, m.hmon, m.rect.left, m.rect.top, m.rect.right, m.rect.bottom
        );
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct VisibleWindow {
    hwnd: isize,
    pid: u32,
    title: String,
}

fn enumerate_visible_windows() -> Vec<VisibleWindow> {
    let mut out: Vec<VisibleWindow> = Vec::new();
    let ptr = &mut out as *mut Vec<VisibleWindow> as isize;
    unsafe {
        let _ = EnumWindows(Some(visible_enum_proc), LPARAM(ptr));
    }
    out
}

unsafe extern "system" fn visible_enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<VisibleWindow>);
    if !IsWindowVisible(hwnd).as_bool() {
        return true.into();
    }
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf);
    let title = if n > 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        return true.into();
    };
    let mut pid: u32 = 0;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    out.push(VisibleWindow {
        hwnd: hwnd.0 as isize,
        pid,
        title,
    });
    true.into()
}

#[derive(Debug, Clone, Copy)]
struct MonitorInfo {
    hmon: isize,
    rect: RECT,
}

fn enumerate_monitors() -> Vec<MonitorInfo> {
    let mut out: Vec<MonitorInfo> = Vec::new();
    let ptr = &mut out as *mut Vec<MonitorInfo> as isize;
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(monitor_enum_proc), LPARAM(ptr));
    }
    out
}

unsafe extern "system" fn monitor_enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<MonitorInfo>);
    out.push(MonitorInfo {
        hmon: hmon.0 as isize,
        rect: *rect,
    });
    true.into()
}
