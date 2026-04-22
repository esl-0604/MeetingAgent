//! Continuous screen capture of the Teams meeting window via
//! Windows.Graphics.Capture, fed into an `IMFSinkWriter`-backed `Recorder`
//! for real-time H.264 encoding into `meeting.mp4`.
//!
//! Earlier revisions saved per-keyframe PNGs into `slides/`; that path has
//! been retired now that the MP4 holds the full video. Share-state detection
//! is kept so we can emit `share.start`/`share.stop` timeline events (useful
//! in the transcript) but no longer gates capture — we record the whole
//! meeting from start to end.

mod phash;
mod share_target;
mod wgc;

pub use share_target::diagnose as diagnose_share;

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::recorder::Recorder;
use crate::state::HwndHandle;
use crate::timeline::{event::now_event_stamps, TimelineEvent};
use share_target::ShareTarget;

pub async fn run(
    cfg: Arc<Config>,
    hwnd: HwndHandle,
    teams_pid: u32,
    recorder: Option<Arc<Recorder>>,
    tx: mpsc::Sender<TimelineEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    info!("screen worker starting (interval {}ms)", cfg.screen.min_frame_interval_ms);

    let cfg2 = cfg.clone();
    let tx2 = tx.clone();
    let rec2 = recorder.clone();
    let mut sd = shutdown.resubscribe();
    let h = tokio::task::spawn_blocking(move || {
        if let Err(e) = run_blocking(cfg2, hwnd, teams_pid, rec2, tx2, &mut sd) {
            warn!("screen capture stopped: {e:#}");
        }
    });

    let _ = shutdown.recv().await;
    let _ = h.await;
    info!("screen worker stopped");
    Ok(())
}

/// Currently-active capture source. `Teams` is the default (record the
/// meeting view); `Shared(target)` is what we switch to during self-share,
/// where `target` is whichever HWND/HMONITOR we inferred from the
/// foreground at share-start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Teams,
    Shared(ShareTarget),
}

fn open_session(source: Source, teams_hwnd: isize) -> Result<wgc::CaptureSession> {
    match source {
        Source::Teams => wgc::CaptureSession::create_for_hwnd(teams_hwnd),
        Source::Shared(ShareTarget::Window(h)) => wgc::CaptureSession::create_for_hwnd(h),
        Source::Shared(ShareTarget::Monitor(m)) => wgc::CaptureSession::create_for_monitor(m),
    }
}

fn run_blocking(
    _cfg: Arc<Config>,
    hwnd: HwndHandle,
    teams_pid: u32,
    recorder: Option<Arc<Recorder>>,
    tx: mpsc::Sender<TimelineEvent>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    crate::uia::com_init_thread();

    let mut source = Source::Teams;
    let mut session = match open_session(source, hwnd.0) {
        Ok(s) => s,
        Err(e) => {
            warn!("WGC session for hwnd {:#x} failed: {e:#}", hwnd.0);
            return Err(e);
        }
    };

    let target_size = recorder.as_ref().map(|r| r.video_size());
    let auto = crate::uia::create_automation()?;
    let mut last_share_state: Option<crate::state::ShareState> = None;

    // Aim for ~10 fps video (the recorder's declared cadence). The actual
    // WGC TryGetNextFrame pace is driven by Windows' compositor, so we
    // poll ~15 fps and drop anything we can't keep up with.
    let poll_interval = Duration::from_millis(66);
    // Share-state detection walks every Teams UIA tree — too expensive to
    // run every frame. Cache the last result for ~1 s; that's plenty
    // responsive for source switching but lets the worker exit promptly
    // on shutdown (was the dominant Ctrl-C latency).
    const SHARE_POLL_EVERY_TICKS: u32 = 15;
    let mut tick: u32 = 0;
    let mut now_share = crate::state::detect_active_share_global(&auto);

    loop {
        if super::audio::shutdown_pending(shutdown) {
            break;
        }
        std::thread::sleep(poll_interval);
        tick = tick.wrapping_add(1);
        if tick % SHARE_POLL_EVERY_TICKS == 0 {
            now_share = crate::state::detect_active_share_global(&auto);
        }

        match (&last_share_state, &now_share) {
            (None, Some(new_s)) => {
                let (t, w) = now_event_stamps();
                let _ = tx.blocking_send(TimelineEvent::ShareStart {
                    t_ms: t,
                    wall: w,
                    presenter: new_s.presenter.clone(),
                });
            }
            (Some(_), None) => {
                let (t, w) = now_event_stamps();
                let _ = tx.blocking_send(TimelineEvent::ShareStop {
                    t_ms: t,
                    wall: w,
                    presenter: last_share_state.as_ref().and_then(|s| s.presenter.clone()),
                });
            }
            _ => {}
        }
        last_share_state = now_share.clone();

        // Pick the right source for the current state. While *we* are
        // sharing, switch to the inferred shared target (the user's window
        // or monitor); otherwise capture the Teams window.
        let want = match now_share.as_ref().and_then(|s| s.presenter.as_deref()) {
            Some("(me)") => match source {
                // Already on a Shared target — keep it (don't re-snapshot
                // foreground every tick; that would cause WGC churn).
                Source::Shared(t) => Source::Shared(t),
                Source::Teams => Source::Shared(share_target::pick(teams_pid)),
            },
            _ => Source::Teams,
        };

        if want != source {
            session.stop();
            match open_session(want, hwnd.0) {
                Ok(s) => {
                    info!("video source switched: {:?} → {:?}", source, want);
                    session = s;
                    source = want;
                    let (t, w) = now_event_stamps();
                    let _ = tx.blocking_send(TimelineEvent::Note {
                        t_ms: t,
                        wall: w,
                        level: "info".into(),
                        msg: format!("video source: {:?}", want),
                    });
                }
                Err(e) => {
                    warn!("failed to switch video source to {:?}: {e:#} (reverting)", want);
                    // Reopen previous source so frames keep flowing.
                    if let Ok(s) = open_session(source, hwnd.0) {
                        session = s;
                    }
                }
            }
        }

        let frame = match session.next_frame() {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => {
                debug!("next_frame error: {e}");
                continue;
            }
        };

        if let (Some(rec), Some((tw, th))) = (recorder.as_ref(), target_size) {
            let scaled = if frame.width == tw && frame.height == th {
                frame.bgra
            } else {
                scale_bgra_nearest(&frame.bgra, frame.width, frame.height, tw, th)
            };
            let ts = rec.now_100ns();
            if let Err(e) = rec.write_video_bgra(&scaled, ts) {
                // Non-fatal: log and keep running. Finalise will close whatever we got.
                debug!("write_video_bgra failed: {e:#}");
            }
        }
    }

    session.stop();
    Ok(())
}

/// Nearest-neighbour BGRA scaler. Good enough for meeting-content video
/// (slides, text, participant tiles) where interpolation would smear edges.
fn scale_bgra_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    if src_w == 0 || src_h == 0 {
        return dst;
    }
    for dy in 0..dst_h {
        let sy = ((dy as u64) * (src_h as u64) / (dst_h as u64)) as u32;
        let sy = sy.min(src_h - 1);
        let src_row = (sy as usize) * (src_w as usize) * 4;
        let dst_row = (dy as usize) * (dst_w as usize) * 4;
        for dx in 0..dst_w {
            let sx = ((dx as u64) * (src_w as u64) / (dst_w as u64)) as u32;
            let sx = sx.min(src_w - 1);
            let si = src_row + (sx as usize) * 4;
            let di = dst_row + (dx as usize) * 4;
            dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    dst
}

#[allow(dead_code)]
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .take(40)
        .collect()
}
