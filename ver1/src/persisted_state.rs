//! Per-agent persisted state, distinct from the user-authored `config.json`.
//!
//! `config.json` is a knob the user edits. `state.json` is internal memory the
//! agent updates itself — currently just the last folder the user picked in
//! the save dialog, so the next meeting defaults to the same archive
//! location.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AgentState {
    /// Most recent folder the user chose in the save-prompt dialog. When set
    /// and still existing, the next session is created here from the start
    /// (instead of `output_root`) and the dialog opens here as well.
    pub last_save_dir: Option<PathBuf>,
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

    /// Update last_save_dir and persist atomically.
    pub fn remember_save_dir(dir: &Path) {
        let mut s = Self::load();
        s.last_save_dir = Some(dir.to_path_buf());
        s.save();
    }
}

fn state_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MeetingAgent")
        .join("state.json")
}
