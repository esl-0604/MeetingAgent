//! Tray icons built from the bundled mascot PNG.
//!
//! Same mascot for all three states; we composite a small coloured circle in
//! the bottom-right corner as a state badge:
//!   - idle      — no badge
//!   - detected  — amber badge (waiting for user response)
//!   - recording — red badge (REC)
//!
//! 32x32 is the standard Windows tray size; the OS down-samples for the 16x16
//! notification-area icons.

use image::imageops::FilterType;
use tray_icon::Icon;

const MASCOT_PNG: &[u8] = include_bytes!("../../assets/icon-32.png");
const SIZE: u32 = 32;

pub fn idle_icon() -> Icon {
    let (rgba, w, h) = base_rgba();
    Icon::from_rgba(rgba, w, h).expect("idle icon")
}

pub fn detected_icon() -> Icon {
    let (mut rgba, w, h) = base_rgba();
    badge(&mut rgba, w, h, 0xE0, 0xA8, 0x00);
    Icon::from_rgba(rgba, w, h).expect("detected icon")
}

pub fn recording_icon() -> Icon {
    let (mut rgba, w, h) = base_rgba();
    badge(&mut rgba, w, h, 0xCC, 0x20, 0x20);
    Icon::from_rgba(rgba, w, h).expect("recording icon")
}

fn base_rgba() -> (Vec<u8>, u32, u32) {
    let img = image::load_from_memory(MASCOT_PNG).expect("decode mascot png");
    // Source is already 32x32 but resize anyway so we're robust to asset
    // changes.
    let resized = img
        .resize_exact(SIZE, SIZE, FilterType::Lanczos3)
        .into_rgba8();
    let (w, h) = resized.dimensions();
    (resized.into_raw(), w, h)
}

/// Composite a small filled circle in the lower-right corner as a state
/// badge. Anti-aliased edge for crispness at 32x32.
fn badge(rgba: &mut [u8], w: u32, h: u32, r: u8, g: u8, b: u8) {
    let cx = w as f32 - 6.5;
    let cy = h as f32 - 6.5;
    let radius = 5.5;
    let edge = 1.0;

    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            if d > radius + edge {
                continue;
            }
            let alpha = if d <= radius - edge {
                1.0
            } else {
                ((radius + edge - d) / (2.0 * edge)).clamp(0.0, 1.0)
            };
            let a = (alpha * 255.0) as u32;
            let idx = ((y * w + x) * 4) as usize;
            rgba[idx] = blend(rgba[idx], r, a);
            rgba[idx + 1] = blend(rgba[idx + 1], g, a);
            rgba[idx + 2] = blend(rgba[idx + 2], b, a);
            rgba[idx + 3] = rgba[idx + 3].max(a as u8);
        }
    }
}

fn blend(dst: u8, src: u8, alpha: u32) -> u8 {
    ((src as u32 * alpha + dst as u32 * (255 - alpha)) / 255) as u8
}
