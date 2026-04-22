//! Average-hash (aHash) implementation tuned for BGRA8 frames.
//!
//! aHash is cheap and good enough for slide-change detection: downsample to
//! 8x8 grayscale, set each bit to 1 if the corresponding pixel is brighter
//! than the mean. Hamming distance between two hashes correlates with visual
//! similarity. For talking-heads it sits in the 0–4 range; for slide
//! transitions it usually jumps above 8.

use anyhow::{Context, Result};
use std::path::Path;

pub fn ahash64(bgra: &[u8], width: u32, height: u32) -> u64 {
    if width == 0 || height == 0 || bgra.len() < (width as usize * height as usize * 4) {
        return 0;
    }
    let mut samples = [0u32; 64];
    let bx = width as f32 / 8.0;
    let by = height as f32 / 8.0;
    for gy in 0..8 {
        for gx in 0..8 {
            let cx = ((gx as f32 + 0.5) * bx) as u32;
            let cy = ((gy as f32 + 0.5) * by) as u32;
            let i = ((cy.min(height - 1) * width + cx.min(width - 1)) * 4) as usize;
            let b = bgra[i] as u32;
            let g = bgra[i + 1] as u32;
            let r = bgra[i + 2] as u32;
            // ITU-R BT.709 luma
            let y = (2126 * r + 7152 * g + 722 * b) / 10000;
            samples[gy * 8 + gx] = y;
        }
    }
    let mean: u32 = samples.iter().sum::<u32>() / 64;
    let mut h: u64 = 0;
    for (i, s) in samples.iter().enumerate() {
        if *s > mean {
            h |= 1u64 << i;
        }
    }
    h
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

pub fn save_png(path: &Path, bgra: &[u8], width: u32, height: u32) -> Result<()> {
    use image::{ImageBuffer, Rgba};
    let mut rgba = vec![0u8; bgra.len()];
    for i in (0..bgra.len()).step_by(4) {
        rgba[i] = bgra[i + 2];
        rgba[i + 1] = bgra[i + 1];
        rgba[i + 2] = bgra[i];
        rgba[i + 3] = bgra[i + 3];
    }
    let img: ImageBuffer<Rgba<u8>, _> =
        ImageBuffer::from_raw(width, height, rgba).context("ImageBuffer::from_raw")?;
    img.save(path).with_context(|| format!("save {}", path.display()))?;
    Ok(())
}
