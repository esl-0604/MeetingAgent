//! Audio capture pipelines.
//!
//! Two independent streams run in parallel, each on its own OS thread:
//!
//!   * **Teams loopback** — uses `ActivateAudioInterfaceAsync` with
//!     `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` so that Windows mixes
//!     only the audio rendered by `ms-teams.exe` (and child processes) into
//!     our capture buffer. This is the official, supported way to grab
//!     per-process audio without hooking. Requires Windows 10 2004 (19041) or
//!     later. If activation fails, we fall back to default-device loopback.
//!   * **Microphone** — straightforward WASAPI capture from the default input
//!     endpoint.
//!
//! Both streams feed an `AudioMixer` that sums them and forwards the mixed
//! samples to the `Recorder` (real-time MP4 encoder). No standalone WAV
//! files are written — the MP4 is the single source of truth.

mod loopback;
mod mic;
mod mixer;

pub use mixer::AudioMixer;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use crate::config::Config;
use crate::recorder::Recorder;
use crate::timeline::TimelineEvent;

pub async fn run(
    cfg: Arc<Config>,
    teams_pid: u32,
    recorder: Option<Arc<Recorder>>,
    tx: mpsc::Sender<TimelineEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    info!("audio worker starting (pid={teams_pid})");

    let mixer: Option<Arc<AudioMixer>> =
        recorder.as_ref().map(|r| Arc::new(AudioMixer::new(r.clone())));

    let mut handles = Vec::new();

    if cfg.audio.capture_teams_loopback {
        let tx2 = tx.clone();
        let cfg2 = cfg.clone();
        let mx = mixer.clone();
        let mut sd = shutdown.resubscribe();
        handles.push(tokio::task::spawn_blocking(move || {
            if let Err(e) = loopback::run(cfg2, teams_pid, mx, tx2, &mut sd) {
                warn!("loopback capture stopped: {e:#}");
            }
        }));
    }

    if cfg.audio.capture_microphone {
        let tx2 = tx.clone();
        let mx = mixer.clone();
        let mut sd = shutdown.resubscribe();
        handles.push(tokio::task::spawn_blocking(move || {
            if let Err(e) = mic::run(mx, tx2, &mut sd) {
                warn!("microphone capture stopped: {e:#}");
            }
        }));
    }

    let _ = shutdown.recv().await;
    for h in handles {
        let _ = h.await;
    }
    info!("audio worker stopped");
    Ok(())
}

/// Channel-side check used by capture loops to abort promptly on shutdown.
pub(crate) fn shutdown_pending(rx: &mut broadcast::Receiver<()>) -> bool {
    matches!(
        rx.try_recv(),
        Ok(()) | Err(broadcast::error::TryRecvError::Closed)
    )
}
