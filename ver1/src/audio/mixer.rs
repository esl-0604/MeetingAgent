//! Mixes Teams loopback and microphone audio into a single 48 kHz / stereo /
//! s16 stream that is fed to the MP4 recorder.
//!
//! The loopback worker is treated as the master clock — every time it
//! delivers a buffer of N samples we sum that buffer with N samples pulled
//! from the mic-side queue (zero-padded if mic is behind). This anchors the
//! recorded audio to the loopback timing (which we cannot afford to let
//! drift relative to the meeting's video) and tolerates mic jitter as long
//! as it stays within the queue cap.

use crate::recorder::Recorder;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// ~2 sec of stereo s16 @ 48 kHz. If the loopback side stalls for longer
/// than this, the mic queue is silently truncated from the front so memory
/// stays bounded; the user-visible effect is mic samples from far in the
/// past being dropped, which is what we want.
const MIC_BUFFER_CAP_SAMPLES: usize = 48_000 * 2 * 2;

pub struct AudioMixer {
    recorder: Arc<Recorder>,
    mic_queue: Mutex<VecDeque<i16>>,
}

impl AudioMixer {
    pub fn new(recorder: Arc<Recorder>) -> Self {
        Self {
            recorder,
            mic_queue: Mutex::new(VecDeque::new()),
        }
    }

    /// Append mic samples (interleaved stereo s16 @ 48 kHz).
    pub fn push_mic(&self, samples: &[i16]) {
        let mut q = self.mic_queue.lock();
        q.extend(samples.iter().copied());
        while q.len() > MIC_BUFFER_CAP_SAMPLES {
            q.pop_front();
        }
    }

    /// Mix the supplied loopback buffer with whatever mic samples are queued
    /// and forward to the recorder. `samples` must be interleaved stereo s16
    /// @ 48 kHz; `ts_100ns` is the recorder timeline timestamp for the start
    /// of this buffer.
    pub fn push_loopback(&self, samples: &[i16], ts_100ns: i64) {
        let mut mixed = samples.to_vec();
        let mut q = self.mic_queue.lock();
        for s in mixed.iter_mut() {
            if let Some(m) = q.pop_front() {
                let sum = (*s as i32) + (m as i32);
                *s = sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
        }
        drop(q);
        let _ = self.recorder.write_audio_pcm_i16(&mixed, ts_100ns);
    }

    /// Recorder timeline timestamp helper so callers don't have to hold a
    /// separate handle to the recorder.
    pub fn recorder_now_100ns(&self) -> i64 {
        self.recorder.now_100ns()
    }
}
