use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
};
use std::{collections::VecDeque, sync::Mutex};

use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream,
    traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _},
};
use serde::{Deserialize, Serialize};

use crate::MediaError;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum AudioDeviceKind {
    Input,
    Output,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub kind: AudioDeviceKind,
    pub is_default: bool,
    pub channels: u16,
    pub sample_rate: u32,
    pub sample_format: String,
}

pub struct InputLevelMonitor {
    _stream: Stream,
    level_bits: Arc<AtomicU32>,
}

pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_FRAME_SAMPLES: usize = 960;

#[derive(Clone, Debug, PartialEq)]
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

pub struct InputFrameSource {
    _stream: Stream,
    receiver: Receiver<AudioFrame>,
    failed: Arc<AtomicBool>,
}

pub struct OutputPlayback {
    _stream: Stream,
    queue: Arc<Mutex<PcmPlaybackBuffer>>,
    reference: Arc<Mutex<Option<Vec<f32>>>>,
    sample_rate: u32,
    failed: Arc<AtomicBool>,
}

impl OutputPlayback {
    pub fn start_default() -> Result<Self, MediaError> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| MediaError::AudioDevice("no default output device".to_owned()))?;
        Self::start_on(&device)
    }

    /// Opens the named output device, falling back to the system default when
    /// the requested device is no longer available.
    pub fn start_output(device_id: &str) -> Result<Self, MediaError> {
        let device = output_device_by_id(device_id).ok_or_else(|| {
            MediaError::AudioDevice(format!("no output device named {device_id:?}"))
        })?;
        Self::start_on(&device)
    }

    fn start_on(device: &cpal::Device) -> Result<Self, MediaError> {
        let supported = device
            .default_output_config()
            .map_err(|error| MediaError::AudioDevice(error.to_string()))?;
        let channels = usize::from(supported.channels());
        let sample_rate = supported.sample_rate();
        let config = supported.config();
        let queue = Arc::new(Mutex::new(PcmPlaybackBuffer::new()));
        let reference = Arc::new(Mutex::new(None));
        let failed = Arc::new(AtomicBool::new(false));
        let stream = match supported.sample_format() {
            SampleFormat::I8 => {
                build_output_stream::<i8>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::I16 => {
                build_output_stream::<i16>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::I24 => {
                build_output_stream::<cpal::I24>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::I32 => {
                build_output_stream::<i32>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::I64 => {
                build_output_stream::<i64>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::U8 => {
                build_output_stream::<u8>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::U16 => {
                build_output_stream::<u16>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::U32 => {
                build_output_stream::<u32>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::U64 => {
                build_output_stream::<u64>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::F32 => {
                build_output_stream::<f32>(device, &config, channels, &queue, &failed)
            }
            SampleFormat::F64 => {
                build_output_stream::<f64>(device, &config, channels, &queue, &failed)
            }
            format => Err(MediaError::AudioDevice(format!(
                "unsupported output sample format {format}"
            ))),
        }?;
        stream
            .play()
            .map_err(|error| MediaError::AudioDevice(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            queue,
            reference,
            sample_rate,
            failed,
        })
    }

    pub fn play(&self, frame: &AudioFrame) -> Result<(), MediaError> {
        if self.has_failed() {
            return Err(MediaError::AudioDevice(
                "output stream stopped unexpectedly".to_owned(),
            ));
        }
        if frame.sample_rate != OPUS_SAMPLE_RATE || frame.samples.len() != OPUS_FRAME_SAMPLES {
            return Err(MediaError::AudioDevice(
                "playback frame must contain 20 ms of mono 48 kHz audio".to_owned(),
            ));
        }
        self.queue
            .lock()
            .map_err(|_| MediaError::AudioDevice("audio playback queue is poisoned".to_owned()))?
            .push_resampled(&frame.samples, frame.sample_rate, self.sample_rate);
        self.reference
            .lock()
            .map_err(|_| MediaError::AudioDevice("audio reference queue is poisoned".to_owned()))?
            .replace(frame.samples.clone());
        Ok(())
    }

    /// Returns the most recently submitted mono playback frame for acoustic
    /// echo cancellation. The output callback remains independent of this
    /// best-effort reference snapshot.
    #[must_use]
    pub fn latest_reference(&self) -> Option<Vec<f32>> {
        self.reference
            .lock()
            .ok()
            .and_then(|reference| reference.clone())
    }

    #[must_use]
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

fn build_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    shared: &Arc<Mutex<PcmPlaybackBuffer>>,
    failed: &Arc<AtomicBool>,
) -> Result<Stream, MediaError>
where
    T: SizedSample + FromSample<f32>,
{
    let queue = Arc::clone(shared);
    let failed = Arc::clone(failed);
    device
        .build_output_stream(
            *config,
            move |output: &mut [T], _| {
                if let Ok(mut queue) = queue.try_lock() {
                    queue.fill_interleaved(output, channels);
                } else {
                    output.fill_with(|| T::from_sample(0.0));
                }
            },
            move |_| failed.store(true, Ordering::Relaxed),
            None,
        )
        .map_err(|error| MediaError::AudioDevice(error.to_string()))
}

struct PcmPlaybackBuffer {
    samples: VecDeque<f32>,
}

impl PcmPlaybackBuffer {
    const MAX_QUEUED_SAMPLES: usize = OPUS_FRAME_SAMPLES * 10;

    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(Self::MAX_QUEUED_SAMPLES),
        }
    }

    fn push_resampled(&mut self, samples: &[f32], source_rate: u32, output_rate: u32) {
        let samples = resample_linear(samples, source_rate, output_rate);
        let overflow = self
            .samples
            .len()
            .saturating_add(samples.len())
            .saturating_sub(Self::MAX_QUEUED_SAMPLES);
        self.samples.drain(..overflow.min(self.samples.len()));
        self.samples.extend(samples);
    }

    fn fill_interleaved<T: Sample + FromSample<f32>>(&mut self, output: &mut [T], channels: usize) {
        for frame in output.chunks_mut(channels.max(1)) {
            let sample = self.samples.pop_front().unwrap_or(0.0);
            for channel in frame {
                *channel = T::from_sample(sample);
            }
        }
    }
}

fn resample_linear(samples: &[f32], source_rate: u32, output_rate: u32) -> Vec<f32> {
    if samples.is_empty() || source_rate == 0 || output_rate == 0 {
        return Vec::new();
    }
    if source_rate == output_rate {
        return samples.to_vec();
    }
    let output_len = samples
        .len()
        .saturating_mul(output_rate as usize)
        .div_ceil(source_rate as usize);
    (0..output_len)
        .map(|output_index| {
            let numerator = output_index.saturating_mul(source_rate as usize);
            let left = (numerator / output_rate as usize).min(samples.len() - 1);
            let right = (left + 1).min(samples.len() - 1);
            let remainder = numerator % output_rate as usize;
            #[allow(clippy::cast_precision_loss)]
            let fraction = remainder as f32 / output_rate as f32;
            samples[left] + (samples[right] - samples[left]) * fraction
        })
        .collect()
}

impl InputFrameSource {
    pub fn start_default() -> Result<Self, MediaError> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| MediaError::AudioDevice("no default input device".to_owned()))?;
        Self::start_on(&device)
    }

    /// Opens the named input device, falling back to the system default when
    /// the requested device is no longer available.
    pub fn start_input(device_id: &str) -> Result<Self, MediaError> {
        let device = input_device_by_id(device_id).ok_or_else(|| {
            MediaError::AudioDevice(format!("no input device named {device_id:?}"))
        })?;
        Self::start_on(&device)
    }

    fn start_on(device: &cpal::Device) -> Result<Self, MediaError> {
        let supported = device
            .default_input_config()
            .map_err(|error| MediaError::AudioDevice(error.to_string()))?;
        let channels = usize::from(supported.channels());
        let sample_rate = supported.sample_rate();
        let config = supported.config();
        let (sender, receiver) = sync_channel(8);
        let failed = Arc::new(AtomicBool::new(false));
        let stream = match supported.sample_format() {
            SampleFormat::I8 => {
                build_frame_stream::<i8>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::I16 => {
                build_frame_stream::<i16>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::I24 => build_frame_stream::<cpal::I24>(
                device,
                &config,
                channels,
                sample_rate,
                sender,
                &failed,
            ),
            SampleFormat::I32 => {
                build_frame_stream::<i32>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::I64 => {
                build_frame_stream::<i64>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::U8 => {
                build_frame_stream::<u8>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::U16 => {
                build_frame_stream::<u16>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::U32 => {
                build_frame_stream::<u32>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::U64 => {
                build_frame_stream::<u64>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::F32 => {
                build_frame_stream::<f32>(device, &config, channels, sample_rate, sender, &failed)
            }
            SampleFormat::F64 => {
                build_frame_stream::<f64>(device, &config, channels, sample_rate, sender, &failed)
            }
            format => Err(MediaError::AudioDevice(format!(
                "unsupported input sample format {format}"
            ))),
        }?;
        stream
            .play()
            .map_err(|error| MediaError::AudioDevice(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            receiver,
            failed,
        })
    }

    pub fn try_frame(&self) -> Result<Option<AudioFrame>, MediaError> {
        if self.has_failed() {
            return Err(MediaError::AudioDevice(
                "input stream stopped unexpectedly".to_owned(),
            ));
        }
        match self.receiver.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(MediaError::AudioDevice(
                "input stream stopped unexpectedly".to_owned(),
            )),
        }
    }

    #[must_use]
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

fn build_frame_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    sample_rate: u32,
    sender: SyncSender<AudioFrame>,
    failed: &Arc<AtomicBool>,
) -> Result<Stream, MediaError>
where
    T: SizedSample + Sample,
    f32: FromSample<T>,
{
    let mut framer = PcmFramer::new(channels, sample_rate);
    let failed = Arc::clone(failed);
    device
        .build_input_stream(
            *config,
            move |data: &[T], _| {
                framer.push(data.iter().copied().map(f32::from_sample), &sender);
            },
            move |_| failed.store(true, Ordering::Relaxed),
            None,
        )
        .map_err(|error| MediaError::AudioDevice(error.to_string()))
}

struct PcmFramer {
    channels: usize,
    source_rate: u32,
    source_samples: Vec<f32>,
    source_position: f64,
    output_samples: Vec<f32>,
}

impl PcmFramer {
    fn new(channels: usize, source_rate: u32) -> Self {
        Self {
            channels: channels.max(1),
            source_rate,
            source_samples: Vec::with_capacity(OPUS_FRAME_SAMPLES * 2),
            source_position: 0.0,
            output_samples: Vec::with_capacity(OPUS_FRAME_SAMPLES * 2),
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn push(&mut self, samples: impl Iterator<Item = f32>, sender: &SyncSender<AudioFrame>) {
        let interleaved = samples.collect::<Vec<_>>();
        for frame in interleaved.chunks_exact(self.channels) {
            #[allow(clippy::cast_precision_loss)]
            let channel_count = self.channels as f32;
            self.source_samples
                .push(frame.iter().sum::<f32>() / channel_count);
        }

        if self.source_rate == 0 {
            return;
        }
        let step = f64::from(self.source_rate) / f64::from(OPUS_SAMPLE_RATE);
        while self.source_position + 1.0 < self.source_samples.len() as f64 {
            let left_index = self.source_position.floor() as usize;
            let fraction = (self.source_position - left_index as f64) as f32;
            let left = self.source_samples[left_index];
            let right = self.source_samples[left_index + 1];
            self.output_samples.push(left + (right - left) * fraction);
            self.source_position += step;
            while self.output_samples.len() >= OPUS_FRAME_SAMPLES {
                let samples = self.output_samples.drain(..OPUS_FRAME_SAMPLES).collect();
                let _ = sender.try_send(AudioFrame {
                    samples,
                    sample_rate: OPUS_SAMPLE_RATE,
                });
            }
        }

        let consumed = self.source_position.floor() as usize;
        if consumed > 0 {
            self.source_samples.drain(..consumed);
            self.source_position -= consumed as f64;
        }
    }
}

impl InputLevelMonitor {
    pub fn start_default() -> Result<Self, MediaError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| MediaError::AudioDevice("no default input device".to_owned()))?;
        let supported = device
            .default_input_config()
            .map_err(|error| MediaError::AudioDevice(error.to_string()))?;
        let config = supported.config();
        let level_bits = Arc::new(AtomicU32::new(0_f32.to_bits()));
        let stream = match supported.sample_format() {
            SampleFormat::I8 => build_level_stream::<i8>(&device, &config, &level_bits),
            SampleFormat::I16 => build_level_stream::<i16>(&device, &config, &level_bits),
            SampleFormat::I24 => build_level_stream::<cpal::I24>(&device, &config, &level_bits),
            SampleFormat::I32 => build_level_stream::<i32>(&device, &config, &level_bits),
            SampleFormat::I64 => build_level_stream::<i64>(&device, &config, &level_bits),
            SampleFormat::U8 => build_level_stream::<u8>(&device, &config, &level_bits),
            SampleFormat::U16 => build_level_stream::<u16>(&device, &config, &level_bits),
            SampleFormat::U32 => build_level_stream::<u32>(&device, &config, &level_bits),
            SampleFormat::U64 => build_level_stream::<u64>(&device, &config, &level_bits),
            SampleFormat::F32 => build_level_stream::<f32>(&device, &config, &level_bits),
            SampleFormat::F64 => build_level_stream::<f64>(&device, &config, &level_bits),
            format => Err(MediaError::AudioDevice(format!(
                "unsupported input sample format {format}"
            ))),
        }?;
        stream
            .play()
            .map_err(|error| MediaError::AudioDevice(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            level_bits,
        })
    }

    #[must_use]
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }
}

fn build_level_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: &Arc<AtomicU32>,
) -> Result<Stream, MediaError>
where
    T: SizedSample + Sample,
    f32: FromSample<T>,
{
    let level = Arc::clone(shared);
    device
        .build_input_stream(
            *config,
            move |data: &[T], _| {
                let sum = data
                    .iter()
                    .map(|sample| {
                        let value = f32::from_sample(*sample);
                        value * value
                    })
                    .sum::<f32>();
                let rms = if data.is_empty() {
                    0.0
                } else {
                    #[allow(clippy::cast_precision_loss)]
                    let sample_count = data.len() as f32;
                    (sum / sample_count).sqrt().clamp(0.0, 1.0)
                };
                level.store(rms.to_bits(), Ordering::Relaxed);
            },
            |_| {},
            None,
        )
        .map_err(|error| MediaError::AudioDevice(error.to_string()))
}

fn input_device_by_id(device_id: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    host.input_devices()
        .ok()?
        .find(|device| device.id().is_ok_and(|id| id.to_string() == device_id))
        .or_else(|| host.default_input_device())
}

fn output_device_by_id(device_id: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    host.output_devices()
        .ok()?
        .find(|device| device.id().is_ok_and(|id| id.to_string() == device_id))
        .or_else(|| host.default_output_device())
}

pub fn enumerate_audio_devices() -> Result<Vec<AudioDeviceInfo>, MediaError> {
    let host = cpal::default_host();
    let default_input = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let default_output = host
        .default_output_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let devices = host
        .devices()
        .map_err(|error| MediaError::AudioDevice(error.to_string()))?;
    let mut found = Vec::new();
    for device in devices {
        let id = device
            .id()
            .map_err(|error| MediaError::AudioDevice(error.to_string()))?
            .to_string();
        let name = device
            .description()
            .map_or_else(|_| id.clone(), |description| description.to_string());
        if let Ok(config) = device.default_input_config() {
            found.push(device_info(
                &id,
                &name,
                AudioDeviceKind::Input,
                default_input.as_deref() == Some(id.as_str()),
                &config,
            ));
        }
        if let Ok(config) = device.default_output_config() {
            found.push(device_info(
                &id,
                &name,
                AudioDeviceKind::Output,
                default_output.as_deref() == Some(id.as_str()),
                &config,
            ));
        }
    }
    found.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| right.is_default.cmp(&left.is_default))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(found)
}

fn device_info(
    id: &str,
    name: &str,
    kind: AudioDeviceKind,
    is_default: bool,
    config: &cpal::SupportedStreamConfig,
) -> AudioDeviceInfo {
    AudioDeviceInfo {
        id: id.to_owned(),
        name: name.to_owned(),
        kind,
        is_default,
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        sample_format: sample_format_name(config.sample_format()).to_owned(),
    }
}

const fn sample_format_name(format: SampleFormat) -> &'static str {
    match format {
        SampleFormat::I8 => "i8",
        SampleFormat::I16 => "i16",
        SampleFormat::I24 => "i24",
        SampleFormat::I32 => "i32",
        SampleFormat::I64 => "i64",
        SampleFormat::U8 => "u8",
        SampleFormat::U16 => "u16",
        SampleFormat::U32 => "u32",
        SampleFormat::U64 => "u64",
        SampleFormat::F32 => "f32",
        SampleFormat::F64 => "f64",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_audio_devices_have_stable_metadata() {
        let devices = enumerate_audio_devices().expect("audio enumeration should work");
        assert!(
            !devices.is_empty(),
            "at least one audio endpoint is expected"
        );
        assert!(devices.iter().all(|device| {
            !device.id.is_empty()
                && !device.name.is_empty()
                && device.channels > 0
                && device.sample_rate > 0
        }));
    }

    #[test]
    fn stereo_input_is_framed_as_twenty_ms_mono() {
        let (sender, receiver) = sync_channel(1);
        let mut framer = PcmFramer::new(2, OPUS_SAMPLE_RATE);
        let stereo = (0..=OPUS_FRAME_SAMPLES).flat_map(|_| [0.25_f32, 0.75_f32]);
        framer.push(stereo, &sender);
        let frame = receiver.try_recv().expect("one frame should be produced");
        assert_eq!(frame.samples.len(), OPUS_FRAME_SAMPLES);
        assert!(
            frame
                .samples
                .iter()
                .all(|sample| (*sample - 0.5).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn non_48khz_input_is_resampled_into_opus_frames() {
        let (sender, receiver) = sync_channel(2);
        let mut framer = PcmFramer::new(1, 44_100);
        framer.push((0..44_100).map(|_| 0.25_f32), &sender);
        let first = receiver.try_recv().expect("resampled frame should exist");
        let second = receiver
            .try_recv()
            .expect("a second resampled frame should exist");
        assert_eq!(first.sample_rate, OPUS_SAMPLE_RATE);
        assert_eq!(first.samples.len(), OPUS_FRAME_SAMPLES);
        assert_eq!(second.samples.len(), OPUS_FRAME_SAMPLES);
        assert!(
            first
                .samples
                .iter()
                .chain(second.samples.iter())
                .all(|sample| (*sample - 0.25).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn mono_playback_is_expanded_and_underruns_to_silence() {
        let mut queue = PcmPlaybackBuffer::new();
        queue.push_resampled(&[0.25, -0.5], OPUS_SAMPLE_RATE, OPUS_SAMPLE_RATE);
        let mut stereo = [1.0_f32; 6];
        queue.fill_interleaved(&mut stereo, 2);
        let expected = [0.25, 0.25, -0.5, -0.5, 0.0, 0.0];
        assert!(
            stereo
                .iter()
                .zip(expected)
                .all(|(actual, expected)| (*actual - expected).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn playback_queue_discards_stale_audio_instead_of_growing() {
        let mut queue = PcmPlaybackBuffer::new();
        queue.push_resampled(
            &vec![0.1; PcmPlaybackBuffer::MAX_QUEUED_SAMPLES],
            OPUS_SAMPLE_RATE,
            OPUS_SAMPLE_RATE,
        );
        queue.push_resampled(
            &vec![0.2; OPUS_FRAME_SAMPLES],
            OPUS_SAMPLE_RATE,
            OPUS_SAMPLE_RATE,
        );
        assert_eq!(queue.samples.len(), PcmPlaybackBuffer::MAX_QUEUED_SAMPLES);
        assert_eq!(queue.samples.back(), Some(&0.2));
    }

    #[test]
    fn playback_resamples_to_the_native_output_rate() {
        let input = [0.0_f32, 1.0, 0.0];
        let output = resample_linear(&input, 48_000, 96_000);
        assert_eq!(output.len(), 6);
        let expected = [0.0, 0.5, 1.0, 0.5, 0.0, 0.0];
        assert!(
            output
                .iter()
                .zip(expected)
                .all(|(actual, expected)| (*actual - expected).abs() < f32::EPSILON)
        );
    }
}
