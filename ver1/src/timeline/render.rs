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
///
/// Teams streams partial caption updates as separate UIA rows: a single
/// utterance produces a chain like `"Yes so"`, `"Yes so the"`, ...,
/// `"Yes, so the BUL 1.0 ..."`, each appearing as a Caption event in the
/// timeline. Without collapsing, transcript.txt looks like 18 lines of the
/// same sentence growing word-by-word. We collapse here (post-capture) by
/// merging consecutive same-speaker captions when one is a normalized
/// prefix of the other — keeping the longest (= the polished, finalised
/// version) and the original start timestamp.
fn write_transcript_txt(session_dir: &Path, events: &[TimelineEvent]) -> Result<()> {
    let collapsed = collapse_caption_partials(events);
    let mut body = String::new();
    for c in &collapsed {
        let who = c.speaker.as_deref().unwrap_or("?");
        writeln!(body, "[{}] {who}: {}", fmt_ts_full(c.t_ms), c.text)?;
    }
    if body.is_empty() {
        body.push_str("(no captions in this session — either live-captions were off, no one spoke, or the panel's DOM moved between windows — see agent.log)\n");
    }
    let out = session_dir.join("transcript.txt");
    std::fs::write(&out, body)?;
    tracing::info!(
        "wrote transcript → {} ({} lines after collapsing partials)",
        out.display(),
        collapsed.len()
    );
    Ok(())
}

struct CollapsedCaption {
    t_ms: u64,
    speaker: Option<String>,
    text: String,
}

fn collapse_caption_partials(events: &[TimelineEvent]) -> Vec<CollapsedCaption> {
    let mut out: Vec<CollapsedCaption> = Vec::new();
    for ev in events {
        let TimelineEvent::Caption { t_ms, speaker, text, .. } = ev else {
            continue;
        };
        if let Some(last) = out.last_mut() {
            if last.speaker == *speaker {
                let curr = normalize_for_compare(text);
                let prev = normalize_for_compare(&last.text);
                // Current extends previous → replace text but keep the
                // earlier start timestamp.
                if curr.len() >= prev.len() && curr.starts_with(&prev) {
                    last.text = text.clone();
                    continue;
                }
                // Previous already covers current (out-of-order arrival or
                // re-emission of a partial) → drop current.
                if prev.starts_with(&curr) {
                    continue;
                }
            }
        }
        out.push(CollapsedCaption {
            t_ms: *t_ms,
            speaker: speaker.clone(),
            text: text.clone(),
        });
    }
    out
}

/// Lowercase, strip everything except letters/digits/whitespace, collapse
/// runs of whitespace. Lets us detect that `"Yes, so so the BUL 1.0"` and
/// `"Yes so so the bul 1.0"` are the same utterance (Teams polishes
/// punctuation/casing on the final pass).
fn normalize_for_compare(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            last_was_space = false;
        } else if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        }
        // punctuation is dropped entirely
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Delete everything except `meeting.mp4` and `transcript.txt`. Called after
/// a successful MP4 finalize so we ship the user a clean two-file folder.
/// On any error a sub-step the cleanup keeps going — leaving an extra
/// intermediate around is preferable to bailing halfway.
pub fn cleanup_intermediates(session_dir: &Path) {
    const KEEP: &[&str] = &["meeting.mp4", "transcript.txt", "agent.log"];
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
