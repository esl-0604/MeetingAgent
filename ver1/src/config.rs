use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub output_root: PathBuf,
    /// When true (the default), show a folder-picker dialog at every session
    /// end — including Ctrl-C — so the user can move the session directory
    /// to a chosen archive location. CLI flag `--no-prompt-save` disables it
    /// for the run (useful for automated/headless usage).
    #[serde(default = "default_prompt_save_on_exit")]
    pub prompt_save_on_exit: bool,
    pub audio: AudioConfig,
    pub screen: ScreenConfig,
    pub caption: CaptionConfig,
    pub detect: DetectConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub capture_teams_loopback: bool,
    pub capture_microphone: bool,
    pub teams_process_name: String,
    /// If true, fall back to default-device loopback when process-loopback activation fails.
    pub fallback_to_default_loopback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenConfig {
    pub enabled: bool,
    pub min_frame_interval_ms: u64,
    /// Hamming-distance threshold on the 64-bit perceptual hash. Frames whose
    /// pHash distance from the last saved frame is >= this value are saved.
    pub phash_threshold: u32,
    /// When true, only capture while UIA detects an active screen-share.
    pub only_during_share: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptionConfig {
    pub enabled: bool,
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectConfig {
    pub poll_interval_ms: u64,
    pub use_log_tail: bool,
}

impl Default for Config {
    fn default() -> Self {
        let output_root = dirs::document_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("MeetingAgent")
            .join("sessions");
        Self {
            output_root,
            prompt_save_on_exit: true,
            audio: AudioConfig {
                capture_teams_loopback: true,
                capture_microphone: true,
                teams_process_name: "ms-teams.exe".into(),
                fallback_to_default_loopback: true,
            },
            screen: ScreenConfig {
                enabled: true,
                min_frame_interval_ms: 500,
                phash_threshold: 8,
                only_during_share: true,
            },
            caption: CaptionConfig {
                enabled: true,
                poll_interval_ms: 400,
            },
            detect: DetectConfig {
                poll_interval_ms: 1500,
                use_log_tail: false,
            },
        }
    }
}

impl Config {
    pub fn load_or_default() -> Self {
        let path = config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => match serde_json::from_str::<Self>(&s) {
                    Ok(c) => {
                        tracing::info!("loaded config from {}", path.display());
                        return c;
                    }
                    Err(e) => tracing::warn!("bad config at {} ({e}), using defaults", path.display()),
                },
                Err(e) => tracing::warn!("unreadable config at {} ({e}), using defaults", path.display()),
            }
        }
        Self::default()
    }
}

fn default_prompt_save_on_exit() -> bool {
    true
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MeetingAgent")
        .join("config.json")
}
