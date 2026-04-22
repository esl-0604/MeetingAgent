//! Default-microphone capture via WASAPI shared mode.
//!
//! We explicitly request 48 kHz stereo IEEE-float and rely on
//! `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` to bridge whatever the device's
//! native format is. That keeps the mic pipeline trivially compatible with
//! the loopback pipeline (same rate / channel count) so the mixer can sum
//! samples 1:1 without resampling.
//!
//! Captured samples are converted to s16 and pushed into the mixer, which
//! folds them into the same MP4 audio track as the Teams loopback. We do
//! not write any standalone WAV — the MP4 is the single source of truth.

use super::mixer::AudioMixer;
use crate::timeline::TimelineEvent;
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_S_BUFFER_EMPTY,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const MIC_SAMPLE_RATE: u32 = 48_000;
const MIC_CHANNELS: u16 = 2;

pub fn run(
    mixer: Option<Arc<AudioMixer>>,
    tx: mpsc::Sender<TimelineEvent>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    crate::uia::com_init_thread();

    let enumer: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let device = unsafe { enumer.GetDefaultAudioEndpoint(eCapture, eCommunications) }
        .context("default mic endpoint")?;
    let client: IAudioClient = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }?;

    let format = build_mic_format(MIC_SAMPLE_RATE, MIC_CHANNELS);
    let buffer_duration_100ns: i64 = 200_0000; // 20 ms — matches loopback
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            buffer_duration_100ns,
            0,
            &format.Format as *const _,
            None,
        )
    }
    .context("mic IAudioClient::Initialize")?;

    let event = unsafe { CreateEventW(None, false, false, None) }?;
    unsafe { client.SetEventHandle(event) }?;
    let capture: IAudioCaptureClient = unsafe { client.GetService() }?;

    unsafe { client.Start() }?;
    info!("microphone capture running ({} ch @ {} Hz)", MIC_CHANNELS, MIC_SAMPLE_RATE);

    let res = pump(&capture, event, MIC_CHANNELS, mixer.as_ref(), shutdown);

    unsafe { let _ = client.Stop(); }
    unsafe { let _ = CloseHandle(event); }
    let _ = tx.blocking_send(TimelineEvent::Note {
        t_ms: crate::clock::now_ms(),
        wall: crate::clock::now_local().to_rfc3339(),
        level: "info".into(),
        msg: "microphone stopped".into(),
    });
    res
}

fn pump(
    capture: &IAudioCaptureClient,
    event: HANDLE,
    channels: u16,
    mixer: Option<&Arc<AudioMixer>>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    let mut s16_scratch: Vec<i16> = Vec::new();
    loop {
        if super::shutdown_pending(shutdown) {
            return Ok(());
        }
        let wait = unsafe { WaitForSingleObject(event, 200) };
        if wait != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            let mut frames: u32 = 0;
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut flags: u32 = 0;
            let mut device_pos: u64 = 0;
            let mut qpc_pos: u64 = 0;
            let hr = unsafe {
                capture.GetBuffer(
                    &mut data_ptr,
                    &mut frames,
                    &mut flags,
                    Some(&mut device_pos),
                    Some(&mut qpc_pos),
                )
            };
            if let Err(e) = hr {
                if e.code() == AUDCLNT_S_BUFFER_EMPTY {
                    break;
                }
                warn!("mic GetBuffer error: {e}");
                break;
            }
            if frames == 0 {
                break;
            }
            let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let total_samples = (frames as usize) * (channels as usize);
            // Drop silent packets entirely — loopback drives the timeline
            // and silent mic samples would just inflate the queue with no
            // useful content.
            if !silent {
                if let Some(mx) = mixer {
                    let slice = unsafe {
                        std::slice::from_raw_parts(data_ptr as *const f32, total_samples)
                    };
                    s16_scratch.clear();
                    s16_scratch.reserve(total_samples);
                    for &f in slice {
                        let v = (f * 32767.0).clamp(-32768.0, 32767.0) as i16;
                        s16_scratch.push(v);
                    }
                    mx.push_mic(&s16_scratch);
                }
            }
            unsafe { capture.ReleaseBuffer(frames) }?;
        }
    }
}

fn build_mic_format(sample_rate: u32, channels: u16) -> WAVEFORMATEXTENSIBLE {
    let bits_per_sample: u16 = 32;
    let block_align = channels * (bits_per_sample / 8);
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: bits_per_sample,
            cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
                - std::mem::size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits_per_sample,
        },
        dwChannelMask: 0x3,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}
