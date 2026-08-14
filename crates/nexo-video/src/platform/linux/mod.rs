//! Linux backend: V4L2 camera enumeration and frame capture, VA-API/PipeWire
//! presence probing, and screen capture through the XDG Desktop Portal
//! `ScreenCast` portal + `PipeWire` (see [`screen`]).
//!
//! # Safety
//!
//! This module is the second `unsafe` exception of the workspace (the crate
//! overrides `unsafe_code = "forbid"` in `Cargo.toml`). All `unsafe` is kept to
//! single, tiny libc calls bounded by the surrounding safe code:
//!
//! * Device discovery opens each `/dev/video*` node, runs `VIDIOC_QUERYCAP`,
//!   reads only the fixed 104-byte capability struct it just filled and closes
//!   the descriptor through `OwnedFd` on drop.
//! * Capture mmaps the V4L2 buffers (lengths reported by `VIDIOC_QUERYBUF`),
//!   copies `bytesused` bytes (reported by `VIDIOC_DQBUF`, bounded by the buffer
//!   length) into a `Vec` and unmaps every buffer on drop, so safe callers never
//!   see a native resource or a stale pointer.
//! * Screen capture creates the `PipeWire` `ThreadLoop` (`ThreadLoopBox::new`
//!   is `unsafe` in pipewire-rs) with `None` name/properties only; everything
//!   else the portal hands us is used through safe wrappers.
//!
//! The V4L2 ABI layout below is the generic 64-bit one (`x86_64`/`aarch64`);
//! the ioctl request codes are recomputed from the `_IOC` macro so they stay
//! tied to the struct sizes.

mod screen;

pub(crate) use screen::{ScreenCapture, enumerate_monitors};

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Instant;

use libc::{MAP_FAILED, MAP_SHARED, PROT_READ, PROT_WRITE};

use crate::capture::{PixelFormat, VideoFrame};
use crate::devices::{VideoDeviceInfo, VideoError};
use crate::probe::{AccelerationApi, CaptureBackend, CodecCapability, MediaKind};

// --- V4L2 ABI (generic 64-bit; linux/videodev2.h) ---------------------------

const V4L2_TYPE: u32 = 0x56; // 'V'
const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;
const IOC_DIRSHIFT: u32 = 30;
const IOC_SIZESHIFT: u32 = 16;
const IOC_TYPESHIFT: u32 = 8;

/// Assemble an ioctl request code from direction, number and payload size.
///
/// Sizes here are small fixed structs, so truncation is impossible in practice.
#[allow(clippy::cast_possible_truncation)]
const fn ioctl_code(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir << IOC_DIRSHIFT) | (V4L2_TYPE << IOC_TYPESHIFT) | nr | ((size as u32) << IOC_SIZESHIFT))
        as libc::c_ulong
}

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;

/// `FourCC` for a 4-char pixel format code.
///
/// The `as` casts are lossless (each byte fits a u32); `From::from` is not
/// usable in a const function yet.
#[allow(clippy::cast_lossless)]
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

const V4L2_PIX_FMT_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
const V4L2_PIX_FMT_YUYV: u32 = fourcc(b'Y', b'U', b'Y', b'V');
const V4L2_PIX_FMT_MJPEG: u32 = fourcc(b'M', b'J', b'P', b'G');

const MAX_BUFFERS: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Format {
    type_: u32,
    _pad: u32,
    raw: [u8; 200],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Requestbuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    reserved: [u32; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: V4l2Timecode,
    sequence: u32,
    memory: u32,
    m_offset: u32,
    m_pad: u32,
    length: u32,
    reserved2: u32,
    reserved: u32,
}

// --- helpers ----------------------------------------------------------------

/// Run a V4L2 ioctl that passes a single struct pointer.
///
/// # Safety
///
/// `request` must be a V4L2 code whose payload is exactly `T`; `data` must
/// point to a valid, writable `T` for the whole call.
unsafe fn v4l2_ioctl<T>(fd: i32, request: libc::c_ulong, data: *mut T) -> Result<(), VideoError> {
    unsafe {
        if libc::ioctl(fd, request, data) < 0 {
            return Err(VideoError::platform(
                std::io::Error::last_os_error().to_string(),
            ));
        }
        Ok(())
    }
}

/// Trim a NUL/space-terminated C byte array into a display string.
fn c_str_trim(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// Candidate camera device nodes, sorted for stable ids.
fn camera_device_paths() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("video"))
        })
        .collect();
    paths.sort();
    paths
}

fn open_device(path: &Path) -> Result<OwnedFd, VideoError> {
    let cpath = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| VideoError::platform("caminho de dispositivo invalido"))?;
    // SAFETY: `cpath` is a valid C string for the device node; the returned fd
    // is immediately owned by `OwnedFd`, which closes it on drop.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(VideoError::platform(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    // SAFETY: `fd` is a freshly opened descriptor we exclusively own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// True when the device node is a video-capture device (not a decoder/encoder
/// or metadata-only node).
fn is_capture_device(caps: &V4l2Capability) -> bool {
    let effective = if caps.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        caps.device_caps
    } else {
        caps.capabilities
    };
    effective & V4L2_CAP_VIDEO_CAPTURE != 0
}

// --- camera enumeration -----------------------------------------------------

/// Enumerate cameras through V4L2 (`/dev/video*`).
#[allow(clippy::unnecessary_wraps)] // required by the crate's platform trait
pub(super) fn enumerate_cameras() -> Result<Vec<VideoDeviceInfo>, VideoError> {
    let mut cameras = Vec::new();
    for path in camera_device_paths() {
        let Ok(fd) = open_device(&path) else {
            continue;
        };
        let mut caps = V4l2Capability::default();
        // SAFETY: `caps` is a full v4l2_capability (104 bytes) that V4L2 fills
        // for the node; `fd` is live for the whole call.
        let query = unsafe {
            v4l2_ioctl(
                fd.as_raw_fd(),
                ioctl_code(IOC_READ, 0, std::mem::size_of::<V4l2Capability>()),
                std::ptr::addr_of_mut!(caps),
            )
        };
        if query.is_err() || !is_capture_device(&caps) {
            continue;
        }
        cameras.push(VideoDeviceInfo {
            id: path.to_string_lossy().into_owned(),
            name: c_str_trim(&caps.card),
        });
    }
    Ok(cameras)
}

// --- camera capture (V4L2 mmap streaming) -----------------------------------

/// One mmap'd V4L2 capture buffer.
struct MmapBuffer {
    ptr: *mut u8,
    len: usize,
}

impl Drop for MmapBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr`/`len` are exactly what mmap returned earlier.
            unsafe { libc::munmap(self.ptr.cast(), self.len) };
        }
    }
}

/// Live camera capture through V4L2 with `mmap` streaming.
pub(crate) struct CaptureSource {
    fd: OwnedFd,
    width: u32,
    height: u32,
    pixel_format: u32,
    buffers: Vec<MmapBuffer>,
    streaming: bool,
    started: Instant,
}

impl CaptureSource {
    /// Open a camera requesting NV12 at the given size, with YUYV fallback.
    pub(crate) fn open(device_id: &str, width: u32, height: u32) -> Result<Self, VideoError> {
        let fd = open_device(Path::new(device_id))?;

        let mut caps = V4l2Capability::default();
        // SAFETY: as in `enumerate_cameras`.
        unsafe {
            v4l2_ioctl(
                fd.as_raw_fd(),
                ioctl_code(IOC_READ, 0, std::mem::size_of::<V4l2Capability>()),
                std::ptr::addr_of_mut!(caps),
            )?;
        }
        if !is_capture_device(&caps) {
            return Err(VideoError::platform(format!(
                "{device_id} nao captura video"
            )));
        }

        let (format, actual_width, actual_height) =
            negotiate_format(fd.as_raw_fd(), width, height)?;

        let mut reqbufs = V4l2Requestbuffers {
            count: MAX_BUFFERS,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: V4L2_MEMORY_MMAP,
            ..V4l2Requestbuffers::default()
        };
        // SAFETY: `reqbufs` is a valid v4l2_requestbuffers for the open node.
        unsafe {
            v4l2_ioctl(
                fd.as_raw_fd(),
                ioctl_code(
                    IOC_READ | IOC_WRITE,
                    8,
                    std::mem::size_of::<V4l2Requestbuffers>(),
                ),
                std::ptr::addr_of_mut!(reqbufs),
            )?;
        }
        if reqbufs.count == 0 {
            return Err(VideoError::platform("V4L2 nao alocou buffers"));
        }

        let mut buffers = Vec::with_capacity(reqbufs.count as usize);
        for index in 0..reqbufs.count {
            let mut buf = V4l2Buffer {
                index,
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
                memory: V4L2_MEMORY_MMAP,
                ..V4l2Buffer::default()
            };
            // SAFETY: as above; `buf` is a valid v4l2_buffer.
            unsafe {
                v4l2_ioctl(
                    fd.as_raw_fd(),
                    ioctl_code(IOC_READ | IOC_WRITE, 9, std::mem::size_of::<V4l2Buffer>()),
                    std::ptr::addr_of_mut!(buf),
                )?;
            }
            if buf.length == 0 {
                return Err(VideoError::platform("buffer V4L2 sem tamanho"));
            }
            // SAFETY: `buf.length`/`buf.m_offset` come from QUERYBUF and describe
            // a device-backed mapping the kernel created for this fd; we own the
            // mapping and unmap it in `MmapBuffer::drop`.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    buf.length as libc::size_t,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    fd.as_raw_fd(),
                    libc::off_t::from(buf.m_offset),
                )
            };
            if ptr == MAP_FAILED {
                return Err(VideoError::platform(
                    std::io::Error::last_os_error().to_string(),
                ));
            }
            buffers.push(MmapBuffer {
                ptr: ptr.cast(),
                len: buf.length as usize,
            });
        }

        let mut source = Self {
            fd,
            width: actual_width,
            height: actual_height,
            pixel_format: format,
            buffers,
            streaming: false,
            started: Instant::now(),
        };
        source.queue_all()?;
        source.stream_on()?;
        Ok(source)
    }

    fn queue_all(&mut self) -> Result<(), VideoError> {
        for index in 0..self.buffers.len() {
            let index =
                u32::try_from(index).map_err(|_| VideoError::platform("muitos buffers V4L2"))?;
            let mut buf = V4l2Buffer {
                index,
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
                memory: V4L2_MEMORY_MMAP,
                ..V4l2Buffer::default()
            };
            // SAFETY: `buf` is a valid v4l2_buffer for an allocated index.
            unsafe {
                v4l2_ioctl(
                    self.fd.as_raw_fd(),
                    ioctl_code(IOC_READ | IOC_WRITE, 15, std::mem::size_of::<V4l2Buffer>()),
                    std::ptr::addr_of_mut!(buf),
                )?;
            }
        }
        Ok(())
    }

    fn stream_on(&mut self) -> Result<(), VideoError> {
        let mut on = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        // SAFETY: `on` is an int holding the buffer type for STREAMON.
        unsafe {
            v4l2_ioctl(
                self.fd.as_raw_fd(),
                ioctl_code(IOC_WRITE, 18, std::mem::size_of::<u32>()),
                std::ptr::addr_of_mut!(on),
            )?;
        }
        self.streaming = true;
        Ok(())
    }

    /// The resolution actually negotiated with the device.
    #[must_use]
    pub(crate) fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Dequeue the next captured frame and requeue its buffer.
    pub(crate) fn read_frame(&mut self) -> Result<Option<VideoFrame>, VideoError> {
        let mut buf = V4l2Buffer {
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: V4L2_MEMORY_MMAP,
            ..V4l2Buffer::default()
        };
        // SAFETY: `buf` is a valid v4l2_buffer; DQBUF blocks until a frame is
        // ready, matching the blocking contract of the public API.
        unsafe {
            v4l2_ioctl(
                self.fd.as_raw_fd(),
                ioctl_code(IOC_READ | IOC_WRITE, 17, std::mem::size_of::<V4l2Buffer>()),
                std::ptr::addr_of_mut!(buf),
            )?;
        }

        let buffer = self
            .buffers
            .get(buf.index as usize)
            .ok_or_else(|| VideoError::platform("indice de buffer invalido do V4L2"))?;
        let count = (buf.bytesused as usize).min(buffer.len);
        // SAFETY: `buffer.ptr` is a live mmap'd mapping of at least `buffer.len`
        // bytes; we read at most that many bytes after DQBUF. `buffer` is
        // unmapped only in Drop, which cannot run while `&mut self` is alive.
        let data = unsafe { std::slice::from_raw_parts(buffer.ptr, count) }.to_vec();

        // SAFETY: `buf` describes the buffer just dequeued; requeue it.
        unsafe {
            v4l2_ioctl(
                self.fd.as_raw_fd(),
                ioctl_code(IOC_READ | IOC_WRITE, 15, std::mem::size_of::<V4l2Buffer>()),
                std::ptr::addr_of_mut!(buf),
            )?;
        }

        Ok(Some(VideoFrame {
            width: self.width,
            height: self.height,
            format: match self.pixel_format {
                V4L2_PIX_FMT_YUYV => PixelFormat::Yuy2,
                V4L2_PIX_FMT_MJPEG => PixelFormat::Mjpg,
                _ => PixelFormat::Nv12,
            },
            timestamp: self.started.elapsed(),
            data: data.into_boxed_slice(),
        }))
    }
}

impl Drop for CaptureSource {
    fn drop(&mut self) {
        if self.streaming {
            let mut off = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            // SAFETY: as in `stream_on`; STREAMOFF stops the capture queue.
            unsafe {
                let _ = libc::ioctl(
                    self.fd.as_raw_fd(),
                    ioctl_code(IOC_WRITE, 19, std::mem::size_of::<u32>()),
                    std::ptr::addr_of_mut!(off),
                );
            }
        }
        // Unmap first (munmap), then release the driver buffers with
        // REQBUFS(0) so the device can be re-configured by the next open.
        self.buffers.clear();
        let mut reqbufs = V4l2Requestbuffers {
            count: 0,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: V4L2_MEMORY_MMAP,
            ..V4l2Requestbuffers::default()
        };
        // SAFETY: `reqbufs` is a valid v4l2_requestbuffers for the open node;
        // releasing buffers on drop is best-effort teardown.
        unsafe {
            let _ = libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl_code(
                    IOC_READ | IOC_WRITE,
                    8,
                    std::mem::size_of::<V4l2Requestbuffers>(),
                ),
                std::ptr::addr_of_mut!(reqbufs),
            );
        }
    }
}

/// Negotiate a capture pixel format at the requested size and read the actual
/// size back. Returns (fourcc, width, height).
///
/// Candidates are tried in order: NV12 (planar, ideal for encoding), then
/// MJPEG (the common UVC webcam format — bandwidth-efficient and streamable at
/// any resolution), then YUYV (raw 4:2:2, last because of its high bandwidth).
///
/// UVC cameras may "succeed" a `S_FMT` without honoring the request, keeping
/// their current format; the read-back must match the requested pixel format,
/// otherwise the next candidate is tried. If no request is honored but the
/// driver kept a supported format, that format is used as a last resort.
///
/// The raw union pointer may not be 4-aligned, but every access through it goes
/// through `write_unaligned`/`read_unaligned`, so the alignment cast is sound.
#[allow(clippy::cast_ptr_alignment)]
fn negotiate_format(fd: i32, width: u32, height: u32) -> Result<(u32, u32, u32), VideoError> {
    let candidates = [V4L2_PIX_FMT_NV12, V4L2_PIX_FMT_MJPEG, V4L2_PIX_FMT_YUYV];
    let mut driver_format: Option<(u32, u32, u32)> = None;
    for fourcc in candidates {
        let mut fmt = V4l2Format {
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            _pad: 0,
            raw: [0; 200],
        };
        // SAFETY: `raw` is the union member of a 208-byte v4l2_format; the
        // v4l2_pix_format sub-struct goes at its start. The byte pointer is not
        // guaranteed aligned to V4l2PixFormat, so reads/writes are unaligned.
        let pix_ptr = unsafe {
            (std::ptr::addr_of_mut!(fmt).cast::<u8>())
                .add(std::mem::offset_of!(V4l2Format, raw))
                .cast::<V4l2PixFormat>()
        };
        // SAFETY: `pix_ptr` points inside the owned `fmt` union member.
        unsafe {
            std::ptr::write_unaligned(
                pix_ptr,
                V4l2PixFormat {
                    width,
                    height,
                    pixelformat: fourcc,
                    ..V4l2PixFormat::default()
                },
            );
        }

        // SAFETY: `fmt` is a valid v4l2_format; S_FMT may rewrite it in place.
        let result = unsafe {
            v4l2_ioctl(
                fd,
                ioctl_code(IOC_READ | IOC_WRITE, 5, std::mem::size_of::<V4l2Format>()),
                std::ptr::addr_of_mut!(fmt),
            )
        };
        if result.is_err() {
            continue;
        }
        // SAFETY: the driver filled the pix sub-struct in place.
        let negotiated = unsafe { std::ptr::read_unaligned(pix_ptr) };
        if negotiated.pixelformat == fourcc {
            return Ok((negotiated.pixelformat, negotiated.width, negotiated.height));
        }
        if candidates.contains(&negotiated.pixelformat) {
            driver_format = Some((negotiated.pixelformat, negotiated.width, negotiated.height));
        }
    }
    if let Some(format) = driver_format {
        return Ok(format);
    }
    Err(VideoError::platform(
        "camera nao aceita NV12, MJPEG nem YUYV no tamanho pedido",
    ))
}

// --- VA-API / PipeWire probing (presence-based) -----------------------------

/// Human-readable GPU name from the DRM device driver, if any is visible.
pub(super) fn gpu() -> Option<String> {
    let drm = Path::new("/sys/class/drm");
    if !drm.is_dir() {
        return None;
    }
    for entry in std::fs::read_dir(drm).ok()?.filter_map(Result::ok) {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let uevent = entry.path().join("device").join("uevent");
        let Ok(text) = std::fs::read_to_string(&uevent) else {
            continue;
        };
        for line in text.lines() {
            if let Some(driver) = line.strip_prefix("DRIVER=") {
                return Some(match driver {
                    "amdgpu" => "AMD GPU (VA-API)".into(),
                    "i915" | "i965" => "Intel GPU (VA-API)".into(),
                    "nouveau" => "NVIDIA GPU (VA-API)".into(),
                    other => format!("GPU ({other})"),
                });
            }
        }
    }
    None
}

/// Hardware encoders reported when a VA-API runtime is present. This is a
/// presence check (render node or libva), not a per-codec capability query.
pub(super) fn hardware_video_encoders() -> Vec<CodecCapability> {
    if !va_api_present() {
        return Vec::new();
    }
    vec![
        CodecCapability {
            name: "H264".into(),
            kind: MediaKind::Video,
            encode: true,
            decode: true,
            acceleration: AccelerationApi::VaApi,
        },
        CodecCapability {
            name: "HEVC".into(),
            kind: MediaKind::Video,
            encode: true,
            decode: true,
            acceleration: AccelerationApi::VaApi,
        },
    ]
}

/// Whether a VA-API runtime is likely available (render node or libva on disk).
fn va_api_present() -> bool {
    const VA_LIBS: [&str; 4] = [
        "/usr/lib/x86_64-linux-gnu/libva.so.2",
        "/usr/lib/libva.so.2",
        "/usr/lib64/libva.so.2",
        "/lib/x86_64-linux-gnu/libva.so.2",
    ];
    VA_LIBS.iter().any(|lib| Path::new(lib).exists()) || std::fs::read_dir("/dev/dri").is_ok()
}

pub(super) fn capture_backends() -> Vec<CaptureBackend> {
    vec![
        CaptureBackend::PipeWire,
        CaptureBackend::Alsa,
        CaptureBackend::Software,
    ]
}
