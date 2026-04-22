//! Real-time MP4 recorder using Media Foundation's `IMFSinkWriter`.
//!
//! This module replaces the per-frame PNG / per-segment WAV pipeline with a
//! single live-encoded `meeting.mp4` file containing:
//!   - H.264 video at 10 fps (1920x1080 max, letterboxed for smaller sources)
//!   - AAC stereo audio at 48 kHz (Teams loopback + microphone mixed down)
//!
//! The recorder owns nothing app-specific — it exposes `write_video()` and
//! `write_audio()` and lets the orchestrator decide what to feed it.
//! Source switching (Teams window ↔ primary monitor during self-share) lives
//! in the `screen` worker; from the recorder's perspective every frame is
//! just BGRA bytes with a monotonic timestamp.

#![allow(dead_code)] // wired up in Phase 2d/2e; kept buildable through the transition

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::info;
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFSinkWriter, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFCreateSinkWriterFromURL, MFStartup, MFSTARTUP_LITE, MF_API_VERSION,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFAudioFormat_AAC,
    MFAudioFormat_PCM, MFMediaType_Audio, MFMediaType_Video, MFVideoFormat_H264,
    MFVideoFormat_RGB32, MFVideoInterlace_Progressive,
};

/// Configuration baked into every meeting.mp4. Kept as constants rather than
/// runtime knobs because this is an internal-tooling project — we pick
/// reasonable defaults and avoid a config sprawl.
pub const VIDEO_FPS: u32 = 10;
pub const VIDEO_BITRATE_BPS: u32 = 6_000_000; // ~6 Mbps; FHD + meeting content is dense
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
pub const AUDIO_BITRATE_BPS: u32 = 128_000; // typical "good enough speech" AAC

/// Thread-safe wrapper around an active SinkWriter. The workers send samples
/// via `write_video_bgra` / `write_audio_pcm_i16`; a single background
/// finalise flushes + closes the writer on session shutdown.
pub struct Recorder {
    inner: Arc<Mutex<Inner>>,
    path: PathBuf,
    /// Microseconds (on the agent's QPC clock) when recording began — used to
    /// compute Media-Foundation-native 100ns timestamps for incoming samples.
    start_us: u64,
    /// Diagnostics — bumped on every successful WriteSample so we can confirm
    /// from the log that audio/video actually reached the SinkWriter.
    audio_samples: AtomicU64,
    video_frames: AtomicU64,
    last_audio_log_ms: AtomicU64,
    last_video_log_ms: AtomicU64,
}

struct Inner {
    /// `Option` so `finalize()` can take the writer out and call `Finalize()`
    /// without needing to be the sole `Arc` owner. After finalize, subsequent
    /// `write_*` calls become no-ops — workers that haven't observed shutdown
    /// yet just lose a few late samples instead of corrupting the file.
    writer: Option<IMFSinkWriter>,
    video_stream_idx: u32,
    audio_stream_idx: u32,
    video_size: (u32, u32),
}

// windows-rs COM objects are !Send by default; we enforce at the API level
// that Recorder is only touched through its Arc<Mutex<Inner>>.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Recorder {
    pub fn create(output: &Path, video_w: u32, video_h: u32) -> Result<Self> {
        ensure_mf_startup()?;

        // --- SinkWriter from URL ---
        let url_hstring: HSTRING = output.to_string_lossy().as_ref().into();
        let writer: IMFSinkWriter = unsafe {
            MFCreateSinkWriterFromURL(PCWSTR(url_hstring.as_ptr()), None, None)
        }
        .with_context(|| format!("MFCreateSinkWriterFromURL({})", output.display()))?;

        // --- Video stream: RGB32 input → H.264 output ---
        let video_out = build_video_output_type(video_w, video_h)?;
        let video_in = build_video_input_type(video_w, video_h)?;
        let video_stream_idx = unsafe { writer.AddStream(&video_out) }.context("AddStream(video)")?;
        unsafe { writer.SetInputMediaType(video_stream_idx, &video_in, None) }
            .context("SetInputMediaType(video)")?;

        // --- Audio stream: PCM s16 input → AAC output ---
        let audio_out = build_audio_output_type()?;
        let audio_in = build_audio_input_type()?;
        let audio_stream_idx = unsafe { writer.AddStream(&audio_out) }.context("AddStream(audio)")?;
        unsafe { writer.SetInputMediaType(audio_stream_idx, &audio_in, None) }
            .context("SetInputMediaType(audio)")?;

        unsafe { writer.BeginWriting() }.context("BeginWriting")?;

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                writer: Some(writer),
                video_stream_idx,
                audio_stream_idx,
                video_size: (video_w, video_h),
            })),
            path: output.to_path_buf(),
            start_us: crate::clock::now_us(),
            audio_samples: AtomicU64::new(0),
            video_frames: AtomicU64::new(0),
            last_audio_log_ms: AtomicU64::new(0),
            last_video_log_ms: AtomicU64::new(0),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn video_size(&self) -> (u32, u32) {
        self.inner.lock().video_size
    }

    /// 100-nanosecond ticks since recording started. Both video and audio
    /// use this so A/V remain on a single monotonic timeline.
    pub fn now_100ns(&self) -> i64 {
        ((crate::clock::now_us().saturating_sub(self.start_us)) as i64) * 10
    }

    /// Submit one BGRA frame. `timestamp_100ns` is 100-nanosecond ticks since
    /// recording start (Media Foundation's native time unit). After
    /// `finalize()` this becomes a no-op.
    pub fn write_video_bgra(&self, bgra: &[u8], timestamp_100ns: i64) -> Result<()> {
        let inner = self.inner.lock();
        let Some(writer) = inner.writer.as_ref() else {
            return Ok(());
        };
        let (w, h) = inner.video_size;
        let expected = (w as usize) * (h as usize) * 4;
        anyhow::ensure!(
            bgra.len() == expected,
            "video frame size mismatch: got {} bytes, expected {}",
            bgra.len(),
            expected
        );
        let sample = create_sample_from_bytes(bgra, timestamp_100ns, frame_duration_100ns())?;
        unsafe { writer.WriteSample(inner.video_stream_idx, &sample) }
            .context("WriteSample(video)")?;
        drop(inner);
        let frames = self.video_frames.fetch_add(1, Ordering::Relaxed) + 1;
        self.maybe_log_video(frames, timestamp_100ns);
        Ok(())
    }

    /// Submit interleaved s16 stereo PCM at 48 kHz. After `finalize()` this
    /// becomes a no-op.
    pub fn write_audio_pcm_i16(&self, samples: &[i16], timestamp_100ns: i64) -> Result<()> {
        let inner = self.inner.lock();
        let Some(writer) = inner.writer.as_ref() else {
            return Ok(());
        };
        let bytes: &[u8] = bytemuck_cast_i16(samples);
        let frame_count = (samples.len() as u64) / (AUDIO_CHANNELS as u64);
        let duration_100ns =
            (frame_count as i64 * 10_000_000) / (AUDIO_SAMPLE_RATE as i64);
        let sample = create_sample_from_bytes(bytes, timestamp_100ns, duration_100ns)?;
        unsafe { writer.WriteSample(inner.audio_stream_idx, &sample) }
            .context("WriteSample(audio)")?;
        drop(inner);
        let total = self.audio_samples.fetch_add(samples.len() as u64, Ordering::Relaxed)
            + samples.len() as u64;
        self.maybe_log_audio(total, timestamp_100ns);
        Ok(())
    }

    fn maybe_log_audio(&self, total_samples: u64, ts_100ns: i64) {
        let now_ms = (ts_100ns / 10_000).max(0) as u64;
        let last = self.last_audio_log_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) >= 5_000 {
            self.last_audio_log_ms.store(now_ms, Ordering::Relaxed);
            let frames = total_samples / (AUDIO_CHANNELS as u64);
            let audio_ms = frames * 1000 / (AUDIO_SAMPLE_RATE as u64);
            info!("recorder: {} ms audio submitted (ts={} ms)", audio_ms, now_ms);
        }
    }

    fn maybe_log_video(&self, total_frames: u64, ts_100ns: i64) {
        let now_ms = (ts_100ns / 10_000).max(0) as u64;
        let last = self.last_video_log_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) >= 5_000 {
            self.last_video_log_ms.store(now_ms, Ordering::Relaxed);
            info!(
                "recorder: {} video frames submitted (ts={} ms)",
                total_frames, now_ms
            );
        }
    }

    /// Take ownership of the SinkWriter out of the shared `Inner`, call
    /// `Finalize()` on it (writing the moov atom so the MP4 is playable),
    /// and drop it. Safe to call from any clone of the `Arc<Recorder>` —
    /// we don't require sole ownership. Subsequent `write_*` calls become
    /// no-ops because the writer is gone.
    pub fn finalize(&self) -> Result<()> {
        let audio_samples = self.audio_samples.load(Ordering::Relaxed);
        let video_frames = self.video_frames.load(Ordering::Relaxed);
        let audio_ms = (audio_samples / (AUDIO_CHANNELS as u64)) * 1000 / (AUDIO_SAMPLE_RATE as u64);
        info!(
            "recorder: finalising — {} video frames, {} ms audio submitted",
            video_frames, audio_ms
        );
        let writer = {
            let mut inner = self.inner.lock();
            inner.writer.take()
        };
        match writer {
            Some(w) => {
                unsafe { w.Finalize() }.context("Finalize")?;
                Ok(())
            }
            None => Ok(()), // already finalized
        }
    }
}

fn bytemuck_cast_i16(samples: &[i16]) -> &[u8] {
    // Safe: i16 has a well-defined byte representation and we're borrowing
    // read-only. Avoids pulling in the bytemuck crate for one call.
    unsafe {
        std::slice::from_raw_parts(samples.as_ptr() as *const u8, std::mem::size_of_val(samples))
    }
}

fn frame_duration_100ns() -> i64 {
    10_000_000 / VIDEO_FPS as i64
}

fn create_sample_from_bytes(
    bytes: &[u8],
    timestamp_100ns: i64,
    duration_100ns: i64,
) -> Result<IMFSample> {
    unsafe {
        let buffer: IMFMediaBuffer =
            MFCreateMemoryBuffer(bytes.len() as u32).context("MFCreateMemoryBuffer")?;
        let mut dst: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        buffer.Lock(&mut dst, Some(&mut max_len), Some(&mut cur_len))?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        buffer.SetCurrentLength(bytes.len() as u32)?;
        buffer.Unlock()?;

        let sample: IMFSample = MFCreateSample().context("MFCreateSample")?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(timestamp_100ns)?;
        sample.SetSampleDuration(duration_100ns)?;
        Ok(sample)
    }
}

fn build_video_output_type(w: u32, h: u32) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, VIDEO_BITRATE_BPS)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        set_frame_size(&t, w, h)?;
        set_frame_rate(&t, VIDEO_FPS, 1)?;
        set_pixel_aspect_ratio(&t, 1, 1)?;
        Ok(t)
    }
}

fn build_video_input_type(w: u32, h: u32) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        // RGB32 here means BGRA8 in memory (little-endian ARGB / BGRA).
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // WGC delivers frames *top-down* (row 0 is the top of the image). MF's
        // default for RGB32 is bottom-up (DIB convention), which is why the
        // output video appears upside-down if we leave the stride unspecified.
        // A positive MF_MT_DEFAULT_STRIDE tells MF the buffer is top-down.
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
        set_frame_size(&t, w, h)?;
        set_frame_rate(&t, VIDEO_FPS, 1)?;
        set_pixel_aspect_ratio(&t, 1, 1)?;
        Ok(t)
    }
}

fn build_audio_output_type() -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS as u32)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE_BPS / 8)?;
        // 0x29 = AAC LC profile (the only profile the MFT AAC encoder supports
        // anyway, but Windows is happier when we say so explicitly).
        t.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
        // 0 = raw AAC (no ADTS/ADIF framing), required for MP4 muxing.
        t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
        Ok(t)
    }
}

fn build_audio_input_type() -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS as u32)?;
        let block = (AUDIO_CHANNELS as u32) * 2; // 16-bit
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_SAMPLE_RATE * block)?;
        Ok(t)
    }
}

/// MF "attribute" values that pack two u32s into a u64 (high = a, low = b).
fn pack_u32_pair(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | (b as u64)
}

fn set_frame_size(t: &IMFMediaType, w: u32, h: u32) -> Result<()> {
    unsafe { t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(w, h))? };
    Ok(())
}

fn set_frame_rate(t: &IMFMediaType, num: u32, den: u32) -> Result<()> {
    unsafe { t.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(num, den))? };
    Ok(())
}

fn set_pixel_aspect_ratio(t: &IMFMediaType, num: u32, den: u32) -> Result<()> {
    unsafe { t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(num, den))? };
    Ok(())
}

fn ensure_mf_startup() -> Result<()> {
    use std::sync::Once;
    static INIT: Once = Once::new();
    let mut result: Result<()> = Ok(());
    INIT.call_once(|| unsafe {
        if let Err(e) = MFStartup(MF_API_VERSION, MFSTARTUP_LITE) {
            result = Err(anyhow::anyhow!("MFStartup: {e}"));
        }
    });
    result
}
