//! Camera device discovery.
//!
//! The concrete enumeration is delegated to [`crate::platform`], which hides
//! the native API used on each target (`Media Foundation` on Windows).

use std::fmt;

/// A single camera available on this machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoDeviceInfo {
    /// Stable identifier suitable for later re-opening the device.
    pub id: String,
    /// Human-readable name shown in the UI.
    pub name: String,
}

/// Error surfaced while enumerating cameras.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoError(String);

impl fmt::Display for VideoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for VideoError {}

impl VideoError {
    /// Build an error from a native backend message.
    #[must_use]
    pub fn platform(message: impl Into<String>) -> Self {
        Self(format!(
            "falha do sistema ao enumerar cameras: {}",
            message.into()
        ))
    }

    /// Build an error when a target has no camera backend yet.
    #[must_use]
    pub fn unsupported(target: impl AsRef<str>) -> Self {
        Self(format!(
            "enumeracao de cameras nao suportada em {}",
            target.as_ref()
        ))
    }

    /// Build an error from the native video encoder backend.
    #[must_use]
    pub fn encoder(message: impl Into<String>) -> Self {
        Self(format!("falha no encoder de video: {}", message.into()))
    }

    /// Build an error from the screen capture backend.
    #[must_use]
    pub fn screen_capture(message: impl Into<String>) -> Self {
        Self(format!(
            "falha do sistema ao capturar tela: {}",
            message.into()
        ))
    }
}

/// List the cameras available on this machine.
///
/// Returns an empty vector when no cameras are present. The returned IDs are
/// stable across calls on the same machine.
pub fn enumerate_cameras() -> Result<Vec<VideoDeviceInfo>, VideoError> {
    crate::platform::enumerate_cameras()
}
