use anyhow::Result;
use chrono::Local;
use std::path::{Path, PathBuf};

pub struct SessionDir {
    pub root: PathBuf,
}

impl SessionDir {
    pub fn create(parent: &Path) -> Result<Self> {
        // Folder naming: YYYY-MM-DD-HHMMSS-MeetingSession.
        let stamp = Local::now().format("%Y-%m-%d-%H%M%S").to_string();
        let dir = parent.join(format!("{stamp}-MeetingSession"));
        std::fs::create_dir_all(&dir)?;
        Ok(Self { root: dir })
    }
}
