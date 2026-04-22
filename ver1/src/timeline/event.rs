use serde::{Deserialize, Serialize};

/// A single thing that happened during a session, emitted by any worker.
///
/// `t_ms` is monotonic milliseconds since agent start — see [`crate::clock`].
/// `wall` is the UTC wall-clock at emission, kept only as a human-readable
/// reference; do not use it for ordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TimelineEvent {
    #[serde(rename = "session.start")]
    SessionStart { t_ms: u64, wall: String, note: Option<String> },

    #[serde(rename = "session.stop")]
    SessionStop { t_ms: u64, wall: String },

    #[serde(rename = "caption")]
    Caption {
        t_ms: u64,
        wall: String,
        speaker: Option<String>,
        text: String,
        /// Stable id Teams assigns (or that we synthesise) so duplicates can be filtered.
        item_id: String,
    },

    #[serde(rename = "audio.segment")]
    AudioSegment {
        t_ms: u64,
        wall: String,
        path: String,
        kind: AudioKind,
        duration_ms: u64,
    },

    #[serde(rename = "slide")]
    Slide {
        t_ms: u64,
        wall: String,
        path: String,
        presenter: Option<String>,
        phash_distance: u32,
    },

    #[serde(rename = "share.start")]
    ShareStart { t_ms: u64, wall: String, presenter: Option<String> },

    #[serde(rename = "share.stop")]
    ShareStop { t_ms: u64, wall: String, presenter: Option<String> },

    #[serde(rename = "note")]
    Note { t_ms: u64, wall: String, level: String, msg: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioKind {
    TeamsLoopback,
    DefaultLoopback,
    Microphone,
}

impl TimelineEvent {
    pub fn t_ms(&self) -> u64 {
        match self {
            Self::SessionStart { t_ms, .. }
            | Self::SessionStop { t_ms, .. }
            | Self::Caption { t_ms, .. }
            | Self::AudioSegment { t_ms, .. }
            | Self::Slide { t_ms, .. }
            | Self::ShareStart { t_ms, .. }
            | Self::ShareStop { t_ms, .. }
            | Self::Note { t_ms, .. } => *t_ms,
        }
    }
}

pub fn now_event_stamps() -> (u64, String) {
    (
        crate::clock::now_ms(),
        crate::clock::now_local().to_rfc3339(),
    )
}
