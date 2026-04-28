//! Custom WebView2-backed popup window — replaces Windows toast notifications.
//!
//! Windows toasts are unreliable: users disable them globally, system policy
//! can block them, and the action-button callback only fires while the toast
//! is "live" (a few seconds). For an app whose whole job is to surface
//! decisions to the user, that's a problem. Instead we draw our own popup
//! window — borderless, always-on-top, anchored to the bottom-right of the
//! primary monitor, with a slide-in animation. Always works, fully under our
//! control, looks consistent with the rest of the GUI.
//!
//! Each `show*()` call spawns its own OS thread with its own tao event loop
//! and wry WebView. That's heavier than a toast, but `WebContext` lets all
//! popups share the same WebView2 user-data folder so the runtime stays
//! warm across calls.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::{EventLoopBuilderExtWindows, WindowBuilderExtWindows};
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};

const ICON_PNG: &[u8] = include_bytes!("../../assets/icon-32.png");
const POPUP_HTML: &str = include_str!("popup.html");

const POPUP_WIDTH: f64 = 360.0;
const POPUP_HEIGHT_INFO: f64 = 130.0;
const POPUP_HEIGHT_PROMPT: f64 = 180.0;
const MARGIN_RIGHT: f64 = 16.0;
const TASKBAR_RESERVE: f64 = 60.0;

/// Master switch for "passive event" popups (caption detected, share started,
/// window resized, etc). Interactive prompts and end-of-meeting confirmations
/// are NOT gated by this — they always show.
static EVENTS_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_events_enabled(on: bool) {
    EVENTS_ENABLED.store(on, Ordering::Relaxed);
}

pub fn events_enabled() -> bool {
    EVENTS_ENABLED.load(Ordering::Relaxed)
}

/// Same enum the legacy toast used; kept so existing call sites compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastAction {
    Record,
    Ignore,
}

/// One-shot informational popup: title + body, auto-dismisses after 5s. Body
/// click and button both just close. Always shows (not gated by toggle).
pub fn show_info(title: &str, body: &str) {
    spawn_popup(PopupConfig {
        head: "MEETING AGENT".into(),
        title: title.into(),
        body: body.into(),
        primary: None,
        secondary: None,
        body_click_dismisses: true,
        timeout_ms: 5000,
        height: POPUP_HEIGHT_INFO,
    });
}

/// Same as [`show_info`] but blocks the calling thread until the popup
/// dismisses (timeout, click, or Esc). Useful when the caller is about to
/// exit — fire-and-forget would race the popup window against process
/// teardown and the user would never see the message.
pub fn show_info_blocking(title: &str, body: &str) {
    let cfg = PopupConfig {
        head: "MEETING AGENT".into(),
        title: title.into(),
        body: body.into(),
        primary: None,
        secondary: None,
        body_click_dismisses: true,
        timeout_ms: 5000,
        height: POPUP_HEIGHT_INFO,
    };
    let _ = run_popup_window(cfg);
}

/// Same as `show_info` but gated by the events-enabled toggle. Use for noisy
/// state-change events (caption detected, share started, window resized…).
pub fn show_event(title: &str, body: &str) {
    if !events_enabled() {
        return;
    }
    spawn_popup(PopupConfig {
        head: "이벤트".into(),
        title: title.into(),
        body: body.into(),
        primary: None,
        secondary: None,
        body_click_dismisses: true,
        timeout_ms: 4500,
        height: POPUP_HEIGHT_INFO,
    });
}

/// "Teams 미팅이 감지되었습니다 — 녹화 / 무시" prompt. Reuses the existing
/// `ToastAction` enum so call sites in `gui/mod.rs` don't change.
pub fn show_meeting_prompt(title_hint: Option<&str>, done: Sender<ToastAction>) {
    let body = match title_hint {
        Some(t) if !t.is_empty() => format!("Teams 미팅이 감지되었습니다: {t}"),
        _ => "Teams 미팅이 감지되었습니다.".to_string(),
    };

    spawn_popup_with_callback(
        PopupConfig {
            head: "미팅 감지됨".into(),
            title: "녹화하시겠습니까?".into(),
            body,
            primary: Some(PopupButton {
                label: "녹화".into(),
                action: "record".into(),
            }),
            secondary: Some(PopupButton {
                label: "무시".into(),
                action: "ignore".into(),
            }),
            body_click_dismisses: false,
            // Popup auto-closes a touch after the caller's 30s recv_timeout
            // so caller's `Timeout` branch wins the race and we get the
            // "missed" state, not "ignored".
            timeout_ms: 31_000,
            height: POPUP_HEIGHT_PROMPT,
        },
        move |result| match result {
            PopupResult::Action(a) if a == "record" => {
                let _ = done.send(ToastAction::Record);
            }
            PopupResult::Action(a) if a == "ignore" => {
                let _ = done.send(ToastAction::Ignore);
            }
            // Timeout / Closed / "_close" button: don't send anything so the
            // caller's recv_timeout differentiates "missed" from "ignored".
            _ => {}
        },
    );
}

/// Shown when the meeting window disappears. Recording continues in the
/// background; the popup gives the user 10 seconds to confirm "녹화 중지 +
/// 폴더 열기" before we auto-finalise. Either the body or the button fires
/// `OpenFolder`. Timeout / close fires `AutoFinalize`.
#[derive(Debug, Clone, Copy)]
pub enum MeetingEndedAction {
    OpenFolder,
    AutoFinalize,
}

pub fn show_meeting_ended(done: Sender<MeetingEndedAction>) {
    spawn_popup_with_callback(
        PopupConfig {
            head: "미팅 종료 감지".into(),
            title: "미팅이 종료된 것 같습니다.".into(),
            body: "10초 안에 응답하지 않으면 자동으로 마무리합니다.".into(),
            primary: Some(PopupButton {
                label: "녹화 중지 + 폴더 열기".into(),
                action: "stop_open".into(),
            }),
            secondary: None,
            body_click_dismisses: false, // body click = stop+open as well
            timeout_ms: 10_000,
            height: POPUP_HEIGHT_PROMPT,
        },
        move |result| {
            let action = match result {
                PopupResult::Action(_) | PopupResult::BodyClick => MeetingEndedAction::OpenFolder,
                _ => MeetingEndedAction::AutoFinalize,
            };
            let _ = done.send(action);
        },
    );
}

/// Shown after a finalize (either auto or user-confirmed). Click anywhere to
/// open the folder. Auto-dismiss after 8 seconds.
pub fn show_post_finalize(session_dir: std::path::PathBuf) {
    spawn_popup_with_callback(
        PopupConfig {
            head: "녹화 완료".into(),
            title: "세션이 저장되었습니다.".into(),
            body: format!("클릭해서 폴더 열기:\n{}", session_dir.display()),
            primary: Some(PopupButton {
                label: "폴더 열기".into(),
                action: "open".into(),
            }),
            secondary: None,
            body_click_dismisses: false,
            timeout_ms: 8000,
            height: POPUP_HEIGHT_PROMPT,
        },
        move |result| match result {
            PopupResult::Action(_) | PopupResult::BodyClick => {
                let _ = std::process::Command::new("explorer.exe")
                    .arg(&session_dir)
                    .spawn();
            }
            _ => {}
        },
    );
}

// ─────────────── internals ───────────────

#[derive(Clone)]
struct PopupButton {
    label: String,
    action: String,
}

#[derive(Clone)]
struct PopupConfig {
    head: String,
    title: String,
    body: String,
    primary: Option<PopupButton>,
    secondary: Option<PopupButton>,
    body_click_dismisses: bool,
    timeout_ms: u64,
    height: f64,
}

#[derive(Debug)]
enum PopupResult {
    Action(String),
    BodyClick,
    Timeout,
    Closed,
}

fn spawn_popup(cfg: PopupConfig) {
    spawn_popup_with_callback(cfg, |_| {});
}

fn spawn_popup_with_callback<F>(cfg: PopupConfig, on_result: F)
where
    F: FnOnce(PopupResult) + Send + 'static,
{
    std::thread::Builder::new()
        .name("meeting-agent-popup".into())
        .spawn(move || {
            let result = match run_popup_window(cfg) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("popup window failed: {e:#}");
                    PopupResult::Closed
                }
            };
            on_result(result);
        })
        .ok();
}

fn run_popup_window(cfg: PopupConfig) -> Result<PopupResult> {
    // Each popup runs on its own OS thread. Tao otherwise panics with
    // "Initializing the event loop outside of the main thread is a
    // significant cross-platform compatibility hazard." We accept that
    // hazard explicitly via `any_thread(true)` — there is no main-thread
    // synchronisation primitive at risk: each popup's event loop is
    // self-contained and short-lived.
    let mut event_loop = EventLoopBuilder::<UserAction>::with_user_event()
        .with_any_thread(true)
        .build();

    // Anchor at bottom-right of the primary monitor, leaving room for the
    // taskbar. We approximate the work area by reserving 60 logical px from
    // the bottom; this is wider than Windows 11's default 48px taskbar.
    let monitor = event_loop
        .primary_monitor()
        .ok_or_else(|| anyhow!("no primary monitor"))?;
    let scale = monitor.scale_factor();
    let logical = monitor.size().to_logical::<f64>(scale);
    let pos_x = logical.width - POPUP_WIDTH - MARGIN_RIGHT;
    let pos_y = logical.height - cfg.height - TASKBAR_RESERVE;

    let window = WindowBuilder::new()
        .with_title("Meeting Agent")
        .with_inner_size(LogicalSize::new(POPUP_WIDTH, cfg.height))
        .with_position(LogicalPosition::new(pos_x, pos_y))
        .with_resizable(false)
        .with_decorations(false)
        .with_always_on_top(true)
        .with_skip_taskbar(true)
        .with_focused(false)
        .with_visible(true)
        .build(&event_loop)
        .map_err(|e| anyhow!("popup window build: {e}"))?;

    let html = render_html(&cfg);

    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("MeetingAgent")
        .join("WebView2");
    let _ = std::fs::create_dir_all(&data_dir);
    let mut web_context = WebContext::new(Some(data_dir));

    let proxy = event_loop.create_proxy();
    let _webview = WebViewBuilder::with_web_context(&mut web_context)
        .with_html(html)
        .with_transparent(true)
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            let body = req.body().to_string();
            let _ = proxy.send_event(UserAction::Ipc(body));
        })
        .build(&window)
        .map_err(|e| anyhow!("popup webview build: {e}"))?;

    // Auto-dismiss timer.
    if cfg.timeout_ms > 0 {
        let proxy_timer = event_loop.create_proxy();
        let to = cfg.timeout_ms;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(to));
            let _ = proxy_timer.send_event(UserAction::Timeout);
        });
    }

    let mut result = PopupResult::Closed;
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserAction::Ipc(arg)) => {
                result = match arg.as_str() {
                    "_body" => PopupResult::BodyClick,
                    other => PopupResult::Action(other.to_string()),
                };
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserAction::Timeout) => {
                result = PopupResult::Timeout;
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                result = PopupResult::Closed;
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    Ok(result)
}

#[derive(Debug)]
enum UserAction {
    Ipc(String),
    Timeout,
}

fn render_html(cfg: &PopupConfig) -> String {
    let icon_data_uri = format!("data:image/png;base64,{}", b64_encode(ICON_PNG));

    let mut buttons_html = String::new();
    if let Some(b) = &cfg.secondary {
        buttons_html.push_str(&format!(
            r#"<button class="secondary" onclick="action('{}', event)">{}</button>"#,
            html_escape(&b.action),
            html_escape(&b.label),
        ));
    }
    if let Some(b) = &cfg.primary {
        buttons_html.push_str(&format!(
            r#"<button class="primary" onclick="action('{}', event)">{}</button>"#,
            html_escape(&b.action),
            html_escape(&b.label),
        ));
    }

    let body_action = if cfg.body_click_dismisses {
        "_body"
    } else if let Some(b) = &cfg.primary {
        b.action.as_str()
    } else {
        "_body"
    };

    POPUP_HTML
        .replace("{{ICON_DATA_URI}}", &icon_data_uri)
        .replace("{{HEAD}}", &html_escape(&cfg.head))
        .replace("{{TITLE}}", &html_escape(&cfg.title))
        .replace("{{BODY}}", &html_escape(&cfg.body).replace('\n', "<br>"))
        .replace("{{ACTIONS}}", &buttons_html)
        .replace("{{BODY_ACTION}}", &html_escape(body_action))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn b64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 << 4) | (b1 >> 4)) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 << 2) | (b2 >> 6)) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

// Convenience: legacy `mpsc::channel` factory used by callers waiting on a
// popup result. Re-exported so call sites don't need to import `mpsc`.
pub fn channel<T>() -> (Sender<T>, mpsc::Receiver<T>) {
    mpsc::channel::<T>()
}
