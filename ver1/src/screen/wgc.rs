//! Windows.Graphics.Capture wrapper: hwnd → BGRA frames.
//!
//! Setup is fiddly because WGC is a WinRT API that hands back D3D11 textures.
//! We:
//!   1. Create a hardware D3D11 device with `BGRA_SUPPORT` (required for
//!      DXGI / WinRT interop).
//!   2. Wrap that device in an `IDirect3DDevice` via
//!      `CreateDirect3D11DeviceFromDXGIDevice`.
//!   3. Get a `GraphicsCaptureItem` for the Teams hwnd via the interop
//!      interface `IGraphicsCaptureItemInterop`.
//!   4. Spin up a `Direct3D11CaptureFramePool` (`CreateFreeThreaded` so we
//!      can pull frames from any thread) and a `GraphicsCaptureSession`.
//!   5. On each `next_frame()` call, pull the latest frame, copy it to a CPU
//!      staging texture, map it, and return the BGRA pixels.

use anyhow::{anyhow, bail, Context, Result};
use windows::core::Interface;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, DXGI_ERROR_UNSUPPORTED};
use windows::Win32::Graphics::Gdi::{HMONITOR, MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

const POOL_BUFFER_COUNT: i32 = 2;

pub struct CapturedFrame {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct CaptureSession {
    /// Held to keep the GraphicsCaptureItem alive for the session lifetime —
    /// the underlying COM object is reference-counted and must outlive `pool`.
    #[allow(dead_code)]
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    d3d_device: ID3D11Device,
    d3d_context: ID3D11DeviceContext,
    /// Kept so we can call `pool.Recreate(...)` when the captured item
    /// grows past the texture we allocated at session start — without it,
    /// only the top-left corner of the new size ends up in the recording.
    direct3d_device: IDirect3DDevice,
    /// Current texture size of `pool`. Compared against each frame's
    /// `ContentSize` so we notice when the Teams window has been resized
    /// (e.g. switching from lobby view to meeting view, or user maximise).
    pool_size: SizeInt32,
}

impl CaptureSession {
    pub fn create_for_hwnd(hwnd: isize) -> Result<Self> {
        let (d3d_device, d3d_context, direct3d_device) = build_d3d11_device()?;
        let interop: IGraphicsCaptureItemInterop = windows::core::factory::<
            GraphicsCaptureItem,
            IGraphicsCaptureItemInterop,
        >()
        .context("interop factory")?;
        let item: GraphicsCaptureItem = unsafe {
            interop.CreateForWindow::<HWND, GraphicsCaptureItem>(HWND(hwnd as _))
        }
        .context("CreateForWindow")?;
        Self::finish_setup(item, d3d_device, d3d_context, &direct3d_device)
    }

    /// Capture an arbitrary monitor by HMONITOR. Used as a fallback during
    /// self-share when we can't pin the capture to a specific window.
    pub fn create_for_monitor(hmonitor: isize) -> Result<Self> {
        let hmon = HMONITOR(hmonitor as _);
        if hmon.is_invalid() {
            bail!("invalid HMONITOR ({:#x})", hmonitor);
        }
        let (d3d_device, d3d_context, direct3d_device) = build_d3d11_device()?;
        let interop: IGraphicsCaptureItemInterop = windows::core::factory::<
            GraphicsCaptureItem,
            IGraphicsCaptureItemInterop,
        >()
        .context("interop factory")?;
        let item: GraphicsCaptureItem = unsafe {
            interop.CreateForMonitor::<HMONITOR, GraphicsCaptureItem>(hmon)
        }
        .context("CreateForMonitor")?;
        Self::finish_setup(item, d3d_device, d3d_context, &direct3d_device)
    }

    /// Convenience: capture the primary monitor. Last-resort fallback when
    /// foreground-window heuristics fail to identify what's being shared.
    pub fn create_for_primary_monitor() -> Result<Self> {
        let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        if hmon.is_invalid() {
            bail!("MonitorFromPoint returned null HMONITOR");
        }
        Self::create_for_monitor(hmon.0 as isize)
    }

    fn finish_setup(
        item: GraphicsCaptureItem,
        d3d_device: ID3D11Device,
        d3d_context: ID3D11DeviceContext,
        direct3d_device: &IDirect3DDevice,
    ) -> Result<Self> {
        let size = item.Size().unwrap_or(SizeInt32 { Width: 1280, Height: 720 });

        // Free-threaded pool so we can drain from a non-UI thread.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            direct3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFER_COUNT,
            size,
        )
        .context("CreateFreeThreaded")?;

        let session = pool.CreateCaptureSession(&item).context("CreateCaptureSession")?;
        // Suppress mouse cursor + selection border for a cleaner slide capture.
        let _ = session.SetIsCursorCaptureEnabled(false);
        // Border suppression requires Win11 22H2+; harmless to attempt and ignore.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().context("StartCapture")?;

        Ok(Self {
            item,
            pool,
            session,
            d3d_device,
            d3d_context,
            direct3d_device: direct3d_device.clone(),
            pool_size: size,
        })
    }

    pub fn next_frame(&mut self) -> Result<Option<CapturedFrame>> {
        let frame = match self.pool.TryGetNextFrame() {
            Ok(f) => f,
            Err(e) => {
                if e.code() == DXGI_ERROR_UNSUPPORTED {
                    bail!("WGC unsupported state");
                }
                return Ok(None);
            }
        };

        // Content has grown past our pool texture (e.g. Teams switched from
        // lobby view to a larger meeting view, or the user maximised).
        // Recreate the pool at the new size; otherwise WGC would keep
        // writing just the top-left corner of the real content into our
        // fixed texture and the MP4 ends up cropped.
        let content_size = frame.ContentSize().unwrap_or(self.pool_size);
        if content_size.Width != self.pool_size.Width
            || content_size.Height != self.pool_size.Height
        {
            tracing::info!(
                "WGC pool resize: {}x{} -> {}x{}",
                self.pool_size.Width,
                self.pool_size.Height,
                content_size.Width,
                content_size.Height
            );
            crate::gui::popup::show_event(
                "캡처 창 크기 변경",
                &format!(
                    "{}×{} → {}×{} 로 캡처 해상도가 자동 조정됩니다.",
                    self.pool_size.Width,
                    self.pool_size.Height,
                    content_size.Width,
                    content_size.Height
                ),
            );
            self.pool
                .Recreate(
                    &self.direct3d_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    POOL_BUFFER_COUNT,
                    content_size,
                )
                .context("FramePool::Recreate")?;
            self.pool_size = content_size;
            // Skip this frame: its surface is still the old texture and
            // may only partially fill the new content extent. The very
            // next frame will arrive at the new size.
            return Ok(None);
        }

        let surface = frame.Surface().context("frame.Surface()")?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let src_tex: ID3D11Texture2D = unsafe { access.GetInterface() }?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src_tex.GetDesc(&mut desc); }

        // Build a CPU-readable staging texture of the same size/format.
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: desc.SampleDesc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe { self.d3d_device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
            .context("CreateTexture2D(staging)")?;
        let staging = staging.ok_or_else(|| anyhow!("no staging texture"))?;

        unsafe { self.d3d_context.CopyResource(&staging, &src_tex); }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.d3d_context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .context("Map staging")?;
        let row_pitch = mapped.RowPitch as usize;
        let stride = (desc.Width as usize) * 4;
        let mut bgra = Vec::with_capacity(stride * desc.Height as usize);
        unsafe {
            let base = mapped.pData as *const u8;
            for y in 0..desc.Height as usize {
                let row = std::slice::from_raw_parts(base.add(y * row_pitch), stride);
                bgra.extend_from_slice(row);
            }
        }
        unsafe { self.d3d_context.Unmap(&staging, 0); }

        Ok(Some(CapturedFrame {
            bgra,
            width: desc.Width,
            height: desc.Height,
        }))
    }

    pub fn stop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        bail!("Windows.Graphics.Capture not supported on this OS");
    }
    let mut d3d_device: Option<ID3D11Device> = None;
    let mut d3d_context: Option<ID3D11DeviceContext> = None;
    let mut feature_level = D3D_FEATURE_LEVEL_11_1;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d3d_device),
            Some(&mut feature_level),
            Some(&mut d3d_context),
        )
    }
    .context("D3D11CreateDevice")?;
    let d3d_device = d3d_device.ok_or_else(|| anyhow!("no D3D11 device"))?;
    let d3d_context = d3d_context.ok_or_else(|| anyhow!("no D3D11 context"))?;
    let dxgi_device: IDXGIDevice = d3d_device.cast().context("cast to IDXGIDevice")?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
        .context("CreateDirect3D11DeviceFromDXGIDevice")?;
    let direct3d_device: IDirect3DDevice = inspectable.cast()?;
    Ok((d3d_device, d3d_context, direct3d_device))
}
