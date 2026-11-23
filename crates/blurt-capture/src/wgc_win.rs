//! Native Windows screen & audio recorder using `Windows.Graphics.Capture` + Direct3D 11 + Media Foundation
//! (`IMFSinkWriter` with hardware H.264 NVENC/QSV/AMF + AAC).
//!
//! Replaces external `ffmpeg` screen grabbing (`gdigrab` / `ddagrab`) and segment concat on Windows:
//! - Captures the monitor natively via `Windows.Graphics.Capture` (cursor included, border suppressed on Win11).
//! - Crops & converts frames to top-down `NV12` and encodes directly to `<session>/recording.mp4` via `IMFSinkWriter`
//!   with `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS = 1`.
//! - Captures microphone (and optional WASAPI system loopback as a second audio track) at 48 kHz AAC in the same MP4,
//!   plus 16 kHz `live/audio.pcm` sidecar for `live.py` live captions.
//! - Supports countdown (`go`/`stop`/`discard`), zero-file seamless pause/resume (PTS offset), lossless PNG stills
//!   (`snap`, `marker`, `snap-region:x,y,w,h`), pointer/click/stroke `events.jsonl`, and `meta.json`.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde_json::{json, Value};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, EnumDisplayMonitors,
    GetDC, GetDIBits, GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, SRCCOPY,
};
use windows::Win32::Media::MediaFoundation::{
    IMFSinkWriter, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateAttributes, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFShutdown, MFStartup,
    MFMediaType_Audio, MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFSTARTUP_NOSOCKET, MF_LOW_LATENCY, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoGetActivationFactory, RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetSystemMetrics, SM_SWAPBUTTON};

use crate::engine::{CaptureOptions, Mic};
use crate::region::Region;
use crate::status::StatusEvent;

const AUDIO_SR: u32 = 48_000;
const LIVE_SR: u32 = 16_000;
const HNS_PER_SEC: i64 = 10_000_000;

/// Running in-process native WGC session.
pub struct NativeSession {
    done: Arc<AtomicBool>,
    kill_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl NativeSession {
    pub fn is_finished(&self) -> bool {
        self.done.load(SeqCst)
    }

    pub fn wait(&mut self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        while !self.done.load(SeqCst) {
            if t0.elapsed() > timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(30));
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        true
    }

    pub fn kill(&mut self) {
        self.kill_flag.store(true, SeqCst);
        let _ = self.wait(Duration::from_secs(2));
    }
}

/// Returns `true` when `opts` asks for the native WGC recorder rather than `record.py --engine ffmpeg`.
pub fn should_use_native(opts: &CaptureOptions) -> bool {
    if std::env::var("BLURT_CAPTURE_ENGINE").as_deref() == Ok("ffmpeg") {
        return false;
    }
    if std::env::var_os("BLURT_FAKE_SCREEN").is_some() {
        return false;
    }
    for (k, v) in &opts.env {
        if k == "BLURT_FAKE_SCREEN" && !v.is_empty() {
            return false;
        }
        if k == "BLURT_CAPTURE_ENGINE" && v == "ffmpeg" {
            return false;
        }
    }
    // Custom extra CLI flags in launcher.args (e.g. unit tests probing record.py's argparse) stay on record.py
    let last_is_py = opts
        .launcher
        .args
        .last()
        .is_some_and(|a| a.to_string_lossy().ends_with("record.py"));
    if !last_is_py {
        return false;
    }
    if let Some(home) = blurt_home(opts) {
        if let Ok(raw) = fs::read_to_string(home.join("config.json")) {
            if let Ok(cfg) = serde_json::from_str::<Value>(&raw) {
                if cfg.pointer("/record/engine").and_then(Value::as_str) == Some("ffmpeg") {
                    return false;
                }
            }
        }
    }
    true
}

fn blurt_home(opts: &CaptureOptions) -> Option<PathBuf> {
    opts.env
        .iter()
        .find(|(k, _)| k == "BLURT_HOME")
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| std::env::var_os("BLURT_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".blurt")))
}

fn plain_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p.to_path_buf()
    }
}

#[derive(Clone, Copy)]
struct TargetGeometry {
    hmon_raw: isize,
    desk_x: i32,
    desk_y: i32,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
    out_w: u32,
    out_h: u32,
}

fn resolve_geometry(region: Option<Region>, screen: Option<u32>, max_width: u32) -> Result<TargetGeometry, String> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hmon = if let Some(r) = region {
            let rc = RECT {
                left: r.x,
                top: r.y,
                right: r.x + r.w as i32,
                bottom: r.y + r.h as i32,
            };
            MonitorFromRect(&rc, MONITOR_DEFAULTTONEAREST)
        } else if let Some(idx) = screen {
            let mons = enumerate_monitors();
            mons.get(idx as usize)
                .copied()
                .unwrap_or_else(|| MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY))
        } else {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        };
        if hmon.0.is_null() {
            return Err("no display monitor found".into());
        }
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return Err("GetMonitorInfoW failed".into());
        }
        let mw = (mi.rcMonitor.right - mi.rcMonitor.left).max(2);
        let mh = (mi.rcMonitor.bottom - mi.rcMonitor.top).max(2);

        let (desk_x, desk_y, crop_x, crop_y, crop_w, crop_h) = if let Some(r) = region {
            let r = r.even();
            // If the region extends across multiple monitors, let the desktop grabber handle it
            if r.x < mi.rcMonitor.left
                || r.y < mi.rcMonitor.top
                || r.x + (r.w as i32) > mi.rcMonitor.right
                || r.y + (r.h as i32) > mi.rcMonitor.bottom
            {
                return Err("region spans multiple monitors".into());
            }
            let cx = (r.x - mi.rcMonitor.left).max(0) as u32;
            let cy = (r.y - mi.rcMonitor.top).max(0) as u32;
            let cw = (r.w.min((mw as u32).saturating_sub(cx)) & !1).max(2);
            let ch = (r.h.min((mh as u32).saturating_sub(cy)) & !1).max(2);
            (r.x, r.y, cx, cy, cw, ch)
        } else {
            let cw = ((mw as u32) & !1).max(2);
            let ch = ((mh as u32) & !1).max(2);
            (mi.rcMonitor.left, mi.rcMonitor.top, 0, 0, cw, ch)
        };

        let max_w = max_width.max(320) & !1;
        let (out_w, out_h) = if crop_w > max_w {
            let ow = max_w;
            let oh = (((crop_h as u64 * ow as u64) / crop_w as u64) as u32 & !1).max(2);
            (ow, oh)
        } else {
            (crop_w, crop_h)
        };

        Ok(TargetGeometry {
            hmon_raw: hmon.0 as isize,
            desk_x,
            desk_y,
            crop_x,
            crop_y,
            crop_w,
            crop_h,
            out_w,
            out_h,
        })
    }
}

fn enumerate_monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rc: *mut RECT,
        data: windows::Win32::Foundation::LPARAM,
    ) -> windows::core::BOOL {
        let v = &mut *(data.0 as *mut Vec<HMONITOR>);
        v.push(hmon);
        true.into()
    }
    let mut out: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            windows::Win32::Foundation::LPARAM(&mut out as *mut _ as isize),
        );
    }
    out
}

struct ComGuard;
impl ComGuard {
    fn init() -> Self {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
        }
        Self
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

struct WgcGrabber {
    _d3d_device: ID3D11Device,
    d3d_ctx: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    pool: Direct3D11CaptureFramePool,
    session: windows::Graphics::Capture::GraphicsCaptureSession,
    geom: TargetGeometry,
}

impl WgcGrabber {
    fn open(geom: TargetGeometry) -> Result<Self, String> {
        unsafe {
            let mut d3d_dev: Option<ID3D11Device> = None;
            let mut d3d_ctx: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d_dev),
                None,
                Some(&mut d3d_ctx),
            )
            .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
            let d3d_dev = d3d_dev.ok_or("no ID3D11Device")?;
            let d3d_ctx = d3d_ctx.ok_or("no ID3D11DeviceContext")?;
            if let Ok(mt) = d3d_dev.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let dxgi_dev: IDXGIDevice = d3d_dev.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;
            let inspectable =
                CreateDirect3D11DeviceFromDXGIDevice(&dxgi_dev).map_err(|e| format!("CreateDirect3D11Device: {e}"))?;
            let winrt_dev: IDirect3DDevice = inspectable.cast().map_err(|e| format!("IDirect3DDevice: {e}"))?;

            let interop: IGraphicsCaptureItemInterop = RoGetActivationFactory(&HSTRING::from(
                "Windows.Graphics.Capture.GraphicsCaptureItem",
            ))
            .map_err(|e| format!("IGraphicsCaptureItemInterop: {e}"))?;
            let item: GraphicsCaptureItem = interop
                .CreateForMonitor(HMONITOR(geom.hmon_raw as *mut _))
                .map_err(|e| format!("CreateForMonitor: {e}"))?;
            let size = item.Size().map_err(|e| format!("item.Size: {e}"))?;
            if size.Width <= 0 || size.Height <= 0 {
                return Err("invalid capture item size".into());
            }

            let desc = D3D11_TEXTURE2D_DESC {
                Width: geom.crop_w,
                Height: geom.crop_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            d3d_dev
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| format!("CreateTexture2D staging: {e}"))?;
            let staging = staging.ok_or("no staging texture")?;

            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_dev,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )
            .map_err(|e| format!("CreateFreeThreaded: {e}"))?;
            let session = pool
                .CreateCaptureSession(&item)
                .map_err(|e| format!("CreateCaptureSession: {e}"))?;
            let _ = session.SetIsCursorCaptureEnabled(true);
            let _ = session.SetIsBorderRequired(false);
            session.StartCapture().map_err(|e| format!("StartCapture: {e}"))?;

            Ok(Self {
                _d3d_device: d3d_dev,
                d3d_ctx,
                staging,
                pool,
                session,
                geom,
            })
        }
    }

    /// Drains the newest frame from the pool if one arrived; writes tightly-packed BGRA (`crop_w * crop_h * 4`) into `bgra_out`.
    fn poll_frame(&self, bgra_out: &mut Vec<u8>) -> bool {
        let mut got = false;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Ok(surf) = frame.Surface() {
                if let Ok(access) = surf.cast::<IDirect3DDxgiInterfaceAccess>() {
                    unsafe {
                        if let Ok(tex) = access.GetInterface::<ID3D11Texture2D>() {
                            let mut tdesc = D3D11_TEXTURE2D_DESC::default();
                            tex.GetDesc(&mut tdesc);
                            if self.geom.crop_x + self.geom.crop_w <= tdesc.Width
                                && self.geom.crop_y + self.geom.crop_h <= tdesc.Height
                            {
                                let src_box = D3D11_BOX {
                                    left: self.geom.crop_x,
                                    top: self.geom.crop_y,
                                    front: 0,
                                    right: self.geom.crop_x + self.geom.crop_w,
                                    bottom: self.geom.crop_y + self.geom.crop_h,
                                    back: 1,
                                };
                                self.d3d_ctx.CopySubresourceRegion(
                                    &self.staging,
                                    0,
                                    0,
                                    0,
                                    0,
                                    &tex,
                                    0,
                                    Some(&src_box),
                                );
                                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                                if self
                                    .d3d_ctx
                                    .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                                    .is_ok()
                                {
                                    let row_bytes = (self.geom.crop_w * 4) as usize;
                                    let pitch = mapped.RowPitch as usize;
                                    bgra_out.resize(row_bytes * self.geom.crop_h as usize, 0);
                                    let src = mapped.pData as *const u8;
                                    for y in 0..self.geom.crop_h as usize {
                                        std::ptr::copy_nonoverlapping(
                                            src.add(y * pitch),
                                            bgra_out.as_mut_ptr().add(y * row_bytes),
                                            row_bytes,
                                        );
                                    }
                                    self.d3d_ctx.Unmap(&self.staging, 0);
                                    got = true;
                                }
                            }
                        }
                    }
                }
            }
            let _ = frame.Close();
        }
        got
    }
}

impl Drop for WgcGrabber {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Converts tightly-packed top-down BGRA (`src_w x src_h`) into top-down NV12 (`dst_w x dst_h`, BT.601 studio range).
fn bgra_to_nv12(bgra: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32, nv12: &mut Vec<u8>) {
    let (sw, sh, dw, dh) = (src_w as usize, src_h as usize, dst_w as usize, dst_h as usize);
    let y_size = dw * dh;
    let uv_size = dw * (dh / 2);
    nv12.resize(y_size + uv_size, 0);
    let (y_plane, uv_plane) = nv12.split_at_mut(y_size);
    let same = sw == dw && sh == dh;

    for dy in 0..dh {
        let sy = if same { dy } else { (dy * sh) / dh };
        let src_row = &bgra[sy * sw * 4..(sy + 1) * sw * 4];
        let y_row = &mut y_plane[dy * dw..(dy + 1) * dw];
        for dx in 0..dw {
            let sx = if same { dx } else { (dx * sw) / dw };
            let p = sx * 4;
            let b = src_row[p] as i32;
            let g = src_row[p + 1] as i32;
            let r = src_row[p + 2] as i32;
            y_row[dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(16, 235) as u8;
        }
    }

    for dy2 in 0..(dh / 2) {
        let dy = dy2 * 2;
        let sy = if same { dy } else { (dy * sh) / dh };
        let src_row = &bgra[sy * sw * 4..(sy + 1) * sw * 4];
        let uv_row = &mut uv_plane[dy2 * dw..(dy2 + 1) * dw];
        for dx2 in 0..(dw / 2) {
            let dx = dx2 * 2;
            let sx = if same { dx } else { (dx * sw) / dw };
            let p = sx * 4;
            let b = src_row[p] as i32;
            let g = src_row[p + 1] as i32;
            let r = src_row[p + 2] as i32;
            let u = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
            let v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
            uv_row[dx] = u;
            uv_row[dx + 1] = v;
        }
    }
}

struct Mp4Writer {
    writer: IMFSinkWriter,
    v_stream: u32,
    mic_stream: Option<u32>,
    sys_stream: Option<u32>,
}

impl Mp4Writer {
    fn open(path: &Path, out_w: u32, out_h: u32, fps: u32, want_mic: bool, want_sys: bool) -> Result<Self, String> {
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 2).map_err(|e| format!("MFCreateAttributes: {e}"))?;
            let attrs = attrs.ok_or("no IMFAttributes")?;
            let _ = attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1);
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let plain = plain_path(path);
            let wide: Vec<u16> = plain
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let writer = MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, &attrs)
                .map_err(|e| format!("MFCreateSinkWriterFromURL: {e}"))?;

            let pixels = (out_w as u64) * (out_h as u64);
            let bitrate = ((pixels * fps as u64 / 12).clamp(1_500_000, 8_000_000)) as u32;

            let v_out = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
            v_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
            v_out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| e.to_string())?;
            v_out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).map_err(|e| e.to_string())?;
            v_out
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| e.to_string())?;
            v_out
                .SetUINT64(&MF_MT_FRAME_SIZE, ((out_w as u64) << 32) | (out_h as u64))
                .map_err(|e| e.to_string())?;
            v_out
                .SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)
                .map_err(|e| e.to_string())?;
            v_out
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
                .map_err(|e| e.to_string())?;
            let v_stream = writer.AddStream(&v_out).map_err(|e| format!("AddStream H264: {e}"))?;

            let v_in = MFCreateMediaType().map_err(|e| e.to_string())?;
            v_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
            v_in.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| e.to_string())?;
            v_in.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| e.to_string())?;
            v_in.SetUINT32(&MF_MT_DEFAULT_STRIDE, out_w).map_err(|e| e.to_string())?;
            v_in.SetUINT64(&MF_MT_FRAME_SIZE, ((out_w as u64) << 32) | (out_h as u64))
                .map_err(|e| e.to_string())?;
            v_in.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)
                .map_err(|e| e.to_string())?;
            v_in.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
                .map_err(|e| e.to_string())?;
            writer
                .SetInputMediaType(v_stream, &v_in, None)
                .map_err(|e| format!("SetInputMediaType NV12: {e}"))?;

            let mic_stream = if want_mic { add_aac_stream(&writer).ok() } else { None };
            let sys_stream = if want_sys { add_aac_stream(&writer).ok() } else { None };

            writer.BeginWriting().map_err(|e| format!("BeginWriting: {e}"))?;
            Ok(Self {
                writer,
                v_stream,
                mic_stream,
                sys_stream,
            })
        }
    }

    fn write_sample(&self, stream: u32, bytes: &[u8], pts_hns: i64, dur_hns: i64) -> Result<(), String> {
        unsafe {
            let buf = MFCreateMemoryBuffer(bytes.len() as u32).map_err(|e| e.to_string())?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None).map_err(|e| e.to_string())?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            buf.Unlock().map_err(|e| e.to_string())?;
            buf.SetCurrentLength(bytes.len() as u32).map_err(|e| e.to_string())?;

            let sample = MFCreateSample().map_err(|e| e.to_string())?;
            sample.AddBuffer(&buf).map_err(|e| e.to_string())?;
            sample.SetSampleTime(pts_hns).map_err(|e| e.to_string())?;
            sample.SetSampleDuration(dur_hns.max(1)).map_err(|e| e.to_string())?;
            self.writer
                .WriteSample(stream, &sample)
                .map_err(|e| format!("WriteSample({stream}): {e}"))
        }
    }

    fn finalize(self) -> Result<(), String> {
        unsafe { self.writer.Finalize().map_err(|e| format!("Finalize: {e}")) }
    }
}

unsafe fn add_aac_stream(writer: &IMFSinkWriter) -> Result<u32, String> {
    let a_out = MFCreateMediaType().map_err(|e| e.to_string())?;
    a_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(|e| e.to_string())?;
    a_out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).map_err(|e| e.to_string())?;
    a_out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SR).map_err(|e| e.to_string())?;
    a_out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1).map_err(|e| e.to_string())?;
    a_out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(|e| e.to_string())?;
    a_out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000).map_err(|e| e.to_string())?;
    let idx = writer.AddStream(&a_out).map_err(|e| e.to_string())?;

    let a_in = MFCreateMediaType().map_err(|e| e.to_string())?;
    a_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(|e| e.to_string())?;
    a_in.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).map_err(|e| e.to_string())?;
    a_in.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SR).map_err(|e| e.to_string())?;
    a_in.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1).map_err(|e| e.to_string())?;
    a_in.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(|e| e.to_string())?;
    a_in.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2).map_err(|e| e.to_string())?;
    a_in.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_SR * 2).map_err(|e| e.to_string())?;
    writer.SetInputMediaType(idx, &a_in, None).map_err(|e| e.to_string())?;
    Ok(idx)
}

fn resample_linear(input: &[f32], in_sr: u32, out_sr: u32) -> Vec<i16> {
    if input.is_empty() {
        return Vec::new();
    }
    if in_sr == out_sr {
        return input
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
    }
    let out_len = ((input.len() as u64 * out_sr as u64) / in_sr as u64) as usize;
    let step = in_sr as f64 / out_sr as f64;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * step;
        let idx = pos as usize;
        let frac = (pos - idx as f64) as f32;
        let a = input[idx.min(input.len() - 1)];
        let b = input[(idx + 1).min(input.len() - 1)];
        let v = a + (b - a) * frac;
        out.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
    }
    out
}

enum AudioPacket {
    Mic { pcm48: Vec<i16>, pcm16: Vec<i16>, rms: f64 },
    Sys { pcm48: Vec<i16> },
}

fn spawn_audio_thread(
    mic: Mic,
    want_sys: bool,
    gate: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    tx: mpsc::Sender<AudioPacket>,
) -> (bool, bool, JoinHandle<()>) {
    let (ready_tx, ready_rx) = mpsc::channel::<(bool, bool)>();
    let h = thread::spawn(move || {
        let host = cpal::default_host();
        let mic_stream = match &mic {
            Mic::None => None,
            Mic::Default => host.default_input_device().and_then(|d| open_cpal_input(&d, true, &gate, &tx)),
            Mic::Named(want) => {
                let low = want.to_lowercase();
                let picked = host
                    .input_devices()
                    .ok()
                    .and_then(|mut ds| ds.find(|d| d.name().is_ok_and(|n| n.to_lowercase().contains(&low))))
                    .or_else(|| host.default_input_device());
                picked.and_then(|d| open_cpal_input(&d, true, &gate, &tx))
            }
        };
        let sys_stream = if want_sys {
            host.default_output_device()
                .and_then(|d| open_cpal_input(&d, false, &gate, &tx))
        } else {
            None
        };
        let _ = ready_tx.send((mic_stream.is_some(), sys_stream.is_some()));
        while !stop_flag.load(SeqCst) {
            thread::sleep(Duration::from_millis(40));
        }
        drop(mic_stream);
        drop(sys_stream);
    });
    let (has_mic, has_sys) = ready_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap_or((false, false));
    (has_mic, has_sys, h)
}

fn open_cpal_input(
    dev: &cpal::Device,
    is_mic: bool,
    gate: &Arc<AtomicBool>,
    tx: &mpsc::Sender<AudioPacket>,
) -> Option<cpal::Stream> {
    let cfg = if is_mic {
        dev.default_input_config().ok()?
    } else {
        dev.default_output_config().ok()?
    };
    let sr = cfg.sample_rate().0;
    let ch = cfg.channels().max(1) as usize;
    let stream_cfg: cpal::StreamConfig = cfg.clone().into();
    let gate = gate.clone();
    let tx = tx.clone();
    let err = |_| {};

    macro_rules! build {
        ($t:ty, $conv:expr) => {{
            dev.build_input_stream(
                &stream_cfg,
                move |data: &[$t], _| {
                    if !gate.load(SeqCst) || data.is_empty() {
                        return;
                    }
                    let mono: Vec<f32> = data
                        .chunks(ch)
                        .map(|f| f.iter().map(|&s| $conv(s)).sum::<f32>() / ch as f32)
                        .collect();
                    let pcm48 = resample_linear(&mono, sr, AUDIO_SR);
                    if is_mic {
                        let pcm16 = resample_linear(&mono, sr, LIVE_SR);
                        let sq: f64 = mono.iter().map(|&v| (v as f64) * (v as f64)).sum();
                        let rms = if mono.is_empty() { 0.0 } else { (sq / mono.len() as f64).sqrt() };
                        let _ = tx.send(AudioPacket::Mic { pcm48, pcm16, rms });
                    } else {
                        let _ = tx.send(AudioPacket::Sys { pcm48 });
                    }
                },
                err,
                None,
            )
            .ok()
        }};
    }

    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => build!(f32, |s: f32| s),
        cpal::SampleFormat::I16 => build!(i16, |s: i16| s as f32 / 32768.0),
        cpal::SampleFormat::U16 => build!(u16, |s: u16| (s as f32 - 32768.0) / 32768.0),
        cpal::SampleFormat::I32 => build!(i32, |s: i32| s as f32 / 2147483648.0),
        _ => None,
    }?;
    stream.play().ok()?;
    Some(stream)
}

fn take_commands(cmd_file: &Path) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(cmd_file) else {
        return Vec::new();
    };
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let taken = cmd_file.with_file_name(".cmd.taken");
    if fs::rename(cmd_file, &taken).is_err() {
        return Vec::new();
    }
    let out = fs::read_to_string(&taken)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let _ = fs::remove_file(&taken);
    out
}

fn write_bgra_png(path: &Path, bgra: &[u8], w: u32, h: u32) -> Result<(), String> {
    if bgra.len() < (w as usize) * (h as usize) * 4 {
        return Err("short bgra buffer".into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for px in bgra[..(w as usize) * (h as usize) * 4].chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())
}

fn grab_desktop_png(path: &Path, x: i32, y: i32, w: u32, h: u32) -> Result<(), String> {
    unsafe {
        let screen_dc = GetDC(Some(HWND::default()));
        if screen_dc.0.is_null() {
            return Err("GetDC failed".into());
        }
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let bmp = CreateCompatibleBitmap(screen_dc, w as i32, h as i32);
        if mem_dc.0.is_null() || bmp.0.is_null() {
            if !bmp.0.is_null() {
                let _ = DeleteObject(bmp.into());
            }
            if !mem_dc.0.is_null() {
                let _ = DeleteDC(mem_dc);
            }
            let _ = ReleaseDC(Some(HWND::default()), screen_dc);
            return Err("CreateCompatibleBitmap failed".into());
        }
        let old = SelectObject(mem_dc, bmp.into());
        let ok = BitBlt(mem_dc, 0, 0, w as i32, h as i32, Some(screen_dc), x, y, SRCCOPY).is_ok();
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
        let lines = if ok {
            GetDIBits(
                mem_dc,
                bmp,
                0,
                h,
                Some(bgra.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };
        let _ = SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(Some(HWND::default()), screen_dc);
        if lines == 0 {
            return Err("GetDIBits failed".into());
        }
        write_bgra_png(path, &bgra, w, h)
    }
}

fn spawn_live_py(opts: &CaptureOptions, session: &Path) -> Option<Child> {
    let home = blurt_home(opts)?;
    if let Ok(raw) = fs::read_to_string(home.join("config.json")) {
        if let Ok(cfg) = serde_json::from_str::<Value>(&raw) {
            if cfg.pointer("/record/live").and_then(Value::as_bool) == Some(false) {
                return None;
            }
        }
    }
    let record_py = opts.launcher.args.last().map(PathBuf::from)?;
    let live_py = record_py.parent()?.join("live.py");
    if !live_py.is_file() {
        return None;
    }
    let uv = if opts.launcher.program.to_string_lossy().to_lowercase().contains("uv") {
        opts.launcher.program.clone()
    } else {
        home.join("bin").join("uv.exe")
    };
    if !uv.is_file() {
        return None;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(session.join("live.log"))
        .ok()?;
    let log2 = log.try_clone().ok()?;
    let mut cmd = Command::new(uv);
    cmd.args([
        "run",
        "-q",
        &plain_path(&live_py).to_string_lossy(),
        "run",
        &plain_path(session).to_string_lossy(),
        "--watch-pid",
        &std::process::id().to_string(),
    ])
    .envs(opts.env.iter().map(|(k, v)| (k, v)))
    .env("PYTHONIOENCODING", "utf-8")
    .env("PYTHONUTF8", "1")
    .stdin(Stdio::null())
    .stdout(log)
    .stderr(log2);
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0200 | 0x0800_0000);
    }
    cmd.spawn().ok()
}

/// Starts the native WGC + Media Foundation recorder on a background thread.
/// Returns `Err` immediately if WGC or hardware/software Media Foundation sink setup fails, allowing `engine::start`
/// to fall back cleanly to `record.py --engine ffmpeg`.
pub fn start_native(
    opts: &CaptureOptions,
    mut emit: Box<dyn FnMut(StatusEvent) + Send>,
) -> Result<NativeSession, String> {
    let max_width = opts.max_width.unwrap_or(1920);
    let geom = resolve_geometry(opts.region, opts.screen, max_width)?;

    let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();
    let done = Arc::new(AtomicBool::new(false));
    let kill_flag = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();
    let kill2 = kill_flag.clone();
    let opts = opts.clone();

    let worker = thread::Builder::new()
        .name("blurt-wgc".into())
        .spawn(move || {
            let _com = ComGuard::init();
            let grabber = match WgcGrabber::open(geom) {
                Ok(g) => {
                    let _ = init_tx.send(Ok(()));
                    g
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    done2.store(true, SeqCst);
                    return;
                }
            };
            run_session(opts, geom, grabber, &kill2, &mut *emit);
            done2.store(true, SeqCst);
        })
        .map_err(|e| e.to_string())?;

    match init_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(NativeSession {
            done,
            kill_flag,
            worker: Some(worker),
        }),
        Ok(Err(e)) => {
            let _ = worker.join();
            Err(e)
        }
        Err(_) => Err("WGC initialization timed out".into()),
    }
}

fn wall_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn run_session(
    opts: CaptureOptions,
    geom: TargetGeometry,
    grabber: WgcGrabber,
    kill_flag: &AtomicBool,
    emit: &mut dyn FnMut(StatusEvent),
) {
    let session = plain_path(&opts.session);
    let cmd_file = session.join(".cmd");
    let _ = fs::remove_file(&cmd_file);
    let started_iso = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let state_file = blurt_home(&opts).map(|h| h.join("recording.json"));
    if let Some(sf) = &state_file {
        if let Some(p) = sf.parent() {
            let _ = fs::create_dir_all(p);
        }
        let _ = fs::write(
            sf,
            json!({
                "pid": std::process::id(),
                "session": session.to_string_lossy(),
                "engine": "wgc",
                "started_at": started_iso,
            })
            .to_string(),
        );
    }

    // 1. Optional 3-2-1 countdown before opening the MP4 file
    if let Some(total) = opts.countdown.filter(|&n| n > 0) {
        let end = Instant::now() + Duration::from_secs(total as u64);
        let mut shown = u32::MAX;
        loop {
            if kill_flag.load(SeqCst) {
                cleanup_state(&state_file, &cmd_file);
                emit(StatusEvent::Discarded);
                return;
            }
            let now = Instant::now();
            if now >= end {
                break;
            }
            let left = end.duration_since(now).as_secs_f64();
            let k = left.ceil() as u32;
            if k != shown && k > 0 {
                shown = k;
                emit(StatusEvent::Other(json!({"ev": "countdown", "n": k, "total": total})));
            }
            let cmds = take_commands(&cmd_file);
            if cmds.iter().any(|c| c == "stop" || c == "discard") {
                emit(StatusEvent::Other(json!({"ev": "countdown", "n": 0, "total": total})));
                cleanup_state(&state_file, &cmd_file);
                emit(StatusEvent::Discarded);
                return;
            }
            if cmds.iter().any(|c| c == "go") {
                break;
            }
            thread::sleep(Duration::from_millis(40));
        }
        emit(StatusEvent::Other(json!({"ev": "countdown", "n": 0, "total": total})));
    }

    // 2. Wait for the first WGC frame (up to 5 s)
    let mut latest_bgra = Vec::new();
    let t_wait = Instant::now();
    while t_wait.elapsed() < Duration::from_secs(5) {
        if grabber.poll_frame(&mut latest_bgra) && !latest_bgra.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
    if latest_bgra.is_empty() {
        drop(grabber);
        cleanup_state(&state_file, &cmd_file);
        emit(StatusEvent::Error {
            msg: "Windows Graphics Capture delivered no frame".into(),
            code: Some("no_frames".into()),
        });
        return;
    }

    // 3. Start audio capture + MP4 sink writer
    let audio_gate = Arc::new(AtomicBool::new(false));
    let audio_stop = Arc::new(AtomicBool::new(false));
    let (audio_tx, audio_rx) = mpsc::channel::<AudioPacket>();
    let (has_mic, has_sys, audio_thread) = spawn_audio_thread(
        opts.mic.clone(),
        opts.system_audio,
        audio_gate.clone(),
        audio_stop.clone(),
        audio_tx,
    );

    let fps = opts.fps.unwrap_or(30).clamp(5, 60);
    let mp4_path = session.join("recording.mp4");
    let mp4 = match Mp4Writer::open(&mp4_path, geom.out_w, geom.out_h, fps, has_mic, has_sys) {
        Ok(w) => w,
        Err(e) => {
            audio_stop.store(true, SeqCst);
            let _ = audio_thread.join();
            drop(grabber);
            cleanup_state(&state_file, &cmd_file);
            emit(StatusEvent::Error {
                msg: e,
                code: Some("mux".into()),
            });
            return;
        }
    };

    // Live caption sidecar (`live/audio.pcm` 16 kHz mono s16le)
    let mut live_pcm: Option<BufWriter<File>> = if has_mic {
        let ldir = session.join("live");
        let _ = fs::create_dir_all(&ldir);
        let _ = fs::remove_file(ldir.join("stop"));
        File::create(ldir.join("audio.pcm")).ok().map(BufWriter::new)
    } else {
        None
    };
    let mut live_proc = if live_pcm.is_some() {
        spawn_live_py(&opts, &session)
    } else {
        None
    };

    // 4. Roll!
    let t0_wall = wall_now();
    let rec_start = Instant::now();
    audio_gate.store(true, SeqCst);
    emit(StatusEvent::Started { t0: round3(t0_wall) });

    let mut nv12_buf = Vec::new();
    bgra_to_nv12(
        &latest_bgra,
        geom.crop_w,
        geom.crop_h,
        geom.out_w,
        geom.out_h,
        &mut nv12_buf,
    );

    let frame_dur_hns = HNS_PER_SEC / (fps as i64);
    let _ = mp4.write_sample(mp4.v_stream, &nv12_buf, 0, frame_dur_hns);
    let mut frames_written: u64 = 1;
    let mut mic_samples_written: u64 = 0;
    let mut sys_samples_written: u64 = 0;

    let mut paused = false;
    let mut pause_started: Option<Instant> = None;
    let mut total_paused = Duration::ZERO;
    let mut discard = false;
    let mut shots: u32 = 0;
    let mut events: Vec<Value> = Vec::new();

    let mut last_level_emit = Instant::now();
    let mut last_live_flush = Instant::now();
    let mut last_cmd_poll = Instant::now();
    let mut last_cursor_emit = Instant::now();
    let mut last_cursor_rel: Option<(f64, f64)> = None;
    let mut mouse_held = [false, false];
    let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) != 0 };
    let max_dur = opts.max_minutes.map(|m| Duration::from_secs_f64((m * 60.0).max(1.0)));

    let active_elapsed = |now: Instant, paused: bool, pause_started: Option<Instant>, total_paused: Duration| -> Duration {
        let p = total_paused + if paused { pause_started.map_or(Duration::ZERO, |s| now.saturating_duration_since(s)) } else { Duration::ZERO };
        now.saturating_duration_since(rec_start).saturating_sub(p)
    };

    loop {
        if kill_flag.load(SeqCst) {
            discard = true;
            break;
        }
        let now = Instant::now();
        let active = active_elapsed(now, paused, pause_started, total_paused);
        if max_dur.is_some_and(|m| active >= m) {
            break;
        }

        // Poll commands every ~60 ms
        if last_cmd_poll.elapsed() >= Duration::from_millis(60) {
            last_cmd_poll = now;
            let cmds = take_commands(&cmd_file);
            if cmds.iter().any(|c| c == "stop" || c == "discard") {
                discard = cmds.iter().any(|c| c == "discard");
                break;
            }
            for c in cmds {
                let t_sec = round3(active_elapsed(Instant::now(), paused, pause_started, total_paused).as_secs_f64());
                if (c == "pause" || c == "toggle") && !paused {
                    paused = true;
                    audio_gate.store(false, SeqCst);
                    pause_started = Some(Instant::now());
                    events.push(json!({"t": t_sec, "type": "pause"}));
                    emit(StatusEvent::Paused { t: Some(t_sec) });
                } else if (c == "resume" || c == "toggle") && paused {
                    if let Some(ps) = pause_started.take() {
                        total_paused += Instant::now().saturating_duration_since(ps);
                    }
                    paused = false;
                    audio_gate.store(true, SeqCst);
                    let t_res = round3(active_elapsed(Instant::now(), false, None, total_paused).as_secs_f64());
                    events.push(json!({"t": t_res, "type": "resume"}));
                    emit(StatusEvent::Resumed { t: Some(t_res) });
                } else if c == "snap" || c == "marker" {
                    if c == "marker" {
                        events.push(json!({"t": t_sec, "type": "marker"}));
                    }
                    shots += 1;
                    let n = shots;
                    let rel_file = format!("shots/{n:03}.png");
                    let png_path = session.join(&rel_file);
                    let via = if c == "marker" { "marker" } else { "command" };
                    events.push(json!({"t": t_sec, "type": "snap", "n": n, "file": rel_file, "via": via}));
                    let _ = grabber.poll_frame(&mut latest_bgra);
                    match write_bgra_png(&png_path, &latest_bgra, geom.crop_w, geom.crop_h) {
                        Ok(()) => {
                            append_shot_line(
                                &session,
                                n,
                                t_sec,
                                [geom.desk_x, geom.desk_y, geom.crop_w as i32, geom.crop_h as i32],
                            );
                            emit(StatusEvent::Snap {
                                n,
                                file: rel_file,
                                t: Some(t_sec),
                            });
                        }
                        Err(msg) => emit(StatusEvent::Error {
                            msg,
                            code: Some("snap".into()),
                        }),
                    }
                } else if let Some(rest) = c.strip_prefix("snap-region:") {
                    let nums: Vec<i32> = rest
                        .split(',')
                        .filter_map(|v| v.trim().parse::<f64>().ok().map(|f| f as i32))
                        .collect();
                    if nums.len() == 4 {
                        let (rx, ry, rw, rh) = (nums[0], nums[1], nums[2].max(2) as u32, nums[3].max(2) as u32);
                        shots += 1;
                        let n = shots;
                        let rel_file = format!("shots/{n:03}.png");
                        let png_path = session.join(&rel_file);
                        events.push(json!({"t": t_sec, "type": "snap", "n": n, "file": rel_file, "via": "picker"}));
                        match grab_desktop_png(&png_path, rx, ry, rw, rh) {
                            Ok(()) => {
                                append_shot_line(&session, n, t_sec, [rx, ry, rw as i32, rh as i32]);
                                emit(StatusEvent::Snap {
                                    n,
                                    file: rel_file,
                                    t: Some(t_sec),
                                });
                            }
                            Err(msg) => emit(StatusEvent::Error {
                                msg,
                                code: Some("snap".into()),
                            }),
                        }
                    } else {
                        emit(StatusEvent::Error {
                            msg: format!("bad {c}"),
                            code: Some("snap".into()),
                        });
                    }
                } else if let Some(rest) = c.strip_prefix("stroke:") {
                    if let Ok(o) = serde_json::from_str::<Value>(rest) {
                        let dur = o.get("dur").and_then(Value::as_f64).unwrap_or(0.0);
                        events.push(json!({
                            "t": t_sec,
                            "type": "stroke",
                            "t0": round3((t_sec - dur).max(0.0)),
                            "visible": 3.0,
                            "shape": o.get("shape"),
                            "points": o.get("points"),
                        }));
                    }
                }
            }
        }

        // Drain audio packets
        while let Ok(pkt) = audio_rx.try_recv() {
            if paused {
                continue;
            }
            match pkt {
                AudioPacket::Mic { pcm48, pcm16, rms } => {
                    if last_level_emit.elapsed() >= Duration::from_millis(100) {
                        last_level_emit = Instant::now();
                        emit(StatusEvent::Level {
                            rms: (rms * 10000.0).round() / 10000.0,
                        });
                    }
                    if let Some(ms) = mp4.mic_stream {
                        if !pcm48.is_empty() {
                            let pts = (mic_samples_written as i64 * HNS_PER_SEC) / AUDIO_SR as i64;
                            let dur = (pcm48.len() as i64 * HNS_PER_SEC) / AUDIO_SR as i64;
                            let bytes: Vec<u8> = pcm48.iter().flat_map(|s| s.to_le_bytes()).collect();
                            let _ = mp4.write_sample(ms, &bytes, pts, dur);
                            mic_samples_written += pcm48.len() as u64;
                        }
                    }
                    if let Some(lp) = live_pcm.as_mut() {
                        let bytes: Vec<u8> = pcm16.iter().flat_map(|s| s.to_le_bytes()).collect();
                        let _ = lp.write_all(&bytes);
                        if last_live_flush.elapsed() >= Duration::from_millis(100) {
                            let _ = lp.flush();
                            last_live_flush = Instant::now();
                        }
                    }
                }
                AudioPacket::Sys { pcm48 } => {
                    if let Some(ss) = mp4.sys_stream {
                        if !pcm48.is_empty() {
                            let pts = (sys_samples_written as i64 * HNS_PER_SEC) / AUDIO_SR as i64;
                            let dur = (pcm48.len() as i64 * HNS_PER_SEC) / AUDIO_SR as i64;
                            let bytes: Vec<u8> = pcm48.iter().flat_map(|s| s.to_le_bytes()).collect();
                            let _ = mp4.write_sample(ss, &bytes, pts, dur);
                            sys_samples_written += pcm48.len() as u64;
                        }
                    }
                }
            }
        }

        if !paused {
            // Poll pointer & mouse clicks (~20 ms loop)
            unsafe {
                let mut pt = POINT::default();
                if GetCursorPos(&mut pt).is_ok() {
                    let inside = pt.x >= geom.desk_x
                        && pt.x < geom.desk_x + geom.crop_w as i32
                        && pt.y >= geom.desk_y
                        && pt.y < geom.desk_y + geom.crop_h as i32;
                    let rel = (
                        round3((pt.x - geom.desk_x) as f64 / geom.crop_w as f64),
                        round3((pt.y - geom.desk_y) as f64 / geom.crop_h as f64),
                    );
                    let t_sec = round3(active.as_secs_f64());
                    for (idx, vk) in [(0usize, 1i32), (1usize, 2i32)] {
                        let down = (GetAsyncKeyState(vk) as u16 & 0x8000) != 0;
                        if down && !mouse_held[idx] && inside {
                            let btn = if (vk == 1) != swapped { "left" } else { "right" };
                            events.push(json!({"t": t_sec, "type": "click", "x": rel.0, "y": rel.1, "button": btn}));
                        }
                        mouse_held[idx] = down;
                    }
                    if inside
                        && Some(rel) != last_cursor_rel
                        && last_cursor_emit.elapsed() >= Duration::from_millis(100)
                    {
                        events.push(json!({"t": t_sec, "type": "cursor", "x": rel.0, "y": rel.1}));
                        last_cursor_rel = Some(rel);
                        last_cursor_emit = Instant::now();
                    }
                }
            }

            // Video frame pacing at target `fps`
            if grabber.poll_frame(&mut latest_bgra) {
                bgra_to_nv12(
                    &latest_bgra,
                    geom.crop_w,
                    geom.crop_h,
                    geom.out_w,
                    geom.out_h,
                    &mut nv12_buf,
                );
            }
            let target_frames = ((active.as_secs_f64() * fps as f64).floor() as u64) + 1;
            while frames_written < target_frames {
                let pts = (frames_written as i64 * HNS_PER_SEC) / fps as i64;
                let _ = mp4.write_sample(mp4.v_stream, &nv12_buf, pts, frame_dur_hns);
                frames_written += 1;
            }
        } else {
            // Keep draining WGC pool while paused so the surface queue never stalls
            let _ = grabber.poll_frame(&mut latest_bgra);
        }

        thread::sleep(Duration::from_millis(16));
    }

    // 5. Tear down & finalize
    audio_gate.store(false, SeqCst);
    audio_stop.store(true, SeqCst);
    let _ = audio_thread.join();
    drop(grabber);

    let final_dur = active_elapsed(Instant::now(), paused, pause_started, total_paused).as_secs_f64();
    if !paused {
        events.push(json!({"t": round3(final_dur), "type": "stop"}));
    }

    if let Some(mut lp) = live_pcm.take() {
        let _ = lp.flush();
    }

    if discard {
        let _ = mp4.finalize();
        let _ = fs::remove_file(&mp4_path);
        if let Some(mut p) = live_proc.take() {
            let _ = p.kill();
        }
        let _ = fs::remove_dir_all(session.join("live"));
        cleanup_temp_audio(&session);
        cleanup_state(&state_file, &cmd_file);
        emit(StatusEvent::Discarded);
        return;
    }

    if let Err(e) = mp4.finalize() {
        if let Some(mut p) = live_proc.take() {
            let _ = p.kill();
        }
        cleanup_temp_audio(&session);
        cleanup_state(&state_file, &cmd_file);
        emit(StatusEvent::Error {
            msg: e,
            code: Some("mux".into()),
        });
        return;
    }

    // Signal `live/stop` and briefly wait for `live.py` if running
    if let Some(mut proc) = live_proc.take() {
        let stop_marker = session.join("live").join("stop");
        let _ = File::create(&stop_marker);
        let t_live = Instant::now();
        while t_live.elapsed() < Duration::from_secs(6) {
            if proc.try_wait().ok().flatten().is_some() {
                break;
            }
            if let Ok(st) = fs::read_to_string(session.join("live").join("status.json")) {
                if let Ok(v) = serde_json::from_str::<Value>(&st) {
                    if matches!(v["asr"].as_str(), Some("done" | "off" | "error")) {
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(150));
        }
    } else {
        let _ = fs::remove_dir_all(session.join("live"));
    }

    // Write events.jsonl
    if let Ok(mut fh) = File::create(session.join("events.jsonl")) {
        for ev in &events {
            let _ = writeln!(fh, "{ev}");
        }
    }

    // Write meta.json
    let region_arr = [geom.desk_x, geom.desk_y, geom.crop_w as i32, geom.crop_h as i32];
    let dur = round3(final_dur.max(frames_written as f64 / fps as f64));
    let meta = json!({
        "started_at": started_iso,
        "engine": "wgc",
        "video": "recording.mp4",
        "duration": dur,
        "segments": 1,
        "av_offset": 0.0,
        "system_audio": has_sys,
        "region": region_arr,
    });
    let _ = fs::write(
        session.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );

    cleanup_temp_audio(&session);
    cleanup_state(&state_file, &cmd_file);

    emit(StatusEvent::Stopped {
        video: "recording.mp4".into(),
        duration: Some(dur),
        shots: Some(shots),
    });
}

fn append_shot_line(session: &Path, n: u32, t: f64, region: [i32; 4]) {
    let line = json!({
        "n": n,
        "file": format!("shots/{n:03}.png"),
        "marked": Value::Null,
        "t": t,
        "wall": round3(wall_now()),
        "region": region,
        "voice": Value::Null,
        "text": Value::Null,
        "strokes": [],
    });
    if let Ok(mut fh) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(session.join("shots.jsonl"))
    {
        let _ = writeln!(fh, "{line}");
    }
}

fn cleanup_temp_audio(session: &Path) {
    for f in ["system.pcm", "system.json"] {
        let _ = fs::remove_file(session.join(f));
    }
}

fn cleanup_state(state_file: &Option<PathBuf>, cmd_file: &Path) {
    if let Some(sf) = state_file {
        let _ = fs::remove_file(sf);
    }
    let _ = fs::remove_file(cmd_file);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_dimensions_and_luma_bounds() {
        // 4x4 pure white BGRA -> NV12 (Y=235, U=128, V=128)
        let bgra = vec![255u8; 4 * 4 * 4];
        let mut nv12 = Vec::new();
        bgra_to_nv12(&bgra, 4, 4, 4, 4, &mut nv12);
        assert_eq!(nv12.len(), 4 * 4 + 4 * 2);
        assert!(nv12[..16].iter().all(|&y| y == 235));
        assert!(nv12[16..].iter().all(|&uv| uv == 128));
    }

    #[test]
    fn nv12_downscale() {
        let bgra = vec![0u8; 8 * 6 * 4];
        let mut nv12 = Vec::new();
        bgra_to_nv12(&bgra, 8, 6, 4, 2, &mut nv12);
        assert_eq!(nv12.len(), 4 * 2 + 4 * 1);
        assert!(nv12[..8].iter().all(|&y| y == 16));
    }

    #[test]
    fn wgc_live_record_snap_pause_resume_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config.json"), r#"{"record":{"live":false}}"#).unwrap();

        let record_py = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills/blurt/scripts/record.py");
        let mut opts = CaptureOptions::new(crate::engine::Launcher::python("python", record_py), tmp.path().join("session"));
        opts.region = Some(Region::new(0, 0, 320, 240));
        opts.mic = Mic::None;
        opts.fps = Some(15);
        opts.countdown = Some(0);
        opts.env.push(("BLURT_HOME".into(), home.into_os_string()));

        assert!(should_use_native(&opts));
        let (tx, rx) = mpsc::channel();
        let mut h = match crate::engine::start(&opts, move |e| {
            let _ = tx.send(e.clone());
        }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("skipped headless: {e}");
                return;
            }
        };

        // Wait for Started
        let started = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(started, StatusEvent::Started { .. }), "got {started:?}");
        thread::sleep(Duration::from_millis(400));

        // Snap
        h.snap().unwrap();
        let snap_ev = loop {
            let e = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if matches!(e, StatusEvent::Snap { .. }) {
                break e;
            }
        };
        assert!(matches!(snap_ev, StatusEvent::Snap { n: 1, .. }));

        // Pause & Resume
        h.pause().unwrap();
        loop {
            if matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), StatusEvent::Paused { .. }) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(200));
        h.resume().unwrap();
        loop {
            if matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), StatusEvent::Resumed { .. }) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(400));

        let fin = h.stop_and_wait(Duration::from_secs(15)).unwrap();
        let StatusEvent::Stopped { video, duration, shots } = fin else {
            panic!("expected Stopped, got {fin:?}");
        };
        assert_eq!(video, "recording.mp4");
        assert_eq!(shots, Some(1));
        assert!(duration.unwrap_or(0.0) >= 0.6);

        let s = h.session();
        let mp4_len = fs::metadata(s.join("recording.mp4")).unwrap().len();
        assert!(mp4_len > 1024, "mp4_len={mp4_len}");
        assert!(s.join("shots/001.png").is_file());
        assert!(s.join("shots.jsonl").is_file());
        assert!(s.join("events.jsonl").is_file());
        let meta: Value = serde_json::from_str(&fs::read_to_string(s.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["engine"], json!("wgc"));
    }
}
