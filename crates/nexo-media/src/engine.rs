use std::collections::HashMap;
use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    EncodedAudioFrame, EncodedVideoFrame, InputFrameSource, JitterBuffer, LanPeerConnection,
    MediaError, OutputPlayback, PeerConnectionError, PlayoutFrame, VoiceDecoder, VoiceEncoder,
    audio_codec::AudioCodecError,
    congestion::{CongestionController, VideoQualityProfile},
    dsp::AudioDspPipeline,
    video_codec::{
        VideoCodecError, VideoDecoder, Vp8Encoder, frame_to_i420, i420_to_nv12, i420_to_rgba,
        resize_i420_nearest,
    },
};
use nexo_core::MediaFrameCipher;
use nexo_video::{EncodedH264Frame, HardwareH264Encoder, ScreenCaptureSource, VideoCaptureSource};

const MAX_FRAMES_PER_TICK: usize = 8;
const AUDIO_RETRY_INITIAL: Duration = Duration::from_millis(250);
const AUDIO_RETRY_MAX: Duration = Duration::from_secs(5);
const VIDEO_WIDTH: u32 = 640;
const VIDEO_HEIGHT: u32 = 480;
const VIDEO_BITRATE_KBPS: u32 = 1_500;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallEngineEvent {
    PeerConnected {
        peer_id: String,
        call_id: Uuid,
    },
    PeerDisconnected {
        peer_id: String,
        call_id: Uuid,
    },
    LocalVideoFrame {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    RemoteVideoFrame {
        peer_id: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// Binary payload received on the authenticated WebRTC data channel.
    DataMessage {
        peer_id: String,
        data: Vec<u8>,
    },
    AudioInputUnavailable,
    AudioInputRecovered,
    AudioOutputUnavailable,
    AudioOutputRecovered,
    VideoUnavailable {
        reason: String,
    },
    VideoRecovered,
}

#[derive(Debug, Error)]
pub enum CallEngineError {
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error(transparent)]
    Codec(#[from] AudioCodecError),
    #[error(transparent)]
    CodecVideo(#[from] VideoCodecError),
    #[error(transparent)]
    VideoDevice(#[from] nexo_video::VideoError),
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
    audio_decoders: HashMap<String, VoiceDecoder>,
    video_decoders: HashMap<String, VideoDecoder>,
    audio_jitters: HashMap<String, JitterBuffer>,
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

#[allow(clippy::struct_excessive_bools)]
pub struct CallEngine {
    input: Option<InputFrameSource>,
    output: Option<OutputPlayback>,
    input_recovery: EndpointRecovery,
    output_recovery: EndpointRecovery,
    video_recovery: EndpointRecovery,
    h264_recovery: EndpointRecovery,
    requested_input: Option<String>,
    requested_output: Option<String>,
    requested_video: Option<String>,
    encoder: VoiceEncoder,
    dsp: AudioDspPipeline,
    video_encoder: Option<Vp8Encoder>,
    h264_encoder: Option<HardwareH264Encoder>,
    remote_h264_capabilities: HashMap<String, bool>,
    media_cipher: Option<MediaFrameCipher>,
    next_media_sequence: u64,
    video_capture_source: Option<VideoCaptureSource>,
    screen_capture_source: Option<ScreenCaptureSource>,
    video_enabled: bool,
    screen_sharing: bool,
    quality_controller: CongestionController,
    video_profile: VideoQualityProfile,
    last_video_timestamp: Instant,
    video_failure_reported: bool,
    relay_enabled: bool,
    local_media_targets: Option<Vec<String>>,
    peers: HashMap<String, PeerAudio>,
    muted: bool,
}

impl CallEngine {
    fn try_hardware_h264_encoder(
        width: u32,
        height: u32,
        bitrate_bps: u32,
    ) -> Option<HardwareH264Encoder> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            HardwareH264Encoder::new(width, height, bitrate_bps)
        }))
        .ok()
        .and_then(Result::ok)
    }

    /// Probe the same H.264 encoder constructor used by a call. Capability
    /// reports must not treat a merely present driver DLL as an encoder that
    /// can actually publish frames.
    #[must_use]
    pub fn hardware_video_encoder_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            Self::try_hardware_h264_encoder(VIDEO_WIDTH, VIDEO_HEIGHT, 1_500_000).is_some()
        })
    }

    #[must_use]
    pub const fn video_quality_profile(&self) -> VideoQualityProfile {
        self.video_profile
    }

    pub fn new() -> Result<Self, CallEngineError> {
        Self::with_devices(None, None, None)
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
        let requested_video = video_device_id
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
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
        let video_capture_source = match requested_video.as_deref() {
            Some(id) => VideoCaptureSource::open(id).ok(),
            None => None,
        };
        let mut video_recovery = EndpointRecovery::default();
        if requested_video.is_some() && video_capture_source.is_none() {
            video_recovery.failed(now);
        }
        let initial_video_profile = VideoQualityProfile {
            target_bitrate_kbps: VIDEO_BITRATE_KBPS,
            target_fps: 30,
            width: VIDEO_WIDTH,
            height: VIDEO_HEIGHT,
        };
        Ok(Self {
            input,
            output,
            input_recovery,
            output_recovery,
            video_recovery,
            h264_recovery: EndpointRecovery::default(),
            requested_input,
            requested_output,
            requested_video,
            encoder: VoiceEncoder::new()?,
            dsp: AudioDspPipeline::new(),
            video_encoder: Vp8Encoder::new(
                initial_video_profile.width,
                initial_video_profile.height,
                initial_video_profile.target_bitrate_kbps,
            )
            .ok(),
            h264_encoder: Self::try_hardware_h264_encoder(
                initial_video_profile.width,
                initial_video_profile.height,
                initial_video_profile.target_bitrate_kbps * 1_000,
            ),
            remote_h264_capabilities: HashMap::new(),
            media_cipher: None,
            next_media_sequence: 0,
            video_capture_source,
            screen_capture_source: None,
            video_enabled: true,
            screen_sharing: false,
            quality_controller: CongestionController::new(initial_video_profile, 150, 3_000),
            video_profile: initial_video_profile,
            last_video_timestamp: Instant::now(),
            video_failure_reported: false,
            relay_enabled: false,
            local_media_targets: None,
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
        let connection =
            LanPeerConnection::new_with_video_codec(self.preferred_video_codec(&peer_id)).await?;
        let offer = connection.create_offer().await?;
        self.peers.insert(
            peer_id,
            PeerAudio {
                call_id,
                connection,
                audio_decoders: HashMap::new(),
                video_decoders: HashMap::new(),
                audio_jitters: HashMap::new(),
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
        let codec = self.preferred_video_codec(&peer_id);
        self.accept_offer_with_codec(peer_id, call_id, offer, codec)
            .await
    }

    /// Accept an SDP offer using the codec selected by the offerer.
    ///
    /// The selected codec is carried alongside the signed call signal rather
    /// than inferred from the order in which separate capability messages
    /// happen to arrive. This keeps both sides of the WebRTC m-line aligned
    /// when H.264 is available.
    pub async fn accept_offer_with_codec(
        &mut self,
        peer_id: String,
        call_id: Uuid,
        offer: String,
        codec: crate::VideoCodec,
    ) -> Result<String, CallEngineError> {
        Box::pin(self.remove_peer(&peer_id)).await?;
        // A peer can only select H.264 when this engine has a working native
        // encoder too. A malformed or stale H.264 selection therefore falls
        // back deterministically before creating the media engine.
        let codec = match codec {
            crate::VideoCodec::H264 if self.h264_encoder.is_some() => crate::VideoCodec::H264,
            _ => crate::VideoCodec::Vp8,
        };
        let connection = LanPeerConnection::new_with_video_codec(codec).await?;
        let answer = connection.accept_offer(offer).await?;
        self.peers.insert(
            peer_id,
            PeerAudio {
                call_id,
                connection,
                audio_decoders: HashMap::new(),
                video_decoders: HashMap::new(),
                audio_jitters: HashMap::new(),
                reported_connected: false,
            },
        );
        Ok(answer)
    }

    /// Returns the codec fixed into an already-created offer connection.
    #[must_use]
    pub fn peer_video_codec(&self, peer_id: &str, call_id: Uuid) -> Option<crate::VideoCodec> {
        self.peers
            .get(peer_id)
            .filter(|peer| peer.call_id == call_id)
            .map(|peer| peer.connection.video_codec())
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

    /// Returns the codecs this engine can actually publish. H.264 is advertised
    /// only when the native platform encoder initialized successfully.
    #[must_use]
    pub fn local_capabilities_payload(&self) -> String {
        if self.h264_encoder.is_some() {
            "video=vp8,h264".to_owned()
        } else {
            "video=vp8".to_owned()
        }
    }

    /// Stores a bounded capability declaration received through the signed call
    /// signal channel. Unknown values are rejected before reaching this method.
    pub fn set_peer_capabilities(&mut self, peer_id: &str, payload: &str) {
        self.remote_h264_capabilities.insert(
            peer_id.to_owned(),
            payload == "video=vp8,h264" && self.h264_encoder.is_some(),
        );
    }

    fn preferred_video_codec(&self, peer_id: &str) -> crate::VideoCodec {
        if self.h264_encoder.is_some()
            && self
                .remote_h264_capabilities
                .get(peer_id)
                .copied()
                .unwrap_or(false)
        {
            crate::VideoCodec::H264
        } else {
            crate::VideoCodec::Vp8
        }
    }

    /// Enables end-to-end media protection for this call. The key is derived
    /// from the shared community secret, so a participant relay only handles
    /// authenticated opaque payloads.
    pub fn set_media_secret(&mut self, call_id: Uuid, secret: &[u8]) {
        self.media_cipher = Some(MediaFrameCipher::derive(call_id, secret));
        self.next_media_sequence = 0;
    }

    fn protect_audio_frame(&mut self, mut frame: EncodedAudioFrame) -> EncodedAudioFrame {
        if let Some(cipher) = self.media_cipher.as_ref() {
            frame.payload = cipher.encrypt(&frame.payload, self.next_media_sequence);
            self.next_media_sequence = self.next_media_sequence.wrapping_add(1);
        }
        frame
    }

    fn protect_video_frame(&mut self, mut frame: EncodedVideoFrame) -> EncodedVideoFrame {
        if let Some(cipher) = self.media_cipher.as_ref() {
            frame.data = cipher
                .encrypt(&frame.data, self.next_media_sequence)
                .into_boxed_slice();
            self.next_media_sequence = self.next_media_sequence.wrapping_add(1);
        }
        frame
    }

    /// Configure the participant-hosted SFU route. When `relay_enabled` is
    /// true, encoded frames received from one peer are forwarded to the other
    /// configured peers. An empty target list disables local publication until
    /// the topology controller has an elected host.
    pub fn configure_media_topology(&mut self, relay_enabled: bool, targets: Vec<String>) {
        self.relay_enabled = relay_enabled;
        self.local_media_targets = Some(targets);
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

    /// Switches the camera to `device_id` (or disables/clears when `None`).
    pub fn select_video(&mut self, device_id: Option<&str>) -> Result<(), CallEngineError> {
        let requested = device_id.filter(|id| !id.is_empty()).map(str::to_owned);
        let started = match requested.as_deref() {
            Some(id) => Some(VideoCaptureSource::open(id)?),
            None => None,
        };
        self.video_capture_source = started;
        self.requested_video = requested;
        self.video_recovery.recovered();
        Ok(())
    }

    pub fn set_video_enabled(&mut self, enabled: bool) {
        self.video_enabled = enabled;
    }

    #[must_use]
    pub fn is_video_enabled(&self) -> bool {
        self.video_enabled
    }

    pub fn set_screen_sharing(&mut self, enabled: bool) -> Result<(), CallEngineError> {
        if enabled && self.screen_capture_source.is_none() {
            let monitors = nexo_video::enumerate_monitors()?;
            let primary = monitors
                .iter()
                .find(|monitor| monitor.is_primary)
                .or_else(|| monitors.first())
                .ok_or_else(|| {
                    nexo_video::VideoError::screen_capture("nenhum monitor disponivel")
                })?;
            self.screen_capture_source = Some(ScreenCaptureSource::open_monitor(&primary.id)?);
        } else if !enabled {
            self.screen_capture_source = None;
        }
        self.screen_sharing = enabled;
        Ok(())
    }

    #[must_use]
    pub fn is_screen_sharing(&self) -> bool {
        self.screen_sharing
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
    pub fn current_video_id(&self) -> Option<String> {
        self.requested_video.clone()
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

    /// Returns the peer IDs that have a negotiated connection for `call_id`.
    ///
    /// Topology decisions must use this set rather than the whole community
    /// membership: an authorized peer can be online without joining this
    /// particular call.
    #[must_use]
    pub fn call_peer_ids(&self, call_id: Uuid) -> Vec<String> {
        self.peers
            .iter()
            .filter(|(_, peer)| peer.call_id == call_id)
            .map(|(peer_id, _)| peer_id.clone())
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
    pub fn is_peer_connected(&self, peer_id: &str, call_id: Uuid) -> bool {
        self.peers
            .get(peer_id)
            .is_some_and(|peer| peer.call_id == call_id && peer.connection.is_connected())
    }

    #[must_use]
    pub fn has_peer(&self, peer_id: &str, call_id: Uuid) -> bool {
        self.peers
            .get(peer_id)
            .is_some_and(|peer| peer.call_id == call_id)
    }

    /// Send one bounded binary payload to a negotiated call peer.
    pub async fn send_data_to(&self, peer_id: &str, data: &[u8]) -> Result<(), CallEngineError> {
        let peer = self
            .peers
            .get(peer_id)
            .ok_or(CallEngineError::UnknownPeer)?;
        peer.connection.send_data(data).await?;
        Ok(())
    }

    pub async fn remove_peer(&mut self, peer_id: &str) -> Result<(), CallEngineError> {
        if let Some(peer) = self.peers.remove(peer_id) {
            peer.connection.close().await?;
        }
        for peer in self.peers.values() {
            peer.connection.release_relay_source(peer_id).await;
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

    fn refresh_video_quality(&mut self, now: Instant) {
        // Do not initialize or recreate native video encoders for headless
        // calls. The profile is applied as soon as a real source is selected.
        if self.video_capture_source.is_none() && self.screen_capture_source.is_none() {
            return;
        }
        let Some(available_bitrate) = self
            .peers
            .values()
            .filter(|peer| peer.connection.is_connected())
            .map(|peer| peer.connection.estimated_video_bitrate_bps())
            .min()
        else {
            return;
        };
        let desired = self
            .quality_controller
            .on_bitrate_estimate(available_bitrate, now);
        let source_resolution = if self.screen_sharing {
            self.screen_capture_source
                .as_ref()
                .map(ScreenCaptureSource::resolution)
        } else {
            self.video_capture_source
                .as_ref()
                .map(VideoCaptureSource::resolution)
        };
        let effective = constrain_video_profile(desired, source_resolution);
        if effective == self.video_profile {
            return;
        }
        if !self.apply_video_profile(effective) {
            self.quality_controller.restore_profile(self.video_profile);
        }
    }

    fn recover_h264_encoder(&mut self, now: Instant) {
        if self.h264_encoder.is_some() || !self.h264_recovery.ready(now) {
            return;
        }
        match Self::try_hardware_h264_encoder(
            self.video_profile.width,
            self.video_profile.height,
            self.video_profile.target_bitrate_kbps * 1_000,
        ) {
            Some(encoder) => {
                self.h264_encoder = Some(encoder);
                self.h264_recovery.recovered();
            }
            None => self.h264_recovery.failed(now),
        }
    }

    fn apply_video_profile(&mut self, profile: VideoQualityProfile) -> bool {
        let next_vp8 = if self.video_encoder.is_some() {
            Vp8Encoder::new(profile.width, profile.height, profile.target_bitrate_kbps).ok()
        } else {
            None
        };
        if self.video_encoder.is_some() && next_vp8.is_none() {
            return false;
        }

        let next_h264 = if self.h264_encoder.is_some() {
            HardwareH264Encoder::new(
                profile.width,
                profile.height,
                profile.target_bitrate_kbps * 1_000,
            )
            .ok()
        } else {
            None
        };
        if self.h264_encoder.is_some() && next_h264.is_none() {
            return false;
        }

        self.video_encoder = next_vp8;
        self.h264_encoder = next_h264;
        self.video_profile = profile;
        true
    }

    #[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
    pub async fn tick(&mut self) -> Result<Vec<CallEngineEvent>, CallEngineError> {
        let now = Instant::now();
        let mut events = Vec::new();
        let mut failed_peers = Vec::new();
        let mut relay_audio = Vec::new();
        let mut relay_video = Vec::new();
        self.refresh_video_quality(now);
        self.recover_h264_encoder(now);
        self.detect_failed_audio_endpoints(now, &mut events);
        self.report_missing_audio_endpoints(&mut events);
        self.recover_audio_endpoints(now, &mut events);
        self.recover_video_endpoint(now, &mut events);
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
            let mut frame = match self.input.as_ref().map(InputFrameSource::try_frame) {
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
            let speaker_reference = self
                .output
                .as_ref()
                .and_then(OutputPlayback::latest_reference);
            self.dsp
                .process_input_frame(&mut frame.samples, speaker_reference.as_deref());
            let packet = self.encoder.encode(&frame)?;
            let packet = self.protect_audio_frame(packet);
            for (peer_id, peer) in &self.peers {
                if self.should_send_media_to(peer_id) && peer.connection.is_connected() {
                    // A write can fail transiently while ICE/DTLS is still
                    // recovering. The receive side will report a definitive
                    // closure; do not evict a peer for one failed frame.
                    let _ = peer.connection.send_audio(&packet).await;
                }
            }
        }

        // Send video frames at the quality tier selected from receiver REMB.
        let video_frame_duration =
            Duration::from_millis(1_000_u64 / u64::from(self.video_profile.target_fps.max(1)));
        let video_elapsed = now.duration_since(self.last_video_timestamp);
        if video_elapsed >= video_frame_duration && (self.video_enabled || self.screen_sharing) {
            let mut i420_data = None;

            let capture_result = if self.screen_sharing {
                self.screen_capture_source
                    .as_mut()
                    .map_or(Ok(None), ScreenCaptureSource::read_frame)
            } else {
                self.video_capture_source
                    .as_mut()
                    .map_or(Ok(None), VideoCaptureSource::read_frame)
            };
            match capture_result {
                Ok(Some(frame)) => {
                    match frame_to_i420(&frame).and_then(|data| {
                        resize_i420_nearest(
                            &data,
                            frame.width,
                            frame.height,
                            self.video_profile.width,
                            self.video_profile.height,
                        )
                    }) {
                        Ok(data) => i420_data = Some(data),
                        Err(error) => self.report_video_failure(&mut events, error.to_string()),
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report_video_failure(&mut events, error.to_string());
                    if self.screen_sharing {
                        self.screen_capture_source = None;
                        self.screen_sharing = false;
                    } else {
                        self.video_capture_source = None;
                        self.video_recovery.failed(now);
                    }
                }
            }

            if let Some(input) = i420_data {
                if let Ok(rgba) =
                    i420_to_rgba(&input, self.video_profile.width, self.video_profile.height)
                {
                    self.clear_video_failure(&mut events);
                    events.push(CallEngineEvent::LocalVideoFrame {
                        width: self.video_profile.width,
                        height: self.video_profile.height,
                        rgba,
                    });
                }
                let needs_vp8 = self.video_encoder.is_some()
                    && self.peers.iter().any(|(peer_id, peer)| {
                        self.should_send_media_to(peer_id)
                            && peer.connection.is_connected()
                            && peer.connection.video_codec() == crate::VideoCodec::Vp8
                    });
                let needs_h264 = self.h264_encoder.is_some()
                    && self.peers.iter().any(|(peer_id, peer)| {
                        self.should_send_media_to(peer_id)
                            && peer.connection.is_connected()
                            && peer.connection.video_codec() == crate::VideoCodec::H264
                    });

                let vp8_frame = if needs_vp8 {
                    match self.video_encoder.as_mut() {
                        Some(encoder) => match encoder.encode_frame(video_frame_duration, &input) {
                            Ok(frame) => frame,
                            Err(error) => {
                                self.video_encoder = None;
                                self.report_video_failure(&mut events, error.to_string());
                                None
                            }
                        },
                        None => None,
                    }
                } else {
                    None
                };

                let h264_frame = if needs_h264 {
                    match i420_to_nv12(&input, self.video_profile.width, self.video_profile.height)
                    {
                        Ok(nv12) => match self.h264_encoder.as_mut() {
                            Some(encoder) => match encoder.encode(video_frame_duration, &nv12) {
                                Ok(Some(EncodedH264Frame {
                                    timestamp,
                                    data,
                                    is_keyframe,
                                })) => Some(crate::EncodedVideoFrame {
                                    codec: crate::VideoCodec::H264,
                                    width: self.video_profile.width,
                                    height: self.video_profile.height,
                                    timestamp,
                                    data,
                                    is_keyframe,
                                }),
                                Ok(None) => None,
                                Err(error) => {
                                    self.h264_encoder = None;
                                    self.h264_recovery.failed(now);
                                    self.report_video_failure(&mut events, error.to_string());
                                    None
                                }
                            },
                            None => None,
                        },
                        Err(error) => {
                            self.report_video_failure(&mut events, error.to_string());
                            None
                        }
                    }
                } else {
                    None
                };

                let vp8_frame = vp8_frame.map(|frame| self.protect_video_frame(frame));
                let h264_frame = h264_frame.map(|frame| self.protect_video_frame(frame));
                for (peer_id, peer) in &self.peers {
                    if !self.should_send_media_to(peer_id) || !peer.connection.is_connected() {
                        continue;
                    }
                    let frame = match peer.connection.video_codec() {
                        crate::VideoCodec::Vp8 => vp8_frame.as_ref(),
                        crate::VideoCodec::H264 => h264_frame.as_ref(),
                    };
                    if let Some(frame) = frame {
                        // Keep the connection alive across a transient RTP
                        // write failure and retry on the next tick.
                        let _ = peer.connection.send_video(frame).await;
                    }
                }
            }

            // A missing capture frame is normal while a camera or portal is
            // warming up. The next clock tick tries again.
            self.last_video_timestamp = now;
        }

        let media_cipher = self.media_cipher.as_ref();
        for (peer_id, peer) in &mut self.peers {
            if failed_peers.iter().any(|failed| failed == peer_id) {
                continue;
            }
            for _ in 0..MAX_FRAMES_PER_TICK {
                match peer.connection.try_received_data() {
                    Ok(Some(message)) => events.push(CallEngineEvent::DataMessage {
                        peer_id: peer_id.clone(),
                        data: message.data.into_vec(),
                    }),
                    Ok(None) => break,
                    Err(_) => {
                        failed_peers.push(peer_id.clone());
                        break;
                    }
                }
            }
            for _ in 0..MAX_FRAMES_PER_TICK {
                match peer.connection.try_received_video() {
                    Ok(Some(packet)) => {
                        let track_id = packet.track_id;
                        let encrypted_frame = packet.frame;
                        let relay_frame = encrypted_frame.clone();
                        let frame = if let Some(cipher) = media_cipher {
                            let Ok(sequence) = MediaFrameCipher::sequence(&encrypted_frame.data)
                            else {
                                continue;
                            };
                            let Ok(data) = cipher.decrypt(&encrypted_frame.data, sequence) else {
                                continue;
                            };
                            EncodedVideoFrame {
                                data: data.into_boxed_slice(),
                                ..encrypted_frame
                            }
                        } else {
                            encrypted_frame
                        };
                        if self.relay_enabled {
                            relay_video.push((peer_id.clone(), relay_frame));
                        }
                        if frame.is_keyframe {
                            // A relay slot may be reused after a publisher
                            // leaves. The first keyframe starts a fresh
                            // decoder state instead of inheriting references
                            // from the previous source.
                            peer.video_decoders.remove(&track_id);
                        }
                        let decoder = peer
                            .video_decoders
                            .entry(track_id.clone())
                            .or_insert(VideoDecoder::new()?);
                        if let Ok(Some(decoded)) = decoder.decode(&frame) {
                            events.push(CallEngineEvent::RemoteVideoFrame {
                                peer_id: format!("{peer_id}/{track_id}"),
                                width: decoded.width,
                                height: decoded.height,
                                rgba: decoded.to_rgba(),
                            });
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        failed_peers.push(peer_id.clone());
                        break;
                    }
                }
            }
        }

        for (peer_id, peer) in &mut self.peers {
            if failed_peers.iter().any(|failed| failed == peer_id) {
                continue;
            }
            for _ in 0..MAX_FRAMES_PER_TICK {
                match peer.connection.try_received_audio() {
                    Ok(Some(packet)) => {
                        let encrypted_frame = packet.frame;
                        let relay_frame = encrypted_frame.clone();
                        let frame = if let Some(cipher) = media_cipher {
                            let Ok(sequence) = MediaFrameCipher::sequence(&encrypted_frame.payload)
                            else {
                                continue;
                            };
                            let Ok(payload) = cipher.decrypt(&encrypted_frame.payload, sequence)
                            else {
                                continue;
                            };
                            EncodedAudioFrame {
                                payload,
                                ..encrypted_frame
                            }
                        } else {
                            encrypted_frame
                        };
                        if self.relay_enabled {
                            relay_audio.push((peer_id.clone(), relay_frame));
                        }
                        peer.audio_jitters
                            .entry(packet.track_id)
                            .or_default()
                            .push(packet.sequence_number, frame);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        failed_peers.push(peer_id.clone());
                        break;
                    }
                }
            }
            let audio_tracks = peer.audio_jitters.keys().cloned().collect::<Vec<_>>();
            for track_id in audio_tracks {
                let Some(frame) = peer
                    .audio_jitters
                    .get_mut(&track_id)
                    .and_then(|jitter| jitter.pop_ready_at(Instant::now()))
                else {
                    continue;
                };
                let voice_decoder = peer
                    .audio_decoders
                    .entry(track_id)
                    .or_insert(VoiceDecoder::new()?);
                let decoded_samples = match frame {
                    PlayoutFrame::Packet(packet) => match voice_decoder.decode(&packet) {
                        Ok(samples) => samples,
                        Err(_) => continue,
                    },
                    PlayoutFrame::Loss { recovery_packet } => {
                        if let Some(packet) = recovery_packet {
                            match voice_decoder.decode_fec(&packet) {
                                Ok(samples) => samples,
                                Err(_) => match voice_decoder.decode_loss() {
                                    Ok(samples) => samples,
                                    Err(_) => continue,
                                },
                            }
                        } else {
                            match voice_decoder.decode_loss() {
                                Ok(samples) => samples,
                                Err(_) => continue,
                            }
                        }
                    }
                };
                if let Some(output) = self.output.as_ref()
                    && output.play(&decoded_samples).is_err()
                {
                    self.output = None;
                    self.output_recovery.failed(now);
                    if self.output_recovery.report_unavailable() {
                        events.push(CallEngineEvent::AudioOutputUnavailable);
                    }
                }
            }
        }
        if self.relay_enabled {
            for (source_id, frame) in relay_audio {
                for (target_id, peer) in &self.peers {
                    if target_id != &source_id
                        && self.should_send_media_to(target_id)
                        && peer.connection.is_connected()
                    {
                        // A full relay slot budget means this source is
                        // temporarily not subscribed on this connection; it
                        // must not tear down the subscriber.
                        let _ = peer.connection.send_relay_audio(&source_id, &frame).await;
                    }
                }
            }
            for (source_id, frame) in relay_video {
                for (target_id, peer) in &self.peers {
                    if target_id != &source_id
                        && self.should_send_media_to(target_id)
                        && peer.connection.is_connected()
                        && peer.connection.video_codec() == frame.codec
                    {
                        // Overflow or transient RTP failure is handled as a
                        // dropped relay frame; the next frame can retry.
                        let _ = peer.connection.send_relay_video(&source_id, &frame).await;
                    }
                }
            }
        }
        failed_peers.sort_unstable();
        failed_peers.dedup();
        for peer_id in failed_peers {
            if let Some(peer) = self.peers.remove(&peer_id) {
                let call_id = peer.call_id;
                let _ = peer.connection.close().await;
                for other in self.peers.values() {
                    other.connection.release_relay_source(&peer_id).await;
                }
                events.push(CallEngineEvent::PeerDisconnected { peer_id, call_id });
            }
        }
        Ok(events)
    }

    fn should_send_media_to(&self, peer_id: &str) -> bool {
        self.local_media_targets
            .as_ref()
            .is_none_or(|targets| targets.iter().any(|target| target == peer_id))
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

    fn recover_video_endpoint(&mut self, now: Instant, events: &mut Vec<CallEngineEvent>) {
        if self.screen_sharing || self.video_capture_source.is_some() {
            return;
        }
        let Some(device_id) = self.requested_video.as_deref() else {
            return;
        };
        if !self.video_recovery.ready(now) {
            return;
        }
        match VideoCaptureSource::open(device_id) {
            Ok(source) => {
                self.video_capture_source = Some(source);
                self.video_recovery.recovered();
                self.clear_video_failure(events);
            }
            Err(_) => self.video_recovery.failed(now),
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

    fn report_video_failure(&mut self, events: &mut Vec<CallEngineEvent>, reason: String) {
        if !self.video_failure_reported {
            self.video_failure_reported = true;
            events.push(CallEngineEvent::VideoUnavailable { reason });
        }
    }

    fn clear_video_failure(&mut self, events: &mut Vec<CallEngineEvent>) {
        if self.video_failure_reported {
            self.video_failure_reported = false;
            events.push(CallEngineEvent::VideoRecovered);
        }
    }
}

/// Fits an encoder profile inside the actual capture source while preserving
/// its aspect ratio. This avoids upscaling a small camera when the network is
/// healthy and prevents 4:3 sources from being stretched into a 16:9 tier.
fn constrain_video_profile(
    mut profile: VideoQualityProfile,
    source_resolution: Option<(u32, u32)>,
) -> VideoQualityProfile {
    let Some((source_width, source_height)) = source_resolution else {
        return profile;
    };
    if source_width < 2 || source_height < 2 {
        return profile;
    }

    let source_width = u64::from(source_width);
    let source_height = u64::from(source_height);
    let target_width = u64::from(profile.width.max(2));
    let target_height = u64::from(profile.height.max(2));
    let (width, height) = if source_width * target_height >= source_height * target_width {
        let width = target_width.min(source_width);
        (width, (width * source_height / source_width).max(2))
    } else {
        let height = target_height.min(source_height);
        ((height * source_width / source_height).max(2), height)
    };

    profile.width = u32::try_from(width & !1).unwrap_or(profile.width);
    profile.height = u32::try_from(height & !1).unwrap_or(profile.height);
    profile
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

    #[test]
    fn missing_requested_video_device_does_not_abort_engine_startup() {
        let engine = CallEngine::with_devices(None, None, Some("nexo-device-that-does-not-exist"));
        assert!(engine.is_ok());
    }

    #[test]
    fn video_profile_fits_capture_aspect_ratio_without_upscaling() {
        let profile = VideoQualityProfile {
            target_bitrate_kbps: 600,
            target_fps: 24,
            width: 854,
            height: 480,
        };
        let fitted = constrain_video_profile(profile, Some((640, 480)));
        assert_eq!((fitted.width, fitted.height), (640, 480));

        let low = VideoQualityProfile {
            target_bitrate_kbps: 150,
            target_fps: 15,
            width: 640,
            height: 360,
        };
        let fitted_low = constrain_video_profile(low, Some((640, 480)));
        assert_eq!((fitted_low.width, fitted_low.height), (480, 360));
    }

    #[test]
    fn video_profile_keeps_wide_source_inside_quality_tier() {
        let profile = VideoQualityProfile {
            target_bitrate_kbps: 1_200,
            target_fps: 30,
            width: 1_280,
            height: 720,
        };
        let fitted = constrain_video_profile(profile, Some((2_560, 1_440)));
        assert_eq!((fitted.width, fitted.height), (1_280, 720));
    }
}
