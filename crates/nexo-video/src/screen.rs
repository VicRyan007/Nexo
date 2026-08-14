//! Screen capture.
//!
//! [`ScreenCaptureSource`] opens a monitor (by the id from
//! [`enumerate_monitors`]) and pulls frames one at a time. Like camera capture,
//! it is synchronous and non-buffering: every `read_frame` call returns at most
//! one frame, so callers that need pacing (e.g. a 20 ms video clock) own the
//! thread.
//!
//! The concrete source reader lives in [`crate::platform`]; on Windows it is
//! backed by Windows Graphics Capture with a `Direct3D11` staging copy, producing
//! [`PixelFormat::Bgra8`] frames in physical pixels.

use crate::capture::VideoFrame;
use crate::devices::VideoError;
use crate::platform::ScreenCapture;

/// A single monitor available on this machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorInfo {
    /// Stable identifier suitable for later re-opening the monitor.
    pub id: String,
    /// Human-readable display name shown in the UI.
    pub name: String,
    /// Whether this is the primary monitor.
    pub is_primary: bool,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

/// List the monitors available on this machine.
///
/// Returns an empty vector when no monitors are present (headless machine).
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>, VideoError> {
    crate::platform::enumerate_monitors()
}

/// A live handle to a screen capture stream of one monitor.
pub struct ScreenCaptureSource {
    source: ScreenCapture,
}

impl ScreenCaptureSource {
    /// Open `monitor_id` for capture.
    pub fn open_monitor(monitor_id: &str) -> Result<Self, VideoError> {
        Ok(Self {
            source: ScreenCapture::open_monitor(monitor_id)?,
        })
    }

    /// The resolution actually delivered for the monitor.
    #[must_use]
    pub fn resolution(&self) -> (u32, u32) {
        self.source.resolution()
    }

    /// Pull the next frame, blocking until one is ready.
    ///
    /// Returns `Ok(None)` once the stream ends. Repeated calls hand out one
    /// frame each and never buffer more than a single sample internally.
    pub fn read_frame(&mut self) -> Result<Option<VideoFrame>, VideoError> {
        self.source.read_frame()
    }
}
