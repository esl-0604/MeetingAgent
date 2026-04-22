//! Timeline event aggregation.
//!
//! Every capture worker pushes typed `TimelineEvent`s onto a channel; a single
//! writer task serialises them in arrival order to `events.jsonl`. After a
//! session ends, [`finalise`] reads that JSONL back and produces a
//! human-readable Markdown summary that interleaves captions and slides.

pub mod event;
mod render;

pub use event::TimelineEvent;
pub use render::{cleanup_intermediates, finalise};

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::{debug, info};

const EVENTS_FILE: &str = "events.jsonl";

pub async fn run_writer(session_dir: PathBuf, mut rx: mpsc::Receiver<TimelineEvent>) -> Result<()> {
    let path = session_dir.join(EVENTS_FILE);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    info!("timeline writer started → {}", path.display());

    while let Some(evt) = rx.recv().await {
        match serde_json::to_string(&evt) {
            Ok(line) => {
                if let Err(e) = file.write_all(line.as_bytes()).await {
                    debug!("write line: {e}");
                }
                let _ = file.write_all(b"\n").await;
            }
            Err(e) => debug!("serialise event: {e}"),
        }
    }
    let _ = file.flush().await;
    info!("timeline writer drained");
    Ok(())
}
