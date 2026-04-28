//! GUI layer: tray icon + toast notifications + state machine.
//!
//! Architecture:
//!   - Main thread: tao event loop (Windows requires this). Owns the tray
//!     icon, menu, and the user-facing state (Idle / Detected / Recording…).
//!   - Background thread: tokio runtime running the detector + a session
//!     orchestrator that obeys `Command`s from the main thread.
//!   - Cross-thread comm:
//!       * detector → main: `EventLoopProxy<UiEvent>` (forwarded by a small
//!         tokio bridge task)
//!       * main → orchestrator: `tokio::sync::mpsc::UnboundedSender<Command>`
//!       * toast callback → main: same `EventLoopProxy<UiEvent>` (callback
//!         runs on a winrt-managed thread)
//!
//! The state machine itself is dirt simple and lives in this file's
//! `handle_*` functions.

mod icons;
pub mod popup;
mod tray;
mod welcome;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use muda::MenuEvent;
use tray_icon::TrayIconEvent;

use crate::config::Config;
use crate::persisted_state::AgentState;
use crate::state::MeetingEvent;
use crate::{run_session, SessionContext};

use self::popup::{MeetingEndedAction, ToastAction};
use self::tray::MenuFlags;

/// Public entry from `main`. Blocks the main thread inside the event loop;
/// returns only when the user picks Quit.
pub fn run(cfg: Arc<Config>) -> ! {
    // Welcome + folder picker FIRST — both run their own modal tao event
    // loops, and on Windows two tao event loops on the same thread don't
    // mix well: the second one's window-message hooks intercept events
    // queued for the first. So we create the main tray event loop only
    // AFTER startup is done.
    if !handle_startup(&cfg) {
        info!("welcome dismissed; exiting before tray bringup");
        std::process::exit(0);
    }

    let event_loop: EventLoop<UiEvent> =
        EventLoopBuilder::<UiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    install_global_event_forwards(proxy.clone());

    let persisted = AgentState::load();
    popup::set_events_enabled(persisted.event_notifications);
    let mut state = GuiState::Idle;
    let mut flags = MenuFlags {
        auto_record: persisted.auto_record_on_detect,
        auto_start: autostart::is_enabled(),
        event_notifications: persisted.event_notifications,
        last_session: persisted.last_session_dir.clone(),
    };
    let mut last_ctx: Option<SessionContext> = None;

    let mut tray_handle = tray::build_initial(&state, &flags);

    // Spin up the background tokio runtime + orchestrator.
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (shutdown_tx, _) = broadcast::channel::<()>(4);

    spawn_background(cfg.clone(), proxy.clone(), cmd_rx, shutdown_tx.clone());

    popup::show_info(
        "Meeting Agent",
        "트레이에서 동작 중입니다. Teams 미팅이 감지되면 알려드리겠습니다.",
    );

    let cfg_for_loop = cfg.clone();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            tao::event::Event::UserEvent(UiEvent::Menu(evt)) => {
                let id = evt.id.0.as_str().to_string();
                handle_menu_click(
                    &id,
                    &mut state,
                    &mut tray_handle,
                    &mut flags,
                    &mut last_ctx,
                    &cfg_for_loop,
                    &cmd_tx,
                    &proxy,
                    control_flow,
                );
            }
            tao::event::Event::UserEvent(UiEvent::Tray(_)) => {
                // Right-click → menu auto-pops; left-click → no-op for now.
            }
            tao::event::Event::UserEvent(UiEvent::Detector(MeetingEvent::Started {
                teams_hwnd,
                teams_pid,
                title,
            })) => {
                let ctx = SessionContext {
                    teams_hwnd,
                    teams_pid,
                    title,
                };
                last_ctx = Some(ctx.clone());
                on_meeting_started(
                    ctx,
                    &mut state,
                    &mut tray_handle,
                    &flags,
                    &cmd_tx,
                    &proxy,
                );
            }
            tao::event::Event::UserEvent(UiEvent::Detector(MeetingEvent::Stopped)) => {
                on_meeting_stopped(&mut state, &mut tray_handle, &flags, &cmd_tx, &proxy);
            }
            tao::event::Event::UserEvent(UiEvent::ToastResult(action)) => {
                on_toast_result(action, &mut state, &mut tray_handle, &flags, &cmd_tx);
            }
            tao::event::Event::UserEvent(UiEvent::ToastTimeout) => {
                on_toast_timeout(&mut state, &mut tray_handle, &flags);
            }
            tao::event::Event::UserEvent(UiEvent::MeetingEndedResult(action)) => {
                on_meeting_ended_result(
                    action,
                    &mut state,
                    &mut tray_handle,
                    &flags,
                    &cmd_tx,
                );
            }
            tao::event::Event::UserEvent(UiEvent::SessionFinalized { dir }) => {
                let open_after = matches!(state, GuiState::Finalizing { open_after: true });
                state = GuiState::Idle;
                if let Some(d) = dir.clone() {
                    flags.last_session = Some(d.clone());
                    AgentState::remember_last_session(&d);
                    if open_after {
                        let _ = std::process::Command::new("explorer.exe").arg(&d).spawn();
                    } else {
                        popup::show_post_finalize(d);
                    }
                } else {
                    popup::show_info("Meeting Agent", "녹화가 종료되었습니다.");
                }
                tray::rebuild(&mut tray_handle, &state, &flags);
            }
            _ => {}
        }
    });
}

#[derive(Debug)]
pub enum UiEvent {
    Menu(MenuEvent),
    /// Tray icon click (left/right). The inner value is unused for now —
    /// right-click pops the menu automatically and we ignore left-click.
    Tray(#[allow(dead_code)] TrayIconEvent),
    Detector(MeetingEvent),
    /// Result of the meeting-detected popup (record / ignore / timeout).
    ToastResult(ToastAction),
    ToastTimeout,
    /// Result of the meeting-ended popup (open folder / auto-finalize).
    MeetingEndedResult(MeetingEndedAction),
    SessionFinalized {
        dir: Option<PathBuf>,
    },
}

#[derive(Debug)]
pub enum Command {
    StartRecording { ctx: SessionContext },
    StopRecording,
    Quit,
}

#[derive(Debug, Clone)]
pub enum GuiState {
    Idle,
    Detected { ctx: SessionContext },
    Ignored { ctx: SessionContext },
    Missed { ctx: SessionContext },
    Recording,
    /// Awaiting the meeting-ended popup response (10s window). Recording is
    /// still in progress on the orchestrator. Either user click → folder
    /// open, or timeout → silent auto-finalize.
    AwaitingEndConfirm,
    Finalizing {
        open_after: bool,
    },
}

impl GuiState {
    fn ctx(&self) -> Option<&SessionContext> {
        match self {
            GuiState::Detected { ctx } | GuiState::Ignored { ctx } | GuiState::Missed { ctx } => {
                Some(ctx)
            }
            _ => None,
        }
    }
}

fn install_global_event_forwards(proxy: EventLoopProxy<UiEvent>) {
    // muda::MenuEvent and tray_icon::TrayIconEvent both expose
    // `set_event_handler` to push events through user-supplied closures.
    let pmenu = proxy.clone();
    MenuEvent::set_event_handler(Some(move |evt: MenuEvent| {
        let _ = pmenu.send_event(UiEvent::Menu(evt));
    }));
    let ptray = proxy;
    TrayIconEvent::set_event_handler(Some(move |evt: TrayIconEvent| {
        let _ = ptray.send_event(UiEvent::Tray(evt));
    }));
}

/// Returns `true` if the user picked a save folder; `false` if they closed
/// the welcome window with the X button.
///
/// The welcome module owns the entire welcome → folder-picker flow: the
/// welcome window stays visible while the picker is shown modal on top of
/// it, and cancelling the picker returns to the welcome (instead of
/// proceeding silently). We just feed it an initial-folder hint and store
/// whatever it returns.
fn handle_startup(cfg: &Arc<Config>) -> bool {
    let mut persisted = AgentState::load();
    let initial = persisted
        .last_save_dir
        .clone()
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| cfg.output_root.clone());

    match welcome::show(initial) {
        Ok(Some(picked)) => {
            persisted.last_save_dir = Some(picked);
            persisted.welcomed = true;
            persisted.save();
            true
        }
        Ok(None) => false,
        Err(e) => {
            warn!("custom welcome failed ({e:#}); falling back to MessageBoxW");
            let _ = native_alert(
                "Meeting Agent에 오신 것을 환영합니다",
                "이 앱은 트레이(우측 하단)에 상주하며,\n\
                 Teams 미팅이 감지되면 토스트 알림으로 녹화 여부를 묻습니다.",
            );
            true
        }
    }
}

fn native_alert(title: &str, body: &str) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
    let title_w: HSTRING = title.into();
    let body_w: HSTRING = body.into();
    unsafe {
        MessageBoxW(
            HWND(std::ptr::null_mut()),
            windows::core::PCWSTR(body_w.as_ptr()),
            windows::core::PCWSTR(title_w.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
    Ok(())
}

fn on_meeting_started(
    ctx: SessionContext,
    state: &mut GuiState,
    tray_handle: &mut tray::TrayHandle,
    flags: &MenuFlags,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    proxy: &EventLoopProxy<UiEvent>,
) {
    if matches!(state, GuiState::Recording | GuiState::Finalizing { .. }) {
        // Already capturing — duplicate "started" from detector during a
        // resize bounce; ignore.
        return;
    }
    if matches!(state, GuiState::AwaitingEndConfirm) {
        // The detector briefly thought the meeting ended (e.g. share-start
        // hides the hangup button for a few seconds) but it's back. Revert
        // to Recording so capture continues. The end-of-meeting popup may
        // still be on screen — its callback no-ops because state ≠
        // AwaitingEndConfirm.
        info!("meeting reappeared during end-confirm; reverting to Recording");
        *state = GuiState::Recording;
        tray::rebuild(tray_handle, state, flags);
        return;
    }

    if flags.auto_record {
        info!("auto-record on, starting recording for detected meeting");
        let _ = cmd_tx.send(Command::StartRecording { ctx });
        *state = GuiState::Recording;
        tray::rebuild(tray_handle, state, flags);
        return;
    }

    *state = GuiState::Detected { ctx: ctx.clone() };
    tray::rebuild(tray_handle, state, flags);

    spawn_meeting_prompt(ctx, proxy.clone());
}

fn on_meeting_stopped(
    state: &mut GuiState,
    tray_handle: &mut tray::TrayHandle,
    flags: &MenuFlags,
    _cmd_tx: &mpsc::UnboundedSender<Command>,
    proxy: &EventLoopProxy<UiEvent>,
) {
    match state {
        GuiState::Recording => {
            info!("meeting end detected; showing 10s confirmation popup");
            *state = GuiState::AwaitingEndConfirm;
            tray::rebuild(tray_handle, state, flags);

            // Spawn the popup; on user reply (or timeout), forward back to
            // the main event loop as `MeetingEndedResult`.
            let proxy_clone = proxy.clone();
            std::thread::spawn(move || {
                let (tx, rx) = popup::channel::<MeetingEndedAction>();
                popup::show_meeting_ended(tx);
                let action = rx.recv().unwrap_or(MeetingEndedAction::AutoFinalize);
                let _ = proxy_clone.send_event(UiEvent::MeetingEndedResult(action));
            });
        }
        GuiState::Detected { .. } | GuiState::Ignored { .. } | GuiState::Missed { .. } => {
            info!("detected meeting ended without recording");
            *state = GuiState::Idle;
            tray_handle_silent_rebuild(tray_handle, state, flags);
        }
        _ => {}
    }
}

fn on_meeting_ended_result(
    action: MeetingEndedAction,
    state: &mut GuiState,
    tray_handle: &mut tray::TrayHandle,
    flags: &MenuFlags,
    cmd_tx: &mpsc::UnboundedSender<Command>,
) {
    if !matches!(state, GuiState::AwaitingEndConfirm) {
        // User pressed "녹화 중지" in the tray menu before the popup
        // resolved — ignore the late callback.
        return;
    }
    let open_after = matches!(action, MeetingEndedAction::OpenFolder);
    let _ = cmd_tx.send(Command::StopRecording);
    *state = GuiState::Finalizing { open_after };
    tray::rebuild(tray_handle, state, flags);
}

fn on_toast_result(
    action: ToastAction,
    state: &mut GuiState,
    tray_handle: &mut tray::TrayHandle,
    flags: &MenuFlags,
    cmd_tx: &mpsc::UnboundedSender<Command>,
) {
    let GuiState::Detected { ctx } = state.clone() else {
        return; // user-prompt is no longer relevant
    };
    match action {
        ToastAction::Record => {
            let _ = cmd_tx.send(Command::StartRecording { ctx });
            *state = GuiState::Recording;
        }
        ToastAction::Ignore => {
            *state = GuiState::Ignored { ctx };
        }
    }
    tray::rebuild(tray_handle, state, flags);
}

fn on_toast_timeout(state: &mut GuiState, tray_handle: &mut tray::TrayHandle, flags: &MenuFlags) {
    if let GuiState::Detected { ctx } = state.clone() {
        info!("meeting prompt timed out (30s); marking as missed");
        *state = GuiState::Missed { ctx };
        tray::rebuild(tray_handle, state, flags);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_menu_click(
    id: &str,
    state: &mut GuiState,
    tray_handle: &mut tray::TrayHandle,
    flags: &mut MenuFlags,
    last_ctx: &mut Option<SessionContext>,
    cfg: &Arc<Config>,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    _proxy: &EventLoopProxy<UiEvent>,
    control_flow: &mut ControlFlow,
) {
    match id {
        tray::ID_START_RECORDING => {
            let ctx = state.ctx().cloned().or_else(|| last_ctx.clone());
            if let Some(ctx) = ctx {
                let _ = cmd_tx.send(Command::StartRecording { ctx });
                *state = GuiState::Recording;
                tray::rebuild(tray_handle, state, flags);
            } else {
                warn!("start-recording requested but no detected meeting context");
            }
        }
        tray::ID_STOP_RECORDING => {
            if matches!(state, GuiState::Recording) {
                let _ = cmd_tx.send(Command::StopRecording);
                *state = GuiState::Finalizing { open_after: false };
                tray::rebuild(tray_handle, state, flags);
            }
        }
        tray::ID_OPEN_LAST_SESSION => {
            if let Some(p) = &flags.last_session {
                open_in_explorer(p);
            }
        }
        tray::ID_CHANGE_SAVE_DIR => {
            let initial = flags
                .last_session
                .clone()
                .or_else(|| AgentState::load().last_save_dir.clone())
                .filter(|p| p.is_dir())
                .unwrap_or_else(|| cfg.output_root.clone());
            if let Ok(Some(picked)) =
                crate::dialog::pick_folder("녹화 저장 폴더 선택", Some(&initial))
            {
                AgentState::remember_save_dir(&picked);
                popup::show_info(
                    "Meeting Agent",
                    &format!("저장 위치 변경됨: {}", picked.display()),
                );
            }
        }
        tray::ID_TOGGLE_AUTO_RECORD => {
            flags.auto_record = !flags.auto_record;
            AgentState::set_auto_record(flags.auto_record);
            tray::rebuild(tray_handle, state, flags);
        }
        tray::ID_TOGGLE_AUTO_START => {
            let target = !flags.auto_start;
            match autostart::set_enabled(target) {
                Ok(()) => {
                    flags.auto_start = target;
                    tray::rebuild(tray_handle, state, flags);
                }
                Err(e) => {
                    warn!("autostart toggle failed: {e:#}");
                    popup::show_info("Meeting Agent", &format!("자동 시작 토글 실패: {e}"));
                }
            }
        }
        tray::ID_TOGGLE_EVENT_NOTIFS => {
            flags.event_notifications = !flags.event_notifications;
            popup::set_events_enabled(flags.event_notifications);
            AgentState::set_event_notifications(flags.event_notifications);
            tray::rebuild(tray_handle, state, flags);
        }
        tray::ID_OPEN_LOG => {
            if let Some(p) = crate::log_file_path() {
                open_in_explorer(&p);
            }
        }
        tray::ID_QUIT => {
            let _ = cmd_tx.send(Command::Quit);
            *control_flow = ControlFlow::Exit;
        }
        _ => {}
    }
}

fn tray_handle_silent_rebuild(
    tray_handle: &mut tray::TrayHandle,
    state: &GuiState,
    flags: &MenuFlags,
) {
    tray::rebuild(tray_handle, state, flags);
}

fn spawn_meeting_prompt(ctx: SessionContext, proxy: EventLoopProxy<UiEvent>) {
    // Run on a fresh OS thread so the toast callback's COM apartment doesn't
    // block the main thread, and so we can layer a 30s timeout on top.
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<ToastAction>();
        let title_hint = ctx.title.clone();
        popup::show_meeting_prompt(title_hint.as_deref(), tx);

        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(action) => {
                let _ = proxy.send_event(UiEvent::ToastResult(action));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = proxy.send_event(UiEvent::ToastTimeout);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = proxy.send_event(UiEvent::ToastTimeout);
            }
        }
        // Keep ctx referenced so the underlying HwndHandle lives until reply.
        drop(ctx);
    });
}

fn open_in_explorer(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

// ────────────── orchestrator (background tokio runtime) ──────────────

fn spawn_background(
    cfg: Arc<Config>,
    proxy: EventLoopProxy<UiEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    shutdown_tx: broadcast::Sender<()>,
) {
    std::thread::Builder::new()
        .name("meeting-agent-rt".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("agent-rt")
                .build()
                .expect("tokio runtime");

            rt.block_on(async move {
                // Detector runs forever; pushes Started/Stopped to UI via proxy.
                let (det_tx, mut det_rx) = mpsc::channel::<MeetingEvent>(16);
                let det_cfg = cfg.clone();
                let det_shutdown = shutdown_tx.subscribe();
                let detector = tokio::spawn(async move {
                    if let Err(e) =
                        crate::state::run_detector(det_cfg, det_tx, det_shutdown).await
                    {
                        error!("detector terminated: {e:#}");
                    }
                });
                let proxy_for_bridge = proxy.clone();
                let bridge = tokio::spawn(async move {
                    while let Some(evt) = det_rx.recv().await {
                        let _ = proxy_for_bridge.send_event(UiEvent::Detector(evt));
                    }
                });

                // Session orchestrator state.
                let mut current_session: Option<tokio::task::JoinHandle<Option<PathBuf>>> = None;
                let mut current_session_shutdown: Option<broadcast::Sender<()>> = None;

                loop {
                    tokio::select! {
                        cmd = cmd_rx.recv() => {
                            match cmd {
                                Some(Command::StartRecording { ctx }) => {
                                    if current_session.is_some() {
                                        warn!("StartRecording while session active — ignoring");
                                        continue;
                                    }
                                    let cfg2 = cfg.clone();
                                    let (sess_sd_tx, sess_sd_rx) = broadcast::channel::<()>(2);
                                    current_session_shutdown = Some(sess_sd_tx);
                                    let proxy_for_session = proxy.clone();
                                    current_session = Some(tokio::spawn(async move {
                                        match run_session(cfg2, Some(ctx), sess_sd_rx).await {
                                            Ok(dir) => {
                                                let _ = proxy_for_session.send_event(UiEvent::SessionFinalized { dir: Some(dir) });
                                                Some(PathBuf::new())
                                            }
                                            Err(e) => {
                                                error!("session failed: {e:#}");
                                                let _ = proxy_for_session.send_event(UiEvent::SessionFinalized { dir: None });
                                                None
                                            }
                                        }
                                    }));
                                }
                                Some(Command::StopRecording) => {
                                    if let Some(tx) = current_session_shutdown.take() {
                                        let _ = tx.send(());
                                    }
                                    if let Some(h) = current_session.take() {
                                        let _ = tokio::time::timeout(Duration::from_secs(60), h).await;
                                    }
                                }
                                Some(Command::Quit) => {
                                    info!("Quit received; winding down");
                                    if let Some(tx) = current_session_shutdown.take() {
                                        let _ = tx.send(());
                                    }
                                    let _ = shutdown_tx.send(());
                                    if let Some(h) = current_session.take() {
                                        let _ = tokio::time::timeout(Duration::from_secs(60), h).await;
                                    }
                                    let _ = tokio::time::timeout(Duration::from_secs(5), detector).await;
                                    let _ = bridge.await;
                                    break;
                                }
                                None => break,
                            }
                        }
                    }
                }
                info!("background runtime exited");
            });
        })
        .expect("spawn background thread");
}

// ────────────── autostart helper ──────────────

mod autostart {
    use anyhow::{Context, Result};
    use auto_launch::AutoLaunchBuilder;

    fn builder() -> Result<auto_launch::AutoLaunch> {
        let exe = std::env::current_exe().context("current_exe")?;
        let exe_str = exe.to_string_lossy().to_string();
        AutoLaunchBuilder::new()
            .set_app_name("MeetingAgent")
            .set_app_path(&exe_str)
            .build()
            .context("auto-launch builder")
    }

    pub fn is_enabled() -> bool {
        builder()
            .and_then(|b| b.is_enabled().context("is_enabled"))
            .unwrap_or(false)
    }

    pub fn set_enabled(on: bool) -> Result<()> {
        let b = builder()?;
        if on {
            b.enable().context("enable")?;
        } else {
            b.disable().context("disable")?;
        }
        Ok(())
    }
}
