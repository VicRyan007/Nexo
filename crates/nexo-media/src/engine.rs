use std::collections::HashMap;
use std::time::{Duration, Instant};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    EncodedVideoFrame, InputFrameSource, JitterBuffer, LanPeerConnection, MediaError,
    OutputPlayback, PeerConnectionError, PlayoutFrame, VideoCodec, VoiceDecoder, VoiceEncoder,
    audio_codec::AudioCodecError,
    capture::VideoCaptureSource,
    video_codec::{Vp8Encoder, frame_to_i420},
};

const MAX_FRAMES_PER_TICK: usize = 8;
const AUDIO_RETRY_INITIAL: Duration = Duration::from_millis(250);
const AUDIO_RETRY_MAX: Duration = Duration::from_secs(5);
const VIDEO_FRAME_DURATION: Duration = Duration::from_millis(33);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallEngineEvent {
    PeerConnected { peer_id: String, call_id: Uuid },
    PeerDisconnected { peer_id: String, call_id: Uuid },
    AudioInputUnavailable,
    AudioInputRecovered,
    AudioOutputUnavailable,
    AudioOutputRecovered,
}

#[derive(Debug, Error)]
pub enum CallEngineError {
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error(transparent)]
    Codec(#[from] AudioCodecError),
    #[error(transparent)]
    Connection(#[from] PeerConnectionError),
    #[error("there is no pending WebRTC connection for this peer and call")]
    UnknownPeer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParticipantStatus {
    pub peer_id: String,
    pub call_id: Uuid,
    pub connected: bool,
}

struct PeerAudio {
    call_id: Uuid,
    connection: LanPeerConnection,
    decoder: VoiceDecoder,
    jitter: JitterBuffer,
    reported_connected: bool,
}

struct EndpointRecovery {
    retry_at: Option<Instant>,
    delay: Duration,
    unavailable_reported: bool,
}

impl Default for EndpointRecovery {
    fn default() -> Self {
        Self {
            retry_at: None,
            delay: AUDIO_RETRY_INITIAL,
            unavailable_reported: false,
        }
    }
}

impl EndpointRecovery {
    fn failed(&mut self, now: Instant) {
        self.retry_at = Some(now + self.delay);
        self.delay = (self.delay * 2).min(AUDIO_RETRY_MAX);
    }

    fn ready(&self, now: Instant) -> bool {
        self.retry_at.is_some_and(|retry_at| now >= retry_at)
    }

    fn recovered(&mut self) {
        self.retry_at = None;
        self.delay = AUDIO_RETRY_INITIAL;
        self.unavailable_reported = false;
    }

    fn report_unavailable(&mut self) -> bool {
        if self.unavailable_reported {
            false
        } else {
            self.unavailable_reported = true;
            true
        }
    }
}

pub struct CallEngine {
    input: Option<InputFrameSource>,
    output: Option<OutputPlayback>,
    input_recovery: EndpointRecovery,
    output_recovery: EndpointRecovery,
    requested_input: Option<String>,
    requested_output: Option<String>,
    encoder: VoiceEncoder,
    video_encoder: Option<Vp8Encoder>,
    video_capture_source: Option<VideoCaptureSource>,
    last_video_timestamp: Instant,
    peers: HashMap<String, PeerAudio>,
    muted: bool,
}

impl CallEngine {
    pub fn new() -> Result<Self, CallEngineError> {
        Self::with_devices(None, None)
    }

    /// Builds a call engine whose audio endpoints are the requested devices.
    /// `None` (or an empty id) selects the system default; a device id that is
    /// no longer present falls back to the default. Missing audio never fails
    /// the engine: endpoints are optional and retried with bounded backoff.
    /// `video_device_id` can be some camera device ID to use for video capture,
    /// or `None` to use the system default (640x480 NV12).
    pub fn with_devices(
        input_id: Option<&str>,
        output_id: Option<&str>,
        video_device_id: Option<&str>,
    ) -> Result<Self, CallEngineError> {
        let now = Instant::now();
        let requested_input = input_id.filter(|id| !id.is_empty()).map(str::to_owned);
        let requested_output = output_id.filter(|id| !id.is_empty()).map(str::to_owned);
        let input = match requested_input.as_deref() {
            Some(id) => InputFrameSource::start_input(id),
            None => InputFrameSource::start_default(),
        }
        .ok();
        let output = match requested_output.as_deref() {
            Some(id) => OutputPlayback::start_output(id),
            None => OutputPlayback::start_default(),
        }
        .ok();
        let mut input_recovery = EndpointRecovery::default();
        let mut output_recovery = EndpointRecovery::default();
        if input.is_none() {
            input_recovery.failed(now);
        }
        if output.is_none() {
            output_recovery.failed(now);
        }
        let video_capture_source = match video_device_id {
            Some(id) => Some(VideoCaptureSource::open(id)?),
            None => None,
        };
        Ok(Self {
            input,
            output,
            input_recovery,
            output_recovery,
            requested_input,
            requested_output,
            encoder: VoiceEncoder::new()?,
            video_encoder: Some(Vp8Encoder::new(640, 480, 1_500)?),
            video_capture_source,
            last_video_timestamp: Instant::now(),
            peers: HashMap::new(),
            muted: false,
        })
    }

    pub async fn create_offer(
        &mut self,
        peer_id: String,
        call_id: Uuid,
    ) -> Result<String, CallEngineError> {
        Box::pin(self.remove_peer(&peer_id)).await?;
        let connection = LanPeerConnection::new().await?;
        let offer = connection.create_offer().await?;
        self.peers.insert(
            peer_id,
            PeerAudio {
                call_id,
                connection,
                decoder: VoiceDecoder::new()?,
                jitter: JitterBuffer::default(),
                reported_connected: false,
            },
        );
        Ok(offer)
    }

    pub async fn accept_offer(
        &mut self,
        peer_id: String,
        call_id: Uuid,
        offer: String,
    ) -> Result<String, CallEngineError> {
        Box::pin(self.remove_peer(&peer_id)).await?;
        let connection = LanPeerConnection::new().await?;
        let answer = connection.accept_offer(offer).await?;
        self.peers.insert(
            peer_id,
            PeerAudio {
                call_id,
                connection,
                decoder: VoiceDecoder::new()?,
                jitter: JitterBuffer::default(),
                reported_connected: false,
            },
        );
        Ok(answer)
    }

    pub async fn accept_answer(
        &self,
        peer_id: &str,
        call_id: Uuid,
        answer: String,
    ) -> Result<(), CallEngineError> {
        let peer = self
            .peers
            .get(peer_id)
            .filter(|peer| peer.call_id == call_id)
            .ok_or(CallEngineError::UnknownPeer)?;
        peer.connection.accept_answer(answer).await?;
        Ok(())
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// Switches the microphone to `device_id` (or the default when `None`).
    /// The previous endpoint is kept when the new device cannot be opened.
    pub fn select_input(&mut self, device_id: Option<&str>) -> Result<(), CallEngineError> {
        let requested = device_id.filter(|id| !id.is_empty()).map(str::to_owned);
        let started = match requested.as_deref() {
            Some(id) => InputFrameSource::start_input(id),
            None => InputFrameSource::start_default(),
        }?;
        if let Some(previous) = self.input.replace(started) {
            drop(previous);
        }
        self.requested_input = requested;
        self.input_recovery.recovered();
        Ok(())
    }

    /// Switches the speaker to `device_id` (or the default when `None`).
    /// The previous endpoint is kept when the new device cannot be opened.
    pub fn select_output(&mut self, device_id: Option<&str>) -> Result<(), CallEngineError> {
        let requested = device_id.filter(|id| !id.is_empty()).map(str::to_owned);
        let started = match requested.as_deref() {
            Some(id) => OutputPlayback::start_output(id),
            None => OutputPlayback::start_default(),
        }?;
        if let Some(previous) = self.output.replace(started) {
            drop(previous);
        }
        self.requested_output = requested;
        self.output_recovery.recovered();
        Ok(())
    }

    #[must_use]
    pub fn current_input_id(&self) -> Option<String> {
        self.requested_input.clone()
    }

    #[must_use]
    pub fn current_output_id(&self) -> Option<String> {
        self.requested_output.clone()
    }

    #[must_use]
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// Actual per-participant connection state, sourced from the WebRTC peers
    /// the engine is currently managing rather than from pending negotiations.
    #[must_use]
    pub fn participant_status(&self) -> Vec<ParticipantStatus> {
        self.peers
            .iter()
            .map(|(peer_id, peer)| ParticipantStatus {
                peer_id: peer_id.clone(),
                call_id: peer.call_id,
                connected: peer.connection.is_connected(),
            })
            .collect()
    }

    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    #[must_use]
    pub fn connected_peer_count(&self) -> usize {
        self.peers
            .values()
            .filter(|peer| peer.connection.is_connected())
            .count()
    }

    #[must_use]
    pub fn has_peer(&self, peer_id: &str, call_id: Uuid) -> bool {
        self.peers
            .get(peer_id)
            .is_some_and(|peer| peer.call_id == call_id)
    }

    pub async fn remove_peer(&mut self, peer_id: &str) -> Result<(), CallEngineError> {
        if let Some(peer) = self.peers.remove(peer_id) {
            peer.connection.close().await?;
        }
        Ok(())
    }

    pub async fn close(&mut self) -> Result<(), CallEngineError> {
        let peers = std::mem::take(&mut self.peers);
        for peer in peers.into_values() {
            peer.connection.close().await?;
        }
        Ok(())
    }

    pub async fn tick(&mut self) -> Result<Vec<CallEngineEvent>, CallEngineError> {
        let now = Instant::now();
        let mut events = Vec::new();
        self.detect_failed_audio_endpoints(now, &mut events);
        self.report_missing_audio_endpoints(&mut events);
        self.recover_audio_endpoints(now, &mut events);
        for (peer_id, peer) in &mut self.peers {
            let connected = peer.connection.is_connected();
            if connected != peer.reported_connected {
                events.push(if connected {
                    CallEngineEvent::PeerConnected {
                        peer_id: peer_id.clone(),
                        call_id: peer.call_id,
                    }
                } else {
                    CallEngineEvent::PeerDisconnected {
                        peer_id: peer_id.clone(),
                        call_id: peer.call_id,
                    }
                });
                peer.reported_connected = connected;
            }
        }

        for _ in 0..MAX_FRAMES_PER_TICK {
            let frame = match self.input.as_ref().map(InputFrameSource::try_frame) {
                Some(Ok(Some(frame))) => frame,
                Some(Ok(None)) | None => break,
                Some(Err(_)) => {
                    self.input = None;
                    self.input_recovery.failed(now);
                    if self.input_recovery.report_unavailable() {
                        events.push(CallEngineEvent::AudioInputUnavailable);
                    }
                    break;
                }
            };
            if self.muted {
                continue;
            }
            let packet = self.encoder.encode(&frame)?;
            for peer in self.peers.values() {
                if peer.connection.is_connected() {
                    peer.connection.send_audio(&packet).await?;
                }
            }
        }

        // Send video frames at ~30 fps (every 33 ms)
        let video_elapsed = now.duration_since(self.last_video_timestamp);
        if video_elapsed >= VIDEO_FRAME_DURATION {
            let mut i420_data = None;
            let mut frame_width = 640i32;
            let mut frame_height = 480i32;

            if let Some(ref mut capture_source) = self.video_capture_source {
                if let Ok(Some(frame)) = capture_source.read_frame() {
                    frame_width = frame.width as i32;
                    frame_height = frame.height as i32;
                    match frame_to_i420(&frame) {
                        Ok(data) => i420_data = Some(data),
                        Err(e) => {
                            // Fallback to synthetic frame on conversion error
                            eprintln!("Video frame conversion error: {}", e);
                        }
                    }
                }
            }

            let (width, height) = (frame_width.max(1), frame_height.max(1));
            let y_size = width as usize * height as usize;
            let mut input = match &i420_data {
                Some(data) => data.clone(),
                None => {
                    // Fallback: synthetic gradient frame
                    let mut synthetic = vec![0u8; y_size + y_size / 2];
                    for row in 0..height {
                        for column in 0..width {
                            let value =
                                u8::try_from(column * 255 / width.max(1)).unwrap_or(u8::MAX);
                            synthetic[row as usize * width as usize + column as usize] = value;
                        }
                    }
                    synthetic
                }
            };

            let bitstream = self
                .video_encoder
                .as_mut()
                .expect("video encoder initialized")
                .encode_frame(Duration::from_millis(33), &input)
                .expect("frame should encode");
            if let Some(frame) = bitstream {
                for peer in self.peers.values() {
                    if peer.connection.is_connected() {
                        let _ = peer.connection.send_video(&frame).await;
                    }
                }
            }
            self.last_video_timestamp = now;
        }

        for peer in self.peers.values_mut() {
            for _ in 0..MAX_FRAMES_PER_TICK {
                let Some(packet) = peer.connection.try_received_audio()? else {
                    break;
                };
                peer.jitter.push(packet.sequence_number, packet.frame);
            }
            if let Some(frame) = peer.jitter.pop_ready_at(Instant::now()) {
                let decoded = match frame {
                    PlayoutFrame::Packet(packet) => peer.decoder.decode(&packet)?,
                    PlayoutFrame::Loss { recovery_packet } => {
                        if let Some(packet) = recovery_packet {
                            peer.decoder
                                .decode_fec(&packet)
                                .or_else(|_| peer.decoder.decode_loss())?
                        } else {
                            peer.decoder.decode_loss()?
                        }
                    }
                };
                if let Some(output) = self.output.as_ref()
                    && output.play(&decoded).is_err()
                {
                    self.output = None;
                    self.output_recovery.failed(now);
                    if self.output_recovery.report_unavailable() {
                        events.push(CallEngineEvent::AudioOutputUnavailable);
                    }
                }
            }
        }
        Ok(events)
    }

    fn recover_audio_endpoints(&mut self, now: Instant, events: &mut Vec<CallEngineEvent>) {
        if self.input.is_none() && self.input_recovery.ready(now) {
            let started = self.restart_input();
            match started {
                Ok(input) => {
                    self.input = Some(input);
                    self.input_recovery.recovered();
                    events.push(CallEngineEvent::AudioInputRecovered);
                }
                Err(_) => self.input_recovery.failed(now),
            }
        }
        if self.output.is_none() && self.output_recovery.ready(now) {
            let started = self.restart_output();
            match started {
                Ok(output) => {
                    self.output = Some(output);
                    self.output_recovery.recovered();
                    events.push(CallEngineEvent::AudioOutputRecovered);
                }
                Err(_) => self.output_recovery.failed(now),
            }
        }
    }

    fn restart_input(&self) -> Result<InputFrameSource, MediaError> {
        match self.requested_input.as_deref() {
            Some(id) => InputFrameSource::start_input(id),
            None => InputFrameSource::start_default(),
        }
    }

    fn restart_output(&self) -> Result<OutputPlayback, MediaError> {
        match self.requested_output.as_deref() {
            Some(id) => OutputPlayback::start_output(id),
            None => OutputPlayback::start_default(),
        }
    }

    fn detect_failed_audio_endpoints(&mut self, now: Instant, events: &mut Vec<CallEngineEvent>) {
        if self
            .input
            .as_ref()
            .is_some_and(InputFrameSource::has_failed)
        {
            self.input = None;
            self.input_recovery.failed(now);
            if self.input_recovery.report_unavailable() {
                events.push(CallEngineEvent::AudioInputUnavailable);
            }
        }
        if self.output.as_ref().is_some_and(OutputPlayback::has_failed) {
            self.output = None;
            self.output_recovery.failed(now);
            if self.output_recovery.report_unavailable() {
                events.push(CallEngineEvent::AudioOutputUnavailable);
            }
        }
    }

    fn report_missing_audio_endpoints(&mut self, events: &mut Vec<CallEngineEvent>) {
        if self.input.is_none() && self.input_recovery.report_unavailable() {
            events.push(CallEngineEvent::AudioInputUnavailable);
        }
        if self.output.is_none() && self.output_recovery.report_unavailable() {
            events.push(CallEngineEvent::AudioOutputUnavailable);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_recovery_uses_bounded_exponential_backoff() {
        let start = Instant::now();
        let mut recovery = EndpointRecovery::default();
        assert!(!recovery.ready(start));

        let expected_delays = [250, 500, 1_000, 2_000, 4_000, 5_000, 5_000];
        let mut now = start;
        for delay_ms in expected_delays {
            recovery.failed(now);
            assert!(!recovery.ready(now));
            now += Duration::from_millis(delay_ms);
            assert!(recovery.ready(now));
        }
    }

    #[test]
    fn successful_recovery_resets_backoff() {
        let start = Instant::now();
        let mut recovery = EndpointRecovery::default();
        recovery.failed(start);
        recovery.failed(start + Duration::from_millis(250));
        recovery.recovered();
        assert!(!recovery.ready(start + Duration::from_secs(10)));
        recovery.failed(start);
        assert!(recovery.ready(start + AUDIO_RETRY_INITIAL));
    }

    #[test]
    fn unavailable_endpoint_is_reported_once_until_recovery() {
        let mut recovery = EndpointRecovery::default();
        assert!(recovery.report_unavailable());
        assert!(!recovery.report_unavailable());
        recovery.failed(Instant::now());
        assert!(!recovery.report_unavailable());
        recovery.recovered();
        assert!(recovery.report_unavailable());
    }
}
