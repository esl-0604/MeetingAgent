//! Process-loopback capture for ms-teams.exe via `ActivateAudioInterfaceAsync`.
//!
//! Algorithm:
//!   1. Build an `AUDIOCLIENT_ACTIVATION_PARAMS` requesting process-loopback
//!      mode targeting the Teams PID, including its process tree (Teams
//!      spins child renderers).
//!   2. Wrap that struct in a `PROPVARIANT` of type `VT_BLOB` and call
//!      `ActivateAudioInterfaceAsync(VAD\\Process_Loopback,
//!      IID_IAudioClient, params, handler)`.
//!   3. Wait synchronously on a Win32 event signalled by our completion
//!      handler. The handler stashes the resulting `IAudioClient` into a
//!      shared `Arc<Mutex>` we both hold.
//!   4. Initialise the client in shared, event-driven, loopback mode at a
//!      fixed PCM-float format.
//!   5. Loop on the buffer event handle, draining `IAudioCaptureClient::GetBuffer`
//!      into a WAV file until shutdown.
//!
//! On any failure, we optionally fall back to default-device loopback (whole
//! system audio) if `audio.fallback_to_default_loopback` is set.

use super::mixer::AudioMixer;
use crate::config::Config;
use crate::timeline::TimelineEvent;
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};
use windows::core::{implement, Interface, PCWSTR, Result as WinResult, PROPVARIANT};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_S_BUFFER_EMPTY, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

const VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK: PCWSTR =
    windows::core::w!("VAD\\Process_Loopback");

pub fn run(
    cfg: Arc<Config>,
    teams_pid: u32,
    mixer: Option<Arc<AudioMixer>>,
    tx: mpsc::Sender<TimelineEvent>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    crate::uia::com_init_thread();

    if teams_pid != 0 {
        match start_process_loopback(teams_pid, mixer.clone(), &tx, shutdown) {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!("process loopback failed (pid={teams_pid}): {e:#}");
                if !cfg.audio.fallback_to_default_loopback {
                    return Err(e);
                }
            }
        }
    }

    info!("audio: falling back to default-device loopback");
    start_default_loopback(mixer, &tx, shutdown)
}

// ---------------------------------------------------------------------------
// Completion handler
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CompletionState {
    hr: i32,
    client: Option<IAudioClient>,
    done: bool,
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct CompletionHandler {
    event: SendableHandle,
    state: Arc<Mutex<CompletionState>>,
}

#[derive(Clone, Copy)]
struct SendableHandle(HANDLE);
unsafe impl Send for SendableHandle {}
unsafe impl Sync for SendableHandle {}

impl IActivateAudioInterfaceCompletionHandler_Impl for CompletionHandler_Impl {
    fn ActivateCompleted(
        &self,
        op: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> WinResult<()> {
        let mut hr_activate = windows::core::HRESULT(0);
        let mut activated: Option<windows::core::IUnknown> = None;
        if let Some(op) = op {
            unsafe {
                let _ = op.GetActivateResult(&mut hr_activate, &mut activated);
            }
        }
        let mut s = self.state.lock();
        s.hr = hr_activate.0;
        if hr_activate.is_ok() {
            if let Some(unk) = activated {
                s.client = unk.cast::<IAudioClient>().ok();
            }
        }
        s.done = true;
        unsafe {
            let _ = SetEvent(self.event.0);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Process loopback path
// ---------------------------------------------------------------------------

fn start_process_loopback(
    teams_pid: u32,
    mixer: Option<Arc<AudioMixer>>,
    tx: &mpsc::Sender<TimelineEvent>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    // Activation params live on the stack — keep them alive until the async
    // operation completes.
    let activation = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: teams_pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };

    // Build a PROPVARIANT(VT_BLOB) by hand. The `windows` crate's PROPVARIANT
    // is a wrapper that owns its memory and would try to CoTaskMemFree our
    // stack pointer on Drop, so we mirror the C layout in our own struct and
    // cast the pointer at the call site.
    #[repr(C, align(8))]
    struct PropvariantBlob {
        vt: u16,
        _r1: u16,
        _r2: u16,
        _r3: u16,
        cb_size: u32,
        _pad: u32,
        p_blob_data: *mut u8,
    }
    let raw_pv = PropvariantBlob {
        vt: VT_BLOB.0,
        _r1: 0,
        _r2: 0,
        _r3: 0,
        cb_size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        p_blob_data: &activation as *const AUDIOCLIENT_ACTIVATION_PARAMS as *mut u8,
    };

    let event_handle = unsafe { CreateEventW(None, false, false, None) }
        .context("CreateEventW for activation")?;
    let state = Arc::new(Mutex::new(CompletionState::default()));
    let handler = CompletionHandler {
        event: SendableHandle(event_handle),
        state: state.clone(),
    };
    let handler_iface: IActivateAudioInterfaceCompletionHandler = handler.into();

    let _op: IActivateAudioInterfaceAsyncOperation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&raw_pv as *const PropvariantBlob as *const PROPVARIANT),
            &handler_iface,
        )
    }
    .context("ActivateAudioInterfaceAsync(VAD\\Process_Loopback)")?;

    let wait = unsafe { WaitForSingleObject(event_handle, 5000) };
    if wait != WAIT_OBJECT_0 {
        unsafe {
            let _ = CloseHandle(event_handle);
        }
        bail!("ActivateAudioInterfaceAsync timed out");
    }

    let (hr, client) = {
        let mut s = state.lock();
        (s.hr, s.client.take())
    };
    unsafe {
        let _ = CloseHandle(event_handle);
    }
    if hr != 0 {
        bail!("activation HRESULT {:#x}", hr as u32);
    }
    let client = client.ok_or_else(|| anyhow!("activation returned no IAudioClient"))?;

    // For the process-loopback virtual device we must request a fixed format —
    // the device has no native mix format.
    let format = build_loopback_format(48000, 2);
    let buffer_duration_100ns: i64 = 200_0000; // 20 ms
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            buffer_duration_100ns,
            0,
            &format.Format as *const _,
            None,
        )
    }
    .context("IAudioClient::Initialize (process loopback)")?;

    let buffer_event = unsafe { CreateEventW(None, false, false, None) }
        .context("CreateEventW for buffer")?;
    unsafe { client.SetEventHandle(buffer_event) }.context("SetEventHandle")?;
    let capture: IAudioCaptureClient = unsafe { client.GetService::<IAudioCaptureClient>() }
        .context("GetService(IAudioCaptureClient)")?;

    unsafe { client.Start() }.context("IAudioClient::Start")?;
    info!("loopback capture running (pid={})", teams_pid);

    let result = capture_pump(&capture, buffer_event, 2, mixer.as_ref(), shutdown);

    unsafe {
        let _ = client.Stop();
    }
    unsafe {
        let _ = CloseHandle(buffer_event);
    }
    let _ = tx.blocking_send(TimelineEvent::Note {
        t_ms: crate::clock::now_ms(),
        wall: crate::clock::now_local().to_rfc3339(),
        level: "info".into(),
        msg: "teams loopback stopped".into(),
    });
    result
}

// ---------------------------------------------------------------------------
// Default device loopback fallback
// ---------------------------------------------------------------------------

fn start_default_loopback(
    mixer: Option<Arc<AudioMixer>>,
    tx: &mpsc::Sender<TimelineEvent>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    let enumer: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("CoCreateInstance(MMDeviceEnumerator)")?;
    let device = unsafe { enumer.GetDefaultAudioEndpoint(eRender, eConsole) }
        .context("GetDefaultAudioEndpoint(eRender)")?;
    let client: IAudioClient =
        unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.context("Activate IAudioClient")?;

    let mix_fmt = unsafe { client.GetMixFormat() }.context("GetMixFormat")?;
    let buffer_duration_100ns: i64 = 200_0000;
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            buffer_duration_100ns,
            0,
            mix_fmt,
            None,
        )
    }
    .context("Initialize default loopback")?;

    let event = unsafe { CreateEventW(None, false, false, None) }?;
    unsafe { client.SetEventHandle(event) }?;
    let capture: IAudioCaptureClient = unsafe { client.GetService() }?;

    let (_sample_rate, channels, _is_float) = unsafe { read_format(mix_fmt) };

    unsafe { client.Start() }?;
    info!("default-loopback capture running ({} ch)", channels);

    let result = capture_pump(&capture, event, channels, mixer.as_ref(), shutdown);

    unsafe {
        let _ = client.Stop();
    }
    unsafe {
        let _ = CloseHandle(event);
    }
    unsafe {
        CoTaskMemFree(Some(mix_fmt as *const _ as *const _));
    }
    let _ = tx.blocking_send(TimelineEvent::Note {
        t_ms: crate::clock::now_ms(),
        wall: crate::clock::now_local().to_rfc3339(),
        level: "info".into(),
        msg: "default loopback stopped".into(),
    });
    result
}

// ---------------------------------------------------------------------------
// Shared pump
// ---------------------------------------------------------------------------

fn capture_pump(
    capture: &IAudioCaptureClient,
    event: HANDLE,
    channels: u16,
    mixer: Option<&Arc<AudioMixer>>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    // Reused s16 buffer to avoid per-packet allocation.
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
                warn!("GetBuffer error: {e}");
                break;
            }
            if frames == 0 {
                break;
            }
            let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let total_samples = (frames as usize) * (channels as usize);
            // Send zeros for silent packets so the audio timeline keeps
            // advancing (and any queued mic audio still flows through).
            if let Some(mx) = mixer {
                if channels == 2 {
                    s16_scratch.clear();
                    if silent {
                        s16_scratch.resize(total_samples, 0);
                    } else {
                        let slice = unsafe {
                            std::slice::from_raw_parts(data_ptr as *const f32, total_samples)
                        };
                        s16_scratch.reserve(total_samples);
                        for &f in slice {
                            let v = (f * 32767.0).clamp(-32768.0, 32767.0) as i16;
                            s16_scratch.push(v);
                        }
                    }
                    let ts = mx.recorder_now_100ns();
                    mx.push_loopback(&s16_scratch, ts);
                }
            }
            unsafe { capture.ReleaseBuffer(frames) }.context("ReleaseBuffer")?;
        }
    }
}

// ---------------------------------------------------------------------------
// Format helpers
// ---------------------------------------------------------------------------

fn build_loopback_format(sample_rate: u32, channels: u16) -> WAVEFORMATEXTENSIBLE {
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
        dwChannelMask: 0x3, // FRONT_LEFT | FRONT_RIGHT
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

unsafe fn read_format(fmt: *const WAVEFORMATEX) -> (u32, u16, bool) {
    // WAVEFORMATEX is `#[repr(packed)]`. Read fields via raw pointer reads
    // to avoid taking references into a misaligned struct.
    let tag = std::ptr::read_unaligned(std::ptr::addr_of!((*fmt).wFormatTag));
    let sample_rate = std::ptr::read_unaligned(std::ptr::addr_of!((*fmt).nSamplesPerSec));
    let channels = std::ptr::read_unaligned(std::ptr::addr_of!((*fmt).nChannels));
    let is_float = if tag == WAVE_FORMAT_EXTENSIBLE as u16 {
        let ext_ptr = fmt as *const WAVEFORMATEXTENSIBLE;
        let sub: windows_core::GUID =
            std::ptr::read_unaligned(std::ptr::addr_of!((*ext_ptr).SubFormat));
        sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        tag == WAVE_FORMAT_IEEE_FLOAT as u16
    };
    (sample_rate, channels, is_float)
}
