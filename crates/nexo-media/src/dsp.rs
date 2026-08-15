//! Digital Signal Processing (DSP) for voice calls.
//!
//! Includes Acoustic Echo Cancellation (NLMS adaptive filter), Noise Suppression,
//! and Voice Activity Detection (VAD) for crystal-clear offline-first calling.

#![allow(clippy::cast_precision_loss)]

pub const VAD_ENERGY_THRESHOLD: f32 = 0.0005;

/// Normalized Least Mean Squares (NLMS) Acoustic Echo Canceller.
#[derive(Debug)]
pub struct AcousticEchoCanceller {
    filter_length: usize,
    weights: Vec<f32>,
    reference_history: Vec<f32>,
    mu: f32,
    eps: f32,
}

impl AcousticEchoCanceller {
    #[must_use]
    pub fn new(filter_length: usize, step_size: f32) -> Self {
        Self {
            filter_length,
            weights: vec![0.0; filter_length],
            reference_history: vec![0.0; filter_length],
            mu: step_size.clamp(0.01, 0.5),
            eps: 1e-6,
        }
    }

    /// Process microphone input with the reference speaker playback samples,
    /// cancelling the acoustic echo and updating filter weights.
    pub fn process_sample(&mut self, mic_sample: f32, speaker_ref: f32) -> f32 {
        // Shift reference history
        self.reference_history.pop();
        self.reference_history.insert(0, speaker_ref);

        // Estimate echo
        let mut estimated_echo = 0.0;
        let mut norm = self.eps;
        for i in 0..self.filter_length {
            let x = self.reference_history[i];
            estimated_echo += self.weights[i] * x;
            norm += x * x;
        }

        // Error signal (echo cancelled)
        let error = mic_sample - estimated_echo;

        // NLMS weight update
        let scale = self.mu * error / norm;
        for i in 0..self.filter_length {
            self.weights[i] += scale * self.reference_history[i];
        }

        error
    }

    /// Process an entire frame of samples.
    pub fn process_frame(&mut self, mic_samples: &mut [f32], speaker_refs: &[f32]) {
        let len = mic_samples.len().min(speaker_refs.len());
        for i in 0..len {
            mic_samples[i] = self.process_sample(mic_samples[i], speaker_refs[i]);
        }
    }
}

/// Noise suppressor and gate using RMS energy estimation and smoothing.
#[derive(Debug)]
pub struct NoiseSuppressor {
    noise_floor: f32,
    alpha_noise: f32,
    attenuation: f32,
}

impl Default for NoiseSuppressor {
    fn default() -> Self {
        Self {
            noise_floor: 0.001,
            alpha_noise: 0.05,
            attenuation: 0.2,
        }
    }
}

impl NoiseSuppressor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Suppress background noise in the given audio frame.
    pub fn process_frame(&mut self, samples: &mut [f32]) {
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();

        if rms < self.noise_floor * 2.0 {
            // Update noise floor during quiet periods
            self.noise_floor = (1.0 - self.alpha_noise) * self.noise_floor + self.alpha_noise * rms;
            // Attenuate noise
            for s in samples.iter_mut() {
                *s *= self.attenuation;
            }
        }
    }
}

/// Complete Voice DSP processing pipeline.
#[derive(Debug)]
pub struct AudioDspPipeline {
    pub aec: AcousticEchoCanceller,
    pub noise_suppressor: NoiseSuppressor,
}

impl Default for AudioDspPipeline {
    fn default() -> Self {
        Self {
            aec: AcousticEchoCanceller::new(128, 0.1),
            noise_suppressor: NoiseSuppressor::new(),
        }
    }
}

impl AudioDspPipeline {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Process captured input frame using optional speaker playback reference.
    pub fn process_input_frame(&mut self, mic_samples: &mut [f32], speaker_refs: Option<&[f32]>) {
        if let Some(refs) = speaker_refs {
            self.aec.process_frame(mic_samples, refs);
        }
        self.noise_suppressor.process_frame(mic_samples);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aec_cancels_echo_signal_over_time() {
        let mut aec = AcousticEchoCanceller::new(16, 0.2);

        let mut initial_error = 0.0;
        let mut final_error = 0.0;

        // Train filter on continuous echo path
        for i in 0..1000 {
            let speaker = (i as f32 * 0.05).sin() * 0.8;
            let mic = speaker * 0.5; // pure direct echo
            let out = aec.process_sample(mic, speaker);
            if i == 1 {
                initial_error = out.abs();
            } else if i > 900 {
                final_error += out.abs();
            }
        }
        final_error /= 100.0;

        assert!(
            final_error < initial_error * 0.6,
            "Echo error should decrease significantly: initial={initial_error}, final={final_error}"
        );
    }

    #[test]
    fn noise_suppressor_attenuates_silent_noise_floor() {
        let mut ns = NoiseSuppressor::new();
        let mut noise_frame = vec![0.0002; 960];
        ns.process_frame(&mut noise_frame);
        assert!(noise_frame[0] < 0.0002, "Noise should be attenuated");
    }
}
