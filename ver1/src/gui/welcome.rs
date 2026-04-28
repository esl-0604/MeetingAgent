//! Welcome window — WebView2-backed splash that owns the entire startup
//! flow (welcome → folder picker → optional re-pick on validation failure).
//!
//! Why bundle the folder picker into welcome's event loop?
//!   - We want the welcome window to stay visible behind the folder picker,
//!     so cancelling the picker takes the user back to the welcome (not to a
//!     blank desktop with the program already running on a stale default).
//!   - The folder picker is shown modal to the welcome HWND, so clicks on
//!     the welcome are blocked while the picker is up — visually it sits
//!     "on top of" the welcome.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::WindowExtWindows;
use tao::window::WindowBuilder;
use windows::Win32::Foundation::HWND;
use wry::{WebContext, WebViewBuilder};

const HTML_TEMPLATE: &str = include_str!("welcome.html");
const ICON_PNG: &[u8] = include_bytes!("../../assets/icon-128.png");

/// Outcome of `show()`. The caller distinguishes:
///   - `Some(path)`: user clicked 시작하기 and picked a save folder
///   - `None`: user dismissed the welcome with X / Esc — caller should exit
pub fn show(default_folder: PathBuf) -> Result<Option<PathBuf>> {
    let mut event_loop = EventLoopBuilder::<WelcomeAction>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("Meeting Agent")
        .with_inner_size(LogicalSize::new(540.0, 440.0))
        .with_resizable(false)
        .with_minimizable(false)
        .with_maximizable(false)
        .with_focused(true)
        .with_visible(true)
        .build(&event_loop)
        .map_err(|e| anyhow!("welcome window build: {e}"))?;

    let welcome_hwnd = HWND(window.hwnd() as _);

    let html = HTML_TEMPLATE.replace("{{ICON_DATA_URI}}", &icon_data_uri());

    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("MeetingAgent")
        .join("WebView2");
    let _ = std::fs::create_dir_all(&data_dir);
    let mut web_context = WebContext::new(Some(data_dir));

    let proxy = event_loop.create_proxy();
    let _webview = WebViewBuilder::with_web_context(&mut web_context)
        .with_html(html)
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            let body = req.body();
            let action = match body.as_str() {
                "ok" => WelcomeAction::PickFolder,
                _ => WelcomeAction::Dismissed,
            };
            let _ = proxy.send_event(action);
        })
        .build(&window)
        .map_err(|e| anyhow!("WebView build: {e}"))?;

    let mut result: Option<PathBuf> = None;
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(WelcomeAction::PickFolder) => {
                // Folder picker is modal-parent to the welcome window: it
                // blocks input on welcome but leaves it visible underneath.
                // The dialog itself handles "name field empty" validation
                // via our IFileDialogEvents callback, so we just accept any
                // path it returns (or stay in welcome on cancel).
                match crate::dialog::pick_folder_with_owner(
                    "녹화 저장 폴더 선택",
                    Some(&default_folder),
                    Some(welcome_hwnd),
                ) {
                    Ok(Some(picked)) => {
                        result = Some(picked);
                        *control_flow = ControlFlow::Exit;
                    }
                    Ok(None) => {
                        // Cancel / X on folder picker — stay in welcome.
                    }
                    Err(e) => {
                        tracing::warn!("folder picker error: {e:#}");
                    }
                }
            }
            Event::UserEvent(WelcomeAction::Dismissed) => {
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    Ok(result)
}

#[derive(Debug, Clone, Copy)]
enum WelcomeAction {
    PickFolder,
    Dismissed,
}

fn icon_data_uri() -> String {
    format!("data:image/png;base64,{}", b64_encode(ICON_PNG))
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
