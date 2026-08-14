//! Runtime capability probing (GPU, capture backends, hardware encoders).
//!
//! The report is assembled from [`crate::platform`] probes and mirrors the
//! vocabulary used by `nexo-media::capability`, so consumers can map it onto
//! their own model without depending on native internals.

use std::fmt;

use crate::platform;

/// What kind of media a codec handles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
}

/// The API a codec or encoder is accelerated by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccelerationApi {
    AmdAmf,
    MediaFoundation,
    VaApi,
    Vulkan,
    Software,
}

/// How frames are captured on this platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureBackend {
    Wasapi,
    WindowsGraphicsCapture,
    PipeWire,
    Alsa,
    Software,
}

/// A codec available for encode/decode, optionally hardware-accelerated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecCapability {
    pub name: String,
    pub kind: MediaKind,
    pub encode: bool,
    pub decode: bool,
    pub acceleration: AccelerationApi,
}

/// Aggregated capabilities detected on this machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityReport {
    pub capture: Vec<CaptureBackend>,
    pub codecs: Vec<CodecCapability>,
    pub gpu_name: Option<String>,
    pub runtime_ready: bool,
}

impl CapabilityReport {
    /// Best video encoder to prefer, hardware first.
    #[must_use]
    pub fn preferred_video_encoder(&self) -> Option<&CodecCapability> {
        self.codecs
            .iter()
            .filter(|codec| codec.kind == MediaKind::Video && codec.encode)
            .min_by_key(|codec| acceleration_rank(codec.acceleration))
    }

    /// Whether the local audio path (capture or playback) is available.
    #[must_use]
    pub fn has_audio_path(&self) -> bool {
        self.capture
            .iter()
            .any(|backend| !matches!(backend, CaptureBackend::Software))
    }
}

impl fmt::Display for AccelerationApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::AmdAmf => "AMF",
            Self::MediaFoundation => "Media Foundation",
            Self::VaApi => "VA-API",
            Self::Vulkan => "Vulkan",
            Self::Software => "Software",
        };
        formatter.write_str(name)
    }
}

/// Lower rank means more preferable (hardware before software).
#[must_use]
pub fn acceleration_rank(api: AccelerationApi) -> u8 {
    match api {
        AccelerationApi::AmdAmf | AccelerationApi::VaApi => 0,
        AccelerationApi::MediaFoundation | AccelerationApi::Vulkan => 1,
        AccelerationApi::Software => 2,
    }
}

/// Stateless probe that inspects the local machine.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CapabilityProbe;

impl CapabilityProbe {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Probe the platform and assemble a full capability report.
    #[must_use]
    pub fn probe(&self) -> CapabilityReport {
        let mut codecs = Vec::new();
        codecs.push(CodecCapability {
            name: "Opus".into(),
            kind: MediaKind::Audio,
            encode: true,
            decode: true,
            acceleration: AccelerationApi::Software,
        });
        codecs.extend(platform::hardware_video_encoders());
        codecs.push(CodecCapability {
            name: "VP8".into(),
            kind: MediaKind::Video,
            encode: true,
            decode: true,
            acceleration: AccelerationApi::Software,
        });

        CapabilityReport {
            capture: platform::capture_backends(),
            codecs,
            gpu_name: platform::gpu(),
            runtime_ready: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn software_video(name: &str) -> CodecCapability {
        CodecCapability {
            name: name.into(),
            kind: MediaKind::Video,
            encode: true,
            decode: true,
            acceleration: AccelerationApi::Software,
        }
    }

    #[test]
    fn ranks_hardware_before_software() {
        let low = acceleration_rank(AccelerationApi::AmdAmf);
        let mid = acceleration_rank(AccelerationApi::Vulkan);
        let high = acceleration_rank(AccelerationApi::Software);
        assert!(low < mid && mid < high);
    }

    #[test]
    fn prefers_hardware_encoder() {
        let report = CapabilityReport {
            capture: Vec::new(),
            codecs: vec![
                software_video("VP8"),
                CodecCapability {
                    name: "H264".into(),
                    kind: MediaKind::Video,
                    encode: true,
                    decode: true,
                    acceleration: AccelerationApi::MediaFoundation,
                },
            ],
            gpu_name: Some("AMD Radeon".into()),
            runtime_ready: true,
        };
        let best = report.preferred_video_encoder().expect("encoder presente");
        assert_eq!(best.name, "H264");
    }

    #[test]
    fn probe_keeps_software_fallback() {
        let report = CapabilityProbe::new().probe();
        assert!(report.runtime_ready);
        let opus = report
            .codecs
            .iter()
            .find(|codec| codec.kind == MediaKind::Audio && codec.name == "Opus")
            .expect("Opus presente");
        assert!(opus.encode && opus.decode);
        let vp8 = report
            .codecs
            .iter()
            .find(|codec| codec.kind == MediaKind::Video && codec.name == "VP8")
            .expect("VP8 presente");
        assert!(vp8.encode);
    }
}
