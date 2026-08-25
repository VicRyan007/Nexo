//! Fallback backend for targets without native video support yet.
//!
//! Keeps the crate buildable everywhere and reports a minimal software set.

use crate::devices::{VideoDeviceInfo, VideoError};
use crate::probe::{CaptureBackend, CodecCapability};
use crate::screen::MonitorInfo;

pub(super) fn enumerate_cameras() -> Result<Vec<VideoDeviceInfo>, VideoError> {
    Err(VideoError::unsupported(std::env::consts::OS))
}

/// Placeholder capture source for unsupported targets.
pub(crate) struct CaptureSource;

impl CaptureSource {
    pub(crate) fn open(_device_id: &str, _width: u32, _height: u32) -> Result<Self, VideoError> {
        Err(VideoError::unsupported(std::env::consts::OS))
    }

    #[must_use]
    pub(crate) fn resolution(&self) -> (u32, u32) {
        (0, 0)
    }

    pub(crate) fn read_frame(&mut self) -> Result<Option<crate::capture::VideoFrame>, VideoError> {
        Ok(None)
    }
}

pub(super) fn enumerate_monitors() -> Result<Vec<MonitorInfo>, VideoError> {
    Err(VideoError::screen_capture(std::env::consts::OS))
}

/// Placeholder screen capture source for unsupported targets.
pub(crate) struct ScreenCapture;

/// No native hardware encoder is available on this target.
pub(crate) struct HardwareH264Encoder;

pub(crate) struct NativeEncodedH264Frame {
    pub(crate) timestamp: std::time::Duration,
    pub(crate) data: Box<[u8]>,
    pub(crate) is_keyframe: bool,
}

impl HardwareH264Encoder {
    pub(crate) fn new(_width: u32, _height: u32, _bitrate_bps: u32) -> Result<Self, VideoError> {
        Err(VideoError::unsupported(std::env::consts::OS))
    }

    pub(crate) fn width(&self) -> u32 {
        0
    }

    pub(crate) fn height(&self) -> u32 {
        0
    }

    pub(crate) fn encode(
        &mut self,
        _timestamp: std::time::Duration,
        _nv12: &[u8],
    ) -> Result<Option<NativeEncodedH264Frame>, VideoError> {
        Err(VideoError::unsupported(std::env::consts::OS))
    }
}

impl ScreenCapture {
    pub(crate) fn open_monitor(_monitor_id: &str) -> Result<Self, VideoError> {
        Err(VideoError::screen_capture(std::env::consts::OS))
    }

    #[must_use]
    pub(crate) fn resolution(&self) -> (u32, u32) {
        (0, 0)
    }

    pub(crate) fn read_frame(&mut self) -> Result<Option<crate::capture::VideoFrame>, VideoError> {
        Ok(None)
    }
}

pub(super) fn gpu() -> Option<String> {
    None
}

pub(super) fn hardware_video_encoders() -> Vec<CodecCapability> {
    Vec::new()
}

pub(super) fn capture_backends() -> Vec<CaptureBackend> {
    vec![CaptureBackend::Software]
}
