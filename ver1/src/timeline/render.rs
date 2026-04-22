use super::event::TimelineEvent;
use anyhow::Result;
use std::fmt::Write;
use std::path::Path;

const EVENTS_FILE: &str = "events.jsonl";

pub fn finalise(session_dir: &Path) -> Result<()> {
    let events_path = session_dir.join(EVENTS_FILE);
    if !events_path.exists() {
        tracing::warn!("no events file to finalise at {}", events_path.display());
        return Ok(());
    }
    let raw = std::fs::read_to_string(&events_path)?;
    let mut events: Vec<TimelineEvent> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    events.sort_by_key(|e| e.t_ms());

    write_transcript_txt(session_dir, &events)
}

/// Plain-text transcript at `<session>/transcript.txt`. One line per
/// finalised utterance — `[HH:MM:SS.mmm] Speaker: text`.
fn write_transcript_txt(session_dir: &Path, events: &[TimelineEvent]) -> Result<()> {
    let mut body = String::new();
    for e in events {
        if let TimelineEvent::Caption { t_ms, speaker, text, .. } = e {
            let who = speaker.as_deref().unwrap_or("?");
            writeln!(body, "[{}] {who}: {text}", fmt_ts_full(*t_ms))?;
        }
    }
    if body.is_empty() {
        body.push_str("(no captions in this session — either live-captions were off, no one spoke, or the panel's DOM moved between windows — see agent.log)\n");
    }
    let out = session_dir.join("transcript.txt");
    std::fs::write(&out, body)?;
    tracing::info!("wrote transcript → {}", out.display());
    Ok(())
}

/// Delete everything except `meeting.mp4` and `transcript.txt`. Called after
/// a successful MP4 finalize so we ship the user a clean two-file folder.
/// On any error a sub-step the cleanup keeps going — leaving an extra
/// intermediate around is preferable to bailing halfway.
pub fn cleanup_intermediates(session_dir: &Path) {
    const KEEP: &[&str] = &["meeting.mp4", "transcript.txt"];
    let entries = match std::fs::read_dir(session_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cleanup: read_dir({}) failed: {e}", session_dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if KEEP.iter().any(|k| *k == name_str.as_ref()) {
            continue;
        }
        let path = entry.path();
        let result = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = result {
            tracing::warn!("cleanup: remove {} failed: {e}", path.display());
        }
    }
    tracing::info!("cleanup: session pared down to meeting.mp4 + transcript.txt");
}

fn fmt_ts_full(ms: u64) -> String {
    let total = ms / 1000;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    let ms_part = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{ms_part:03}")
}
