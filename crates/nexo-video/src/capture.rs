//! Camera frame capture.
//!
//! [`VideoCaptureSource`] opens a camera (by the id from
//! [`crate::enumerate_cameras`]) and exposes the newest decoded frame without
//! blocking the caller. The platform reader runs on a dedicated worker.
//!
//! The concrete source reader lives in [`crate::platform`]; on Windows it is
//! backed by `IMFSourceReader` with an NV12-negotiated media type and native
//! fallback.

use std::time::Duration;

use crate::devices::VideoError;
use crate::frame_worker::FrameWorker;
use crate::platform::CaptureSource;

/// Pixel layout of the frame bytes delivered by a capture source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    Nv12,
    Yuy2,
    Mjpg,
    /// BGRA8, 4 bytes per pixel (Windows Graphics Capture output).
    Bgra8,
    Unknown,
}

/// A single captured frame with its raw bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Media timestamp in seconds since the stream started.
    pub timestamp: Duration,
    pub data: Box<[u8]>,
}

impl VideoFrame {
    /// Exact byte count of an NV12 frame, or `None` for invalid dimensions.
    #[must_use]
    pub fn nv12_size(width: u32, height: u32) -> Option<usize> {
        let y = usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?;
        let chroma = (y / 4).checked_mul(2)?;
        y.checked_add(chroma)
    }
}

/// A live handle to a camera capture stream.
pub struct VideoCaptureSource {
    resolution: (u32, u32),
    worker: FrameWorker,
}

impl VideoCaptureSource {
    /// Open a camera at its native size, defaulting the request to 640x480.
    pub fn open(device_id: &str) -> Result<Self, VideoError> {
        Self::open_with_resolution(device_id, 640, 480)
    }

    /// Open a camera requesting `width`x`height` NV12 output. The camera
    /// decides the actual resolution; read it back with [`Self::resolution`].
    pub fn open_with_resolution(
        device_id: &str,
        width: u32,
        height: u32,
    ) -> Result<Self, VideoError> {
        let device_id = device_id.to_owned();
        let (worker, resolution) = FrameWorker::spawn_open(
            move || CaptureSource::open(&device_id, width, height),
            CaptureSource::resolution,
            CaptureSource::read_frame,
        )?;
        Ok(Self { resolution, worker })
    }

    /// The resolution actually negotiated with the device.
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

#[cfg(test)]
mod tests {
    use super::VideoFrame;

    #[test]
    fn nv12_size_matches_luma_plus_chroma() {
        assert_eq!(VideoFrame::nv12_size(640, 480), Some(640 * 480 + 640 * 240));
        assert_eq!(VideoFrame::nv12_size(2, 2), Some(4 + 2));
    }

    #[test]
    fn nv12_size_rejects_overflowing_dimensions() {
        assert_eq!(VideoFrame::nv12_size(0, 0), Some(0));
        // u32::MAX x u32::MAX overflows usize on both 32- and 64-bit targets;
        // pairs that stay representable (e.g. u32::MAX x 2) are valid.
        assert!(VideoFrame::nv12_size(u32::MAX, u32::MAX).is_none());
    }
}
