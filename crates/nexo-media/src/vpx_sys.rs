#![allow(
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::missing_safety_doc
)]

use std::ffi::{c_char, c_int, c_long, c_uchar, c_uint, c_ulong, c_void};

pub const VPX_DL_REALTIME: c_long = 1;
pub const VPX_EFLAG_FORCE_KF: c_long = 1;
pub const VPX_ERROR_RESILIENT_DEFAULT: c_uint = 1;
pub const VPX_FRAME_IS_KEY: c_uint = 1;
pub const VPX_ENCODER_ABI_VERSION: c_int = 30;
pub const VPX_DECODER_ABI_VERSION: c_int = 12;

#[repr(u32)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum vpx_img_fmt {
    VPX_IMG_FMT_NONE = 0,
    VPX_IMG_FMT_I420 = 258,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct vpx_image_t {
    pub fmt: vpx_img_fmt,
    pub cs: u32,
    pub range: u32,
    pub w: c_uint,
    pub h: c_uint,
    pub bit_depth: c_uint,
    pub d_w: c_uint,
    pub d_h: c_uint,
    pub r_w: c_uint,
    pub r_h: c_uint,
    pub x_chroma_shift: c_uint,
    pub y_chroma_shift: c_uint,
    pub planes: [*mut c_uchar; 4],
    pub stride: [c_int; 4],
    pub bps: c_int,
    pub user_priv: *mut c_void,
    pub img_data: *mut c_uchar,
    pub img_data_owner: c_int,
    pub self_allocd: c_int,
    pub fb_priv: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct vpx_rational {
    pub num: c_int,
    pub den: c_int,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct vpx_codec_enc_cfg_t {
    pub g_usage: c_uint,
    pub g_threads: c_uint,
    pub g_profile: c_uint,
    pub g_w: c_uint,
    pub g_h: c_uint,
    pub g_bit_depth: u32,
    pub g_input_bit_depth: c_uint,
    pub g_timebase: vpx_rational,
    pub g_error_resilient: c_uint,
    pub g_pass: u32,
    pub g_lag_in_frames: c_uint,
    pub rc_dropframe_thresh: c_uint,
    pub rc_resize_allowed: c_uint,
    pub rc_scaled_width: c_uint,
    pub rc_scaled_height: c_uint,
    pub rc_resize_up_thresh: c_uint,
    pub rc_resize_down_thresh: c_uint,
    pub rc_end_usage: u32,
    pub _pad: [u8; 256],
    pub rc_target_bitrate: c_uint,
}

#[repr(u32)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum vpx_codec_err_t {
    VPX_CODEC_OK = 0,
    VPX_CODEC_ERROR = 1,
    VPX_CODEC_MEM_ERROR = 2,
    VPX_CODEC_ABI_MISMATCH = 3,
    VPX_CODEC_INCAPABLE = 4,
    VPX_CODEC_UNSUP_BITSTREAM = 5,
    VPX_CODEC_UNSUP_FEATURE = 6,
    VPX_CODEC_CORRUPT_FRAME = 7,
    VPX_CODEC_INVALID_PARAM = 8,
    VPX_CODEC_LIST_END = 9,
}

#[repr(u32)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum vpx_codec_cx_pkt_kind {
    VPX_CODEC_CX_FRAME_PKT = 0,
    VPX_CODEC_STATS_PKT = 1,
    VPX_CODEC_FPMB_STATS_PKT = 2,
    VPX_CODEC_PSNR_PKT = 3,
    VPX_CODEC_CUSTOM_PKT = 256,
}

pub type vpx_codec_pts_t = i64;
pub type vpx_codec_iter_t = *const c_void;
pub type vpx_enc_frame_flags_t = c_long;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vpx_codec_ctx_t {
    pub name: *const c_char,
    pub iface: *const c_void,
    pub err: vpx_codec_err_t,
    pub err_detail: *const c_char,
    pub init_flags: c_long,
    pub config: *const c_void,
    pub priv_: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct vpx_codec_cx_pkt_frame {
    pub buf: *mut c_void,
    pub sz: usize,
    pub pts: vpx_codec_pts_t,
    pub duration: c_ulong,
    pub flags: c_uint,
    pub partition_id: c_int,
    pub width: [c_uint; 5],
    pub height: [c_uint; 5],
    pub spatial_layer_encoded: [u8; 5],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union vpx_codec_cx_pkt_data {
    pub frame: vpx_codec_cx_pkt_frame,
    pub pad: [u8; 128],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct vpx_codec_cx_pkt {
    pub kind: vpx_codec_cx_pkt_kind,
    pub data: vpx_codec_cx_pkt_data,
}

pub type vpx_codec_cx_pkt_t = vpx_codec_cx_pkt;

unsafe extern "C" {
    pub fn vpx_codec_vp8_cx() -> *const c_void;
    pub fn vpx_codec_vp8_dx() -> *const c_void;
    pub fn vpx_codec_enc_config_default(
        iface: *const c_void,
        cfg: *mut vpx_codec_enc_cfg_t,
        usage: c_uint,
    ) -> vpx_codec_err_t;
    pub fn vpx_codec_enc_init_ver(
        ctx: *mut vpx_codec_ctx_t,
        iface: *const c_void,
        cfg: *const vpx_codec_enc_cfg_t,
        flags: c_long,
        ver: c_int,
    ) -> vpx_codec_err_t;
    pub fn vpx_codec_dec_init_ver(
        ctx: *mut vpx_codec_ctx_t,
        iface: *const c_void,
        cfg: *const c_void,
        flags: c_long,
        ver: c_int,
    ) -> vpx_codec_err_t;
    pub fn vpx_img_wrap(
        img: *mut vpx_image_t,
        fmt: vpx_img_fmt,
        d_w: c_uint,
        d_h: c_uint,
        stride_align: c_uint,
        img_data: *mut c_uchar,
    ) -> *mut vpx_image_t;
    pub fn vpx_codec_encode(
        ctx: *mut vpx_codec_ctx_t,
        img: *const vpx_image_t,
        pts: vpx_codec_pts_t,
        duration: c_ulong,
        flags: vpx_enc_frame_flags_t,
        deadline: c_long,
    ) -> vpx_codec_err_t;
    pub fn vpx_codec_get_cx_data(
        ctx: *mut vpx_codec_ctx_t,
        iter: *mut vpx_codec_iter_t,
    ) -> *const vpx_codec_cx_pkt_t;
    pub fn vpx_codec_decode(
        ctx: *mut vpx_codec_ctx_t,
        data: *const u8,
        data_sz: c_uint,
        user_priv: *mut c_void,
        deadline: c_long,
    ) -> vpx_codec_err_t;
    pub fn vpx_codec_get_frame(
        ctx: *mut vpx_codec_ctx_t,
        iter: *mut vpx_codec_iter_t,
    ) -> *mut vpx_image_t;
    pub fn vpx_codec_destroy(ctx: *mut vpx_codec_ctx_t) -> vpx_codec_err_t;
    pub fn vpx_codec_error(ctx: *mut vpx_codec_ctx_t) -> *const c_char;
    pub fn vpx_codec_error_detail(ctx: *mut vpx_codec_ctx_t) -> *const c_char;
}
