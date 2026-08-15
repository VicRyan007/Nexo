#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#define VPX_CODEC_OK 0
#define VPX_CODEC_ERROR 1
#define VPX_CODEC_MEM_ERROR 2
#define VPX_CODEC_ABI_MISMATCH 3
#define VPX_CODEC_INCAPABLE 4
#define VPX_CODEC_UNSUP_BITSTREAM 5
#define VPX_CODEC_UNSUP_FEATURE 6
#define VPX_CODEC_CORRUPT_FRAME 7
#define VPX_CODEC_INVALID_PARAM 8
#define VPX_CODEC_LIST_END 9

#define VPX_CODEC_CX_FRAME_PKT 0
#define VPX_FRAME_IS_KEY 1
#define VPX_IMG_FMT_I420 258

typedef struct vpx_image {
    unsigned int fmt;
    unsigned int cs;
    unsigned int range;
    unsigned int w;
    unsigned int h;
    unsigned int bit_depth;
    unsigned int d_w;
    unsigned int d_h;
    unsigned int r_w;
    unsigned int r_h;
    unsigned int x_chroma_shift;
    unsigned int y_chroma_shift;
    unsigned char *planes[4];
    int stride[4];
    int bps;
    void *user_priv;
    unsigned char *img_data;
    int img_data_owner;
    int self_allocd;
    void *fb_priv;
} vpx_image_t;

typedef struct vpx_codec_enc_cfg {
    unsigned int g_usage;
    unsigned int g_threads;
    unsigned int g_profile;
    unsigned int g_w;
    unsigned int g_h;
    unsigned int g_bit_depth;
    unsigned int g_input_bit_depth;
    struct {
        int num;
        int den;
    } g_timebase;
    unsigned int g_error_resilient;
    unsigned int g_pass;
    unsigned int g_lag_in_frames;
    unsigned int rc_dropframe_thresh;
    unsigned int rc_resize_allowed;
    unsigned int rc_scaled_width;
    unsigned int rc_scaled_height;
    unsigned int rc_resize_up_thresh;
    unsigned int rc_resize_down_thresh;
    unsigned int rc_end_usage;
    unsigned char pad[256];
} vpx_codec_enc_cfg_t;

typedef struct vpx_codec_ctx {
    const char *name;
    const void *iface;
    int err;
    const char *err_detail;
    long init_flags;
    union {
        const void *dec;
        const void *enc;
        const void *raw;
    } config;
    void *priv_;
} vpx_codec_ctx_t;

typedef struct vpx_codec_cx_pkt {
    int kind;
    union {
        struct {
            void *buf;
            size_t sz;
            int64_t pts;
            unsigned long duration;
            unsigned int flags;
            int partition_id;
            unsigned int width[5];
            unsigned int height[5];
            uint8_t spatial_layer_encoded[5];
        } frame;
        unsigned char pad[128];
    } data;
} vpx_codec_cx_pkt_t;

typedef struct EncoderState {
    unsigned int width;
    unsigned int height;
    unsigned char *encoded_buf;
    size_t encoded_buf_cap;
    vpx_codec_cx_pkt_t pkt;
    int has_pkt;
} EncoderState;

typedef struct DecoderState {
    unsigned int width;
    unsigned int height;
    unsigned char *y_plane;
    unsigned char *u_plane;
    unsigned char *v_plane;
    size_t plane_cap;
    vpx_image_t img;
    int has_frame;
} DecoderState;

static const char *dummy_cx_algo = "vp8_cx";
static const char *dummy_dx_algo = "vp8_dx";

const void *vpx_codec_vp8_cx(void) {
    return (const void *)&dummy_cx_algo;
}

const void *vpx_codec_vp8_dx(void) {
    return (const void *)&dummy_dx_algo;
}

int vpx_codec_enc_config_default(const void *iface, vpx_codec_enc_cfg_t *cfg, unsigned int usage) {
    (void)iface;
    (void)usage;
    if (!cfg) return VPX_CODEC_INVALID_PARAM;
    memset(cfg, 0, sizeof(vpx_codec_enc_cfg_t));
    cfg->g_w = 640;
    cfg->g_h = 480;
    cfg->g_timebase.num = 1;
    cfg->g_timebase.den = 90000;
    return VPX_CODEC_OK;
}

int vpx_codec_enc_init_ver(vpx_codec_ctx_t *ctx, const void *iface, const vpx_codec_enc_cfg_t *cfg, long flags, int ver) {
    (void)flags;
    (void)ver;
    if (!ctx || !cfg) return VPX_CODEC_INVALID_PARAM;
    memset(ctx, 0, sizeof(vpx_codec_ctx_t));
    ctx->name = "nexo_vp8_encoder";
    ctx->iface = iface;
    
    EncoderState *st = (EncoderState *)calloc(1, sizeof(EncoderState));
    if (!st) {
        ctx->err = VPX_CODEC_MEM_ERROR;
        return VPX_CODEC_MEM_ERROR;
    }
    st->width = cfg->g_w;
    st->height = cfg->g_h;
    ctx->priv_ = st;
    return VPX_CODEC_OK;
}

int vpx_codec_dec_init_ver(vpx_codec_ctx_t *ctx, const void *iface, const void *cfg, long flags, int ver) {
    (void)cfg;
    (void)flags;
    (void)ver;
    if (!ctx) return VPX_CODEC_INVALID_PARAM;
    memset(ctx, 0, sizeof(vpx_codec_ctx_t));
    ctx->name = "nexo_vp8_decoder";
    ctx->iface = iface;
    
    DecoderState *st = (DecoderState *)calloc(1, sizeof(DecoderState));
    if (!st) {
        ctx->err = VPX_CODEC_MEM_ERROR;
        return VPX_CODEC_MEM_ERROR;
    }
    ctx->priv_ = st;
    return VPX_CODEC_OK;
}

vpx_image_t *vpx_img_wrap(vpx_image_t *img, unsigned int fmt, unsigned int d_w, unsigned int d_h, unsigned int stride_align, unsigned char *img_data) {
    (void)stride_align;
    if (!img || !img_data) return NULL;
    memset(img, 0, sizeof(vpx_image_t));
    img->fmt = fmt;
    img->w = d_w;
    img->d_w = d_w;
    img->h = d_h;
    img->d_h = d_h;
    img->stride[0] = (int)d_w;
    img->stride[1] = (int)(d_w / 2);
    img->stride[2] = (int)(d_w / 2);
    img->planes[0] = img_data;
    img->planes[1] = img_data + (d_w * d_h);
    img->planes[2] = img_data + (d_w * d_h) + (d_w * d_h / 4);
    img->img_data = img_data;
    return img;
}

int vpx_codec_encode(vpx_codec_ctx_t *ctx, const vpx_image_t *img, int64_t pts, unsigned long duration, long flags, long deadline) {
    (void)deadline;
    if (!ctx || !ctx->priv_) return VPX_CODEC_INVALID_PARAM;
    EncoderState *st = (EncoderState *)ctx->priv_;
    if (!img) {
        st->has_pkt = 0;
        return VPX_CODEC_OK;
    }
    
    unsigned int width = img->d_w;
    unsigned int height = img->d_h;
    size_t y_size = (size_t)width * height;
    size_t uv_size = y_size / 4;
    size_t raw_payload_size = y_size + uv_size * 2;
    size_t total_size = 10 + raw_payload_size;
    
    if (st->encoded_buf_cap < total_size) {
        unsigned char *new_buf = (unsigned char *)realloc(st->encoded_buf, total_size);
        if (!new_buf) {
            ctx->err = VPX_CODEC_MEM_ERROR;
            return VPX_CODEC_MEM_ERROR;
        }
        st->encoded_buf = new_buf;
        st->encoded_buf_cap = total_size;
    }
    
    /* VP8 Keyframe uncompressed header (RFC 6386 Section 9.1) */
    /* 3 bytes frame tag */
    st->encoded_buf[0] = 0x10; /* keyframe (bit 0 = 0), show_frame (bit 4 = 1) */
    st->encoded_buf[1] = 0x00;
    st->encoded_buf[2] = 0x00;
    /* 3 bytes start code */
    st->encoded_buf[3] = 0x9D;
    st->encoded_buf[4] = 0x01;
    st->encoded_buf[5] = 0x2A;
    /* 2 bytes width (14-bit width + 2-bit scale) */
    st->encoded_buf[6] = (unsigned char)(width & 0xFF);
    st->encoded_buf[7] = (unsigned char)((width >> 8) & 0x3F);
    /* 2 bytes height (14-bit height + 2-bit scale) */
    st->encoded_buf[8] = (unsigned char)(height & 0xFF);
    st->encoded_buf[9] = (unsigned char)((height >> 8) & 0x3F);
    
    /* Copy planes sequentially: Y, U, V */
    unsigned char *dst = st->encoded_buf + 10;
    for (unsigned int r = 0; r < height; r++) {
        memcpy(dst + r * width, img->planes[0] + r * img->stride[0], width);
    }
    dst += y_size;
    for (unsigned int r = 0; r < height / 2; r++) {
        memcpy(dst + r * (width / 2), img->planes[1] + r * img->stride[1], width / 2);
    }
    dst += uv_size;
    for (unsigned int r = 0; r < height / 2; r++) {
        memcpy(dst + r * (width / 2), img->planes[2] + r * img->stride[2], width / 2);
    }
    
    memset(&st->pkt, 0, sizeof(st->pkt));
    st->pkt.kind = VPX_CODEC_CX_FRAME_PKT;
    st->pkt.data.frame.buf = st->encoded_buf;
    st->pkt.data.frame.sz = total_size;
    st->pkt.data.frame.pts = pts;
    st->pkt.data.frame.duration = duration;
    st->pkt.data.frame.flags = VPX_FRAME_IS_KEY;
    (void)flags;
    st->has_pkt = 1;
    return VPX_CODEC_OK;
}

const vpx_codec_cx_pkt_t *vpx_codec_get_cx_data(vpx_codec_ctx_t *ctx, const void **iter) {
    if (!ctx || !ctx->priv_ || !iter) return NULL;
    EncoderState *st = (EncoderState *)ctx->priv_;
    if (st->has_pkt && *iter == NULL) {
        *iter = (const void *)1;
        st->has_pkt = 0;
        return &st->pkt;
    }
    return NULL;
}

int vpx_codec_decode(vpx_codec_ctx_t *ctx, const uint8_t *data, unsigned int data_sz, void *user_priv, long deadline) {
    (void)user_priv;
    (void)deadline;
    if (!ctx || !ctx->priv_) return VPX_CODEC_INVALID_PARAM;
    DecoderState *st = (DecoderState *)ctx->priv_;
    if (!data || data_sz < 10) {
        ctx->err = VPX_CODEC_CORRUPT_FRAME;
        return VPX_CODEC_CORRUPT_FRAME;
    }
    
    /* Verify VP8 start code */
    if (data[3] != 0x9D || data[4] != 0x01 || data[5] != 0x2A) {
        ctx->err = VPX_CODEC_UNSUP_BITSTREAM;
        return VPX_CODEC_UNSUP_BITSTREAM;
    }
    
    unsigned int width = (unsigned int)data[6] | (((unsigned int)(data[7] & 0x3F)) << 8);
    unsigned int height = (unsigned int)data[8] | (((unsigned int)(data[9] & 0x3F)) << 8);
    
    if (width == 0 || height == 0) {
        ctx->err = VPX_CODEC_CORRUPT_FRAME;
        return VPX_CODEC_CORRUPT_FRAME;
    }
    
    size_t y_size = (size_t)width * height;
    size_t uv_size = y_size / 4;
    size_t expected_total = 10 + y_size + uv_size * 2;
    if (data_sz < expected_total) {
        ctx->err = VPX_CODEC_CORRUPT_FRAME;
        return VPX_CODEC_CORRUPT_FRAME;
    }
    
    if (st->plane_cap < y_size) {
        unsigned char *new_y = (unsigned char *)realloc(st->y_plane, y_size);
        unsigned char *new_u = (unsigned char *)realloc(st->u_plane, uv_size);
        unsigned char *new_v = (unsigned char *)realloc(st->v_plane, uv_size);
        if (!new_y || !new_u || !new_v) {
            ctx->err = VPX_CODEC_MEM_ERROR;
            return VPX_CODEC_MEM_ERROR;
        }
        st->y_plane = new_y;
        st->u_plane = new_u;
        st->v_plane = new_v;
        st->plane_cap = y_size;
    }
    
    st->width = width;
    st->height = height;
    
    const unsigned char *src = data + 10;
    memcpy(st->y_plane, src, y_size);
    src += y_size;
    memcpy(st->u_plane, src, uv_size);
    src += uv_size;
    memcpy(st->v_plane, src, uv_size);
    
    memset(&st->img, 0, sizeof(vpx_image_t));
    st->img.fmt = VPX_IMG_FMT_I420;
    st->img.w = width;
    st->img.d_w = width;
    st->img.h = height;
    st->img.d_h = height;
    st->img.stride[0] = (int)width;
    st->img.stride[1] = (int)(width / 2);
    st->img.stride[2] = (int)(width / 2);
    st->img.planes[0] = st->y_plane;
    st->img.planes[1] = st->u_plane;
    st->img.planes[2] = st->v_plane;
    st->has_frame = 1;
    return VPX_CODEC_OK;
}

vpx_image_t *vpx_codec_get_frame(vpx_codec_ctx_t *ctx, const void **iter) {
    if (!ctx || !ctx->priv_ || !iter) return NULL;
    DecoderState *st = (DecoderState *)ctx->priv_;
    if (st->has_frame && *iter == NULL) {
        *iter = (const void *)1;
        st->has_frame = 0;
        return &st->img;
    }
    return NULL;
}

int vpx_codec_destroy(vpx_codec_ctx_t *ctx) {
    if (!ctx) return VPX_CODEC_OK;
    if (ctx->priv_) {
        if (ctx->iface == (const void *)&dummy_cx_algo) {
            EncoderState *st = (EncoderState *)ctx->priv_;
            free(st->encoded_buf);
            free(st);
        } else {
            DecoderState *st = (DecoderState *)ctx->priv_;
            free(st->y_plane);
            free(st->u_plane);
            free(st->v_plane);
            free(st);
        }
        ctx->priv_ = NULL;
    }
    return VPX_CODEC_OK;
}

const char *vpx_codec_error(vpx_codec_ctx_t *ctx) {
    if (!ctx || ctx->err == VPX_CODEC_OK) return "Success";
    switch (ctx->err) {
        case VPX_CODEC_MEM_ERROR: return "Memory allocation error";
        case VPX_CODEC_CORRUPT_FRAME: return "Corrupt frame";
        case VPX_CODEC_UNSUP_BITSTREAM: return "Unsupported bitstream";
        default: return "Codec error";
    }
}

const char *vpx_codec_error_detail(vpx_codec_ctx_t *ctx) {
    return ctx ? ctx->err_detail : NULL;
}
