//! Per-agent persisted state, distinct from the user-authored `config.json`.
//!
//! `config.json` is a knob the user edits. `state.json` is internal memory the
//! agent updates itself — last save dir, last session dir, GUI toggles
//! (auto-record), and the welcome-once flag.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    /// Most recent folder the user chose in the save-folder dialog. New
    /// sessions are created directly under this path; the dialog opens here.
    pub last_save_dir: Option<PathBuf>,

    /// Most recent session directory we created. Used by the "Open last
    /// session" menu item and by Phase B sync queueing.
    #[serde(default)]
    pub last_session_dir: Option<PathBuf>,

    /// When true, skip the meeting-detected toast and start recording
    /// automatically. Toggled from the tray menu.
    #[serde(default)]
    pub auto_record_on_detect: bool,

    /// When true, surface state-change events (caption detected, share
    /// started/ended, capture source switch, …) as popups. Default true.
    #[serde(default = "default_true")]
    pub event_notifications: bool,

    /// First-launch flag. We show the welcome dialog + force a save-folder
    /// pick once; afterwards we stay silent on startup.
    #[serde(default)]
    pub welcomed: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            last_save_dir: None,
            last_session_dir: None,
            auto_record_on_detect: false,
            event_notifications: true,
            welcomed: false,
        }
    }
}

impl AgentState {
    pub fn load() -> Self {
        let p = state_path();
        if !p.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str::<Self>(&s).unwrap_or_else(|e| {
                tracing::warn!("bad state at {} ({e}), using defaults", p.display());
                Self::default()
            }),
            Err(e) => {
                tracing::warn!("unreadable state at {} ({e}), using defaults", p.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let p = state_path();
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            if let Err(e) = std::fs::write(&p, s) {
                tracing::warn!("failed to write state to {}: {e}", p.display());
            }
        }
    }

    pub fn remember_save_dir(dir: &Path) {
        let mut s = Self::load();
        s.last_save_dir = Some(dir.to_path_buf());
        s.save();
    }

    pub fn remember_last_session(dir: &Path) {
        let mut s = Self::load();
        s.last_session_dir = Some(dir.to_path_buf());
        s.save();
    }

    pub fn set_auto_record(on: bool) {
        let mut s = Self::load();
        s.auto_record_on_detect = on;
        s.save();
    }

    pub fn set_event_notifications(on: bool) {
        let mut s = Self::load();
        s.event_notifications = on;
        s.save();
    }
}

fn state_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MeetingAgent")
        .join("state.json")
}
