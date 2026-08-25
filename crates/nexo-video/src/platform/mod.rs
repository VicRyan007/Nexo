//! Native platform backends.
//!
//! Every entry point here is a safe function; the underlying native calls live
//! in per-target modules. Unsupported targets return empty results so the crate
//! compiles and degrades gracefully everywhere.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod other;

#[cfg(target_os = "windows")]
use windows as platform_impl;

#[cfg(target_os = "linux")]
use linux as platform_impl;

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
use other as platform_impl;

/// Cross-platform capture source; each backend implements the same surface.
#[cfg(target_os = "windows")]
pub(crate) use windows::CaptureSource;

#[cfg(target_os = "linux")]
pub(crate) use linux::CaptureSource;

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub(crate) use other::CaptureSource;

#[cfg(target_os = "windows")]
pub(crate) use windows::HardwareH264Encoder;
/// Cross-platform screen capture source; each backend implements the same surface.
#[cfg(target_os = "windows")]
pub(crate) use windows::ScreenCapture;

#[cfg(target_os = "linux")]
pub(crate) use linux::HardwareH264Encoder;
#[cfg(target_os = "linux")]
pub(crate) use linux::ScreenCapture;

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub(crate) use other::HardwareH264Encoder;
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub(crate) use other::ScreenCapture;

/// Enumerate cameras through the target backend.
pub(super) fn enumerate_cameras()
-> Result<Vec<crate::devices::VideoDeviceInfo>, crate::devices::VideoError> {
    platform_impl::enumerate_cameras()
}

/// Enumerate monitors through the target backend.
pub(super) fn enumerate_monitors()
-> Result<Vec<crate::screen::MonitorInfo>, crate::devices::VideoError> {
    platform_impl::enumerate_monitors()
}

/// Human-readable GPU name, if the platform exposes one.
pub(super) fn gpu() -> Option<String> {
    platform_impl::gpu()
}

/// Hardware-accelerated video encoders detected on this machine.
pub(super) fn hardware_video_encoders() -> Vec<crate::probe::CodecCapability> {
    platform_impl::hardware_video_encoders()
}

/// Capture backends available on this machine.
pub(super) fn capture_backends() -> Vec<crate::probe::CaptureBackend> {
    platform_impl::capture_backends()
}
