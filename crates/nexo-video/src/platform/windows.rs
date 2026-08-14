//! Windows backend: Media Foundation camera enumeration and hardware probing,
//! plus Windows Graphics Capture screen capture.
//!
//! # Safety
//!
//! This module is the only place in the workspace that calls native Windows
//! APIs, and is why `crate::Cargo.toml` overrides the workspace-wide
//! `unsafe_code = "forbid"`. All `unsafe` here is kept to single, tiny calls
//! bounded by the surrounding safe code:
//!
//! * Every entry point is a safe function and fully owns its resources. The
//!   [`MediaSession`] guard releases the COM/MF reference counts on drop, and
//!   each allocation from Media Foundation (device arrays, strings) is freed
//!   with `CoTaskMemFree` before returning.
//! * Pointer arithmetic is limited to `std::slice::from_raw_parts` over arrays
//!   whose length Media Foundation just reported; the code never trusts user
//!   input with these pointers.
//! * Screen capture hands out only owned copies: D3D11 frames are copied into
//!   a CPU-readable staging texture and row-by-row into a `Vec` before the
//!   frame/surface are released, so safe callers never see a native resource.
//!
//! Safe callers never see a raw pointer, so the rest of the crate (and
//! workspace) stays `unsafe`-free.

use std::time::{Duration, Instant};

use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
    IGraphicsCaptureItemStatics,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, LPARAM, RECT, RPC_E_CHANGED_MODE, S_OK, TRUE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION,
    MFCreateAttributes, MFCreateDeviceSource, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFSTARTUP_NOSOCKET, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_MJPG, MFVideoFormat_NV12, MFVideoFormat_YUY2,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::RoGetActivationFactory;
use windows::core::{BOOL, GUID, HSTRING, Interface, PCWSTR, PWSTR};

use crate::capture::{PixelFormat, VideoFrame};
use crate::devices::{VideoDeviceInfo, VideoError};
use crate::probe::{AccelerationApi, CaptureBackend, CodecCapability, MediaKind};

/// One video frame goes in as NV12; the encoder converts to `subtype` on the way out.
const NV12_INPUT: MFT_REGISTER_TYPE_INFO = MFT_REGISTER_TYPE_INFO {
    guidMajorType: MFMediaType_Video,
    guidSubtype: MFVideoFormat_NV12,
};

/// Stream index selecting the first video stream in a `IMFSourceReader`; the
/// native constant is a negative sentinel that must be cast to `u32`.
#[allow(clippy::cast_sign_loss)]
const FIRST_VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// COM + Media Foundation reference-count guard for the calling thread.
struct MediaSession {
    co_owned: bool,
    mf_owned: bool,
}

impl MediaSession {
    /// Initialize COM (MTA) and Media Foundation, tolerating a thread that is
    /// already initialized. Failure to acquire the session is reported through
    /// [`VideoError`].
    unsafe fn init() -> Result<Self, VideoError> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let co_owned = hr == S_OK;
            if hr.0 < 0 && hr != RPC_E_CHANGED_MODE {
                return Err(VideoError::platform(format!("CoInitializeEx: {hr}")));
            }

            match MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) {
                Ok(()) => Ok(Self {
                    co_owned,
                    mf_owned: true,
                }),
                Err(error) => {
                    if co_owned {
                        CoUninitialize();
                    }
                    Err(VideoError::platform(format!("MFStartup: {error}")))
                }
            }
        }
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        unsafe {
            if self.mf_owned {
                let _ = MFShutdown();
            }
            if self.co_owned {
                CoUninitialize();
            }
        }
    }
}

/// Enumerate cameras using Media Foundation's device source store.
pub(super) fn enumerate_cameras() -> Result<Vec<VideoDeviceInfo>, VideoError> {
    let _session = unsafe { MediaSession::init()? };

    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(std::ptr::addr_of_mut!(attributes), 1)
            .map_err(|error| VideoError::platform(format!("MFCreateAttributes: {error}")))?;
        let attributes = attributes
            .ok_or_else(|| VideoError::platform("MFCreateAttributes devolveu atributos nulos"))?;

        attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|error| VideoError::platform(format!("SetGUID: {error}")))?;

        let mut raw_devices: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(
            &attributes,
            std::ptr::addr_of_mut!(raw_devices),
            std::ptr::addr_of_mut!(count),
        )
        .map_err(|error| VideoError::platform(format!("MFEnumDeviceSources: {error}")))?;

        let devices = std::slice::from_raw_parts_mut(raw_devices, count as usize);
        let mut result = Vec::with_capacity(devices.len());
        for device in devices.iter_mut() {
            if let Some(activate) = device.take()
                && let Some(info) = read_device_info(&activate)
            {
                result.push(info);
            }
        }
        CoTaskMemFree(Some(raw_devices.cast()));
        Ok(result)
    }
}

/// Read friendly name and stable symbolic link from one activation object.
///
/// # Safety
///
/// `activate` must be a live `IMFActivate` returned by Media Foundation; the
/// caller owns it and releases it afterwards. Buffers produced by
/// `GetAllocatedString` are freed here.
unsafe fn read_device_info(activate: &IMFActivate) -> Option<VideoDeviceInfo> {
    unsafe {
        let name = read_pwstr(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?;
        let id = read_pwstr(
            activate,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        )
        .unwrap_or_else(|| name.clone());
        Some(VideoDeviceInfo { id, name })
    }
}

/// Read a `PWSTR` string attribute and free the buffer Media Foundation owned.
///
/// # Safety
///
/// `activate` must be a live `IMFActivate`; the returned string is a fully
/// owned `String`.
unsafe fn read_pwstr(activate: &IMFActivate, key: &GUID) -> Option<String> {
    unsafe {
        let mut buffer = PWSTR::null();
        let mut length = 0u32;
        if activate
            .GetAllocatedString(
                std::ptr::addr_of!(*key),
                std::ptr::addr_of_mut!(buffer),
                std::ptr::addr_of_mut!(length),
            )
            .is_err()
        {
            return None;
        }
        let text = buffer.to_string().unwrap_or_default();
        CoTaskMemFree(Some(buffer.as_ptr().cast()));
        Some(text)
    }
}

/// Live camera capture through an `IMFSourceReader`.
pub(crate) struct CaptureSource {
    _session: MediaSession,
    reader: IMFSourceReader,
    width: u32,
    height: u32,
    format: PixelFormat,
    ended: bool,
}

impl CaptureSource {
    /// Open `device_id` (a VIDCAP symbolic link) and request NV12 at
    /// `width`x`height`, falling back to the native media type.
    pub(crate) fn open(device_id: &str, width: u32, height: u32) -> Result<Self, VideoError> {
        unsafe {
            let session = MediaSession::init()?;

            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(std::ptr::addr_of_mut!(attributes), 2)
                .map_err(|error| VideoError::platform(format!("MFCreateAttributes: {error}")))?;
            let attributes = attributes.ok_or_else(|| {
                VideoError::platform("MFCreateAttributes devolveu atributos nulos")
            })?;

            attributes
                .SetGUID(
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
                )
                .map_err(|error| VideoError::platform(format!("SetGUID source type: {error}")))?;

            let symbolic_link_key = MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK;
            let symbolic_link = HSTRING::from(device_id);
            attributes
                .SetString(std::ptr::addr_of!(symbolic_link_key), &symbolic_link)
                .map_err(|error| {
                    VideoError::platform(format!("SetString symbolic link: {error}"))
                })?;

            let source = MFCreateDeviceSource(&attributes)
                .map_err(|error| VideoError::platform(format!("MFCreateDeviceSource: {error}")))?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, None).map_err(|error| {
                VideoError::platform(format!("MFCreateSourceReaderFromMediaSource: {error}"))
            })?;

            let (width, height, format) = negotiate_media_type(&reader, width, height)?;

            Ok(Self {
                _session: session,
                reader,
                width,
                height,
                format,
                ended: false,
            })
        }
    }

    /// The resolution actually negotiated with the device.
    #[must_use]
    pub(crate) fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Pull the next sample synchronously. Returns `Ok(None)` at end of stream.
    pub(crate) fn read_frame(&mut self) -> Result<Option<VideoFrame>, VideoError> {
        unsafe {
            if self.ended {
                return Ok(None);
            }

            let sample = loop {
                let mut flags = 0u32;
                let mut timestamp = 0i64;
                let mut sample: Option<IMFSample> = None;
                self.reader
                    .ReadSample(
                        FIRST_VIDEO_STREAM,
                        0,
                        None,
                        Some(std::ptr::addr_of_mut!(flags)),
                        Some(std::ptr::addr_of_mut!(timestamp)),
                        Some(std::ptr::addr_of_mut!(sample)),
                    )
                    .map_err(|error| VideoError::platform(format!("ReadSample: {error}")))?;

                if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                    self.ended = true;
                    return Ok(None);
                }
                if let Some(sample) = sample {
                    break (timestamp, sample);
                }
            };

            let (timestamp, sample) = sample;
            let buffer = sample.ConvertToContiguousBuffer().map_err(|error| {
                VideoError::platform(format!("ConvertToContiguousBuffer: {error}"))
            })?;

            let data = {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut max_length = 0u32;
                let mut current_length = 0u32;
                buffer
                    .Lock(
                        std::ptr::addr_of_mut!(ptr),
                        Some(std::ptr::addr_of_mut!(max_length)),
                        Some(std::ptr::addr_of_mut!(current_length)),
                    )
                    .map_err(|error| VideoError::platform(format!("Lock: {error}")))?;
                let bytes = std::slice::from_raw_parts(ptr, current_length as usize).to_vec();
                buffer
                    .Unlock()
                    .map_err(|error| VideoError::platform(format!("Unlock: {error}")))?;
                bytes
            };

            Ok(Some(VideoFrame {
                width: self.width,
                height: self.height,
                format: self.format,
                timestamp: std::time::Duration::from_nanos(
                    u64::try_from(timestamp.max(0))
                        .unwrap_or(0)
                        .saturating_mul(100),
                ),
                data: data.into_boxed_slice(),
            }))
        }
    }
}

/// COM reference-count guard for the calling thread (screen capture path).
struct ComSession {
    co_owned: bool,
}

impl ComSession {
    /// Initialize COM (MTA), tolerating a thread that is already initialized.
    unsafe fn init() -> Result<Self, VideoError> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let co_owned = hr == S_OK;
            if hr.0 < 0 && hr != RPC_E_CHANGED_MODE {
                return Err(VideoError::screen_capture(format!("CoInitializeEx: {hr}")));
            }
            Ok(Self { co_owned })
        }
    }
}

impl Drop for ComSession {
    fn drop(&mut self) {
        if self.co_owned {
            unsafe { CoUninitialize() }
        }
    }
}

/// One enumerated monitor, in GDI terms: handle + device id + flags + bounds.
type RawMonitor = (HMONITOR, String, bool, u32, u32);

/// List the monitors on this machine via GDI.
pub(super) fn enumerate_monitors() -> Result<Vec<crate::screen::MonitorInfo>, VideoError> {
    collect_monitor_info().map(|monitors| {
        monitors
            .into_iter()
            .map(
                |(_, name, is_primary, width, height)| crate::screen::MonitorInfo {
                    id: name.clone(),
                    name,
                    is_primary,
                    width,
                    height,
                },
            )
            .collect()
    })
}

/// Enumerate monitors once, returning raw GDI data.
fn collect_monitor_info() -> Result<Vec<RawMonitor>, VideoError> {
    unsafe {
        let mut collected: Vec<RawMonitor> = Vec::new();
        let result = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(std::ptr::addr_of_mut!(collected) as isize),
        );
        if !result.as_bool() {
            return Err(VideoError::screen_capture("EnumDisplayMonitors falhou"));
        }
        Ok(collected)
    }
}

/// GDI monitor enumeration callback; writes into the `Vec` passed through `data`.
///
/// # Safety
///
/// `data` must point at the caller-owned `Vec<RawMonitor>` that owns
/// `EnumDisplayMonitors`; the callback runs synchronously before it returns.
unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let mut info: MONITORINFOEXW = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).unwrap_or(0);
    // SAFETY: `info` is owned by this callback; GDI fills it in place.
    if unsafe { GetMonitorInfoW(monitor, std::ptr::addr_of_mut!(info.monitorInfo)) }.as_bool() {
        let bounds = info.monitorInfo.rcMonitor;
        let width = u32::try_from(bounds.right.saturating_sub(bounds.left)).unwrap_or(0);
        let height = u32::try_from(bounds.bottom.saturating_sub(bounds.top)).unwrap_or(0);
        let is_primary = info.monitorInfo.dwFlags & 1 != 0;
        let name = wide_to_string(&info.szDevice);
        let collected = unsafe { &mut *(data.0 as *mut Vec<RawMonitor>) };
        collected.push((monitor, name, is_primary, width, height));
    }
    TRUE
}

/// Live screen capture of one monitor through Windows Graphics Capture.
pub(crate) struct ScreenCapture {
    _session: ComSession,
    device: ID3D11Device,
    winrt_device: IDirect3DDevice,
    context: ID3D11DeviceContext,
    frame_pool: Direct3D11CaptureFramePool,
    _capture_session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    started: Instant,
    last_frame_at: Instant,
}

impl ScreenCapture {
    /// Open `monitor_id` (a GDI device name like `\\.\DISPLAY1`) for capture.
    pub(crate) fn open_monitor(monitor_id: &str) -> Result<Self, VideoError> {
        unsafe {
            let session = ComSession::init()?;
            let monitor = collect_monitor_info()?
                .into_iter()
                .find(|(_, name, _, _, _)| name == monitor_id)
                .map(|(handle, ..)| handle)
                .ok_or_else(|| {
                    VideoError::screen_capture(format!("monitor desconhecido: {monitor_id}"))
                })?;

            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(std::ptr::addr_of_mut!(device)),
                None,
                None,
            )
            .map_err(|error| VideoError::screen_capture(format!("D3D11CreateDevice: {error}")))?;
            let device =
                device.ok_or_else(|| VideoError::screen_capture("sem dispositivo D3D11"))?;

            let dxgi_device: IDXGIDevice = device
                .cast()
                .map_err(|error| VideoError::screen_capture(format!("IDXGIDevice: {error}")))?;
            let inspectable =
                CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device).map_err(|error| {
                    VideoError::screen_capture(format!(
                        "CreateDirect3D11DeviceFromDXGIDevice: {error}"
                    ))
                })?;
            let winrt_device: IDirect3DDevice = inspectable
                .cast()
                .map_err(|error| VideoError::screen_capture(format!("IDirect3DDevice: {error}")))?;

            let factory: IGraphicsCaptureItemStatics = RoGetActivationFactory(&HSTRING::from(
                "Windows.Graphics.Capture.GraphicsCaptureItem",
            ))
            .map_err(|error| {
                VideoError::screen_capture(format!("RoGetActivationFactory: {error}"))
            })?;
            let interop: IGraphicsCaptureItemInterop = factory.cast().map_err(|error| {
                VideoError::screen_capture(format!("IGraphicsCaptureItemInterop: {error}"))
            })?;
            let item: GraphicsCaptureItem = interop.CreateForMonitor(monitor).map_err(|error| {
                VideoError::screen_capture(format!("CreateForMonitor: {error}"))
            })?;

            let size = item
                .Size()
                .map_err(|error| VideoError::screen_capture(format!("tamanho do item: {error}")))?;
            let (width, height) = (
                u32::try_from(size.Width).unwrap_or(0),
                u32::try_from(size.Height).unwrap_or(0),
            );
            if width == 0 || height == 0 {
                return Err(VideoError::screen_capture("item sem tamanho"));
            }

            let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                4,
                SizeInt32 {
                    Width: i32::try_from(width).unwrap_or(i32::MAX),
                    Height: i32::try_from(height).unwrap_or(i32::MAX),
                },
            )
            .map_err(|error| VideoError::screen_capture(format!("CreateFreeThreaded: {error}")))?;
            let capture_session = frame_pool.CreateCaptureSession(&item).map_err(|error| {
                VideoError::screen_capture(format!("CreateCaptureSession: {error}"))
            })?;
            capture_session
                .StartCapture()
                .map_err(|error| VideoError::screen_capture(format!("StartCapture: {error}")))?;

            let staging = create_staging_texture(&device, width, height)?;
            let context = device.GetImmediateContext().map_err(|error| {
                VideoError::screen_capture(format!("GetImmediateContext: {error}"))
            })?;
            let now = Instant::now();

            Ok(Self {
                _session: session,
                device,
                winrt_device,
                context,
                frame_pool,
                _capture_session: capture_session,
                _item: item,
                staging,
                width,
                height,
                started: now,
                last_frame_at: now,
            })
        }
    }

    /// The resolution actually delivered for the monitor.
    #[must_use]
    pub(crate) fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Pull the next frame synchronously, copying it to CPU memory.
    pub(crate) fn read_frame(&mut self) -> Result<Option<VideoFrame>, VideoError> {
        unsafe {
            let frame = loop {
                if let Ok(frame) = self.frame_pool.TryGetNextFrame() {
                    break frame;
                }
                if self.last_frame_at.elapsed() > Duration::from_secs(5) {
                    return Err(VideoError::screen_capture("captura sem quadros novos"));
                }
                std::thread::sleep(Duration::from_millis(5));
            };

            let surface = frame.Surface().map_err(|error| {
                VideoError::screen_capture(format!("superficie do quadro: {error}"))
            })?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|error| {
                VideoError::screen_capture(format!("acesso DXGI do quadro: {error}"))
            })?;
            let texture: ID3D11Texture2D = access.GetInterface().map_err(|error| {
                VideoError::screen_capture(format!("textura do quadro: {error}"))
            })?;

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(std::ptr::addr_of_mut!(desc));
            if desc.Width != self.width || desc.Height != self.height {
                self.width = desc.Width;
                self.height = desc.Height;
                self.frame_pool
                    .Recreate(
                        &self.winrt_device,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        4,
                        SizeInt32 {
                            Width: i32::try_from(desc.Width).unwrap_or(i32::MAX),
                            Height: i32::try_from(desc.Height).unwrap_or(i32::MAX),
                        },
                    )
                    .map_err(|error| VideoError::screen_capture(format!("Recreate: {error}")))?;
                self.staging = create_staging_texture(&self.device, self.width, self.height)?;
            }

            self.context.CopyResource(&self.staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(
                    &self.staging,
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|error| VideoError::screen_capture(format!("Map: {error}")))?;
            if mapped.pData.is_null() {
                self.context.Unmap(&self.staging, 0);
                return Err(VideoError::screen_capture("Map devolveu ponteiro nulo"));
            }

            let row_bytes = self.width as usize * 4;
            let mut data = vec![0_u8; row_bytes * self.height as usize];
            for row in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(
                    mapped.pData.add(row * mapped.RowPitch as usize),
                    data.as_mut_ptr()
                        .add(row * row_bytes)
                        .cast::<std::ffi::c_void>(),
                    row_bytes,
                );
            }
            self.context.Unmap(&self.staging, 0);

            self.last_frame_at = Instant::now();
            drop(frame);

            Ok(Some(VideoFrame {
                width: self.width,
                height: self.height,
                format: PixelFormat::Bgra8,
                timestamp: self.started.elapsed(),
                data: data.into_boxed_slice(),
            }))
        }
    }
}

/// Create a CPU-readable staging texture the size of the capture surface.
///
/// # Safety
///
/// `device` must be a live `ID3D11Device` owned by the caller.
unsafe fn create_staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, VideoError> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0),
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(
                std::ptr::addr_of!(desc),
                None,
                Some(std::ptr::addr_of_mut!(staging)),
            )
            .map_err(|error| VideoError::screen_capture(format!("CreateTexture2D: {error}")))?;
        staging.ok_or_else(|| VideoError::screen_capture("staging sem textura"))
    }
}

///
/// Negotiate NV12 at the requested size (or native size) with software
/// fallback to the device's native media type. Returns the final size/format.
///
/// # Safety
///
/// `reader` must be a live `IMFSourceReader`; the function never releases it.
unsafe fn negotiate_media_type(
    reader: &IMFSourceReader,
    width: u32,
    height: u32,
) -> Result<(u32, u32, PixelFormat), VideoError> {
    unsafe {
        let stream = FIRST_VIDEO_STREAM;
        let native = reader
            .GetCurrentMediaType(stream)
            .map_err(|error| VideoError::platform(format!("GetCurrentMediaType: {error}")))?;
        let (native_w, native_h) = media_type_size(&native);
        let native_format = pixel_format(&native);

        let proposed = MFCreateMediaType()
            .map_err(|error| VideoError::platform(format!("MFCreateMediaType: {error}")))?;
        proposed
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|error| VideoError::platform(format!("SetGUID major type: {error}")))?;
        proposed
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|error| VideoError::platform(format!("SetGUID subtype: {error}")))?;

        let (request_w, request_h) = if width > 0 && height > 0 {
            (width, height)
        } else {
            (native_w, native_h)
        };
        if request_w > 0 && request_h > 0 {
            let frame_size_key = MF_MT_FRAME_SIZE;
            proposed
                .SetUINT64(
                    std::ptr::addr_of!(frame_size_key),
                    packed_size(request_w, request_h),
                )
                .map_err(|error| VideoError::platform(format!("SetUINT64 frame size: {error}")))?;
        }

        if reader.SetCurrentMediaType(stream, None, &proposed).is_ok() {
            let current = reader
                .GetCurrentMediaType(stream)
                .map_err(|error| VideoError::platform(format!("GetCurrentMediaType: {error}")))?;
            let (actual_w, actual_h) = media_type_size(&current);
            if actual_w > 0 && actual_h > 0 {
                return Ok((actual_w, actual_h, pixel_format(&current)));
            }
        }

        Ok((native_w, native_h, native_format))
    }
}

/// `UINT64` packing used by `MF_MT_FRAME_SIZE`: high word is width.
const fn packed_size(width: u32, height: u32) -> u64 {
    ((width as u64) << 32) | height as u64
}

/// Read `MF_MT_FRAME_SIZE` from a media type, defaulting to `(0, 0)`.
#[must_use]
fn media_type_size(media_type: &IMFMediaType) -> (u32, u32) {
    let frame_size_key = MF_MT_FRAME_SIZE;
    let size = unsafe { media_type.GetUINT64(std::ptr::addr_of!(frame_size_key)) }.unwrap_or(0);
    (
        u32::try_from(size >> 32).unwrap_or(0),
        u32::try_from(size).unwrap_or(0),
    )
}

/// Map the `MF_MT_SUBTYPE` of a media type to a `PixelFormat`.
#[must_use]
fn pixel_format(media_type: &IMFMediaType) -> PixelFormat {
    let subtype_key = MF_MT_SUBTYPE;
    let Ok(subtype) = (unsafe { media_type.GetGUID(std::ptr::addr_of!(subtype_key)) }) else {
        return PixelFormat::Unknown;
    };
    if subtype == MFVideoFormat_NV12 {
        PixelFormat::Nv12
    } else if subtype == MFVideoFormat_YUY2 {
        PixelFormat::Yuy2
    } else if subtype == MFVideoFormat_MJPG {
        PixelFormat::Mjpg
    } else {
        PixelFormat::Unknown
    }
}

/// GPU name from the first adapter DXGI exposes.
pub(super) fn gpu() -> Option<String> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc: DXGI_ADAPTER_DESC1 = adapter.GetDesc1().ok()?;
        Some(wide_to_string(&desc.Description))
    }
}

/// Detect hardware video encoders exposed by Media Foundation MFTs.
pub(super) fn hardware_video_encoders() -> Vec<CodecCapability> {
    let mut codecs = Vec::new();

    if has_amf_runtime() {
        codecs.push(CodecCapability {
            name: "H264".into(),
            kind: MediaKind::Video,
            encode: true,
            decode: false,
            acceleration: AccelerationApi::AmdAmf,
        });
    }

    if has_hardware_mft(&MFVideoFormat_H264) {
        codecs.push(CodecCapability {
            name: "H264".into(),
            kind: MediaKind::Video,
            encode: true,
            decode: true,
            acceleration: AccelerationApi::MediaFoundation,
        });
    }

    if has_hardware_mft(&MFVideoFormat_HEVC) {
        codecs.push(CodecCapability {
            name: "HEVC".into(),
            kind: MediaKind::Video,
            encode: true,
            decode: true,
            acceleration: AccelerationApi::MediaFoundation,
        });
    }

    codecs
}

/// Capture backends available on Windows.
pub(super) fn capture_backends() -> Vec<CaptureBackend> {
    vec![
        CaptureBackend::Wasapi,
        CaptureBackend::WindowsGraphicsCapture,
        CaptureBackend::Software,
    ]
}

/// Whether the AMD AMF runtime (`amfrt64.dll`) is already loaded.
fn has_amf_runtime() -> bool {
    const AMF_RT64: &[u16] = &[
        0x61, 0x6d, 0x66, 0x72, 0x74, 0x36, 0x34, 0x2e, 0x64, 0x6c, 0x6c, 0,
    ];
    unsafe { GetModuleHandleW(PCWSTR(AMF_RT64.as_ptr())).is_ok_and(|module| !module.is_invalid()) }
}

/// Whether a hardware MFT video encoder for `subtype` exists.
///
/// # Safety
///
/// Media Foundation is initialized for the duration of the query through
/// [`MediaSession`]; the returned `IMFActivate` array is released before this
/// function returns.
fn has_hardware_mft(subtype: &GUID) -> bool {
    unsafe {
        let Ok(_session) = MediaSession::init() else {
            return false;
        };

        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: *subtype,
        };
        let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
        let nv12_input = NV12_INPUT;
        let mut raw_devices: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let hr = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(std::ptr::addr_of!(nv12_input)),
            Some(std::ptr::addr_of!(output)),
            std::ptr::addr_of_mut!(raw_devices),
            std::ptr::addr_of_mut!(count),
        );
        if hr.is_err() {
            return false;
        }

        let present = count > 0;
        if present {
            let devices = std::slice::from_raw_parts_mut(raw_devices, count as usize);
            for device in devices.iter_mut() {
                if let Some(activate) = device.take() {
                    drop(activate);
                }
            }
        }
        CoTaskMemFree(Some(raw_devices.cast()));
        present
    }
}

/// Decode a fixed-size null-terminated UTF-16 buffer into a `String`.
#[must_use]
fn wide_to_string(wide: &[u16]) -> String {
    let length = wide.iter().take_while(|&&unit| unit != 0).count();
    String::from_utf16_lossy(&wide[..length])
}

#[cfg(test)]
mod tests {
    use super::wide_to_string;

    #[test]
    fn decodes_utf16_up_to_terminator() {
        let buffer = ['A' as u16, 'm' as u16, 'd' as u16, 0, 'x' as u16];
        assert_eq!(wide_to_string(&buffer), "Amd");
    }
}
