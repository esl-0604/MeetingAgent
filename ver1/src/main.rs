//! Meeting Agent — L2 capture for Microsoft Teams (New Teams / Windows 11).
//!
//! Runs alongside Teams, detects meeting start/stop, and in parallel captures:
//!   - live captions via UI Automation on the WebView2 DOM
//!   - audio via WASAPI process-loopback on ms-teams.exe + microphone
//!   - shared-screen frames via Windows.Graphics.Capture into a real-time MP4
//!
//! Default startup is **GUI mode**: a tray icon + toast prompts. The console
//! window is suppressed in release builds via `windows_subsystem = "windows"`.
//! Pass `--console` for the legacy stdout-only behaviour (handy for debugging
//! and headless / unattended runs).

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod audio;
mod clock;
mod config;
mod dialog;
mod gui;
mod output;
mod persisted_state;
mod recorder;
mod screen;
mod state;
mod timeline;
mod uia;

use anyhow::{Context, Result};
use clap::Parser;
use once_cell::sync::OnceCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Console::{
    AttachConsole, AllocConsole, SetConsoleCtrlHandler, ATTACH_PARENT_PROCESS, CTRL_BREAK_EVENT,
    CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::System::Threading::CreateMutexW;

use crate::state::MeetingEvent;
use crate::timeline::TimelineEvent;

/// Shared shutdown signal so the console-control handler thread can ask the
/// tokio runtime on the main thread to wind down (console mode only).
static SHUTDOWN_TX: OnceCell<broadcast::Sender<()>> = OnceCell::new();

/// Path to the live log file. Truncated at process startup; copied into each
/// session folder as `agent.log` after the session finalises.
static LOG_FILE_PATH: OnceCell<std::path::PathBuf> = OnceCell::new();

/// Held alive for the lifetime of the process so the non-blocking file
/// writer keeps flushing.
static LOG_FILE_GUARD: OnceCell<tracing_appender::non_blocking::WorkerGuard> = OnceCell::new();

/// Public accessor for the GUI's "Open log" menu item.
pub fn log_file_path() -> Option<std::path::PathBuf> {
    LOG_FILE_PATH.get().cloned()
}

#[derive(Parser, Debug)]
#[command(version, about = "Teams meeting capture agent (L2: WASAPI / WGC / UIA)")]
struct Cli {
    /// Run in legacy console mode (no tray, no toasts, stdout logging).
    #[arg(long)]
    console: bool,

    /// Console mode: start capturing immediately without waiting for Teams
    /// meeting detection. (Implies --console.)
    #[arg(long)]
    force_start: bool,

    /// Override session output root directory.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Log verbosity filter (e.g. "info", "meeting_agent=debug,warn").
    #[arg(long, default_value = "info")]
    log: String,

    /// Diagnostic mode: enumerate Teams windows, dump UIA tree, list
    /// candidate Leave/나가기 elements. Implies --console.
    #[arg(long)]
    diagnose: bool,

    /// Diagnostic: locate the caption container and dump its full subtree.
    #[arg(long)]
    diagnose_captions: bool,

    /// Diagnostic: dump UIA hits for screen-share targets. Run while sharing.
    #[arg(long)]
    diagnose_share: bool,

    /// Limit UIA tree-dump depth in --diagnose / --diagnose-captions mode.
    #[arg(long, default_value_t = 6)]
    diagnose_depth: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Anything that needs a console implies --console; surface it cleanly.
    let needs_console =
        cli.console || cli.force_start || cli.diagnose || cli.diagnose_captions || cli.diagnose_share;
    if needs_console {
        attach_console();
    }

    // Block a second simultaneous launch. Diagnose / force-start CLI runs
    // are exempt — those are short-lived and shouldn't fight a long-running
    // tray instance for the global mutex.
    if !cli.diagnose && !cli.diagnose_captions && !cli.diagnose_share && !cli.force_start {
        if already_running() {
            // For console mode, just print + exit so we don't bring up a
            // popup that could race against the tray instance's UI.
            if needs_console {
                eprintln!("Meeting Agent is already running.");
            } else {
                gui::popup::show_info_blocking(
                    "이미 실행 중입니다",
                    "Meeting Agent이 이미 실행 중입니다.\n\
                     트레이(시계 옆)에서 마스코트 아이콘을 확인해주세요.",
                );
            }
            return Ok(());
        }
    }

    init_tracing(&cli.log, needs_console)?;
    clock::init();

    if cli.diagnose {
        return state::diagnose(cli.diagnose_depth);
    }
    if cli.diagnose_captions {
        return uia::captions::diagnose(cli.diagnose_depth);
    }
    if cli.diagnose_share {
        return screen::diagnose_share(cli.diagnose_depth);
    }

    if needs_console {
        run_console(cli)
    } else {
        run_gui(cli)
    }
}

// ────────────── GUI mode (default) ──────────────

fn run_gui(cli: Cli) -> Result<()> {
    info!("meeting-agent starting in GUI mode");
    let cfg = build_config(cli.output.clone())?;
    gui::run(cfg);
    // gui::run blocks indefinitely; if it returns we treat it as a clean exit.
    #[allow(unreachable_code)]
    Ok(())
}

// ────────────── Console mode (legacy / debug) ──────────────

fn run_console(cli: Cli) -> Result<()> {
    info!("meeting-agent starting in console mode");

    unsafe {
        if let Err(e) = SetConsoleCtrlHandler(Some(console_ctrl_handler), true) {
            warn!("SetConsoleCtrlHandler failed: {e}");
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("agent-rt")
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(async_main_console(cli))
}

extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            if let Some(tx) = SHUTDOWN_TX.get() {
                let _ = tx.send(());
            }
            let deadline = std::time::Instant::now() + Duration::from_millis(4500);
            while std::time::Instant::now() < deadline {
                if SHUTDOWN_TX.get().is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            true.into()
        }
        CTRL_C_EVENT | CTRL_BREAK_EVENT => false.into(),
        _ => false.into(),
    }
}

async fn async_main_console(cli: Cli) -> Result<()> {
    let cfg = build_config(cli.output.clone())?;

    let (meeting_tx, mut meeting_rx) = mpsc::channel::<MeetingEvent>(16);
    let (shutdown_tx, _shutdown_rx) = broadcast::channel::<()>(4);
    let _ = SHUTDOWN_TX.set(shutdown_tx.clone());

    let detect_cfg = cfg.clone();
    let detect_shutdown = shutdown_tx.subscribe();
    let detect_handle = tokio::spawn(async move {
        if let Err(e) = state::run_detector(detect_cfg, meeting_tx, detect_shutdown).await {
            error!("detector terminated: {e:#}");
        }
    });

    let ctrl_c_shutdown = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Ctrl-C received, shutting down");
            let _ = ctrl_c_shutdown.send(());
        }
    });

    let mut current_session: Option<tokio::task::JoinHandle<()>> = None;

    if cli.force_start {
        let cfg2 = cfg.clone();
        let sh = shutdown_tx.subscribe();
        current_session = Some(tokio::spawn(async move {
            match run_session(cfg2, None, sh).await {
                Ok(_) => {}
                Err(e) => error!("forced session failed: {e:#}"),
            }
        }));
    }

    loop {
        tokio::select! {
            maybe_evt = meeting_rx.recv() => {
                match maybe_evt {
                    Some(MeetingEvent::Started { teams_hwnd, teams_pid, title }) => {
                        if current_session.is_some() {
                            warn!("meeting start while previous session still running — ignoring");
                            continue;
                        }
                        info!("meeting start detected: pid={teams_pid} hwnd={teams_hwnd:?} title={title:?}");
                        let ctx = SessionContext { teams_hwnd, teams_pid, title };
                        let cfg2 = cfg.clone();
                        let sh = shutdown_tx.subscribe();
                        current_session = Some(tokio::spawn(async move {
                            if let Err(e) = run_session(cfg2, Some(ctx), sh).await {
                                error!("session failed: {e:#}");
                            }
                        }));
                    }
                    Some(MeetingEvent::Stopped) => {
                        if let Some(h) = current_session.take() {
                            info!("meeting stop signalled; awaiting session drain");
                            let _ = h.await;
                            info!("session finalised");
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = shutdown_tx.send(());
                break;
            }
        }
        if shutdown_tx.receiver_count() == 0 {
            break;
        }
    }

    let _ = shutdown_tx.send(());
    if let Some(h) = current_session.take() {
        let _ = tokio::time::timeout(Duration::from_secs(600), h).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), detect_handle).await;
    info!("meeting-agent exited");
    Ok(())
}

// ────────────── Common bits used by both modes ──────────────

fn build_config(output_override: Option<PathBuf>) -> Result<Arc<config::Config>> {
    let mut cfg = config::Config::load_or_default();
    let agent_state = persisted_state::AgentState::load();

    // Effective output_root selection (highest priority first):
    //   1. CLI --output
    //   2. state.json's last_save_dir (if it still exists)
    //   3. config.json's output_root
    if let Some(o) = output_override {
        cfg.output_root = o;
    } else if let Some(last) = agent_state.last_save_dir.as_ref() {
        if last.is_dir() {
            info!("using last-picked save dir as output root: {}", last.display());
            cfg.output_root = last.clone();
        }
    }

    std::fs::create_dir_all(&cfg.output_root)
        .with_context(|| format!("create output root {:?}", cfg.output_root))?;
    info!("output root: {}", cfg.output_root.display());
    Ok(Arc::new(cfg))
}

#[derive(Clone, Debug)]
pub struct SessionContext {
    pub teams_hwnd: state::HwndHandle,
    pub teams_pid: u32,
    pub title: Option<String>,
}

/// Run a single capture session end-to-end. Returns the session directory on
/// success so callers (GUI) can show "Open last session" / "Recording done"
/// notifications.
pub async fn run_session(
    cfg: Arc<config::Config>,
    ctx: Option<SessionContext>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<PathBuf> {
    // Pull the latest user-picked folder from state.json each session — this
    // way the tray-menu "저장 위치 변경…" change takes effect immediately for
    // the very next recording, without having to restart the agent. Falls
    // back to cfg.output_root if state.json is empty or the path is gone.
    let session_parent = persisted_state::AgentState::load()
        .last_save_dir
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| cfg.output_root.clone());
    let session = output::SessionDir::create(&session_parent)?;
    info!("session dir: {}", session.root.display());
    let session_root = session.root.clone();

    let (tl_tx, tl_rx) = mpsc::channel::<TimelineEvent>(1024);

    let tl_root = session.root.clone();
    let tl_handle = tokio::spawn(async move {
        if let Err(e) = timeline::run_writer(tl_root, tl_rx).await {
            error!("timeline writer failed: {e:#}");
        }
    });

    let recorder: Option<Arc<recorder::Recorder>> = if cfg.screen.enabled && ctx.is_some() {
        let mp4_path = session.root.join("meeting.mp4");
        match recorder::Recorder::create(&mp4_path, 1920, 1080) {
            Ok(r) => {
                info!("recorder initialised → {}", r.path().display());
                Some(Arc::new(r))
            }
            Err(e) => {
                warn!("recorder init failed: {e:#} (continuing without MP4)");
                None
            }
        }
    } else {
        None
    };

    let mut workers: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    {
        let cfg = cfg.clone();
        let tx = tl_tx.clone();
        let pid = ctx.as_ref().map(|c| c.teams_pid).unwrap_or(0);
        let rec = recorder.clone();
        let sh = shutdown.resubscribe();
        workers.push(tokio::spawn(async move {
            if let Err(e) = audio::run(cfg, pid, rec, tx, sh).await {
                error!("audio worker failed: {e:#}");
            }
        }));
    }

    if cfg.screen.enabled {
        if let Some(ctx) = ctx.as_ref() {
            let cfg = cfg.clone();
            let tx = tl_tx.clone();
            let hwnd = ctx.teams_hwnd.clone();
            let pid = ctx.teams_pid;
            let rec = recorder.clone();
            let sh = shutdown.resubscribe();
            workers.push(tokio::spawn(async move {
                if let Err(e) = screen::run(cfg, hwnd, pid, rec, tx, sh).await {
                    error!("screen worker failed: {e:#}");
                }
            }));
        } else {
            warn!("no Teams hwnd → screen capture disabled for this session");
        }
    }

    if cfg.caption.enabled {
        let cfg = cfg.clone();
        let tx = tl_tx.clone();
        let hwnd = ctx.as_ref().map(|c| c.teams_hwnd.clone());
        let sh = shutdown.resubscribe();
        workers.push(tokio::spawn(async move {
            if let Err(e) = uia::captions::run(cfg, hwnd, tx, sh).await {
                error!("caption worker failed: {e:#}");
            }
        }));
    }

    drop(tl_tx);

    let _ = shutdown.recv().await;
    info!("session shutdown requested; stopping workers");

    let drain_workers = async {
        let mut set = tokio::task::JoinSet::new();
        for w in workers {
            set.spawn(async move {
                let _ = w.await;
            });
        }
        while set.join_next().await.is_some() {}
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), drain_workers).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), tl_handle).await;

    let mp4_ok = match recorder.as_ref() {
        Some(rec) => match rec.finalize() {
            Ok(()) => {
                info!("recorder closed (MP4 written)");
                true
            }
            Err(e) => {
                warn!("recorder.finalize: {e:#}");
                false
            }
        },
        None => false,
    };
    drop(recorder);

    timeline::finalise(&session.root)?;
    info!("session finalised: {}", session.root.display());

    if mp4_ok {
        timeline::cleanup_intermediates(&session.root);
    } else {
        warn!("MP4 finalize failed; keeping events.jsonl as fallback for diagnosis");
    }

    snapshot_log_into_session(&session.root);

    info!("session done: {}", session.root.display());
    Ok(session_root)
}

/// Process-wide single-instance check via a named mutex. Subsequent launches
/// see the existing mutex and return `true` so we can show a "이미 실행
/// 중입니다" notice and exit. The first instance leaks the mutex handle so
/// the OS keeps the named object alive for the lifetime of the process.
fn already_running() -> bool {
    let name: HSTRING = "Local\\MeetingAgent_SingleInstance_v1".into();
    let result = unsafe { CreateMutexW(None, false, &name) };
    match result {
        Ok(handle) => {
            let last_err = unsafe { GetLastError() };
            if last_err == ERROR_ALREADY_EXISTS {
                let _ = unsafe { CloseHandle(handle) };
                true
            } else {
                // HANDLE is a Copy wrapper around a raw pointer, so just
                // discarding it doesn't call CloseHandle. The mutex stays
                // alive for the lifetime of the process — exactly what
                // single-instance enforcement needs.
                let _ = handle;
                false
            }
        }
        // CreateMutexW failed — assume no conflict and let startup proceed.
        Err(_) => false,
    }
}

fn attach_console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            // No parent console (double-clicked .exe with --console somehow):
            // give the user a fresh window.
            let _ = AllocConsole();
        }
    }
}

fn init_tracing(filter: &str, console_attached: bool) -> Result<()> {
    let env = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));

    let log_dir = std::env::temp_dir();
    let log_name = "meeting-agent.log";
    let log_path = log_dir.join(log_name);
    let _ = std::fs::remove_file(&log_path);
    let file_appender = tracing_appender::rolling::never(&log_dir, log_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let _ = LOG_FILE_PATH.set(log_path);
    let _ = LOG_FILE_GUARD.set(guard);

    let file_layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_writer(non_blocking);

    let registry = tracing_subscriber::registry().with(env).with(file_layer);

    if console_attached {
        // Add a stdout layer only when we actually have a console to write to.
        // In GUI mode there's no console, so writing to stdout is a no-op.
        let console_layer = fmt::layer().with_target(true).with_level(true);
        registry
            .with(console_layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!("tracing init: {e}"))?;
    } else {
        registry
            .try_init()
            .map_err(|e| anyhow::anyhow!("tracing init: {e}"))?;
    }
    Ok(())
}

fn snapshot_log_into_session(session_dir: &std::path::Path) {
    let Some(src) = LOG_FILE_PATH.get() else {
        return;
    };
    if !src.exists() {
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    let dst = session_dir.join("agent.log");
    if let Err(e) = std::fs::copy(src, &dst) {
        warn!("failed to snapshot log to {}: {e}", dst.display());
    } else {
        info!("wrote agent.log → {}", dst.display());
    }
}
