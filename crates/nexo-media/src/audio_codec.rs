use rusty_opus::{Application, OpusDecoder, OpusEncoder, SignalType};
use thiserror::Error;

use crate::{AudioFrame, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE};

const VOICE_BITRATE_BPS: i32 = 32_000;
const MAX_OPUS_PACKET_BYTES: usize = 1_275;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedAudioFrame {
    pub payload: Vec<u8>,
    pub sample_count: usize,
    pub sample_rate: u32,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum AudioCodecError {
    #[error("invalid PCM frame: {0}")]
    InvalidFrame(String),
    #[error("Opus encoder initialization failed: {0}")]
    EncoderInitialization(String),
    #[error("Opus decoder initialization failed: {0}")]
    DecoderInitialization(String),
    #[error("Opus encoding failed: {0}")]
    Encode(String),
    #[error("Opus decoding failed: {0}")]
    Decode(String),
}

pub struct VoiceEncoder {
    inner: OpusEncoder,
    output: Vec<u8>,
}

impl VoiceEncoder {
    pub fn new() -> Result<Self, AudioCodecError> {
        let mut inner = OpusEncoder::new(OPUS_SAMPLE_RATE.cast_signed(), 1, Application::Voip)
            .map_err(|error| AudioCodecError::EncoderInitialization(error.to_owned()))?;
        inner.bitrate_bps = VOICE_BITRATE_BPS;
        inner.complexity = 8;
        inner.use_cbr = false;
        inner.use_inband_fec = true;
        inner.use_dtx = true;
        inner.packet_loss_perc = 10;
        inner.signal_type = Some(SignalType::Voice);
        Ok(Self {
            inner,
            output: vec![0; MAX_OPUS_PACKET_BYTES],
        })
    }

    pub fn encode(&mut self, frame: &AudioFrame) -> Result<EncodedAudioFrame, AudioCodecError> {
        validate_pcm_frame(frame)?;
        let encoded_len = self
            .inner
            .encode(&frame.samples, OPUS_FRAME_SAMPLES, &mut self.output)
            .map_err(|error| AudioCodecError::Encode(error.to_owned()))?;
        Ok(EncodedAudioFrame {
            payload: self.output[..encoded_len].to_vec(),
            sample_count: OPUS_FRAME_SAMPLES,
            sample_rate: OPUS_SAMPLE_RATE,
        })
    }
}

pub struct VoiceDecoder {
    inner: OpusDecoder,
    output: Vec<f32>,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, AudioCodecError> {
        let inner = OpusDecoder::new(OPUS_SAMPLE_RATE.cast_signed(), 1)
            .map_err(|error| AudioCodecError::DecoderInitialization(error.to_owned()))?;
        Ok(Self {
            inner,
            output: vec![0.0; OPUS_FRAME_SAMPLES],
        })
    }

    pub fn decode(&mut self, frame: &EncodedAudioFrame) -> Result<AudioFrame, AudioCodecError> {
        validate_encoded_frame(frame)?;
        let decoded_samples = self
            .inner
            .decode(&frame.payload, OPUS_FRAME_SAMPLES, &mut self.output)
            .map_err(|error| AudioCodecError::Decode(error.to_owned()))?;
        Ok(AudioFrame {
            samples: self.output[..decoded_samples].to_vec(),
            sample_rate: OPUS_SAMPLE_RATE,
        })
    }

    pub fn decode_loss(&mut self) -> Result<AudioFrame, AudioCodecError> {
        let decoded_samples = self
            .inner
            .decode(&[], OPUS_FRAME_SAMPLES, &mut self.output)
            .map_err(|error| AudioCodecError::Decode(error.to_owned()))?;
        Ok(self.output_frame(decoded_samples))
    }

    pub fn decode_fec(
        &mut self,
        recovery_packet: &EncodedAudioFrame,
    ) -> Result<AudioFrame, AudioCodecError> {
        validate_encoded_frame(recovery_packet)?;
        let decoded_samples = self
            .inner
            .decode_fec(
                &recovery_packet.payload,
                OPUS_FRAME_SAMPLES,
                &mut self.output,
            )
            .map_err(|error| AudioCodecError::Decode(error.to_owned()))?;
        Ok(self.output_frame(decoded_samples))
    }

    fn output_frame(&self, decoded_samples: usize) -> AudioFrame {
        AudioFrame {
            samples: self.output[..decoded_samples].to_vec(),
            sample_rate: OPUS_SAMPLE_RATE,
        }
    }
}

fn validate_encoded_frame(frame: &EncodedAudioFrame) -> Result<(), AudioCodecError> {
    if frame.payload.is_empty() || frame.payload.len() > MAX_OPUS_PACKET_BYTES {
        return Err(AudioCodecError::InvalidFrame(
            "encoded payload size is outside the Opus packet bounds".to_owned(),
        ));
    }
    if frame.sample_count != OPUS_FRAME_SAMPLES || frame.sample_rate != OPUS_SAMPLE_RATE {
        return Err(AudioCodecError::InvalidFrame(
            "encoded frame is not 20 ms mono at 48 kHz".to_owned(),
        ));
    }
    Ok(())
}

fn validate_pcm_frame(frame: &AudioFrame) -> Result<(), AudioCodecError> {
    if frame.sample_rate != OPUS_SAMPLE_RATE || frame.samples.len() != OPUS_FRAME_SAMPLES {
        return Err(AudioCodecError::InvalidFrame(
            "PCM input must contain exactly 20 ms of mono 48 kHz audio".to_owned(),
        ));
    }
    if frame.samples.iter().any(|sample| !sample.is_finite()) {
        return Err(AudioCodecError::InvalidFrame(
            "PCM input contains a non-finite sample".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::*;

    #[test]
    fn voice_frame_round_trips_through_pure_rust_opus() {
        let samples = (0..OPUS_FRAME_SAMPLES)
            .map(|index| {
                #[allow(clippy::cast_precision_loss)]
                let time = index as f32 / OPUS_SAMPLE_RATE as f32;
                (TAU * 440.0 * time).sin() * 0.25
            })
            .collect();
        let input = AudioFrame {
            samples,
            sample_rate: OPUS_SAMPLE_RATE,
        };
        let mut encoder = VoiceEncoder::new().expect("voice encoder should initialize");
        let packet = encoder.encode(&input).expect("voice frame should encode");
        assert!(!packet.payload.is_empty());
        assert!(packet.payload.len() <= MAX_OPUS_PACKET_BYTES);

        let mut decoder = VoiceDecoder::new().expect("voice decoder should initialize");
        let output = decoder.decode(&packet).expect("voice frame should decode");
        assert_eq!(output.samples.len(), OPUS_FRAME_SAMPLES);
        assert!(output.samples.iter().all(|sample| sample.is_finite()));
        assert!(output.samples.iter().any(|sample| sample.abs() > 0.001));
    }

    #[test]
    fn malformed_pcm_frame_is_rejected() {
        let frame = AudioFrame {
            samples: vec![0.0; 100],
            sample_rate: OPUS_SAMPLE_RATE,
        };
        let mut encoder = VoiceEncoder::new().expect("voice encoder should initialize");
        assert!(matches!(
            encoder.encode(&frame),
            Err(AudioCodecError::InvalidFrame(_))
        ));
    }

    #[test]
    fn decoder_conceals_a_missing_voice_frame() {
        let mut decoder = VoiceDecoder::new().expect("voice decoder should initialize");
        let concealed = decoder
            .decode_loss()
            .expect("packet loss should be concealed");
        assert_eq!(concealed.samples.len(), OPUS_FRAME_SAMPLES);
        assert!(concealed.samples.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn decoder_recovers_a_previous_frame_from_inband_fec() {
        let samples = (0..OPUS_FRAME_SAMPLES)
            .map(|index| {
                #[allow(clippy::cast_precision_loss)]
                let time = index as f32 / OPUS_SAMPLE_RATE as f32;
                (TAU * 220.0 * time).sin() * 0.25
            })
            .collect::<Vec<_>>();
        let frame = AudioFrame {
            samples,
            sample_rate: OPUS_SAMPLE_RATE,
        };
        let mut encoder = VoiceEncoder::new().expect("voice encoder should initialize");
        let first = encoder.encode(&frame).expect("first frame should encode");
        let second = encoder.encode(&frame).expect("second frame should encode");
        let mut decoder = VoiceDecoder::new().expect("voice decoder should initialize");
        decoder.decode(&first).expect("first frame should decode");
        let recovered = decoder
            .decode_fec(&second)
            .expect("the next packet should recover the missing interval");
        assert_eq!(recovered.samples.len(), OPUS_FRAME_SAMPLES);
        assert!(recovered.samples.iter().all(|sample| sample.is_finite()));
        let current = decoder
            .decode(&second)
            .expect("the recovery packet should still decode normally");
        assert_eq!(current.samples.len(), OPUS_FRAME_SAMPLES);
    }
}
