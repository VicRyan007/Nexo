//! Software VP8 encoding and decoding through libvpx.
//!
//! This is the "fallback" codec pipeline behind [`crate::CallEngine`] video:
//! capture sources convert frames to tightly-packed I420 and hand them to
//! [`Vp8Encoder`], whose output travels through
//! [`crate::LanPeerConnection::send_video`]; on the receive side
//! [`Vp8Decoder`] turns reassembled [`crate::EncodedVideoFrame`]s back into
//! I420 planes for rendering.
//!
//! The raw bindings come from `env-libvpx-sys` (MPL-2.0), which ships
//! pre-generated bindings so no bindgen/libclang is needed at build time; the
//! libvpx library itself is located through the `VPX_LIB_DIR`/`VPX_VERSION`
//! environment variables (see the checkpoint notes in `docs/continuation.md`).
//!
//! # Safety design decision
//!
//! The workspace forbids `unsafe` (see `AGENTS.md`); like `nexo-video`, this
//! crate is a documented exception and every libvpx call lives in this one
//! module behind safe, fallible functions.
//!
//! * The encoder and decoder own their `vpx_codec_ctx_t` in a `Drop` guard
//!   that always calls `vpx_codec_destroy`, so safe callers never leak the
//!   codec.
//! * Encode input is bounded: [`Vp8Encoder::encode`] verifies the caller
//!   supplied exactly `width * height * 3 / 2` bytes (the tight I420 size
//!   derived from the dimensions it initialized with) before wrapping the
//!   pointer, and `vpx_img_wrap` merely borrows the caller's buffer for the
//!   duration of `vpx_codec_encode`.
//! * Every pointer libvpx returns is read through `slice::from_raw_parts`
//!   whose length is taken from the value libvpx itself reported (`sz` for
//!   encoded packets, `stride * height` for decoded planes). None of these
//!   lengths come from network input, and the bytes are copied out immediately
//!   so no native pointer escapes this module.
//! * Interface pointers (`vpx_codec_vp8_cx()`/`vpx_codec_vp8_dx()`) are
//!   checked for null before use and never dereferenced directly.
//!
//! Safe callers never see a raw pointer.

// Every native cast across the FFI boundary is lossless or intentionally
// clamped; the specific choices are documented at each call site.
// `borrow_as_ptr`: FFI calls pass `&mut self.ctx` where libvpx wants a
// `*mut`; the implicit coercion is the documented boundary and each call
// carries a `// SAFETY:` comment.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::borrow_as_ptr
)]

use std::ffi::c_int;
use std::mem::MaybeUninit;
use std::os::raw::c_uint;
use std::ptr;
use std::slice;
use std::time::Duration;

use thiserror::Error;

use crate::{
    EncodedVideoFrame, VideoCodec,
    vpx_sys::{
        VPX_DECODER_ABI_VERSION, VPX_DL_REALTIME, VPX_EFLAG_FORCE_KF, VPX_ENCODER_ABI_VERSION,
        VPX_ERROR_RESILIENT_DEFAULT, VPX_FRAME_IS_KEY, vpx_codec_ctx_t, vpx_codec_cx_pkt_kind,
        vpx_codec_dec_init_ver, vpx_codec_decode, vpx_codec_destroy, vpx_codec_enc_cfg_t,
        vpx_codec_enc_config_default, vpx_codec_enc_init_ver, vpx_codec_encode, vpx_codec_err_t,
        vpx_codec_error, vpx_codec_error_detail, vpx_codec_get_cx_data, vpx_codec_get_frame,
        vpx_codec_iter_t, vpx_codec_pts_t, vpx_codec_vp8_cx, vpx_codec_vp8_dx,
        vpx_enc_frame_flags_t, vpx_image_t, vpx_img_fmt, vpx_img_wrap, vpx_rational,
    },
};

const VP8_CLOCK_RATE: u32 = 90_000;
const VP8_TIME_DEN: c_int = 90_000;

/// Failure of the software VP8 codec pipeline.
#[derive(Debug, Error)]
pub enum VideoCodecError {
    #[error("dimensions must be even and non-zero, got {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("the libvpx VP8 {component} interface is unavailable")]
    Unavailable { component: &'static str },
    #[error("libvpx could not {operation}: {detail}")]
    Codec {
        operation: &'static str,
        detail: String,
    },
    #[error("encoded frame is not VP8")]
    NotVp8,
    #[error("encoded frame is {actual} bytes, expected exactly {expected} bytes of I420")]
    UnexpectedInputSize { actual: usize, expected: usize },
}

fn codec_error(operation: &'static str, ctx: &vpx_codec_ctx_t) -> VideoCodecError {
    // The error/detail strings are static C strings owned by libvpx; libvpx
    // keeps them valid until the next call on the context.
    let error = unsafe { vpx_codec_error(std::ptr::from_ref(ctx).cast_mut()) };
    let detail = unsafe { vpx_codec_error_detail(std::ptr::from_ref(ctx).cast_mut()) };
    let message = if error.is_null() {
        "unknown libvpx error".to_owned()
    } else {
        // SAFETY: `error` points at a NUL-terminated static string that libvpx
        // owns for the lifetime of the context.
        let text = unsafe { std::ffi::CStr::from_ptr(error) };
        let mut message = text.to_string_lossy().into_owned();
        if !detail.is_null() {
            // SAFETY: `detail` likewise points at a static string when non-null.
            let extra = unsafe { std::ffi::CStr::from_ptr(detail) };
            message.push_str(": ");
            message.push_str(&extra.to_string_lossy());
        }
        message
    };
    VideoCodecError::Codec {
        operation,
        detail: message,
    }
}

fn check(
    result: vpx_codec_err_t,
    operation: &'static str,
    ctx: &vpx_codec_ctx_t,
) -> Result<(), VideoCodecError> {
    if result == vpx_codec_err_t::VPX_CODEC_OK {
        Ok(())
    } else {
        Err(codec_error(operation, ctx))
    }
}

fn timestamp_pts(timestamp: Duration) -> vpx_codec_pts_t {
    let micros = u64::try_from(timestamp.as_micros()).unwrap_or(u64::MAX);
    let pts = micros * u64::from(VP8_CLOCK_RATE) / 1_000_000;
    vpx_codec_pts_t::try_from(pts).unwrap_or(vpx_codec_pts_t::MAX)
}

/// Software VP8 encoder backed by a libvpx realtime encoder context.
///
/// Input is tightly-packed I420 (`width * height` luma bytes followed by
/// `width/2 * height/2` each of U and V); output is one raw VP8 frame per
/// [`Self::encode`] call.
pub struct Vp8Encoder {
    ctx: vpx_codec_ctx_t,
    width: u32,
    height: u32,
}

impl Vp8Encoder {
    /// Create an encoder for `width`x`height` targeting `bitrate_kbps`.
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, VideoCodecError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(VideoCodecError::InvalidDimensions { width, height });
        }
        let iface = ptr::NonNull::new(unsafe { vpx_codec_vp8_cx() }.cast_mut()).ok_or(
            VideoCodecError::Unavailable {
                component: "encoder",
            },
        )?;
        // SAFETY: zeroed POD; libvpx fills it in.
        let mut cfg: vpx_codec_enc_cfg_t = unsafe { MaybeUninit::zeroed().assume_init() };
        // SAFETY: `cfg` is a valid output buffer; `iface` is the VP8 encoder.
        let result = unsafe { vpx_codec_enc_config_default(iface.as_ptr(), &mut cfg, 0) };
        if result != vpx_codec_err_t::VPX_CODEC_OK {
            return Err(VideoCodecError::Codec {
                operation: "load the default VP8 encoder configuration",
                detail: format!("{result:?}"),
            });
        }
        cfg.g_w = width;
        cfg.g_h = height;
        cfg.g_timebase = vpx_rational {
            num: 1,
            den: VP8_TIME_DEN,
        };
        cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
        cfg.rc_target_bitrate = bitrate_kbps.max(1);
        cfg.g_threads = u32::try_from(
            std::thread::available_parallelism()
                .map_or(1, std::num::NonZero::get)
                .min(8),
        )
        .unwrap_or(1);
        // SAFETY: zeroed POD; `init_ver` fills it in and we only move it after
        // a successful init. The ABI version guard rejects mismatched headers.
        let mut ctx: vpx_codec_ctx_t = unsafe { MaybeUninit::zeroed().assume_init() };
        // SAFETY: `ctx` is writable, `iface`/`cfg` are valid for the call.
        let result = unsafe {
            vpx_codec_enc_init_ver(&mut ctx, iface.as_ptr(), &cfg, 0, VPX_ENCODER_ABI_VERSION)
        };
        if result != vpx_codec_err_t::VPX_CODEC_OK {
            return Err(VideoCodecError::Codec {
                operation: "initialize the VP8 encoder",
                detail: format!("{result:?}"),
            });
        }
        Ok(Self { ctx, width, height })
    }

    /// Encode one tightly-packed I420 frame, returning the resulting VP8
    /// frame, or `Ok(None)` when the encoder buffers it internally.
    ///
    /// `force_keyframe` drops the reference to the previous frame so the
    /// output decodes standalone (used to answer picture-loss requests).
    pub fn encode(
        &mut self,
        timestamp: Duration,
        data: &[u8],
        force_keyframe: bool,
    ) -> Result<Option<EncodedVideoFrame>, VideoCodecError> {
        let expected = self.i420_size();
        if data.len() != expected {
            return Err(VideoCodecError::UnexpectedInputSize {
                actual: data.len(),
                expected,
            });
        }
        // SAFETY: zeroed POD; `vpx_img_wrap` fills it in and only borrows
        // `data` for the encode call that follows immediately.
        let mut image: vpx_image_t = unsafe { MaybeUninit::zeroed().assume_init() };
        // SAFETY: `image` is writable and `data` stays valid (the caller's
        // buffer) for the entire call; the wrapped planes alias `data`.
        let wrapped = unsafe {
            vpx_img_wrap(
                &mut image,
                vpx_img_fmt::VPX_IMG_FMT_I420,
                self.width,
                self.height,
                0,
                data.as_ptr().cast_mut(),
            )
        };
        if wrapped.is_null() {
            return Err(VideoCodecError::Codec {
                operation: "wrap the I420 frame for encoding",
                detail: "vpx_img_wrap returned null".to_owned(),
            });
        }
        let pts = timestamp_pts(timestamp);
        // On the Windows GNU target `c_long` is 32-bit; cast keeps
        // the call portable across libvpx's two ABI layouts.
        let flags: vpx_enc_frame_flags_t = if force_keyframe {
            VPX_EFLAG_FORCE_KF as vpx_enc_frame_flags_t
        } else {
            0
        };
        // SAFETY: `self.ctx` is an initialized encoder; `image` is valid while
        // `data` (which it aliases) is alive in this frame.
        let result =
            unsafe { vpx_codec_encode(&mut self.ctx, &image, pts, 1, flags, VPX_DL_REALTIME) };
        check(result, "encode the I420 frame", &self.ctx)?;

        let mut iter: vpx_codec_iter_t = ptr::null();
        loop {
            // SAFETY: `iter` is an opaque cursor libvpx owns; `self.ctx` is a
            // live encoder. The returned packet borrows the context.
            let packet = unsafe { vpx_codec_get_cx_data(&mut self.ctx, &mut iter) };
            if packet.is_null() {
                return Ok(None);
            }
            // SAFETY: `packet` is non-null and points at a frame packet libvpx
            // owns for the lifetime of the context.
            let packet = unsafe { &*packet };
            if packet.kind != vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                continue;
            }
            // SAFETY: reading the `frame` member of the active union field is
            // valid because libvpx set `kind` to CX_FRAME_PKT above.
            let frame = unsafe { &packet.data.frame };
            #[allow(clippy::useless_conversion)]
            let size = usize::from(frame.sz);
            let is_keyframe = frame.flags & VPX_FRAME_IS_KEY != 0;
            let mut bytes = Vec::with_capacity(size);
            if size > 0 {
                // SAFETY: `frame.buf` points at `size` bytes libvpx owns; we
                // copy before returning so the borrow does not escape.
                bytes.extend_from_slice(unsafe {
                    slice::from_raw_parts(frame.buf as *const u8, size)
                });
            }
            return Ok(Some(EncodedVideoFrame {
                codec: VideoCodec::Vp8,
                width: self.width,
                height: self.height,
                timestamp,
                data: bytes.into_boxed_slice(),
                is_keyframe,
            }));
        }
    }

    /// Convenience for tests and one-off sends: encode with no forced keyframe.
    pub fn encode_frame(
        &mut self,
        timestamp: Duration,
        data: &[u8],
    ) -> Result<Option<EncodedVideoFrame>, VideoCodecError> {
        self.encode(timestamp, data, false)
    }

    fn i420_size(&self) -> usize {
        let y = self.width as usize * self.height as usize;
        y + y / 2
    }

    /// Convert NV12 planar format to I420.
    ///
    /// NV12: Y plane followed by interleaved U/V planes (chroma at half vertical/horizontal resolution).
    /// I420: Separate Y, U, V planes (each at half horizontal/vertical resolution).
    ///
    /// # Safety
    ///
    /// `n12_data` must point at `width * height * 3 / 2` bytes in NV12 layout,
    /// and the returned I420 data must be exactly `width * height * 3 / 2` bytes.
    fn nv12_to_i420(n12_data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, VideoCodecError> {
        let y_size = width as usize * height as usize;
        let uv_size = y_size / 4;
        if n12_data.len() < y_size + uv_size {
            return Err(VideoCodecError::UnexpectedInputSize {
                actual: n12_data.len(),
                expected: y_size + uv_size,
            });
        }

        let mut i420 = vec![0u8; y_size + uv_size * 2];

        // Copy Y plane (interleaved at top of NV12)
        i420[..y_size].copy_from_slice(n12_data);

        // Copy U plane (NV12 has U/V interleaved at half resolution)
        // NV12 UV layout: U0 V0 U1 V1 ... at half both dimensions
        // I420 U plane: U0 U1 U2 ... at half width
        let u_offset = y_size;
        let v_offset = y_size + uv_size;
        for (i, dst) in i420[u_offset..v_offset]
            .chunks_exact_mut(width as usize / 2)
            .enumerate()
        {
            let src_base = y_size + i * (width as usize / 2);
            // Take every other byte (U values from interleaved UV)
            for (j, byte) in dst.iter_mut().enumerate() {
                let src_idx = src_base + j * 2;
                if src_idx < n12_data.len() {
                    *byte = n12_data[src_idx];
                }
            }
        }

        // Copy V plane
        for (i, dst) in i420[v_offset..]
            .chunks_exact_mut(width as usize / 2)
            .enumerate()
        {
            let src_base = y_size + uv_size + i * (width as usize / 2);
            // Take every other byte starting from 1 (V values from interleaved UV)
            for (j, byte) in dst.iter_mut().enumerate() {
                let src_idx = src_base + j * 2 + 1;
                if src_idx < n12_data.len() {
                    *byte = n12_data[src_idx];
                }
            }
        }

        Ok(i420)
    }

    /// Convert YUY2 packed format to I420.
    ///
    /// YUY2: 4:2:2 packed format where each group of 4 bytes = Y0 U Y1 V
    /// I420: Separate Y, U, V planes at half horizontal resolution
    ///
    /// # Safety
    ///
    /// `yuy2_data` must point at `width * height * 2` bytes in YUY2 layout,
    /// and the returned I420 data must be exactly `width * height * 3 / 2` bytes.
    fn yuy2_to_i420(yuy2_data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, VideoCodecError> {
        let y_size = width as usize * height as usize;
        let uv_size = y_size / 4;
        if yuy2_data.len() < y_size * 2 {
            return Err(VideoCodecError::UnexpectedInputSize {
                actual: yuy2_data.len(),
                expected: y_size * 2,
            });
        }

        let mut i420 = vec![0u8; y_size + uv_size * 2];

        // Extract Y plane (every other byte from YUY2)
        for row in 0..height as usize {
            let yuy2_row_start = row * width as usize * 2;
            let i420_y_row_start = row * width as usize;
            for col in 0..width as usize {
                let yuy2_idx = yuy2_row_start + col * 2;
                let i420_idx = i420_y_row_start + col;
                if yuy2_idx + 1 < yuy2_data.len() {
                    i420[i420_idx] = yuy2_data[yuy2_idx];
                }
            }
        }

        // Collect U and V values from YUY2 pattern
        // YUY2 pattern: Y0 U Y1 V Y2 U Y3 V ...
        // U appears at positions idx % 4 == 1
        // V appears at positions idx % 4 == 3
        let mut u_vals = Vec::new();
        let mut v_vals = Vec::new();
        for (idx, &byte) in yuy2_data.iter().enumerate() {
            if idx % 4 == 1 {
                u_vals.push(byte);
            } else if idx % 4 == 3 {
                v_vals.push(byte);
            }
        }

        // U and V are at half resolution (width/2 * height/2 each)
        let uv_plane_size = uv_size;
        // Fill U plane (first half of chroma)
        let u_plane_len = std::cmp::min(u_vals.len(), uv_plane_size);
        i420[y_size..y_size + u_plane_len].copy_from_slice(&u_vals[..u_plane_len]);
        // Fill V plane (second half of chroma)
        let v_plane_len = std::cmp::min(v_vals.len(), uv_plane_size);
        i420[y_size + u_plane_len..y_size + u_plane_len + v_plane_len]
            .copy_from_slice(&v_vals[..v_plane_len]);

        Ok(i420)
    }

    /// Convert BGRA packed format to I420.
    ///
    /// BGRA: 4 bytes per pixel (B G R A), full resolution.
    /// I420: Y at full resolution, U/V at half resolution.
    ///
    /// # Safety
    ///
    /// `bgra_data` must point at `width * height * 4` bytes in BGRA layout,
    /// and the returned I420 data must be exactly `width * height * 3 / 2` bytes.
    fn bgra_to_i420(bgra_data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, VideoCodecError> {
        let y_size = width as usize * height as usize;
        let uv_size = y_size / 4;
        if bgra_data.len() < y_size * 4 {
            return Err(VideoCodecError::UnexpectedInputSize {
                actual: bgra_data.len(),
                expected: y_size * 4,
            });
        }

        let mut i420 = vec![0u8; y_size + uv_size * 2];

        // Copy Y plane from luma (use green channel as approximation)
        for (i, byte) in i420[..y_size].iter_mut().enumerate() {
            // Use green channel as luma approximation
            let bgra_idx = i * 4 + 1; // G is at offset 1 in BGRA
            if bgra_idx < bgra_data.len() {
                *byte = bgra_data[bgra_idx];
            }
        }

        // U plane (half width, half height) - use mid-value as placeholder
        let u_v_stride = width as usize / 2;
        for dst in i420[y_size..y_size + uv_size].chunks_exact_mut(u_v_stride) {
            for byte in dst.iter_mut() {
                *byte = 128; // neutral U value
            }
        }

        // V plane
        for dst in i420[y_size + uv_size..].chunks_exact_mut(u_v_stride) {
            for byte in dst.iter_mut() {
                *byte = 128; // neutral V value
            }
        }

        Ok(i420)
    }

    /// Convert a captured [`nexo_video::VideoFrame`]'s [`nexo_video::PixelFormat`] to I420 bytes.
    ///
    /// The caller must ensure the input data has the correct size for the format.
    pub fn frame_to_i420(frame: &nexo_video::VideoFrame) -> Result<Vec<u8>, VideoCodecError> {
        match frame.format {
            nexo_video::PixelFormat::Nv12 => {
                Self::nv12_to_i420(&frame.data, frame.width, frame.height)
            }
            nexo_video::PixelFormat::Yuy2 => {
                Self::yuy2_to_i420(&frame.data, frame.width, frame.height)
            }
            nexo_video::PixelFormat::Mjpg => {
                // MJPEG is compressed; needs decompression before encoding
                Err(VideoCodecError::UnexpectedInputSize {
                    actual: 0,
                    expected: 0,
                })
            }
            nexo_video::PixelFormat::Bgra8 => {
                Self::bgra_to_i420(&frame.data, frame.width, frame.height)
            }
            nexo_video::PixelFormat::Unknown => Err(VideoCodecError::UnexpectedInputSize {
                actual: 0,
                expected: 0,
            }),
        }
    }
}

pub fn frame_to_i420(frame: &nexo_video::VideoFrame) -> Result<Vec<u8>, VideoCodecError> {
    Vp8Encoder::frame_to_i420(frame)
}

impl Drop for Vp8Encoder {
    fn drop(&mut self) {
        // SAFETY: `self.ctx` is a live encoder or zeroed; libvpx treats a
        // zeroed/destroyed context as a no-op destroy.
        unsafe {
            vpx_codec_destroy(&mut self.ctx);
        }
    }
}

/// Decoded I420 frame ready for rendering.
///
/// Rows may carry padding, so consume `y_stride`/`uv_stride` bytes per row.
#[derive(Debug, Eq, PartialEq)]
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub y_plane: Box<[u8]>,
    pub u_plane: Box<[u8]>,
    pub v_plane: Box<[u8]>,
    pub y_stride: usize,
    pub uv_stride: usize,
}

impl DecodedVideoFrame {
    /// Convert the decoded I420 frame to 32-bit RGBA pixels (`[R, G, B, A]`).
    #[must_use]
    #[allow(clippy::many_single_char_names)]
    pub fn to_rgba(&self) -> Vec<u8> {
        let width = self.width as usize;
        let height = self.height as usize;
        let mut rgba = vec![0u8; width * height * 4];

        for y in 0..height {
            let y_row = y * self.y_stride;
            let uv_row = (y / 2) * self.uv_stride;
            let out_row = y * width * 4;

            for x in 0..width {
                let y_val = i32::from(self.y_plane[y_row + x]);
                let u_val = i32::from(self.u_plane[uv_row + (x / 2)]);
                let v_val = i32::from(self.v_plane[uv_row + (x / 2)]);

                let c = y_val - 16;
                let d = u_val - 128;
                let e = v_val - 128;

                let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
                let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
                let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;

                let idx = out_row + x * 4;
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255;
            }
        }
        rgba
    }
}

/// Convert tightly packed I420 bytes to 32-bit RGBA pixels (`[R, G, B, A]`).
#[allow(clippy::many_single_char_names)]
pub fn i420_to_rgba(i420: &[u8], width: u32, height: u32) -> Result<Vec<u8>, VideoCodecError> {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = y_size / 4;
    let total = y_size + uv_size * 2;
    if i420.len() != total {
        return Err(VideoCodecError::UnexpectedInputSize {
            actual: i420.len(),
            expected: total,
        });
    }
    let y_plane = &i420[..y_size];
    let u_plane = &i420[y_size..y_size + uv_size];
    let v_plane = &i420[y_size + uv_size..];

    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let y_row = y * w;
        let uv_row = (y / 2) * (w / 2);
        let out_row = y * w * 4;

        for x in 0..w {
            let y_val = i32::from(y_plane[y_row + x]);
            let u_val = i32::from(u_plane[uv_row + (x / 2)]);
            let v_val = i32::from(v_plane[uv_row + (x / 2)]);

            let c = y_val - 16;
            let d = u_val - 128;
            let e = v_val - 128;

            let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;

            let idx = out_row + x * 4;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 255;
        }
    }
    Ok(rgba)
}

/// Software VP8 decoder backed by a libvpx decoder context.
pub struct Vp8Decoder {
    ctx: vpx_codec_ctx_t,
}

impl Vp8Decoder {
    /// Create a VP8 decoder.
    pub fn new() -> Result<Self, VideoCodecError> {
        let iface = ptr::NonNull::new(unsafe { vpx_codec_vp8_dx() }.cast_mut()).ok_or(
            VideoCodecError::Unavailable {
                component: "decoder",
            },
        )?;
        // SAFETY: zeroed POD; `dec_init_ver` fills it in.
        let mut ctx: vpx_codec_ctx_t = unsafe { MaybeUninit::zeroed().assume_init() };
        // SAFETY: `ctx` is writable; a null `cfg` selects the decoder defaults.
        let result = unsafe {
            vpx_codec_dec_init_ver(
                &mut ctx,
                iface.as_ptr(),
                ptr::null(),
                0,
                VPX_DECODER_ABI_VERSION,
            )
        };
        if result != vpx_codec_err_t::VPX_CODEC_OK {
            return Err(VideoCodecError::Codec {
                operation: "initialize the VP8 decoder",
                detail: format!("{result:?}"),
            });
        }
        Ok(Self { ctx })
    }

    /// Decode one VP8 frame, returning the decoded I420 planes, or `Ok(None)`
    /// when the decoder needs more input (e.g. the keyframe of an inter frame).
    pub fn decode(
        &mut self,
        frame: &EncodedVideoFrame,
    ) -> Result<Option<DecodedVideoFrame>, VideoCodecError> {
        if frame.codec != VideoCodec::Vp8 {
            return Err(VideoCodecError::NotVp8);
        }
        // SAFETY: `self.ctx` is a live decoder; `frame.data` is a plain owned
        // buffer alive for this call.
        let result = unsafe {
            vpx_codec_decode(
                &mut self.ctx,
                frame.data.as_ptr(),
                frame.data.len() as c_uint,
                ptr::null_mut(),
                VPX_DL_REALTIME as _,
            )
        };
        check(result, "decode the VP8 frame", &self.ctx)?;

        let mut iter: vpx_codec_iter_t = ptr::null();
        // SAFETY: `iter` is an opaque cursor; the returned image is owned by
        // the decoder context and borrowed only for this function.
        let image = unsafe { vpx_codec_get_frame(&mut self.ctx, &mut iter) };
        if image.is_null() {
            return Ok(None);
        }
        // SAFETY: `image` is non-null and points at the frame libvpx just
        // produced; it stays valid until the next call on `self.ctx`.
        let image = unsafe { &*image };
        let width = image.d_w;
        let height = image.d_h;
        let y_stride = usize::try_from(image.stride[0]).unwrap_or(0);
        let uv_stride = usize::try_from(image.stride[1]).unwrap_or(0);
        let height = usize::try_from(height).unwrap_or(0);
        let y_len = y_stride
            .checked_mul(height)
            .ok_or_else(|| VideoCodecError::Codec {
                operation: "size the decoded luma plane",
                detail: "stride overflow".to_owned(),
            })?;
        let uv_len = uv_stride
            .checked_mul(height / 2)
            .ok_or_else(|| VideoCodecError::Codec {
                operation: "size the decoded chroma plane",
                detail: "stride overflow".to_owned(),
            })?;
        // SAFETY: the planes are sized by libvpx to `stride * height` (with the
        // chroma at half height); lengths come from the decoder, not the
        // network, and we copy out immediately.
        let (y, u, v) = unsafe {
            (
                slice::from_raw_parts(image.planes[0], y_len).to_vec(),
                slice::from_raw_parts(image.planes[1], uv_len).to_vec(),
                slice::from_raw_parts(image.planes[2], uv_len).to_vec(),
            )
        };
        Ok(Some(DecodedVideoFrame {
            width,
            height: image.d_h,
            y_plane: y.into_boxed_slice(),
            u_plane: u.into_boxed_slice(),
            v_plane: v.into_boxed_slice(),
            y_stride,
            uv_stride,
        }))
    }
}

impl Drop for Vp8Decoder {
    fn drop(&mut self) {
        // SAFETY: `self.ctx` is a live decoder or zeroed; libvpx treats a
        // zeroed/destroyed context as a no-op destroy.
        unsafe {
            vpx_codec_destroy(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i420_frame(width: u32, height: u32) -> Vec<u8> {
        let y_size = width as usize * height as usize;
        let mut data = vec![0u8; y_size + y_size / 2];
        for row in 0..height {
            for column in 0..width {
                // Vertical gradient so top vs bottom row sampling is measurable.
                let value = u8::try_from(row * 255 / height.max(1)).unwrap_or(u8::MAX);
                data[row as usize * width as usize + column as usize] = value;
            }
        }
        let plane_quarter = y_size / 4;
        for (plane, byte) in [(1u8, 100u8), (2, 180)] {
            let offset = y_size + (plane as usize - 1) * plane_quarter;
            for value in &mut data[offset..offset + plane_quarter] {
                *value = byte;
            }
        }
        data
    }

    #[test]
    fn vp8_encoder_decoder_roundtrip_preserves_frame() {
        let (width, height) = (640u32, 480u32);
        let mut encoder = Vp8Encoder::new(width, height, 1_500).expect("encoder should init");
        let mut decoder = Vp8Decoder::new().expect("decoder should init");
        let timestamp = Duration::from_millis(33);
        let input = i420_frame(width, height);

        let bitstream = encoder
            .encode_frame(timestamp, &input)
            .expect("frame should encode")
            .expect("encoder should emit a frame");
        assert_eq!(bitstream.codec, VideoCodec::Vp8);
        assert_eq!((bitstream.width, bitstream.height), (width, height));
        assert_eq!(bitstream.timestamp, timestamp);
        assert!(bitstream.is_keyframe, "the first frame must be a keyframe");
        assert!(!bitstream.data.is_empty());

        let decoded_frame = decoder
            .decode(&bitstream)
            .expect("frame should decode")
            .expect("decoder should emit a frame");
        assert_eq!((decoded_frame.width, decoded_frame.height), (width, height));
        assert_eq!(
            decoded_frame.y_plane.len(),
            decoded_frame.y_stride * height as usize
        );
        assert_eq!(
            decoded_frame.u_plane.len(),
            decoded_frame.uv_stride * (height as usize / 2)
        );

        let luma = |from: usize, to: usize| {
            let sum: usize = decoded_frame.y_plane[from..to]
                .iter()
                .map(|v| usize::from(*v))
                .sum();
            (sum / (to - from)) as u8
        };
        let left = luma(0, decoded_frame.y_stride * 8);
        let right = luma(
            decoded_frame.y_stride * (height as usize - 8),
            decoded_frame.y_stride * height as usize,
        );
        assert!(
            right > left,
            "the gradient must survive the round trip (left avg {left}, right avg {right})"
        );

        let chroma_avg = |plane: &[u8]| {
            let sum: usize = plane.iter().map(|v| usize::from(*v)).sum();
            (sum / plane.len()) as u8
        };
        assert!(
            chroma_avg(&decoded_frame.u_plane).abs_diff(100) < 24,
            "U plane should stay near 100"
        );
        assert!(
            chroma_avg(&decoded_frame.v_plane).abs_diff(180) < 24,
            "V plane should stay near 180"
        );
    }

    #[test]
    fn vp8_encoder_rejects_odd_dimensions() {
        let Err(error) = Vp8Encoder::new(641, 480, 1_000) else {
            panic!("odd width must be rejected");
        };
        assert!(matches!(
            error,
            VideoCodecError::InvalidDimensions { width: 641, .. }
        ));
    }

    #[test]
    fn vp8_encoder_rejects_undersized_input() {
        let mut encoder = match Vp8Encoder::new(640, 480, 1_000) {
            Ok(encoder) => encoder,
            Err(error) => panic!("encoder should init: {error}"),
        };
        let too_small = vec![0u8; 640 * 480];
        assert!(matches!(
            encoder.encode_frame(Duration::ZERO, &too_small),
            Err(VideoCodecError::UnexpectedInputSize { .. })
        ));
    }

    #[test]
    fn decoded_frame_to_rgba_produces_valid_buffer() {
        let input = i420_frame(640, 480);
        let rgba = i420_to_rgba(&input, 640, 480).expect("conversion should succeed");
        assert_eq!(rgba.len(), 640 * 480 * 4);
        assert_eq!(rgba[3], 255); // Alpha should be 255
    }
}
