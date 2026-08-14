use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AccelerationApi {
    AmdAmf,
    MediaFoundation,
    VaApi,
    Vulkan,
    Software,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CaptureBackend {
    Wasapi,
    WindowsGraphicsCapture,
    PipeWire,
    Alsa,
    Software,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CodecCapability {
    pub name: String,
    pub kind: MediaKind,
    pub encode: bool,
    pub decode: bool,
    pub acceleration: AccelerationApi,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityReport {
    pub capture: Vec<CaptureBackend>,
    pub codecs: Vec<CodecCapability>,
    pub gpu_name: Option<String>,
    pub runtime_ready: bool,
}

impl CapabilityReport {
    #[must_use]
    pub fn preferred_video_encoder(&self) -> Option<&CodecCapability> {
        self.codecs
            .iter()
            .filter(|codec| codec.kind == MediaKind::Video && codec.encode)
            .min_by_key(|codec| acceleration_rank(codec.acceleration))
    }

    #[must_use]
    pub fn has_audio_path(&self) -> bool {
        self.codecs
            .iter()
            .any(|codec| codec.kind == MediaKind::Audio && codec.encode && codec.decode)
    }
}

pub trait RuntimeProbe {
    fn probe(&self) -> CapabilityReport;
}

const fn acceleration_rank(api: AccelerationApi) -> u8 {
    match api {
        AccelerationApi::AmdAmf | AccelerationApi::VaApi => 0,
        AccelerationApi::MediaFoundation | AccelerationApi::Vulkan => 1,
        AccelerationApi::Software => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_encoder_wins_over_software_fallback() {
        let report = CapabilityReport {
            capture: vec![CaptureBackend::Wasapi],
            codecs: vec![
                CodecCapability {
                    name: "H264".into(),
                    kind: MediaKind::Video,
                    encode: true,
                    decode: true,
                    acceleration: AccelerationApi::Software,
                },
                CodecCapability {
                    name: "H264".into(),
                    kind: MediaKind::Video,
                    encode: true,
                    decode: true,
                    acceleration: AccelerationApi::AmdAmf,
                },
            ],
            gpu_name: Some("AMD Radeon".into()),
            runtime_ready: true,
        };
        assert_eq!(
            report
                .preferred_video_encoder()
                .expect("an encoder should be selected")
                .acceleration,
            AccelerationApi::AmdAmf
        );
    }
}
