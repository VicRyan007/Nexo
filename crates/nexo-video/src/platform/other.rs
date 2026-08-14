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
