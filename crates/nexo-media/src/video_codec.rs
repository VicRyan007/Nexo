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

use openh264::{decoder::Decoder, formats::YUVSource};

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
    #[error("encoded frame is not H.264")]
    NotH264,
    #[error("could not decode MJPEG frame: {detail}")]
    Mjpeg { detail: String },
    #[error("encoded frame is {actual} bytes, expected exactly {expected} bytes of I420")]
    UnexpectedInputSize { actual: usize, expected: usize },
}

/// Software H.264 decoder used for frames emitted by a hardware encoder on a
/// different machine. It is deliberately independent from the local encoder:
/// receive capability must never depend on whether this machine has a GPU MFT.
pub struct H264Decoder {
    decoder: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self, VideoCodecError> {
        let decoder = Decoder::new().map_err(|error| VideoCodecError::Codec {
            operation: "initialize the H.264 decoder",
            detail: error.to_string(),
        })?;
        Ok(Self { decoder })
    }

    pub fn decode(
        &mut self,
        frame: &EncodedVideoFrame,
    ) -> Result<Option<DecodedVideoFrame>, VideoCodecError> {
        if frame.codec != VideoCodec::H264 {
            return Err(VideoCodecError::NotH264);
        }
        let Some(decoded) =
            self.decoder
                .decode(&frame.data)
                .map_err(|error| VideoCodecError::Codec {
                    operation: "decode the H.264 frame",
                    detail: error.to_string(),
                })?
        else {
            return Ok(None);
        };
        let (width, height) = decoded.dimensions();
        let (y_stride, uv_stride, _) = decoded.strides();
        if width == 0 || height == 0 || y_stride < width || uv_stride < width / 2 {
            return Err(VideoCodecError::Codec {
                operation: "validate the decoded H.264 planes",
                detail: format!(
                    "invalid dimensions or strides {width}x{height} / {y_stride}/{uv_stride}"
                ),
            });
        }
        let y_len = y_stride
            .checked_mul(height)
            .ok_or_else(|| VideoCodecError::Codec {
                operation: "size the decoded H.264 luma plane",
                detail: "stride overflow".to_owned(),
            })?;
        let uv_len = uv_stride
            .checked_mul(height / 2)
            .ok_or_else(|| VideoCodecError::Codec {
                operation: "size the decoded H.264 chroma planes",
                detail: "stride overflow".to_owned(),
            })?;
        let (y, u, v) = (decoded.y(), decoded.u(), decoded.v());
        if y.len() < y_len || u.len() < uv_len || v.len() < uv_len {
            return Err(VideoCodecError::Codec {
                operation: "validate the decoded H.264 buffers",
                detail: "decoder returned a short plane".to_owned(),
            });
        }
        Ok(Some(DecodedVideoFrame {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
            y_plane: y[..y_len].to_vec().into_boxed_slice(),
            u_plane: u[..uv_len].to_vec().into_boxed_slice(),
            v_plane: v[..uv_len].to_vec().into_boxed_slice(),
            y_stride,
            uv_stride,
        }))
    }
}

/// Decoder selected by the codec on each received frame.
pub enum VideoDecoder {
    Vp8(Vp8Decoder),
    H264(H264Decoder),
}

impl VideoDecoder {
    pub fn new() -> Result<Self, VideoCodecError> {
        Ok(Self::Vp8(Vp8Decoder::new()?))
    }

    pub fn decode(
        &mut self,
        frame: &EncodedVideoFrame,
    ) -> Result<Option<DecodedVideoFrame>, VideoCodecError> {
        match (self, frame.codec) {
            (Self::Vp8(decoder), VideoCodec::Vp8) => decoder.decode(frame),
            (Self::H264(decoder), VideoCodec::H264) => decoder.decode(frame),
            (slot @ Self::Vp8(_), VideoCodec::H264) => {
                *slot = Self::H264(H264Decoder::new()?);
                match slot {
                    Self::H264(decoder) => decoder.decode(frame),
                    Self::Vp8(_) => unreachable!("decoder slot was just replaced"),
                }
            }
            (slot @ Self::H264(_), VideoCodec::Vp8) => {
                *slot = Self::Vp8(Vp8Decoder::new()?);
                match slot {
                    Self::Vp8(decoder) => decoder.decode(frame),
                    Self::H264(_) => unreachable!("decoder slot was just replaced"),
                }
            }
        }
    }
}

fn validate_dimensions(width: u32, height: u32) -> Result<(usize, usize), VideoCodecError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(VideoCodecError::InvalidDimensions { width, height });
    }
    let width =
        usize::try_from(width).map_err(|_| VideoCodecError::InvalidDimensions { width, height })?;
    let height = usize::try_from(height).map_err(|_| VideoCodecError::InvalidDimensions {
        width: u32::try_from(width).unwrap_or(u32::MAX),
        height,
    })?;
    Ok((width, height))
}

fn i420_size(width: usize, height: usize) -> Result<usize, VideoCodecError> {
    let y_size = width
        .checked_mul(height)
        .ok_or(VideoCodecError::InvalidDimensions {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
        })?;
    y_size
        .checked_add(y_size / 2)
        .ok_or(VideoCodecError::InvalidDimensions {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
        })
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
        let (width, height) = validate_dimensions(width, height)?;
        let y_size = width * height;
        let uv_size = y_size / 4;
        if n12_data.len() < y_size + uv_size {
            return Err(VideoCodecError::UnexpectedInputSize {
                actual: n12_data.len(),
                expected: y_size + uv_size,
            });
        }

        let mut i420 = vec![0u8; y_size + uv_size * 2];

        // Copy Y plane (interleaved at top of NV12)
        i420[..y_size].copy_from_slice(&n12_data[..y_size]);

        // Copy U plane (NV12 has U/V interleaved at half resolution)
        // NV12 UV layout: U0 V0 U1 V1 ... at half both dimensions
        // I420 U plane: U0 U1 U2 ... at half width
        let u_offset = y_size;
        let v_offset = y_size + uv_size;
        for (i, dst) in i420[u_offset..v_offset]
            .chunks_exact_mut(width / 2)
            .enumerate()
        {
            let src_base = y_size + i * width;
            // Take every other byte (U values from interleaved UV)
            for (j, byte) in dst.iter_mut().enumerate() {
                let src_idx = src_base + j * 2;
                if src_idx < y_size + uv_size * 2 {
                    *byte = n12_data[src_idx];
                }
            }
        }

        // Copy V plane
        for (i, dst) in i420[v_offset..].chunks_exact_mut(width / 2).enumerate() {
            let src_base = y_size + i * width + 1;
            // Take every other byte starting from 1 (V values from interleaved UV)
            for (j, byte) in dst.iter_mut().enumerate() {
                let src_idx = src_base + j * 2;
                if src_idx < y_size + uv_size * 2 {
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
        let (width, height) = validate_dimensions(width, height)?;
        let y_size = width * height;
        let uv_size = y_size / 4;
        if yuy2_data.len() < y_size * 2 {
            return Err(VideoCodecError::UnexpectedInputSize {
                actual: yuy2_data.len(),
                expected: y_size * 2,
            });
        }

        let mut i420 = vec![0u8; y_size + uv_size * 2];

        // Extract Y plane (every other byte from YUY2)
        for row in 0..height {
            let yuy2_row_start = row * width * 2;
            let i420_y_row_start = row * width;
            for col in 0..width {
                let yuy2_idx = yuy2_row_start + col * 2;
                let i420_idx = i420_y_row_start + col;
                if yuy2_idx + 1 < yuy2_data.len() {
                    i420[i420_idx] = yuy2_data[yuy2_idx];
                }
            }
        }

        // YUY2 carries one U/V pair for every horizontal 2-pixel group on
        // every row (4:2:2). I420 needs one pair for every 2x2 block (4:2:0),
        // so average the corresponding chroma samples from two source rows.
        let u_offset = y_size;
        let v_offset = y_size + uv_size;
        let uv_width = width / 2;
        let uv_height = height / 2;
        for row in 0..uv_height {
            for column in 0..uv_width {
                let top = ((row * 2) * width + column * 2) * 2;
                let bottom = (((row * 2 + 1) * width) + column * 2) * 2;
                let u = u32::midpoint(
                    u32::from(yuy2_data[top + 1]),
                    u32::from(yuy2_data[bottom + 1]),
                );
                let v = u32::midpoint(
                    u32::from(yuy2_data[top + 3]),
                    u32::from(yuy2_data[bottom + 3]),
                );
                let index = row * uv_width + column;
                i420[u_offset + index] = u8::try_from(u).unwrap_or(u8::MAX);
                i420[v_offset + index] = u8::try_from(v).unwrap_or(u8::MAX);
            }
        }

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
        let (width, height) = validate_dimensions(width, height)?;
        let y_size = width * height;
        let uv_size = y_size / 4;
        if bgra_data.len() < y_size * 4 {
            return Err(VideoCodecError::UnexpectedInputSize {
                actual: bgra_data.len(),
                expected: y_size * 4,
            });
        }

        let mut i420 = vec![0u8; y_size + uv_size * 2];

        // Convert every pixel's RGB components to the full-resolution Y plane.
        // Screen capture is commonly colorful, so using only the green channel
        // here would silently turn the shared desktop into grayscale video.
        for row in 0..height {
            for column in 0..width {
                let pixel = (row * width + column) * 4;
                let blue = bgra_data[pixel];
                let green = bgra_data[pixel + 1];
                let red = bgra_data[pixel + 2];
                i420[row * width + column] = rgb_to_y(red, green, blue);
            }
        }

        // Average each 2x2 RGB block into the subsampled U/V planes. This is
        // the same 4:2:0 layout expected by the VP8 and H.264 encoders.
        let u_offset = y_size;
        let v_offset = y_size + uv_size;
        let uv_width = width / 2;
        let uv_height = height / 2;
        for row in 0..uv_height {
            for column in 0..uv_width {
                let mut u_sum = 0u32;
                let mut v_sum = 0u32;
                for source_row in 0..2 {
                    for source_column in 0..2 {
                        let pixel =
                            ((row * 2 + source_row) * width + column * 2 + source_column) * 4;
                        let blue = bgra_data[pixel];
                        let green = bgra_data[pixel + 1];
                        let red = bgra_data[pixel + 2];
                        u_sum += u32::from(rgb_to_u(red, green, blue));
                        v_sum += u32::from(rgb_to_v(red, green, blue));
                    }
                }
                let index = row * uv_width + column;
                i420[u_offset + index] = u8::try_from(u_sum / 4).unwrap_or(u8::MAX);
                i420[v_offset + index] = u8::try_from(v_sum / 4).unwrap_or(u8::MAX);
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
                Self::mjpg_to_i420(&frame.data, frame.width, frame.height)
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

    fn mjpg_to_i420(mjpg_data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, VideoCodecError> {
        let (width, height) = validate_dimensions(width, height)?;
        let decoded = image::load_from_memory_with_format(mjpg_data, image::ImageFormat::Jpeg)
            .map_err(|error| VideoCodecError::Mjpeg {
                detail: error.to_string(),
            })?
            .to_rgb8();
        if decoded.width() != u32::try_from(width).unwrap_or(u32::MAX)
            || decoded.height() != u32::try_from(height).unwrap_or(u32::MAX)
        {
            return Err(VideoCodecError::Mjpeg {
                detail: format!(
                    "quadro JPEG tem {}x{}, mas a camera informou {}x{}",
                    decoded.width(),
                    decoded.height(),
                    width,
                    height
                ),
            });
        }

        let y_size = width * height;
        let uv_width = width / 2;
        let uv_height = height / 2;
        let uv_size = uv_width * uv_height;
        let mut output = vec![0u8; y_size + uv_size * 2];
        for y in 0..height {
            for x in 0..width {
                let pixel = decoded.get_pixel(x as u32, y as u32).0;
                output[y * width + x] = rgb_to_y(pixel[0], pixel[1], pixel[2]);
            }
        }
        let u_offset = y_size;
        let v_offset = y_size + uv_size;
        for y in 0..uv_height {
            for x in 0..uv_width {
                let mut u_sum = 0u32;
                let mut v_sum = 0u32;
                for row in 0..2 {
                    for column in 0..2 {
                        let pixel = decoded
                            .get_pixel((x * 2 + column) as u32, (y * 2 + row) as u32)
                            .0;
                        u_sum += rgb_to_u(pixel[0], pixel[1], pixel[2]) as u32;
                        v_sum += rgb_to_v(pixel[0], pixel[1], pixel[2]) as u32;
                    }
                }
                let index = y * uv_width + x;
                output[u_offset + index] = u8::try_from(u_sum / 4).unwrap_or(u8::MAX);
                output[v_offset + index] = u8::try_from(v_sum / 4).unwrap_or(u8::MAX);
            }
        }
        Ok(output)
    }
}

fn clamp_to_u8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or_default()
}

fn rgb_to_y(red: u8, green: u8, blue: u8) -> u8 {
    clamp_to_u8(
        (66 * i32::from(red) + 129 * i32::from(green) + 25 * i32::from(blue) + 128) / 256 + 16,
    )
}

fn rgb_to_u(red: u8, green: u8, blue: u8) -> u8 {
    clamp_to_u8(
        (-38 * i32::from(red) - 74 * i32::from(green) + 112 * i32::from(blue) + 128) / 256 + 128,
    )
}

fn rgb_to_v(red: u8, green: u8, blue: u8) -> u8 {
    clamp_to_u8(
        (112 * i32::from(red) - 94 * i32::from(green) - 18 * i32::from(blue) + 128) / 256 + 128,
    )
}

/// Resize a tightly packed I420 frame with bounded nearest-neighbour sampling.
/// The video engine uses this to keep camera and monitor captures within the
/// fixed software VP8 profile while accepting any even source resolution.
pub fn resize_i420_nearest(
    input: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>, VideoCodecError> {
    let (source_width, source_height) = validate_dimensions(source_width, source_height)?;
    let (target_width, target_height) = validate_dimensions(target_width, target_height)?;
    let source_size = i420_size(source_width, source_height)?;
    if input.len() != source_size {
        return Err(VideoCodecError::UnexpectedInputSize {
            actual: input.len(),
            expected: source_size,
        });
    }
    let target_size = i420_size(target_width, target_height)?;
    if source_width == target_width && source_height == target_height {
        return Ok(input.to_vec());
    }

    let source_y_size = source_width * source_height;
    let source_uv_size = source_y_size / 4;
    let target_y_size = target_width * target_height;
    let target_uv_width = target_width / 2;
    let target_uv_height = target_height / 2;
    let source_uv_width = source_width / 2;
    let source_uv_height = source_height / 2;
    let source_y = &input[..source_y_size];
    let source_u = &input[source_y_size..source_y_size + source_uv_size];
    let source_v = &input[source_y_size + source_uv_size..];
    let mut output = vec![0u8; target_size];
    let target_y = &mut output[..target_y_size];
    for target_y_index in 0..target_height {
        let source_row = target_y_index * source_height / target_height;
        for target_x_index in 0..target_width {
            let source_column = target_x_index * source_width / target_width;
            target_y[target_y_index * target_width + target_x_index] =
                source_y[source_row * source_width + source_column];
        }
    }
    let (target_u, target_v) =
        output[target_y_size..].split_at_mut(target_uv_width * target_uv_height);
    for target_y_index in 0..target_uv_height {
        let source_row = target_y_index * source_uv_height / target_uv_height;
        for target_x_index in 0..target_uv_width {
            let source_column = target_x_index * source_uv_width / target_uv_width;
            target_u[target_y_index * target_uv_width + target_x_index] =
                source_u[source_row * source_uv_width + source_column];
            target_v[target_y_index * target_uv_width + target_x_index] =
                source_v[source_row * source_uv_width + source_column];
        }
    }
    Ok(output)
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

/// Convert tightly packed I420 into tightly packed NV12.
///
/// This is the upload format accepted by the Windows hardware H.264 MFT.
/// The conversion is deliberately explicit and bounded so a native encoder
/// never receives a buffer whose layout was inferred from untrusted input.
pub fn i420_to_nv12(i420: &[u8], width: u32, height: u32) -> Result<Vec<u8>, VideoCodecError> {
    let (width, height) = validate_dimensions(width, height)?;
    let y_size = width
        .checked_mul(height)
        .ok_or(VideoCodecError::InvalidDimensions {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
        })?;
    let chroma_size = y_size / 4;
    let expected =
        y_size
            .checked_add(chroma_size * 2)
            .ok_or(VideoCodecError::InvalidDimensions {
                width: u32::try_from(width).unwrap_or(u32::MAX),
                height: u32::try_from(height).unwrap_or(u32::MAX),
            })?;
    if i420.len() != expected {
        return Err(VideoCodecError::UnexpectedInputSize {
            actual: i420.len(),
            expected,
        });
    }
    let mut nv12 = vec![0u8; expected];
    nv12[..y_size].copy_from_slice(&i420[..y_size]);
    let u = &i420[y_size..y_size + chroma_size];
    let v = &i420[y_size + chroma_size..];
    for (index, pair) in nv12[y_size..].chunks_exact_mut(2).enumerate() {
        pair[0] = u[index];
        pair[1] = v[index];
    }
    Ok(nv12)
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
        let Some(pixel_count) = width.checked_mul(height) else {
            return Vec::new();
        };
        let Some(byte_count) = pixel_count.checked_mul(4) else {
            return Vec::new();
        };
        let mut rgba = vec![0u8; byte_count];

        for y in 0..height {
            let y_row = y * self.y_stride;
            let uv_row = (y / 2) * self.uv_stride;
            let out_row = y * width * 4;

            for x in 0..width {
                let y_val = i32::from(*self.y_plane.get(y_row + x).unwrap_or(&0));
                let u_val = i32::from(*self.u_plane.get(uv_row + (x / 2)).unwrap_or(&128));
                let v_val = i32::from(*self.v_plane.get(uv_row + (x / 2)).unwrap_or(&128));

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
    let (w, h) = validate_dimensions(width, height)?;
    let y_size = w
        .checked_mul(h)
        .ok_or(VideoCodecError::InvalidDimensions { width, height })?;
    let uv_size = y_size / 4;
    let total = i420_size(w, h)?;
    if i420.len() != total {
        return Err(VideoCodecError::UnexpectedInputSize {
            actual: i420.len(),
            expected: total,
        });
    }
    let y_plane = &i420[..y_size];
    let u_plane = &i420[y_size..y_size + uv_size];
    let v_plane = &i420[y_size + uv_size..];

    let byte_count = y_size
        .checked_mul(4)
        .ok_or(VideoCodecError::InvalidDimensions { width, height })?;
    let mut rgba = vec![0u8; byte_count];
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
        let (width_usize, height_usize) = validate_dimensions(width, height)?;
        let y_stride = usize::try_from(image.stride[0]).unwrap_or(0);
        let uv_stride = usize::try_from(image.stride[1]).unwrap_or(0);
        if y_stride < width_usize || uv_stride < width_usize / 2 {
            return Err(VideoCodecError::Codec {
                operation: "validate the decoded VP8 planes",
                detail: format!("invalid strides {y_stride}/{uv_stride} for {width}x{height}"),
            });
        }
        if image.planes[0].is_null() || image.planes[1].is_null() || image.planes[2].is_null() {
            return Err(VideoCodecError::Codec {
                operation: "validate the decoded VP8 planes",
                detail: "libvpx returned a null plane".to_owned(),
            });
        }
        let height = height_usize;
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

    #[test]
    fn bgra_conversion_preserves_chroma_for_screen_capture() {
        let frame = nexo_video::VideoFrame {
            width: 2,
            height: 2,
            format: nexo_video::PixelFormat::Bgra8,
            timestamp: Duration::ZERO,
            data: [0, 0, 255, 255].repeat(4).into_boxed_slice(),
        };
        let i420 = Vp8Encoder::frame_to_i420(&frame).expect("BGRA should convert");

        assert_eq!(&i420[..4], &[rgb_to_y(255, 0, 0); 4]);
        assert_eq!(i420[4], rgb_to_u(255, 0, 0));
        assert_eq!(i420[5], rgb_to_v(255, 0, 0));
        assert_ne!(i420[4], 128, "red screen content must not become grayscale");
        assert_ne!(i420[5], 128, "red screen content must not become grayscale");
    }

    #[test]
    fn nv12_conversion_copies_only_the_luma_plane_and_separates_chroma() {
        let width = 4_u32;
        let height = 2_u32;
        let y_size = usize::try_from(width * height).expect("test dimensions fit");
        let mut nv12 = vec![0u8; y_size + y_size / 2];
        nv12[..y_size].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        nv12[y_size..].copy_from_slice(&[10, 20, 30, 40]);
        let i420 =
            Vp8Encoder::nv12_to_i420(&nv12, width, height).expect("valid NV12 should convert");
        assert_eq!(&i420[..y_size], &nv12[..y_size]);
        assert_eq!(&i420[y_size..y_size + 2], &[10, 30]);
        assert_eq!(&i420[y_size + 2..], &[20, 40]);
    }

    #[test]
    fn yuy2_conversion_vertically_subsamples_chroma_into_i420() {
        let width = 4_u32;
        let height = 4_u32;
        let mut yuy2 = Vec::new();
        for row in 0..height {
            let u = 10 + row * 20;
            let v = 20 + row * 20;
            for group in 0..(width / 2) {
                yuy2.extend_from_slice(&[
                    16 + u8::try_from(row + group).unwrap_or_default(),
                    u8::try_from(u).unwrap_or_default(),
                    16,
                    u8::try_from(v).unwrap_or_default(),
                ]);
            }
        }

        let i420 =
            Vp8Encoder::yuy2_to_i420(&yuy2, width, height).expect("valid YUY2 should convert");
        let y_size = usize::try_from(width * height).expect("test dimensions fit");
        let uv_size = y_size / 4;
        assert_eq!(&i420[y_size..y_size + uv_size], &[20, 20, 60, 60]);
        assert_eq!(&i420[y_size + uv_size..], &[30, 30, 70, 70]);
    }

    #[test]
    fn i420_to_nv12_interleaves_chroma_without_changing_luma() {
        let i420 = [1_u8, 2, 3, 4, 5, 6, 7, 8, 10, 20, 30, 40];
        let nv12 = i420_to_nv12(&i420, 4, 2).expect("valid I420 should convert");
        assert_eq!(nv12, vec![1, 2, 3, 4, 5, 6, 7, 8, 10, 30, 20, 40]);
    }

    #[test]
    fn invalid_capture_dimensions_are_rejected_without_panicking() {
        let result = Vp8Encoder::frame_to_i420(&nexo_video::VideoFrame {
            width: 0,
            height: 0,
            format: nexo_video::PixelFormat::Nv12,
            timestamp: Duration::ZERO,
            data: vec![0; 16].into_boxed_slice(),
        });
        assert!(matches!(
            result,
            Err(VideoCodecError::InvalidDimensions { .. })
        ));
    }

    #[test]
    fn rgba_conversion_rejects_invalid_dimensions_without_panicking() {
        assert!(matches!(
            i420_to_rgba(&[], 0, 480),
            Err(VideoCodecError::InvalidDimensions { .. })
        ));
        assert!(matches!(
            i420_to_rgba(&[], 641, 480),
            Err(VideoCodecError::InvalidDimensions { .. })
        ));
    }

    #[test]
    fn malformed_decoded_planes_render_as_black_instead_of_panicking() {
        let frame = DecodedVideoFrame {
            width: 4,
            height: 2,
            y_plane: vec![16].into_boxed_slice(),
            u_plane: vec![128].into_boxed_slice(),
            v_plane: vec![128].into_boxed_slice(),
            y_stride: 1,
            uv_stride: 1,
        };
        let rgba = frame.to_rgba();
        assert_eq!(rgba.len(), 4 * 2 * 4);
        assert_eq!(&rgba[..4], &[0, 0, 0, 255]);
    }

    #[test]
    fn i420_resize_accepts_a_different_even_capture_resolution() {
        let source = vec![128u8; 4 * 2 * 3 / 2];
        let resized =
            resize_i420_nearest(&source, 4, 2, 2, 2).expect("even I420 frames should resize");
        assert_eq!(resized.len(), 2 * 2 * 3 / 2);
    }

    #[test]
    fn mjpeg_frames_are_decoded_to_i420() {
        let rgb = [
            255, 0, 0, 0, 255, 0, // red, green
            0, 0, 255, 255, 255, 255, // blue, white
        ];
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode(&rgb, 2, 2, image::ExtendedColorType::Rgb8)
            .expect("test JPEG should encode");
        let i420 = Vp8Encoder::mjpg_to_i420(&jpeg, 2, 2).expect("test JPEG should decode");
        assert_eq!(i420.len(), 2 * 2 * 3 / 2);
    }
}
