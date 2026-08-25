//! Screen capture.
//!
//! [`ScreenCaptureSource`] opens a monitor (by the id from
//! [`enumerate_monitors`]) and exposes the newest frame without blocking the
//! caller. The platform reader runs on a dedicated worker.
//!
//! The concrete source reader lives in [`crate::platform`]; on Windows it is
//! backed by Windows Graphics Capture with a `Direct3D11` staging copy, producing
//! [`PixelFormat::Bgra8`] frames in physical pixels.

use crate::capture::VideoFrame;
use crate::devices::VideoError;
use crate::frame_worker::FrameWorker;
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
    resolution: (u32, u32),
    worker: FrameWorker,
}

impl ScreenCaptureSource {
    /// Open `monitor_id` for capture.
    pub fn open_monitor(monitor_id: &str) -> Result<Self, VideoError> {
        let monitor_id = monitor_id.to_owned();
        let (worker, resolution) = FrameWorker::spawn_open(
            move || ScreenCapture::open_monitor(&monitor_id),
            ScreenCapture::resolution,
            ScreenCapture::read_frame,
        )?;
        Ok(Self { resolution, worker })
    }

    /// The resolution actually delivered for the monitor.
    #[must_use]
    pub fn resolution(&self) -> (u32, u32) {
        self.resolution
    }

    /// Pull the newest frame without blocking the caller.
    ///
    /// Returns `Ok(None)` when the capture thread has not produced a newer
    /// frame yet. The capture thread owns the platform reader and can block
    /// independently while waiting for the next sample.
    pub fn read_frame(&mut self) -> Result<Option<VideoFrame>, VideoError> {
        self.worker.take_latest().transpose()
    }
}
