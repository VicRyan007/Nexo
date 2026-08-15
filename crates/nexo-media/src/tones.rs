//! Procedural audio tone synthesis for call ringtones and desktop notifications.
//!
//! Generates lightweight, crystal-clear sine-wave chimes with exponential decay envelopes
//! without needing bulky external sound files.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::f32::consts::PI;

/// Kinds of audio notification tones supported by Nexo.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioToneKind {
    IncomingCall,
    MessageReceived,
    PeerJoined,
    PeerLeft,
}

/// Generates PCM f32 audio samples for a specific notification tone at `sample_rate`.
#[must_use]
pub fn generate_tone(kind: AudioToneKind, sample_rate: u32) -> Vec<f32> {
    match kind {
        AudioToneKind::IncomingCall => generate_incoming_call(sample_rate),
        AudioToneKind::MessageReceived => generate_message_chime(sample_rate),
        AudioToneKind::PeerJoined => generate_peer_joined(sample_rate),
        AudioToneKind::PeerLeft => generate_peer_left(sample_rate),
    }
}

/// Generates a dual-frequency ringtone burst (440 Hz + 480 Hz) lasting 1.2 seconds.
fn generate_incoming_call(sample_rate: u32) -> Vec<f32> {
    let duration_secs = 1.2;
    let total_samples = (duration_secs * sample_rate as f32) as usize;
    let mut buffer = Vec::with_capacity(total_samples);

    for i in 0..total_samples {
        let t = i as f32 / sample_rate as f32;
        // Two burst pulses (0.0 - 0.4s and 0.6 - 1.0s)
        let is_active = (t < 0.4) || (0.6..1.0).contains(&t);
        if is_active {
            let s1 = (2.0 * PI * 440.0 * t).sin();
            let s2 = (2.0 * PI * 480.0 * t).sin();
            let envelope = ((t % 0.4) * 20.0).min(1.0) * ((0.4 - (t % 0.4)) * 20.0).clamp(0.0, 1.0);
            buffer.push((s1 + s2) * 0.15 * envelope);
        } else {
            buffer.push(0.0);
        }
    }
    buffer
}

/// Generates an ascending two-tone notification chime (D5: 587 Hz -> A5: 880 Hz) lasting 250 ms.
fn generate_message_chime(sample_rate: u32) -> Vec<f32> {
    let duration_secs = 0.25;
    let total_samples = (duration_secs * sample_rate as f32) as usize;
    let split_sample = total_samples / 2;
    let mut buffer = Vec::with_capacity(total_samples);

    for i in 0..total_samples {
        let (freq, note_t) = if i < split_sample {
            (587.33, i as f32 / sample_rate as f32)
        } else {
            (880.00, (i - split_sample) as f32 / sample_rate as f32)
        };
        let t = i as f32 / sample_rate as f32;
        let decay = (-note_t * 22.0).exp();
        let sample = (2.0 * PI * freq * t).sin() * 0.2 * decay;
        buffer.push(sample);
    }
    buffer
}

/// Generates an upward tri-tone (440 -> 554 -> 659 Hz) for peer joining.
fn generate_peer_joined(sample_rate: u32) -> Vec<f32> {
    let duration_secs = 0.30;
    let total_samples = (duration_secs * sample_rate as f32) as usize;
    let step = total_samples / 3;
    let freqs = [440.0, 554.37, 659.25];
    let mut buffer = Vec::with_capacity(total_samples);

    for i in 0..total_samples {
        let tone_idx = (i / step).min(2);
        let freq = freqs[tone_idx];
        let note_t = (i % step) as f32 / sample_rate as f32;
        let t = i as f32 / sample_rate as f32;
        let decay = (-note_t * 18.0).exp();
        let sample = (2.0 * PI * freq * t).sin() * 0.18 * decay;
        buffer.push(sample);
    }
    buffer
}

/// Generates a downward tri-tone (659 -> 554 -> 440 Hz) for peer leaving.
fn generate_peer_left(sample_rate: u32) -> Vec<f32> {
    let duration_secs = 0.30;
    let total_samples = (duration_secs * sample_rate as f32) as usize;
    let step = total_samples / 3;
    let freqs = [659.25, 554.37, 440.0];
    let mut buffer = Vec::with_capacity(total_samples);

    for i in 0..total_samples {
        let tone_idx = (i / step).min(2);
        let freq = freqs[tone_idx];
        let note_t = (i % step) as f32 / sample_rate as f32;
        let t = i as f32 / sample_rate as f32;
        let decay = (-note_t * 18.0).exp();
        let sample = (2.0 * PI * freq * t).sin() * 0.18 * decay;
        buffer.push(sample);
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_generate_bounded_pcm_samples() {
        let kinds = [
            AudioToneKind::IncomingCall,
            AudioToneKind::MessageReceived,
            AudioToneKind::PeerJoined,
            AudioToneKind::PeerLeft,
        ];

        for kind in kinds {
            let samples = generate_tone(kind, 48_000);
            assert!(!samples.is_empty());
            for s in samples {
                assert!(s.is_finite());
                assert!(
                    s.abs() <= 1.0,
                    "Sample should be clamped within [-1.0, 1.0]"
                );
            }
        }
    }
}
