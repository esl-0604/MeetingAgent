//! Meeting Agent — L2 capture for Microsoft Teams (New Teams / Windows 11).
//!
//! Runs alongside Teams, detects meeting start/stop, and in parallel captures:
//!   - live captions via UI Automation on the WebView2 DOM
//!   - audio via WASAPI process-loopback on ms-teams.exe + microphone
//!   - shared-screen keyframes via Windows.Graphics.Capture + perceptual-hash dedup
//!
//! The orchestrator ties all three streams to a single QPC-based timeline and
//! renders a session folder (audio/, slides/, transcript/, timeline.json).

mod audio;
mod clock;
mod config;
mod dialog;
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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use windows::Win32::Foundation::BOOL;
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};

use crate::state::MeetingEvent;
use crate::timeline::TimelineEvent;

/// Shared shutdown signal so the console-control handler thread can ask the
/// tokio runtime on the main thread to wind down.
static SHUTDOWN_TX: OnceCell<broadcast::Sender<()>> = OnceCell::new();

#[derive(Parser, Debug)]
#[command(version, about = "Teams meeting capture agent (L2: WASAPI / WGC / UIA)")]
struct Cli {
    /// Start capturing immediately without waiting for Teams meeting detection
    #[arg(long)]
    force_start: bool,

    /// Override session output root directory
    #[arg(long)]
    output: Option<std::path::PathBuf>,

    /// Log verbosity filter (e.g. "info", "meeting_agent=debug,warn")
    #[arg(long, default_value = "info")]
    log: String,

    /// Diagnostic mode: enumerate Teams windows, dump UIA tree, list candidate
    /// "Leave"/"나가기" elements. Use this when the agent fails to detect a
    /// meeting that is clearly running.
    #[arg(long)]
    diagnose: bool,

    /// Diagnostic mode: locate the caption container and dump its full subtree
    /// so we can see the per-row structure. Use this when captions are being
    /// extracted but speaker / text are mangled.
    #[arg(long)]
    diagnose_captions: bool,

    /// Diagnostic mode: walk every Teams window's UIA tree and print elements
    /// whose name hints at the active screen-share target ("You're sharing X",
    /// "공유 중", etc.). Run this WHILE you are sharing — it tells us what
    /// Teams exposes so we can replace the foreground-window heuristic with a
    /// direct read.
    #[arg(long)]
    diagnose_share: bool,

    /// Limit UIA tree-dump depth in --diagnose / --diagnose-captions mode.
    #[arg(long, default_value_t = 6)]
    diagnose_depth: u32,

    /// At session end, open a folder-picker so the session directory can be
    /// moved to a chosen archive location. Overrides `prompt_save_on_exit`
    /// from config.json.
    #[arg(long)]
    prompt_save: bool,

    /// Disable the save-prompt for this run (overrides config).
    #[arg(long, conflicts_with = "prompt_save")]
    no_prompt_save: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(&cli.log)?;
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

    info!("meeting-agent starting (Windows / Teams L2 capture)");

    // Catch console-window close (X button), user logoff and system shutdown
    // so we can finalise WAVs and render summary.md before Windows kills us.
    // CTRL_C_EVENT is left to tokio::signal::ctrl_c() in async_main.
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

    rt.block_on(async_main(cli))
}

/// Runs on a Windows-managed thread. Returns TRUE for events we handle so
/// Windows gives us the cleanup grace period (~5s). For CTRL_C_EVENT we
/// return FALSE so the default tokio handler can fire (which we treat as a
/// "normal" shutdown and DOES show the save dialog).
extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            if let Some(tx) = SHUTDOWN_TX.get() {
                let _ = tx.send(());
            }
            // Spin briefly so Windows doesn't kill us before the runtime
            // observes the shutdown signal. The runtime will exit on its own.
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

async fn async_main(cli: Cli) -> Result<()> {
    let mut cfg = config::Config::load_or_default();
    let agent_state = persisted_state::AgentState::load();

    // Effective output_root selection (highest priority first):
    //   1. CLI --output
    //   2. state.json's last_save_dir (if it still exists)
    //   3. config.json's output_root
    if let Some(o) = cli.output {
        cfg.output_root = o;
    } else if let Some(last) = agent_state.last_save_dir.as_ref() {
        if last.is_dir() {
            info!(
                "using last-picked save dir as output root: {}",
                last.display()
            );
            cfg.output_root = last.clone();
        }
    }

    if cli.prompt_save {
        cfg.prompt_save_on_exit = true;
    }
    if cli.no_prompt_save {
        cfg.prompt_save_on_exit = false;
    }
    std::fs::create_dir_all(&cfg.output_root)
        .with_context(|| format!("create output root {:?}", cfg.output_root))?;
    let cfg = Arc::new(cfg);
    info!("output root: {}", cfg.output_root.display());

    let (meeting_tx, mut meeting_rx) = mpsc::channel::<MeetingEvent>(16);
    let (shutdown_tx, _shutdown_rx) = broadcast::channel::<()>(4);

    // Publish the shutdown sender so the console-control handler (running on
    // a separate Windows-managed thread) can ask for graceful shutdown.
    let _ = SHUTDOWN_TX.set(shutdown_tx.clone());

    // Meeting state detector (UIA probe + optional log tail) runs for the whole lifetime.
    let detect_cfg = cfg.clone();
    let detect_shutdown = shutdown_tx.subscribe();
    let detect_handle = tokio::spawn(async move {
        if let Err(e) = state::run_detector(detect_cfg, meeting_tx, detect_shutdown).await {
            error!("detector terminated: {e:#}");
        }
    });

    // Handle Ctrl-C -> graceful shutdown
    let ctrl_c_shutdown = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Ctrl-C received, shutting down");
            let _ = ctrl_c_shutdown.send(());
        }
    });

    // Main loop: Start -> spawn session; Stop -> let session drain.
    let mut current_session: Option<tokio::task::JoinHandle<()>> = None;

    // If force_start, register the synthesised session so a real meeting
    // detection won't try to spawn a second concurrent session against the
    // same audio device.
    if cli.force_start {
        let cfg2 = cfg.clone();
        let sh = shutdown_tx.subscribe();
        current_session = Some(tokio::spawn(async move {
            if let Err(e) = run_session(cfg2, None, sh).await {
                error!("forced session failed: {e:#}");
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
        // No tight timeout here: when the save-prompt is on, the session task
        // is waiting for the user to pick a folder via the modal dialog and
        // we mustn't yank it out from under them. 10 minutes is enough rope
        // for distracted clicking but still bounds a truly hung worker.
        let _ = tokio::time::timeout(Duration::from_secs(600), h).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), detect_handle).await;
    info!("meeting-agent exited");
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SessionContext {
    pub teams_hwnd: state::HwndHandle,
    pub teams_pid: u32,
    pub title: Option<String>,
}

async fn run_session(
    cfg: Arc<config::Config>,
    ctx: Option<SessionContext>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    // Pick the save destination *before* any capture starts, so the MP4 is
    // written directly to its final home — no end-of-meeting move (which
    // would otherwise mean copying GB-scale files across drives while the
    // user waits to see the dialog close).
    let session_parent = pick_session_parent(&cfg).await;
    let session = output::SessionDir::create(&session_parent)?;
    info!("session dir: {}", session.root.display());

    let (tl_tx, tl_rx) = mpsc::channel::<TimelineEvent>(1024);

    // Timeline writer task — consumes events from every producer.
    let tl_root = session.root.clone();
    let tl_handle = tokio::spawn(async move {
        if let Err(e) = timeline::run_writer(tl_root, tl_rx).await {
            error!("timeline writer failed: {e:#}");
        }
    });

    // Real-time MP4 recorder (video: 1920x1080 @ 10fps H.264 6Mbps, audio:
    // 48kHz stereo AAC). Only created when we have a Teams hwnd so the
    // video source is meaningful.
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

    // Workers
    let mut workers: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Audio — runs regardless of whether we have a Teams hwnd (microphone alone is useful).
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

    // Screen capture → real-time MP4 video track.
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
            warn!("no Teams hwnd available → screen capture disabled for this session");
        }
    }

    // Captions — UIA. Needs the Teams root element which we re-discover from hwnd or globally.
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

    // Drop our copy of the timeline sender so the writer can finish when all producers are gone.
    drop(tl_tx);

    // Wait for shutdown or worker completion.
    let _ = shutdown.recv().await;
    info!("session shutdown requested; stopping workers");

    // Drain all workers concurrently with a single total budget. Per-worker
    // sequential timeouts compounded — a slow worker blocked the next from
    // even being polled — so a 1-min meeting could spend several seconds
    // waiting after Ctrl-C even though each worker only needed ~500 ms.
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

    // Close the MP4 before rendering text summaries. `finalize()` works on
    // any Arc clone — it takes the SinkWriter out of the shared inner and
    // calls Finalize() — so we don't need sole ownership of the Arc. Any
    // worker thread that hasn't observed shutdown yet will see the writer
    // as None and silently skip its WriteSample call.
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

    // Phase 3: pare the session down to the two user-facing files
    // (meeting.mp4 + transcript.txt). Skip cleanup if MP4 finalize failed —
    // we'd rather leave events.jsonl behind for diagnosis than silently
    // delete the only debugging breadcrumb.
    if mp4_ok {
        timeline::cleanup_intermediates(&session.root);
    } else {
        warn!("MP4 finalize failed; keeping events.jsonl as fallback for diagnosis");
    }

    info!("session done: {}", session.root.display());
    Ok(())
}

/// Show the folder picker (if enabled) and return the directory in which
/// the new session folder will be created. The MP4 is written *directly*
/// into this location — no end-of-meeting move — so for a 1-hour meeting
/// we save 30+ seconds of waiting after Ctrl-C.
///
/// Pre-selects the last-picked folder so a user who always saves to the
/// same place can just hit Enter. Cancel → fall back to `cfg.output_root`.
async fn pick_session_parent(cfg: &Arc<config::Config>) -> std::path::PathBuf {
    if !cfg.prompt_save_on_exit {
        return cfg.output_root.clone();
    }
    let initial = persisted_state::AgentState::load()
        .last_save_dir
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| cfg.output_root.clone());
    let initial_for_thread = initial.clone();
    let picked = tokio::task::spawn_blocking(move || {
        dialog::pick_folder("Save meeting to…", Some(&initial_for_thread))
    })
    .await;
    match picked {
        Ok(Ok(Some(p))) => {
            persisted_state::AgentState::remember_save_dir(&p);
            info!("session will be saved under: {}", p.display());
            p
        }
        Ok(Ok(None)) => {
            info!(
                "folder dialog cancelled — using default ({})",
                cfg.output_root.display()
            );
            cfg.output_root.clone()
        }
        Ok(Err(e)) => {
            warn!("folder dialog failed: {e:#} — using default");
            cfg.output_root.clone()
        }
        Err(e) => {
            warn!("folder dialog task panicked: {e} — using default");
            cfg.output_root.clone()
        }
    }
}

fn init_tracing(filter: &str) -> Result<()> {
    let env = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(env)
        .with(fmt::layer().with_target(true).with_level(true))
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing init: {e}"))?;
    Ok(())
}
