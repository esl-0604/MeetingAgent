//! Live-caption extraction via UIA.
//!
//! New Teams renders captions inside the WebView2 DOM as a list-like region.
//! Microsoft does not document the exact AutomationIds, and they shift across
//! Teams versions, so we use *layered heuristics*:
//!
//!   1. Find the Teams window (or use the hwnd handed to us by the detector).
//!   2. Locate the caption container — try several name/automation-id patterns
//!      across English/Korean Teams UI.
//!   3. Inside the container, treat each direct child as one caption row.
//!      For each row, extract `(speaker, text)` by reading text-type leaves.
//!   4. Hash the (speaker, text) pair to derive a stable item id; emit only
//!      newly seen items.
//!
//! The container search runs every poll cycle until something is found, then
//! caches the element. If the cached element disappears (Teams re-renders),
//! we drop it and rediscover.

use anyhow::Result;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::state::HwndHandle;
use crate::timeline::TimelineEvent;
use crate::uia::{
    self, automation_id_of, class_of, control_type_of, find_descendant_by_name_contains,
    name_of, walk_descendants, WalkAction,
};

const CAPTION_NAME_NEEDLES: &[&str] = &[
    "captions", "live captions", "subtitles",
    "캡션", "라이브 캡션", "자막",
];
const CAPTION_AID_HINTS: &[&str] = &[
    "captions", "captionsList", "live-captions", "captionRegion",
];
const RECENT_CAPTION_RING: usize = 256;

pub async fn run(
    cfg: Arc<Config>,
    hwnd: Option<HwndHandle>,
    tx: mpsc::Sender<TimelineEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    info!("caption worker starting (poll {}ms)", cfg.caption.poll_interval_ms);

    // UIA must run on a thread we own — not the tokio worker. Drive it via
    // spawn_blocking. Pass the shutdown receiver in directly so the blocking
    // loop can poll it (dropping the JoinHandle does NOT cancel the task).
    let shutdown_inner = shutdown.resubscribe();
    let cfg2 = cfg.clone();
    let blocking = tokio::task::spawn_blocking(move || {
        if let Err(e) = poll_loop(cfg2, hwnd, tx, shutdown_inner) {
            warn!("caption poll loop exited: {e:#}");
        }
    });

    let _ = shutdown.recv().await;
    let _ = blocking.await;
    info!("caption worker stopped");
    Ok(())
}

fn poll_loop(
    cfg: Arc<Config>,
    hwnd: Option<HwndHandle>,
    tx: mpsc::Sender<TimelineEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    uia::com_init_thread();
    let auto = uia::create_automation()?;

    let mut cached_root: Option<windows::Win32::UI::Accessibility::IUIAutomationElement> = None;
    let mut seen: VecDeque<String> = VecDeque::with_capacity(RECENT_CAPTION_RING);
    let mut backoff_misses: u32 = 0;
    // The most-recent caption row in the panel — held back from emission
    // because it may still be growing word-by-word ("I" → "I was talking").
    // Flushed once on shutdown so the final utterance isn't lost.
    let mut pending_final: Option<CaptionItem> = None;
    // If scan_captions returns 0 rows this many polls in a row we treat the
    // cached container element as stale (Teams re-parents the captions panel
    // when the user starts/stops a screen-share or pops it out to its own
    // window) and force a fresh tree search. 10 × 400 ms ≈ 4 s of silence
    // before we spend the cost of re-discovery.
    const STALE_AFTER_EMPTY_SCANS: u32 = 10;
    let mut empty_scan_streak: u32 = 0;
    // One-shot flag so we only fire the "captions detected" popup the first
    // time we locate the container per session.
    let mut announced_captions = false;

    loop {
        if tx.is_closed() {
            break;
        }
        if matches!(
            shutdown.try_recv(),
            Ok(()) | Err(tokio::sync::broadcast::error::TryRecvError::Closed)
        ) {
            break;
        }

        // Refresh cached caption container if needed.
        if cached_root.is_none() {
            cached_root = locate_caption_container(&auto, hwnd.as_ref())?;
            if cached_root.is_some() {
                info!("caption container located");
                let _ = tx.blocking_send(TimelineEvent::Note {
                    t_ms: crate::clock::now_ms(),
                    wall: crate::clock::now_local().to_rfc3339(),
                    level: "info".into(),
                    msg: "caption container located".into(),
                });
                if !announced_captions {
                    announced_captions = true;
                    crate::gui::popup::show_event(
                        "자막 감지됨",
                        "Teams 라이브 캡션을 transcript.txt에 기록합니다.",
                    );
                }
                backoff_misses = 0;
                empty_scan_streak = 0;
            } else {
                backoff_misses = backoff_misses.saturating_add(1);
                if backoff_misses % 10 == 1 {
                    debug!("caption container not yet found (open Teams Live Captions panel)");
                }
            }
        }

        if let Some(root) = cached_root.clone() {
            match scan_captions(&auto, &root) {
                Ok(items) => {
                    if items.is_empty() {
                        empty_scan_streak = empty_scan_streak.saturating_add(1);
                        if empty_scan_streak >= STALE_AFTER_EMPTY_SCANS {
                            debug!(
                                "cached caption container looks stale after {} empty scans; re-locating",
                                empty_scan_streak
                            );
                            cached_root = None;
                            empty_scan_streak = 0;
                            // Fall through to sleep; next loop iteration
                            // will re-locate against all Teams windows.
                        }
                    } else {
                        empty_scan_streak = 0;
                        // Skip the LAST row — it's Teams' still-growing line
                        // (e.g. "I" → "I was" → "I was talking"). By the next
                        // poll where a new row has appeared below it, the
                        // previously-last row has finalised and will be
                        // emitted here at a non-last index.
                        //
                        // Trade-off: the very last spoken line of the meeting
                        // (no successor will ever appear) is emitted on
                        // shutdown via the `pending_final` path at loop exit.
                        let last_idx = items.len() - 1;
                        for (idx, it) in items.iter().enumerate() {
                            if idx == last_idx {
                                continue;
                            }
                            if seen.iter().any(|id| id == &it.id) {
                                continue;
                            }
                            if seen.len() >= RECENT_CAPTION_RING {
                                seen.pop_front();
                            }
                            seen.push_back(it.id.clone());

                            let evt = TimelineEvent::Caption {
                                t_ms: crate::clock::now_ms(),
                                wall: crate::clock::now_local().to_rfc3339(),
                                speaker: it.speaker.clone(),
                                text: it.text.clone(),
                                item_id: it.id.clone(),
                            };
                            if tx.blocking_send(evt).is_err() {
                                return Ok(());
                            }
                        }
                        // Track the most-recent ("pending") row so we can
                        // emit it once on shutdown if it never gets a
                        // successor (final utterance of the meeting).
                        if let Some(last) = items.into_iter().last() {
                            pending_final = Some(last);
                        }
                    }
                }
                Err(e) => {
                    warn!("caption scan failed, dropping cached root: {e:#}");
                    cached_root = None;
                    empty_scan_streak = 0;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(cfg.caption.poll_interval_ms.max(50)));
    }

    // Flush the still-pending last caption (the meeting's final utterance,
    // which never got displaced by a successor row).
    if let Some(p) = pending_final {
        if !seen.iter().any(|id| id == &p.id) {
            let evt = TimelineEvent::Caption {
                t_ms: crate::clock::now_ms(),
                wall: crate::clock::now_local().to_rfc3339(),
                speaker: p.speaker,
                text: p.text,
                item_id: p.id,
            };
            let _ = tx.blocking_send(evt);
        }
    }
    Ok(())
}

#[derive(Debug)]
struct CaptionItem {
    speaker: Option<String>,
    text: String,
    id: String,
}

/// Class name suffix that identifies one caption row in the New Teams DOM
/// (verified 2026-04 against version 26072.x). The full class is something
/// like `fui-ChatMessageCompact__body ___10lx575 ...` with hashed CSS-in-JS
/// utility classes, but the human-readable prefix is stable.
const ROW_CLASS_NEEDLE: &str = "chatmessagecompact__body";

fn scan_captions(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    root: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Result<Vec<CaptionItem>> {
    // 1. Find every caption row by class-name suffix.
    let mut rows: Vec<windows::Win32::UI::Accessibility::IUIAutomationElement> = Vec::new();
    walk_descendants(auto, root, 6, false, |e, _depth| {
        let cls = class_of(e).to_lowercase();
        if cls.contains(ROW_CLASS_NEEDLE) {
            rows.push(e.clone());
            // Don't recurse into a row — the row's own text leaves are extracted
            // separately below, in document order.
            WalkAction::SkipChildren
        } else {
            WalkAction::Continue
        }
    })?;

    // The walker pops in reverse-DOM order; reverse so chronologically older
    // captions come first.
    rows.reverse();

    // 2. For each row, extract text leaves in document order.
    //    Layout in current Teams: [Text "spoken content", Text "speaker name"].
    let mut items: Vec<CaptionItem> = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut leaves: Vec<String> = Vec::new();
        walk_descendants(auto, row, 4, false, |e, _depth| {
            // 50020 = UIA_TextControlTypeId
            if control_type_of(e) == 50020 {
                let n = name_of(e).trim().to_string();
                if !n.is_empty() {
                    leaves.push(n);
                }
            }
            WalkAction::Continue
        })?;
        // Walker visits children in reverse-DOM order. Reversing gives DOM
        // order, which for a Teams caption row is `[speaker_name, spoken_text]`
        // (the speaker label is the first child, the utterance comes second).
        leaves.reverse();

        let (speaker, text) = match leaves.len() {
            0 => continue,
            1 => (None, leaves.remove(0)),
            _ => {
                let speaker = Some(leaves.remove(0));
                let text = leaves.join(" ");
                (speaker, text)
            }
        };
        if text.is_empty() {
            continue;
        }
        let id = stable_id(&speaker, &text);
        items.push(CaptionItem { speaker, text, id });
    }

    Ok(items)
}

fn stable_id(speaker: &Option<String>, text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    speaker.hash(&mut h);
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn locate_caption_container(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
    hwnd: Option<&HwndHandle>,
) -> Result<Option<windows::Win32::UI::Accessibility::IUIAutomationElement>> {
    // Search order: the hwnd the detector handed us first, then every other
    // visible Teams window. Teams silently moves the "라이브 자막" panel
    // between windows (main view → mini-view when sharing → pop-out window
    // if the user clicks the pop-out button), so a cached container from
    // one window will stop receiving rows once that happens. Scanning all
    // windows on re-discovery lets us follow the panel wherever it landed.
    let mut roots: Vec<windows::Win32::UI::Accessibility::IUIAutomationElement> = Vec::new();
    if let Some(h) = hwnd {
        if let Ok(e) = uia::element_from_hwnd(auto, h.0) {
            roots.push(e);
        }
    }
    let desktop = unsafe { auto.GetRootElement() }?;
    if let Ok(others) = crate::state::find_teams_elements(auto, &desktop) {
        for e in others {
            roots.push(e);
        }
    }
    if roots.is_empty() {
        return Ok(None);
    }

    for r in roots {
        // Strategy A: direct name-based match
        if let Some(hit) = find_descendant_by_name_contains(auto, &r, CAPTION_NAME_NEEDLES, 18)? {
            return Ok(Some(hit));
        }
        // Strategy B: AutomationId hints
        let mut hit: Option<windows::Win32::UI::Accessibility::IUIAutomationElement> = None;
        walk_descendants(auto, &r, 18, false, |e, _| {
            let aid = automation_id_of(e).to_lowercase();
            let cls = class_of(e).to_lowercase();
            if CAPTION_AID_HINTS.iter().any(|h| aid.contains(h) || cls.contains(h)) {
                // Only accept list-like containers (control type 50008 = List, 50025 = Group).
                let ct = control_type_of(e);
                if ct == 50008 || ct == 50025 || ct == 50026 {
                    hit = Some(e.clone());
                    return WalkAction::Stop;
                }
            }
            WalkAction::Continue
        })?;
        if hit.is_some() {
            return Ok(hit);
        }
    }
    Ok(None)
}

/// `--diagnose-captions` entrypoint. Locates the caption container the same
/// way the live worker does, then dumps its full subtree so we can design a
/// proper row-level parser.
pub fn diagnose(max_depth: u32) -> Result<()> {
    crate::uia::com_init_thread();
    let auto = crate::uia::create_automation()?;

    let container = locate_caption_container(&auto, None)?;
    let container = match container {
        Some(c) => c,
        None => {
            println!("\n(NOT FOUND) caption container could not be located.");
            println!("Open Teams, join a meeting, and ensure Live Captions is ON.");
            return Ok(());
        }
    };

    println!("\n=== Caption container located ===");
    println!(
        "  ct={}  loc={:?}  name={:?}  aid={:?}  class={:?}",
        crate::uia::control_type_of(&container),
        crate::uia::localized_type_of(&container),
        crate::uia::name_of(&container),
        crate::uia::automation_id_of(&container),
        crate::uia::class_of(&container),
    );

    println!("\n=== Container subtree (depth {max_depth}) ===");
    let _ = crate::uia::dump_tree(&auto, &container, max_depth);

    println!("\n=== What current scan_captions() would extract ===");
    match scan_captions(&auto, &container) {
        Ok(items) => {
            println!("({} items)", items.len());
            for (i, it) in items.iter().enumerate() {
                println!(
                    "  [{i:02}] speaker={:?} text={:?}",
                    it.speaker,
                    if it.text.len() > 100 {
                        format!("{}…(len={})", &it.text[..100], it.text.len())
                    } else {
                        it.text.clone()
                    }
                );
            }
        }
        Err(e) => println!("scan_captions error: {e}"),
    }

    Ok(())
}
