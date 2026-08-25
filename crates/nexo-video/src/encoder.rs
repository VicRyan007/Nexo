//! Optional hardware video encoders.
//!
//! The public wrapper is deliberately small: callers provide tightly packed
//! NV12 frames and receive complete Annex-B H.264 access units. Native setup
//! failures are ordinary `VideoError`s so media sessions can fall back to VP8
//! without taking down the UI or the call.

use std::time::Duration;

use crate::{devices::VideoError, platform};

/// One H.264 access unit produced by a native encoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedH264Frame {
    pub timestamp: Duration,
    pub data: Box<[u8]>,
    pub is_keyframe: bool,
}

/// Hardware-backed H.264 encoder selected by the platform backend.
pub struct HardwareH264Encoder {
    inner: platform::HardwareH264Encoder,
}

impl HardwareH264Encoder {
    /// Create an encoder for an even, non-zero NV12 frame size.
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Result<Self, VideoError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(VideoError::platform(format!(
                "dimensoes invalidas para H.264: {width}x{height}"
            )));
        }
        Ok(Self {
            inner: platform::HardwareH264Encoder::new(width, height, bitrate_bps)?,
        })
    }

    /// Encode one tightly packed NV12 frame.
    pub fn encode(
        &mut self,
        timestamp: Duration,
        nv12: &[u8],
    ) -> Result<Option<EncodedH264Frame>, VideoError> {
        let expected = usize::try_from(self.inner.width())
            .ok()
            .and_then(|width| {
                usize::try_from(self.inner.height())
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|y| y.checked_add(y / 2))
            .ok_or_else(|| VideoError::platform("tamanho NV12 excede os limites"))?;
        if nv12.len() != expected {
            return Err(VideoError::platform(format!(
                "frame NV12 tem {} bytes, esperado {expected}",
                nv12.len()
            )));
        }
        self.inner.encode(timestamp, nv12).map(|encoded| {
            encoded.map(|encoded| EncodedH264Frame {
                timestamp: encoded.timestamp,
                data: encoded.data,
                is_keyframe: encoded.is_keyframe,
            })
        })
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.inner.width()
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.inner.height()
    }
}
