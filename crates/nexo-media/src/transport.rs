use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc as sync_mpsc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use if_addrs::get_if_addrs;
use rtc::{
    media::Sample,
    media_stream::MediaStreamTrack,
    peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8},
    peer_connection::transport::RTCIceServer,
    rtp::codec::h264::H264Packet,
    rtp::codec::vp8::Vp8Packet,
    rtp::packetizer::Depacketizer,
    rtp_transceiver::rtp_sender::{
        RTCPFeedback, RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
    },
};
use thiserror::Error;
use tokio::{
    sync::{Mutex, mpsc, watch},
    time::timeout,
};
use uuid::Uuid;
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent, RTCDataChannelState},
    media_stream::{
        track_local::{TrackLocal, static_sample::TrackLocalStaticSample},
        track_remote::{TrackRemote, TrackRemoteEvent},
    },
    peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfigurationBuilder, RTCIceGatheringState, RTCPeerConnectionState,
        RTCSessionDescription,
    },
    rtp_transceiver::RtpSender,
};

use crate::{
    EncodedAudioFrame, EncodedVideoFrame, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE,
    ReceivedVideoPacket, VideoCodec,
};
use nexo_core::NatConfig;

const OPUS_PAYLOAD_TYPE: u8 = 111;
const VP8_PAYLOAD_TYPE: u8 = 96;
const H264_PAYLOAD_TYPE: u8 = 102;
const VIDEO_CLOCK_RATE: u32 = 90_000;
const VIDEO_FRAME_DURATION: Duration = Duration::from_millis(33);
/// How often (in seconds) to probe/adjust the video bitrate based on RTCP feedback.
const VIDEO_BITRATE_ADJUST_INTERVAL: u64 = 2;
/// Pre-negotiated video slots avoid renegotiation when a participant joins a
/// call. Slot zero is reserved for the local publisher on direct links;
/// relay sources use the remaining slots.
pub(crate) const VIDEO_TRACK_SLOTS: usize = 16;
/// Pre-negotiated audio slots keep relayed publishers on independent Opus
/// SSRCs and tracks, so each source can have its own jitter buffer.
pub(crate) const AUDIO_TRACK_SLOTS: usize = 16;
/// Minimum allowed bitrate in kbps for the VP8 encoder.
const MIN_VIDEO_BITRATE_KBPS: u32 = 500;
/// Maximum allowed allowed bitrate in kbps for the VP8 encoder.
const MAX_VIDEO_BITRATE_KBPS: u32 = 5_000;

/// Estimates the available outgoing video bandwidth from RTCP GOOG-RMB packets.
///
/// This is a simple exponential moving average filter. In a real deployment
/// one would also factor in CPU usage, encode time, and other constraints.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct VideoBitrateEstimator {
    /// EMA coefficient: α = 2 / (`α_steps` + 1), larger → more smoothing
    ema_alpha: f64,
    /// EMA steps controls smoothing; 3 gives ~33% weight to newest sample
    ema_steps: u32,
    /// Last estimated bitrate in kbps, based on EMA of RTCP RMB reports
    estimated_bps: u32,
    /// Last time we updated the EMA
    last_update: Instant,
}

impl VideoBitrateEstimator {
    /// Create a new estimator with the given EMA smoothing steps.
    pub fn new(ema_steps: u32) -> Self {
        Self {
            ema_alpha: 2.0 / (f64::from(ema_steps) + 1.0),
            ema_steps,
            estimated_bps: 2_000_000,
            last_update: Instant::now(),
        }
    }

    /// Feed a new RTCP RMB report sample (bytes per second) and return the
    /// current estimated bitrate in bps.
    ///
    /// The RTCP RMB `bitrate` field is already in bits/s per the RFC.
    #[allow(dead_code, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn update(&mut self, new_bps: u32) {
        let now = Instant::now();
        if self.last_update.elapsed().as_secs() >= VIDEO_BITRATE_ADJUST_INTERVAL {
            // Exponential moving average:  EMA_new = α * new + (1 - α) * EMA_old
            let val = (self.ema_alpha * f64::from(new_bps))
                + ((1.0 - self.ema_alpha) * f64::from(self.estimated_bps)).max(0.0);
            self.estimated_bps = val as u32;
            self.last_update = now;
        }
    }

    /// Return the current estimated bitrate in bps (clamped to [MIN..MAX]).
    #[must_use]
    pub fn estimated_bps(&self) -> u32 {
        self.estimated_bps
    }
}
const MEDIA_EVENT_CAPACITY: usize = 32;
const VIDEO_EVENT_CAPACITY: usize = 8;
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(8);
const AUDIO_FRAME_DURATION: Duration = Duration::from_millis(20);
/// WebRTC data-channel messages are deliberately kept below the common SCTP
/// message limit. File-transfer code fragments larger payloads above this
/// layer and therefore cannot exhaust the receive queue with one message.
pub const DATA_CHANNEL_MESSAGE_BYTES: usize = 12 * 1024;
const DATA_CHANNEL_LABEL: &str = "nexo-control";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedAudioPacket {
    pub sequence_number: u16,
    /// Stable negotiated track id. Relay sources use one audio slot each.
    pub track_id: String,
    pub frame: EncodedAudioFrame,
}

/// One bounded message received on the authenticated WebRTC data channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedDataMessage {
    pub data: Box<[u8]>,
}

#[derive(Debug, Error)]
pub enum PeerConnectionError {
    #[error("the WebRTC media engine could not initialize: {0}")]
    MediaEngine(String),
    #[error("the WebRTC peer connection could not initialize: {0}")]
    Connection(String),
    #[error("the WebRTC offer could not be created: {0}")]
    Offer(String),
    #[error("the WebRTC audio track failed: {0}")]
    AudioTrack(String),
    #[error("the WebRTC video track failed: {0}")]
    VideoTrack(String),
    #[error("timed out while waiting for WebRTC {0}")]
    Timeout(&'static str),
}

#[derive(Clone)]
struct EventHandler {
    gathering_events: mpsc::Sender<()>,
    connection_state: watch::Sender<ConnectionStateEvent>,
    inbound_audio: sync_mpsc::SyncSender<ReceivedAudioPacket>,
    inbound_video: sync_mpsc::SyncSender<ReceivedVideoPacket>,
    inbound_data: mpsc::Sender<ReceivedDataMessage>,
    connected: Arc<AtomicBool>,
    /// RTCP bandwidth estimator, updated from remote receiver reports.
    video_bitrate_estimator: Arc<StdMutex<VideoBitrateEstimator>>,
    data_channel: Arc<StdMutex<Option<Arc<dyn DataChannel>>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionStateEvent {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

fn connection_state_event(state: RTCPeerConnectionState) -> Option<ConnectionStateEvent> {
    match state {
        RTCPeerConnectionState::New => Some(ConnectionStateEvent::New),
        RTCPeerConnectionState::Connecting => Some(ConnectionStateEvent::Connecting),
        RTCPeerConnectionState::Connected => Some(ConnectionStateEvent::Connected),
        RTCPeerConnectionState::Disconnected => Some(ConnectionStateEvent::Disconnected),
        RTCPeerConnectionState::Failed => Some(ConnectionStateEvent::Failed),
        RTCPeerConnectionState::Closed => Some(ConnectionStateEvent::Closed),
        _ => None,
    }
}

fn spawn_data_channel_reader(
    data_channel: Arc<dyn DataChannel>,
    sender: mpsc::Sender<ReceivedDataMessage>,
) {
    tokio::spawn(async move {
        while let Some(event) = data_channel.poll().await {
            match event {
                DataChannelEvent::OnMessage(message)
                    if message.data.len() <= DATA_CHANNEL_MESSAGE_BYTES =>
                {
                    if sender
                        .send(ReceivedDataMessage {
                            data: message.data.to_vec().into_boxed_slice(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                DataChannelEvent::OnClose => break,
                _ => {}
            }
        }
    });
}

#[async_trait]
impl PeerConnectionEventHandler for EventHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathering_events.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let is_connected = state == RTCPeerConnectionState::Connected;
        self.connected.store(is_connected, Ordering::Release);
        if let Some(event) = connection_state_event(state) {
            let _ = self.connection_state.send(event);
        }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let Ok(label) = data_channel.label().await else {
            return;
        };
        if label != DATA_CHANNEL_LABEL {
            return;
        }
        if let Ok(mut stored) = self.data_channel.lock() {
            *stored = Some(Arc::clone(&data_channel));
        }
        spawn_data_channel_reader(data_channel, self.inbound_data.clone());
    }

    #[allow(clippy::too_many_lines)]
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        match track.kind().await {
            RtpCodecKind::Audio => {
                let sender = self.inbound_audio.clone();
                let track_id = track.track_id().await;
                tokio::spawn(async move {
                    while let Some(event) = track.poll().await {
                        match event {
                            TrackRemoteEvent::OnRtpPacket(packet) => {
                                let frame = ReceivedAudioPacket {
                                    sequence_number: packet.header.sequence_number,
                                    track_id: track_id.clone(),
                                    frame: EncodedAudioFrame {
                                        payload: packet.payload.to_vec(),
                                        sample_count: OPUS_FRAME_SAMPLES,
                                        sample_rate: OPUS_SAMPLE_RATE,
                                    },
                                };
                                let _ = sender.try_send(frame);
                            }
                            TrackRemoteEvent::OnEnded => break,
                            _ => {}
                        }
                    }
                });
            }
            RtpCodecKind::Video => {
                let sender = self.inbound_video.clone();
                let estimator = Arc::clone(&self.video_bitrate_estimator);
                let track_id = track.track_id().await;
                let codec = if let Some(ssrc) = track.ssrcs().await.into_iter().next() {
                    track.codec(ssrc).await.and_then(|codec| {
                        codec
                            .mime_type
                            .eq_ignore_ascii_case(MIME_TYPE_H264)
                            .then_some(VideoCodec::H264)
                    })
                } else {
                    None
                };
                tokio::spawn(async move {
                    let mut depacketizer = if codec == Some(VideoCodec::H264) {
                        VideoDepacketizer::H264(H264Packet::default())
                    } else {
                        VideoDepacketizer::Vp8(Vp8Packet::default())
                    };
                    let mut access_unit = Vec::new();
                    let mut dimensions = None;
                    while let Some(event) = track.poll().await {
                        match event {
                            TrackRemoteEvent::OnRtpPacket(packet) => {
                                if let Ok(chunk) = depacketizer.depacketize(&packet.payload) {
                                    access_unit.extend_from_slice(&chunk);
                                }
                                if packet.header.marker {
                                    let sequence_number = packet.header.sequence_number;
                                    let timestamp = Duration::from_micros(
                                        u64::from(packet.header.timestamp) * 1_000_000
                                            / u64::from(VIDEO_CLOCK_RATE),
                                    );
                                    let codec = depacketizer.codec();
                                    let data = std::mem::take(&mut access_unit);
                                    let is_keyframe = match codec {
                                        VideoCodec::Vp8 => vp8_access_unit_is_keyframe(&data),
                                        VideoCodec::H264 => h264_access_unit_contains_idr(&data),
                                    };
                                    if codec == VideoCodec::Vp8 {
                                        dimensions = vp8_dimensions(&data).or(dimensions);
                                    }
                                    let (width, height) = dimensions.unwrap_or_default();
                                    let packet = ReceivedVideoPacket {
                                        sequence_number,
                                        track_id: track_id.clone(),
                                        frame: EncodedVideoFrame {
                                            codec,
                                            width,
                                            height,
                                            timestamp,
                                            data: data.into_boxed_slice(),
                                            is_keyframe,
                                        },
                                    };
                                    let _ = sender.try_send(packet);
                                }
                            }
                            TrackRemoteEvent::OnRtcpPacket(packet) => {
                                for rtcp_packet in packet {
                                    let Some(remb) = rtcp_packet.as_any().downcast_ref::<
                                        rtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate,
                                    >() else {
                                        continue;
                                    };
                                    if !remb.bitrate.is_finite() || remb.bitrate <= 0.0 {
                                        continue;
                                    }
                                    #[allow(
                                        clippy::cast_possible_truncation,
                                        clippy::cast_sign_loss
                                    )]
                                    let bitrate = remb.bitrate as u32;
                                    if let Ok(mut shared_estimator) = estimator.lock() {
                                        shared_estimator.update(bitrate);
                                    }
                                }
                            }
                            TrackRemoteEvent::OnEnded => break,
                            _ => {}
                        }
                    }
                });
            }
            _ => {}
        }
    }
}

enum VideoDepacketizer {
    Vp8(Vp8Packet),
    H264(H264Packet),
}

impl VideoDepacketizer {
    fn codec(&self) -> VideoCodec {
        match self {
            Self::Vp8(_) => VideoCodec::Vp8,
            Self::H264(_) => VideoCodec::H264,
        }
    }
}

impl Depacketizer for VideoDepacketizer {
    fn depacketize(&mut self, payload: &Bytes) -> rtc::shared::error::Result<Bytes> {
        match self {
            Self::Vp8(depacketizer) => depacketizer.depacketize(payload),
            Self::H264(depacketizer) => depacketizer.depacketize(payload),
        }
    }

    fn is_partition_head(&self, payload: &Bytes) -> bool {
        match self {
            Self::Vp8(depacketizer) => depacketizer.is_partition_head(payload),
            Self::H264(depacketizer) => depacketizer.is_partition_head(payload),
        }
    }

    fn is_partition_tail(&self, marker: bool, payload: &Bytes) -> bool {
        match self {
            Self::Vp8(depacketizer) => depacketizer.is_partition_tail(marker, payload),
            Self::H264(depacketizer) => depacketizer.is_partition_tail(marker, payload),
        }
    }
}

pub struct LanPeerConnection {
    inner: Box<dyn PeerConnection>,
    audio_tracks: Vec<Arc<TrackLocalStaticSample>>,
    audio_ssrcs: Vec<u32>,
    video_tracks: Vec<Arc<TrackLocalStaticSample>>,
    video_ssrcs: Vec<u32>,
    video_senders: Vec<Arc<dyn RtpSender>>,
    gathering_receiver: Mutex<mpsc::Receiver<()>>,
    connection_state_receiver: Mutex<watch::Receiver<ConnectionStateEvent>>,
    audio_receiver: sync_mpsc::Receiver<ReceivedAudioPacket>,
    video_receiver: sync_mpsc::Receiver<ReceivedVideoPacket>,
    data_receiver: mpsc::Receiver<ReceivedDataMessage>,
    last_video_timestamp_micros: AtomicU64,
    connected: Arc<AtomicBool>,
    video_bitrate_estimator: Arc<StdMutex<VideoBitrateEstimator>>,
    current_max_bitrate: Arc<AtomicU32>,
    data_channel: Arc<StdMutex<Option<Arc<dyn DataChannel>>>>,
    data_sender: mpsc::Sender<ReceivedDataMessage>,
    video_codec: VideoCodec,
    relay_audio_slots: Mutex<RelaySlotState>,
    relay_video_slots: Mutex<RelaySlotState>,
}

struct RelaySlotState {
    assignments: HashMap<String, usize>,
    free_slots: Vec<usize>,
}

impl LanPeerConnection {
    pub async fn new() -> Result<Self, PeerConnectionError> {
        Self::new_with_video_codec(VideoCodec::Vp8).await
    }

    /// Create a connection with an explicit negotiated video codec. VP8 is
    /// still the default; H.264 is selected only when the caller has a native
    /// encoder and can keep the matching decoder path available.
    #[allow(clippy::too_many_lines)]
    pub async fn new_with_video_codec(
        video_codec: VideoCodec,
    ) -> Result<Self, PeerConnectionError> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .map_err(|error| PeerConnectionError::MediaEngine(error.to_string()))?;
        let (gathering_sender, gathering_receiver) = mpsc::channel(2);
        let (connection_state_sender, connection_state_receiver) =
            watch::channel(ConnectionStateEvent::New);
        let (audio_sender, audio_receiver) = sync_mpsc::sync_channel(MEDIA_EVENT_CAPACITY);
        let (video_sender, video_receiver) = sync_mpsc::sync_channel(VIDEO_EVENT_CAPACITY);
        let (data_sender, data_receiver) = mpsc::channel(MEDIA_EVENT_CAPACITY);
        let connected = Arc::new(AtomicBool::new(false));
        let video_bitrate_estimator = Arc::new(StdMutex::new(VideoBitrateEstimator::new(3)));
        let current_max_bitrate = Arc::new(AtomicU32::new(2_000_000));
        let data_channel = Arc::new(StdMutex::new(None));
        let configuration = RTCConfigurationBuilder::new()
            .with_ice_servers(nat_ice_servers())
            .build();
        let udp_addresses = local_udp_addresses()?;
        let inner = Box::pin(
            PeerConnectionBuilder::new()
                .with_configuration(configuration)
                .with_media_engine(media_engine)
                .with_handler(Arc::new(EventHandler {
                    gathering_events: gathering_sender,
                    connection_state: connection_state_sender,
                    inbound_audio: audio_sender,
                    inbound_video: video_sender,
                    inbound_data: data_sender.clone(),
                    connected: Arc::clone(&connected),
                    video_bitrate_estimator: Arc::clone(&video_bitrate_estimator),
                    data_channel: Arc::clone(&data_channel),
                }))
                .with_data_channel_send_buffer_limit(4 * 1024 * 1024)
                .with_udp_addrs(udp_addresses)
                .build(),
        )
        .await
        .map_err(|error| PeerConnectionError::Connection(error.to_string()))?;
        let mut audio_tracks = Vec::with_capacity(AUDIO_TRACK_SLOTS);
        let mut audio_ssrcs = Vec::with_capacity(AUDIO_TRACK_SLOTS);
        for slot in 0..AUDIO_TRACK_SLOTS {
            let audio_ssrc = random_ssrc();
            let audio_track = Arc::new(
                TrackLocalStaticSample::new(Instant::now(), opus_track(audio_ssrc, slot))
                    .map_err(|error| PeerConnectionError::AudioTrack(error.to_string()))?,
            );
            inner
                .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
                .await
                .map_err(|error| PeerConnectionError::AudioTrack(error.to_string()))?;
            audio_tracks.push(audio_track);
            audio_ssrcs.push(audio_ssrc);
        }
        let mut video_tracks = Vec::with_capacity(VIDEO_TRACK_SLOTS);
        let mut video_ssrcs = Vec::with_capacity(VIDEO_TRACK_SLOTS);
        let mut video_senders = Vec::with_capacity(VIDEO_TRACK_SLOTS);
        for slot in 0..VIDEO_TRACK_SLOTS {
            let video_ssrc = random_ssrc();
            let video_track_description = video_track(
                video_codec,
                video_ssrc,
                current_max_bitrate.load(Ordering::Relaxed),
                slot,
            );
            let video_track = Arc::new(
                TrackLocalStaticSample::new(Instant::now(), video_track_description)
                    .map_err(|error| PeerConnectionError::VideoTrack(error.to_string()))?,
            );
            let sender = inner
                .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal>)
                .await
                .map_err(|error| PeerConnectionError::VideoTrack(error.to_string()))?;
            video_tracks.push(video_track);
            video_ssrcs.push(video_ssrc);
            video_senders.push(sender);
        }
        let connection = Self {
            inner: Box::new(inner),
            audio_tracks,
            audio_ssrcs,
            video_tracks,
            video_ssrcs,
            video_senders,
            gathering_receiver: Mutex::new(gathering_receiver),
            connection_state_receiver: Mutex::new(connection_state_receiver),
            audio_receiver,
            video_receiver,
            data_receiver,
            last_video_timestamp_micros: AtomicU64::new(0),
            connected,
            video_bitrate_estimator,
            current_max_bitrate,
            data_channel,
            data_sender,
            video_codec,
            relay_audio_slots: Mutex::new(RelaySlotState {
                assignments: HashMap::new(),
                free_slots: (1..AUDIO_TRACK_SLOTS).rev().collect(),
            }),
            relay_video_slots: Mutex::new(RelaySlotState {
                assignments: HashMap::new(),
                free_slots: (1..VIDEO_TRACK_SLOTS).rev().collect(),
            }),
        };
        connection.start_bitrate_monitoring();
        Ok(connection)
    }

    pub async fn create_offer(&self) -> Result<String, PeerConnectionError> {
        let data_channel = self
            .inner
            .create_data_channel("nexo-control", None)
            .await
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
        if let Ok(mut stored) = self.data_channel.lock() {
            *stored = Some(Arc::clone(&data_channel));
        }
        spawn_data_channel_reader(data_channel, self.data_sender.clone());
        let offer = self
            .inner
            .create_offer(None)
            .await
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
        self.inner
            .set_local_description(offer)
            .await
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
        self.wait_for_gathering().await?;
        self.local_sdp().await
    }

    pub async fn accept_offer(&self, sdp: String) -> Result<String, PeerConnectionError> {
        let offer = RTCSessionDescription::offer(sdp)
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
        self.inner
            .set_remote_description(offer)
            .await
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
        let answer = self
            .inner
            .create_answer(None)
            .await
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
        self.inner
            .set_local_description(answer)
            .await
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
        self.wait_for_gathering().await?;
        self.local_sdp().await
    }

    pub async fn accept_answer(&self, sdp: String) -> Result<(), PeerConnectionError> {
        let answer = RTCSessionDescription::answer(sdp)
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
        self.inner
            .set_remote_description(answer)
            .await
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))
    }

    pub async fn wait_until_connected(&self) -> Result<(), PeerConnectionError> {
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut receiver = self.connection_state_receiver.lock().await;
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }
        timeout(NEGOTIATION_TIMEOUT, async {
            loop {
                match *receiver.borrow() {
                    ConnectionStateEvent::Connected => return Ok(()),
                    ConnectionStateEvent::Disconnected => {
                        return Err(PeerConnectionError::Connection(
                            "ICE/DTLS desconectou antes da chamada iniciar; verifique NAT e firewall"
                                .to_owned(),
                        ));
                    }
                    ConnectionStateEvent::Failed => {
                        return Err(PeerConnectionError::Connection(
                            "ICE/DTLS falhou; verifique servidores STUN/TURN, NAT e firewall"
                                .to_owned(),
                        ));
                    }
                    ConnectionStateEvent::Closed => {
                        return Err(PeerConnectionError::Connection(
                            "a conexao WebRTC foi fechada antes de conectar".to_owned(),
                        ));
                    }
                    ConnectionStateEvent::New | ConnectionStateEvent::Connecting => {}
                }
                receiver.changed().await.map_err(|_| {
                    PeerConnectionError::Connection(
                        "o fluxo de estado da conexao WebRTC foi encerrado".to_owned(),
                    )
                })?;
            }
        })
        .await
        .map_err(|_| PeerConnectionError::Timeout("connection"))?
    }

    /// Send one bounded binary message over the negotiated WebRTC data
    /// channel. The caller owns fragmentation and should use
    /// [`DATA_CHANNEL_MESSAGE_BYTES`] as the maximum payload size.
    pub async fn send_data(&self, data: &[u8]) -> Result<(), PeerConnectionError> {
        if data.is_empty() || data.len() > DATA_CHANNEL_MESSAGE_BYTES {
            return Err(PeerConnectionError::Connection(format!(
                "mensagem de dados fora do limite de {DATA_CHANNEL_MESSAGE_BYTES} bytes"
            )));
        }
        let channel = self
            .data_channel
            .lock()
            .ok()
            .and_then(|stored| stored.clone())
            .ok_or_else(|| {
                PeerConnectionError::Connection(
                    "canal de dados WebRTC ainda nao esta disponivel".to_owned(),
                )
            })?;
        let deadline = Instant::now() + NEGOTIATION_TIMEOUT;
        loop {
            match channel
                .ready_state()
                .await
                .map_err(|error| PeerConnectionError::Connection(error.to_string()))?
            {
                RTCDataChannelState::Open => break,
                RTCDataChannelState::Closing | RTCDataChannelState::Closed => {
                    return Err(PeerConnectionError::Connection(
                        "canal de dados WebRTC foi encerrado".to_owned(),
                    ));
                }
                _ if Instant::now() >= deadline => {
                    return Err(PeerConnectionError::Timeout("data channel"));
                }
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        channel
            .send(bytes::BytesMut::from(data))
            .await
            .map_err(|error| PeerConnectionError::Connection(error.to_string()))
    }

    /// Pull the next bounded data-channel message without blocking the media
    /// loop.
    pub fn try_received_data(
        &mut self,
    ) -> Result<Option<ReceivedDataMessage>, PeerConnectionError> {
        match self.data_receiver.try_recv() {
            Ok(message) => Ok(Some(message)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(PeerConnectionError::Connection(
                "canal de dados WebRTC encerrou inesperadamente".to_owned(),
            )),
        }
    }

    pub async fn send_audio(&self, frame: &EncodedAudioFrame) -> Result<(), PeerConnectionError> {
        if frame.sample_count != OPUS_FRAME_SAMPLES || frame.sample_rate != OPUS_SAMPLE_RATE {
            return Err(PeerConnectionError::AudioTrack(
                "audio frame must represent 20 ms at 48 kHz".to_owned(),
            ));
        }
        self.send_audio_on(0, frame).await
    }

    /// Send an audio frame through a pre-negotiated publisher slot.
    pub async fn send_audio_on(
        &self,
        slot: usize,
        frame: &EncodedAudioFrame,
    ) -> Result<(), PeerConnectionError> {
        if frame.sample_count != OPUS_FRAME_SAMPLES || frame.sample_rate != OPUS_SAMPLE_RATE {
            return Err(PeerConnectionError::AudioTrack(
                "audio frame must represent 20 ms at 48 kHz".to_owned(),
            ));
        }
        let Some(audio_track) = self.audio_tracks.get(slot) else {
            return Err(PeerConnectionError::AudioTrack(format!(
                "audio slot {slot} is outside the negotiated range"
            )));
        };
        let Some(audio_ssrc) = self.audio_ssrcs.get(slot).copied() else {
            return Err(PeerConnectionError::AudioTrack(format!(
                "audio SSRC for slot {slot} is missing"
            )));
        };
        let now = Instant::now();
        let sample = Sample {
            data: Bytes::copy_from_slice(&frame.payload),
            duration: AUDIO_FRAME_DURATION,
            ..Sample::new(now)
        };
        audio_track
            .write_sample(audio_ssrc, OPUS_PAYLOAD_TYPE, &sample, &[])
            .await
            .map_err(|error| PeerConnectionError::AudioTrack(error.to_string()))
    }

    /// Send a relayed audio source through a unique slot on this connection.
    pub async fn send_relay_audio(
        &self,
        source_id: &str,
        frame: &EncodedAudioFrame,
    ) -> Result<(), PeerConnectionError> {
        let slot = {
            let mut state = self.relay_audio_slots.lock().await;
            if let Some(slot) = state.assignments.get(source_id).copied() {
                slot
            } else {
                let slot = state.free_slots.pop().ok_or_else(|| {
                    PeerConnectionError::AudioTrack(
                        "não há slots de áudio relay disponíveis nesta conexão".to_owned(),
                    )
                })?;
                state.assignments.insert(source_id.to_owned(), slot);
                slot
            }
        };
        self.send_audio_on(slot, frame).await
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    #[must_use]
    pub const fn video_codec(&self) -> VideoCodec {
        self.video_codec
    }

    /// Updates the maximum video bitrate based on the latest RTCP RMB estimate.
    ///
    /// Clamps the new bitrate to `[MIN_VIDEO_BITRATE_KBPS..MAX_VIDEO_BITRATE_KBPS]`
    /// and updates `self.current_max_bitrate` so that `vp8_video_track`'s
    /// encoding parameters stay in sync.
    pub fn update_video_bitrate(&self) {
        let estimated = self
            .video_bitrate_estimator
            .lock()
            .map_or(2_000_000, |estimator| estimator.estimated_bps());
        // Clamp to allowed range [500 kbps .. 5 Mbps].
        let clamped = estimated.clamp(
            MIN_VIDEO_BITRATE_KBPS * 1_000,
            MAX_VIDEO_BITRATE_KBPS * 1_000,
        );
        self.current_max_bitrate.store(clamped, Ordering::Relaxed);
    }

    /// Returns the latest receiver-side REMB estimate used by adaptive video
    /// quality. The conservative default keeps a fresh connection usable
    /// before the first RTCP report arrives.
    #[must_use]
    pub fn estimated_video_bitrate_bps(&self) -> u32 {
        self.video_bitrate_estimator
            .lock()
            .map_or(2_000_000, |estimator| estimator.estimated_bps())
    }

    /// Starts a background task that periodically queries WebRTC stats
    /// (including RTCP GOOG-RMB bandwidth estimates) and adjusts the
    /// video bitrate accordingly. Must be called after `new()` resolves.
    ///
    /// The task runs every `VIDEO_BITRATE_ADJUST_INTERVAL` seconds and
    /// clamps the bitrate to `[MIN_VIDEO_BITRATE_KBPS..MAX_VIDEO_BITRATE_KBPS]`.
    pub fn start_bitrate_monitoring(&self) {
        let estimator = Arc::clone(&self.video_bitrate_estimator);
        let max_bitrate = Arc::clone(&self.current_max_bitrate);
        let senders = self.video_senders.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(VIDEO_BITRATE_ADJUST_INTERVAL));
            loop {
                interval.tick().await;
                let estimated = estimator
                    .lock()
                    .map_or(2_000_000, |estimator| estimator.estimated_bps());
                let clamped = estimated.clamp(
                    MIN_VIDEO_BITRATE_KBPS * 1_000,
                    MAX_VIDEO_BITRATE_KBPS * 1_000,
                );
                max_bitrate.store(clamped, Ordering::Relaxed);
                for sender in &senders {
                    let Ok(mut parameters) = sender.get_parameters().await else {
                        continue;
                    };
                    for encoding in &mut parameters.encodings {
                        encoding.max_bitrate = clamped;
                    }
                    let _ = sender.set_parameters(parameters, None).await;
                }
            }
        });
    }

    pub fn try_received_audio(&self) -> Result<Option<ReceivedAudioPacket>, PeerConnectionError> {
        match self.audio_receiver.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(sync_mpsc::TryRecvError::Empty) => Ok(None),
            Err(sync_mpsc::TryRecvError::Disconnected) => Err(PeerConnectionError::Connection(
                "remote audio stream stopped unexpectedly".to_owned(),
            )),
        }
    }

    /// Packetizes one raw VP8 frame and sends it on the video track. The RTP
    /// timestamp advances by the frame's own delta from the previous frame
    /// (falling back to a nominal 33 ms interval), so callers only need to
    /// supply monotonic timestamps.
    pub async fn send_video(&self, frame: &EncodedVideoFrame) -> Result<(), PeerConnectionError> {
        self.send_video_on(0, frame).await
    }

    /// Send a frame through a pre-negotiated publisher slot.
    pub async fn send_video_on(
        &self,
        slot: usize,
        frame: &EncodedVideoFrame,
    ) -> Result<(), PeerConnectionError> {
        if frame.codec != self.video_codec {
            return Err(PeerConnectionError::VideoTrack(format!(
                "frame codec {:?} does not match the negotiated {:?}",
                frame.codec, self.video_codec
            )));
        }
        let micros = u64::try_from(frame.timestamp.as_micros()).unwrap_or(u64::MAX);
        let last = self.last_video_timestamp_micros.load(Ordering::Relaxed);
        let duration = if last != 0 && micros > last {
            Duration::from_micros(micros - last)
        } else {
            VIDEO_FRAME_DURATION
        };
        self.last_video_timestamp_micros
            .store(micros, Ordering::Relaxed);
        let now = Instant::now();
        let sample = Sample {
            data: Bytes::copy_from_slice(&frame.data),
            duration,
            ..Sample::new(now)
        };
        let payload_type = match self.video_codec {
            VideoCodec::Vp8 => VP8_PAYLOAD_TYPE,
            VideoCodec::H264 => H264_PAYLOAD_TYPE,
        };
        let Some(video_track) = self.video_tracks.get(slot) else {
            return Err(PeerConnectionError::VideoTrack(format!(
                "video slot {slot} is outside the negotiated range"
            )));
        };
        let Some(video_ssrc) = self.video_ssrcs.get(slot).copied() else {
            return Err(PeerConnectionError::VideoTrack(format!(
                "video SSRC for slot {slot} is missing"
            )));
        };
        video_track
            .write_sample(video_ssrc, payload_type, &sample, &[])
            .await
            .map_err(|error| PeerConnectionError::VideoTrack(error.to_string()))
    }

    /// Send a relayed source through a unique slot on this connection.
    /// Allocation is local to the connection because each subscriber only
    /// needs a stable track identity for the relay it is currently attached
    /// to.
    pub async fn send_relay_video(
        &self,
        source_id: &str,
        frame: &EncodedVideoFrame,
    ) -> Result<(), PeerConnectionError> {
        let slot = {
            let mut state = self.relay_video_slots.lock().await;
            if let Some(slot) = state.assignments.get(source_id).copied() {
                slot
            } else {
                let slot = state.free_slots.pop().ok_or_else(|| {
                    PeerConnectionError::VideoTrack(
                        "não há slots de vídeo relay disponíveis nesta conexão".to_owned(),
                    )
                })?;
                state.assignments.insert(source_id.to_owned(), slot);
                slot
            }
        };
        self.send_video_on(slot, frame).await
    }

    /// Release a relay slot after its source leaves the call.
    pub async fn release_relay_source(&self, source_id: &str) {
        let mut state = self.relay_video_slots.lock().await;
        if let Some(slot) = state.assignments.remove(source_id) {
            state.free_slots.push(slot);
        }
        drop(state);
        self.release_relay_audio_source(source_id).await;
    }

    /// Release a relayed audio source after it leaves the call.
    pub async fn release_relay_audio_source(&self, source_id: &str) {
        let mut state = self.relay_audio_slots.lock().await;
        if let Some(slot) = state.assignments.remove(source_id) {
            state.free_slots.push(slot);
        }
    }

    pub fn try_received_video(&self) -> Result<Option<ReceivedVideoPacket>, PeerConnectionError> {
        match self.video_receiver.try_recv() {
            Ok(packet) => Ok(Some(packet)),
            Err(sync_mpsc::TryRecvError::Empty) => Ok(None),
            Err(sync_mpsc::TryRecvError::Disconnected) => Err(PeerConnectionError::Connection(
                "remote video stream stopped unexpectedly".to_owned(),
            )),
        }
    }

    pub async fn close(&self) -> Result<(), PeerConnectionError> {
        self.inner
            .close()
            .await
            .map_err(|error| PeerConnectionError::Connection(error.to_string()))
    }

    async fn wait_for_gathering(&self) -> Result<(), PeerConnectionError> {
        let mut receiver = self.gathering_receiver.lock().await;
        timeout(NEGOTIATION_TIMEOUT, receiver.recv())
            .await
            .map_err(|_| PeerConnectionError::Timeout("ICE candidate gathering"))?
            .ok_or_else(|| {
                PeerConnectionError::Connection(
                    "ICE gathering event stream stopped unexpectedly".to_owned(),
                )
            })
    }

    async fn local_sdp(&self) -> Result<String, PeerConnectionError> {
        self.inner
            .local_description()
            .await
            .map(|description| description.sdp)
            .ok_or_else(|| PeerConnectionError::Offer("local description is missing".to_owned()))
    }
}

fn opus_track(ssrc: u32, slot: usize) -> MediaStreamTrack {
    MediaStreamTrack::new(
        "nexo-call".to_owned(),
        format!("nexo-audio-{slot}"),
        format!("Audio slot {slot}"),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            active: true,
            codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: OPUS_SAMPLE_RATE,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            max_bitrate: 32_000,
            ..Default::default()
        }],
    )
}

fn video_track(
    video_codec: VideoCodec,
    ssrc: u32,
    max_bitrate: u32,
    slot: usize,
) -> MediaStreamTrack {
    match video_codec {
        VideoCodec::Vp8 => vp8_video_track(ssrc, max_bitrate, slot),
        VideoCodec::H264 => h264_video_track(ssrc, max_bitrate, slot),
    }
}

fn vp8_video_track(ssrc: u32, max_bitrate: u32, slot: usize) -> MediaStreamTrack {
    MediaStreamTrack::new(
        "nexo-call".to_owned(),
        format!("nexo-video-{slot}"),
        format!("Video slot {slot}"),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            active: true,
            codec: RTCRtpCodec {
                mime_type: MIME_TYPE_VP8.to_owned(),
                clock_rate: VIDEO_CLOCK_RATE,
                channels: 0,
                // Matches the media engine's VP8 registration so SDP
                // negotiation picks the same payload type on both ends.
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![
                    RTCPFeedback {
                        typ: "goog-remb".to_owned(),
                        parameter: String::new(),
                    },
                    RTCPFeedback {
                        typ: "ccm".to_owned(),
                        parameter: "fir".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: String::new(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "pli".to_owned(),
                    },
                ],
            },
            //  Bitrate will be dynamically adjusted by the caller via the estimator.
            max_bitrate,
            ..Default::default()
        }],
    )
}

fn h264_video_track(ssrc: u32, max_bitrate: u32, slot: usize) -> MediaStreamTrack {
    MediaStreamTrack::new(
        "nexo-call".to_owned(),
        format!("nexo-video-{slot}"),
        format!("Video slot {slot}"),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            active: true,
            codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: VIDEO_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
                        .to_owned(),
                rtcp_feedback: vec![
                    RTCPFeedback {
                        typ: "goog-remb".to_owned(),
                        parameter: String::new(),
                    },
                    RTCPFeedback {
                        typ: "ccm".to_owned(),
                        parameter: "fir".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: String::new(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "pli".to_owned(),
                    },
                ],
            },
            max_bitrate,
            ..Default::default()
        }],
    )
}

fn random_ssrc() -> u32 {
    let bytes = Uuid::new_v4().into_bytes();
    let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    value.max(1)
}

fn vp8_access_unit_is_keyframe(data: &[u8]) -> bool {
    data.first().is_some_and(|byte| byte & 1 == 0)
}

fn vp8_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 10 || !vp8_access_unit_is_keyframe(data) || data[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let width = u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff);
    let height = u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff);
    (width > 0 && height > 0).then_some((width, height))
}

fn h264_access_unit_contains_idr(data: &[u8]) -> bool {
    let mut index = 0;
    while index + 3 < data.len() {
        let start_length = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        let nal_index = index + start_length;
        if nal_index < data.len() && data[nal_index] & 0x1f == 5 {
            return true;
        }
        index = nal_index;
    }
    false
}

fn local_udp_addresses() -> Result<Vec<String>, PeerConnectionError> {
    let mut addresses = vec!["127.0.0.1:0".to_owned()];
    for interface in
        get_if_addrs().map_err(|error| PeerConnectionError::Connection(error.to_string()))?
    {
        let name = interface.name.to_ascii_lowercase();
        if [
            "docker",
            "vethernet",
            "wsl",
            "virtualbox",
            "vmware",
            "tailscale",
            "zerotier",
        ]
        .iter()
        .any(|virtual_name| name.contains(virtual_name))
        {
            continue;
        }
        let ip = interface.ip();
        if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
            continue;
        }
        let address = match ip {
            std::net::IpAddr::V4(value) if !value.is_link_local() => format!("{value}:0"),
            std::net::IpAddr::V6(value) if !value.is_unicast_link_local() => {
                format!("[{value}]:0")
            }
            _ => continue,
        };
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    Ok(addresses)
}

fn nat_ice_servers() -> Vec<RTCIceServer> {
    let config = nat_config_from_environment();
    let mut servers = config
        .stun_servers
        .into_iter()
        .map(|url| RTCIceServer {
            urls: vec![url],
            ..Default::default()
        })
        .collect::<Vec<_>>();
    servers.extend(config.turn_servers.into_iter().map(|server| RTCIceServer {
        urls: server.urls,
        username: server.username.unwrap_or_default(),
        credential: server.credential.unwrap_or_default(),
    }));
    servers
}

fn nat_config_from_environment() -> NatConfig {
    nat_config_from_values(
        std::env::var("NEXO_STUN_SERVERS").ok().as_deref(),
        std::env::var("NEXO_TURN_SERVERS").ok().as_deref(),
    )
}

fn nat_config_from_values(stun_value: Option<&str>, turn_value: Option<&str>) -> NatConfig {
    let mut config = NatConfig::new();
    if let Some(value) = stun_value {
        for url in value
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            let server = RTCIceServer {
                urls: vec![url.to_owned()],
                ..Default::default()
            };
            if server.urls().is_ok() {
                config.add_stun(url.to_owned());
            }
        }
    }
    if let Some(value) = turn_value {
        for entry in value
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let mut fields = entry.split('|').map(str::trim);
            let Some(url) = fields.next().filter(|url| !url.is_empty()) else {
                continue;
            };
            let Some(username) = fields.next().filter(|username| !username.is_empty()) else {
                continue;
            };
            let Some(credential) = fields.next().filter(|credential| !credential.is_empty()) else {
                continue;
            };
            if fields.next().is_some() {
                continue;
            }
            let server = RTCIceServer {
                urls: vec![url.to_owned()],
                username: username.to_owned(),
                credential: credential.to_owned(),
            };
            if server.urls().is_ok() {
                config.add_turn(nexo_core::IceServer::turn(url, username, credential));
            }
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, f32::consts::TAU};

    use super::*;
    use crate::{AudioFrame, VoiceDecoder, VoiceEncoder, Vp8Decoder, Vp8Encoder};

    async fn connect_pair(
        offerer: &LanPeerConnection,
        answerer: &LanPeerConnection,
    ) -> Result<(), PeerConnectionError> {
        let offer = offerer.create_offer().await?;
        let answer = answerer.accept_offer(offer).await?;
        offerer.accept_answer(answer).await?;
        offerer.wait_until_connected().await?;
        answerer.wait_until_connected().await
    }

    fn encoded_test_audio(frequency_hz: f32) -> EncodedAudioFrame {
        let input = AudioFrame {
            samples: (0..OPUS_FRAME_SAMPLES)
                .map(|index| {
                    #[allow(clippy::cast_precision_loss)]
                    let time = index as f32 / OPUS_SAMPLE_RATE as f32;
                    (TAU * frequency_hz * time).sin() * 0.2
                })
                .collect(),
            sample_rate: OPUS_SAMPLE_RATE,
        };
        let mut encoder = VoiceEncoder::new().expect("voice encoder should initialize");
        encoder
            .encode(&input)
            .expect("test voice frame should encode")
    }

    fn encoded_test_video() -> EncodedVideoFrame {
        let (width, height) = (320_u32, 240_u32);
        let mut encoder =
            Vp8Encoder::new(width, height, 1_000).expect("video encoder should initialize");
        let y_size = width as usize * height as usize;
        let mut input = vec![0_u8; y_size + y_size / 2];
        for row in 0..height {
            for column in 0..width {
                input[row as usize * width as usize + column as usize] =
                    u8::try_from((row + column) % 256).unwrap_or_default();
            }
        }
        encoder
            .encode_frame(Duration::ZERO, &input)
            .expect("test video frame should encode")
            .expect("video encoder should emit a frame")
    }

    async fn receive_audio(peer: &LanPeerConnection) -> ReceivedAudioPacket {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(packet) = peer
                    .try_received_audio()
                    .expect("audio queue should remain open")
                {
                    break packet;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("audio should arrive")
    }

    async fn receive_video(peer: &LanPeerConnection) -> ReceivedVideoPacket {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(packet) = peer
                    .try_received_video()
                    .expect("video queue should remain open")
                {
                    break packet;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("video should arrive")
    }

    async fn receive_data(peer: &mut LanPeerConnection) -> ReceivedDataMessage {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(message) = peer
                    .try_received_data()
                    .expect("data queue should remain open")
                {
                    break message;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("data-channel message should arrive")
    }

    async fn close_with_timeout(peer: LanPeerConnection) {
        let _ = timeout(Duration::from_secs(2), peer.close()).await;
    }

    #[test]
    fn nat_environment_parser_accepts_optional_stun_and_turn_entries() {
        let config = nat_config_from_values(
            Some("stun:one.example:3478, stun:two.example:3478,not-a-url"),
            Some("turn:relay.example:3478|alice|secret;invalid"),
        );
        assert_eq!(config.stun_servers.len(), 2);
        assert_eq!(config.turn_servers.len(), 1);
        assert_eq!(config.turn_servers[0].username.as_deref(), Some("alice"));
    }

    #[test]
    fn connection_failures_are_classified_for_diagnostics() {
        assert_eq!(
            connection_state_event(RTCPeerConnectionState::Connected),
            Some(ConnectionStateEvent::Connected)
        );
        assert_eq!(
            connection_state_event(RTCPeerConnectionState::Disconnected),
            Some(ConnectionStateEvent::Disconnected)
        );
        assert_eq!(
            connection_state_event(RTCPeerConnectionState::Failed),
            Some(ConnectionStateEvent::Failed)
        );
        assert_eq!(
            connection_state_event(RTCPeerConnectionState::New),
            Some(ConnectionStateEvent::New)
        );
        assert_eq!(
            connection_state_event(RTCPeerConnectionState::Connecting),
            Some(ConnectionStateEvent::Connecting)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_until_connected_accepts_state_reached_before_waiter() {
        let offerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("offerer should initialize");
        let answerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("answerer should initialize");
        let offer = offerer
            .create_offer()
            .await
            .expect("offer should be created");
        let answer = answerer
            .accept_offer(offer)
            .await
            .expect("answer should be created");
        offerer
            .accept_answer(answer)
            .await
            .expect("answer should be applied");

        // Exercise the ordering that used to lose the one-shot connected event.
        tokio::time::sleep(Duration::from_millis(150)).await;
        offerer
            .wait_until_connected()
            .await
            .expect("offerer should observe the retained connected state");
        answerer
            .wait_until_connected()
            .await
            .expect("answerer should observe the retained connected state");

        offerer.close().await.expect("offerer should close cleanly");
        answerer
            .close()
            .await
            .expect("answerer should close cleanly");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_lan_peers_exchange_bounded_data_channel_messages() {
        let mut offerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("native WebRTC peer should initialize");
        let mut answerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("second native WebRTC peer should initialize");
        connect_pair(&offerer, &answerer)
            .await
            .expect("data-channel peers should connect");

        let payload = vec![0x5a; DATA_CHANNEL_MESSAGE_BYTES];
        offerer
            .send_data(&payload)
            .await
            .expect("bounded data should enter the WebRTC channel");
        let received = receive_data(&mut answerer).await;
        assert_eq!(received.data.as_ref(), payload.as_slice());

        answerer
            .send_data(b"reverse")
            .await
            .expect("answerer should send data back over WebRTC");
        let reverse = receive_data(&mut offerer).await;
        assert_eq!(reverse.data.as_ref(), b"reverse");

        let oversized = vec![0_u8; DATA_CHANNEL_MESSAGE_BYTES + 1];
        assert!(offerer.send_data(&oversized).await.is_err());
        close_with_timeout(offerer).await;
        close_with_timeout(answerer).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_lan_peers_exchange_encrypted_opus_audio() {
        let offerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("native WebRTC peer should initialize");
        let answerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("second native WebRTC peer should initialize");
        let offer = offerer
            .create_offer()
            .await
            .expect("SDP offer should be generated");
        assert!(offer.contains("m=audio"));
        assert!(offer.to_ascii_lowercase().contains("opus/48000"));
        let answer = answerer
            .accept_offer(offer)
            .await
            .expect("answer should be created");
        offerer
            .accept_answer(answer)
            .await
            .expect("answer should be applied");
        offerer
            .wait_until_connected()
            .await
            .expect("offerer should connect");
        answerer
            .wait_until_connected()
            .await
            .expect("answerer should connect");

        let input = AudioFrame {
            samples: (0..OPUS_FRAME_SAMPLES)
                .map(|index| {
                    #[allow(clippy::cast_precision_loss)]
                    let time = index as f32 / OPUS_SAMPLE_RATE as f32;
                    (TAU * 440.0 * time).sin() * 0.25
                })
                .collect(),
            sample_rate: OPUS_SAMPLE_RATE,
        };
        let mut encoder = VoiceEncoder::new().expect("voice encoder should initialize");
        let packet = encoder.encode(&input).expect("voice frame should encode");
        offerer
            .send_audio(&packet)
            .await
            .expect("audio should enter the SRTP track");

        let received = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(frame) = answerer
                    .try_received_audio()
                    .expect("remote audio queue should remain open")
                {
                    break frame;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("encrypted audio should arrive");
        let mut decoder = VoiceDecoder::new().expect("voice decoder should initialize");
        let output = decoder
            .decode(&received.frame)
            .expect("received Opus should decode");
        assert_eq!(output.samples.len(), OPUS_FRAME_SAMPLES);
        assert!(output.samples.iter().any(|sample| sample.abs() > 0.001));

        offerer.close().await.expect("peer should close cleanly");
        answerer.close().await.expect("peer should close cleanly");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_audio_keeps_publishers_on_distinct_tracks() {
        let offerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("native WebRTC peer should initialize");
        let answerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("second native WebRTC peer should initialize");
        let offer = offerer
            .create_offer()
            .await
            .expect("SDP offer should be generated");
        let answer = answerer
            .accept_offer(offer)
            .await
            .expect("answer should be created");
        offerer
            .accept_answer(answer)
            .await
            .expect("answer should be applied");
        offerer
            .wait_until_connected()
            .await
            .expect("offerer should connect");
        answerer
            .wait_until_connected()
            .await
            .expect("answerer should connect");

        let input = AudioFrame {
            samples: (0..OPUS_FRAME_SAMPLES)
                .map(|index| {
                    #[allow(clippy::cast_precision_loss)]
                    let time = index as f32 / OPUS_SAMPLE_RATE as f32;
                    (TAU * 440.0 * time).sin() * 0.25
                })
                .collect(),
            sample_rate: OPUS_SAMPLE_RATE,
        };
        let mut encoder = VoiceEncoder::new().expect("voice encoder should initialize");
        let packet = encoder.encode(&input).expect("voice frame should encode");
        offerer
            .send_relay_audio("publisher-a", &packet)
            .await
            .expect("first relay source should be sent");
        offerer
            .send_relay_audio("publisher-b", &packet)
            .await
            .expect("second relay source should be sent");

        let tracks = timeout(Duration::from_secs(5), async {
            let mut tracks = HashSet::new();
            while tracks.len() < 2 {
                if let Some(packet) = answerer
                    .try_received_audio()
                    .expect("remote audio queue should remain open")
                {
                    tracks.insert(packet.track_id);
                } else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            tracks
        })
        .await
        .expect("both relay audio sources should arrive");
        assert_eq!(
            tracks,
            HashSet::from(["nexo-audio-1".to_owned(), "nexo-audio-2".to_owned()])
        );

        offerer.close().await.expect("peer should close cleanly");
        answerer.close().await.expect("peer should close cleanly");
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn participant_relay_forwards_audio_and_video_between_three_peers() {
        // B publishes to the participant-hosted relay A. A forwards the
        // encrypted media envelope to C over an independent WebRTC link.
        let publisher = Box::pin(LanPeerConnection::new())
            .await
            .expect("publisher should initialize");
        let relay_in = Box::pin(LanPeerConnection::new())
            .await
            .expect("relay ingress should initialize");
        let relay_out = Box::pin(LanPeerConnection::new())
            .await
            .expect("relay egress should initialize");
        let subscriber = Box::pin(LanPeerConnection::new())
            .await
            .expect("subscriber should initialize");

        connect_pair(&publisher, &relay_in)
            .await
            .expect("publisher should connect to relay");
        connect_pair(&relay_out, &subscriber)
            .await
            .expect("relay should connect to subscriber");

        let audio = AudioFrame {
            samples: (0..OPUS_FRAME_SAMPLES)
                .map(|index| {
                    #[allow(clippy::cast_precision_loss)]
                    let time = index as f32 / OPUS_SAMPLE_RATE as f32;
                    (TAU * 330.0 * time).sin() * 0.2
                })
                .collect(),
            sample_rate: OPUS_SAMPLE_RATE,
        };
        let mut audio_encoder = VoiceEncoder::new().expect("voice encoder should initialize");
        let audio_packet = audio_encoder
            .encode(&audio)
            .expect("voice frame should encode");
        publisher
            .send_relay_audio("publisher-b", &audio_packet)
            .await
            .expect("publisher audio should enter relay ingress");
        let forwarded_audio = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(packet) = relay_in
                    .try_received_audio()
                    .expect("relay ingress audio queue should remain open")
                {
                    break packet;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("relay should receive publisher audio");
        relay_out
            .send_relay_audio("publisher-b", &forwarded_audio.frame)
            .await
            .expect("relay audio should enter egress");
        let received_audio = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(packet) = subscriber
                    .try_received_audio()
                    .expect("subscriber audio queue should remain open")
                {
                    break packet;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("subscriber should receive forwarded audio");
        assert_eq!(received_audio.track_id, "nexo-audio-1");
        assert_eq!(received_audio.frame.payload, forwarded_audio.frame.payload);

        let (width, height) = (320_u32, 240_u32);
        let mut video_encoder =
            Vp8Encoder::new(width, height, 1_000).expect("video encoder should initialize");
        let y_size = width as usize * height as usize;
        let mut input = vec![0_u8; y_size + y_size / 2];
        for row in 0..height {
            for column in 0..width {
                input[row as usize * width as usize + column as usize] =
                    u8::try_from((row + column) % 256).unwrap_or_default();
            }
        }
        let video_frame = video_encoder
            .encode_frame(Duration::ZERO, &input)
            .expect("video frame should encode")
            .expect("video encoder should emit a frame");
        publisher
            .send_relay_video("publisher-b", &video_frame)
            .await
            .expect("publisher video should enter relay ingress");
        let forwarded_video = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(packet) = relay_in
                    .try_received_video()
                    .expect("relay ingress video queue should remain open")
                {
                    break packet;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("relay should receive publisher video");
        relay_out
            .send_relay_video("publisher-b", &forwarded_video.frame)
            .await
            .expect("relay video should enter egress");
        let received_video = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(packet) = subscriber
                    .try_received_video()
                    .expect("subscriber video queue should remain open")
                {
                    break packet;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("subscriber should receive forwarded video");
        assert_eq!(received_video.track_id, "nexo-video-1");
        assert_eq!(received_video.frame.codec, VideoCodec::Vp8);
        assert_eq!(received_video.frame.data, forwarded_video.frame.data);
        let mut video_decoder = Vp8Decoder::new().expect("subscriber decoder should initialize");
        let decoded_frame = video_decoder
            .decode(&received_video.frame)
            .expect("forwarded video should decode")
            .expect("forwarded keyframe should produce an image");
        assert_eq!((decoded_frame.width, decoded_frame.height), (width, height));

        publisher
            .close()
            .await
            .expect("publisher should close cleanly");
        relay_in
            .close()
            .await
            .expect("relay ingress should close cleanly");
        relay_out
            .close()
            .await
            .expect("relay egress should close cleanly");
        subscriber
            .close()
            .await
            .expect("subscriber should close cleanly");
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn relay_fans_out_four_publishers_to_two_subscribers() {
        const PUBLISHER_COUNT: usize = 4;
        const SUBSCRIBER_COUNT: usize = 2;

        let mut publishers = Vec::with_capacity(PUBLISHER_COUNT);
        let mut relay_ingress = Vec::with_capacity(PUBLISHER_COUNT);
        for _ in 0..PUBLISHER_COUNT {
            publishers.push(
                Box::pin(LanPeerConnection::new())
                    .await
                    .expect("publisher should initialize"),
            );
            relay_ingress.push(
                Box::pin(LanPeerConnection::new())
                    .await
                    .expect("relay ingress should initialize"),
            );
        }

        let mut relay_egress = Vec::with_capacity(SUBSCRIBER_COUNT);
        let mut subscribers = Vec::with_capacity(SUBSCRIBER_COUNT);
        for _ in 0..SUBSCRIBER_COUNT {
            relay_egress.push(
                Box::pin(LanPeerConnection::new())
                    .await
                    .expect("relay egress should initialize"),
            );
            subscribers.push(
                Box::pin(LanPeerConnection::new())
                    .await
                    .expect("subscriber should initialize"),
            );
        }

        for (publisher, ingress) in publishers.iter().zip(&relay_ingress) {
            connect_pair(publisher, ingress)
                .await
                .expect("publisher should connect to relay");
        }
        for (egress, subscriber) in relay_egress.iter().zip(&subscribers) {
            connect_pair(egress, subscriber)
                .await
                .expect("relay should connect to subscriber");
        }

        let audio = encoded_test_audio(220.0);
        let video = encoded_test_video();
        // Give each remote track callback time to attach before the first
        // frame. The production engine retries on every media tick; this
        // deterministic test keeps the same behavior by processing one source
        // at a time instead of overflowing an ingress queue.
        tokio::time::sleep(Duration::from_millis(200)).await;
        for (index, (publisher, ingress)) in publishers.iter().zip(&relay_ingress).enumerate() {
            let source_id = format!("publisher-{index}");
            publisher
                .send_relay_audio(&source_id, &audio)
                .await
                .expect("publisher audio should enter relay");
            let audio_packet = receive_audio(ingress).await;
            publisher
                .send_relay_video(&source_id, &video)
                .await
                .expect("publisher video should enter relay");
            let video_packet = receive_video(ingress).await;
            for egress in &relay_egress {
                egress
                    .send_relay_audio(&source_id, &audio_packet.frame)
                    .await
                    .expect("relay audio should fan out");
                // RTP has no retransmission at this layer. A second keyframe
                // models the production media tick and makes the test assert
                // eventual track delivery rather than one lucky datagram.
                for _ in 0..2 {
                    egress
                        .send_relay_video(&source_id, &video_packet.frame)
                        .await
                        .expect("relay video should fan out");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }

        for subscriber in &subscribers {
            let mut audio_tracks = HashSet::new();
            let mut video_tracks = HashSet::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while (audio_tracks.len() < PUBLISHER_COUNT || video_tracks.len() < PUBLISHER_COUNT)
                && Instant::now() < deadline
            {
                if audio_tracks.len() < PUBLISHER_COUNT
                    && let Some(packet) = subscriber
                        .try_received_audio()
                        .expect("subscriber audio queue should remain open")
                {
                    audio_tracks.insert(packet.track_id);
                }
                if video_tracks.len() < PUBLISHER_COUNT
                    && let Some(packet) = subscriber
                        .try_received_video()
                        .expect("subscriber video queue should remain open")
                {
                    video_tracks.insert(packet.track_id);
                }
                if audio_tracks.len() < PUBLISHER_COUNT || video_tracks.len() < PUBLISHER_COUNT {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            assert_eq!(
                audio_tracks.len(),
                PUBLISHER_COUNT,
                "subscriber did not receive all relay audio tracks"
            );
            assert_eq!(
                video_tracks.len(),
                PUBLISHER_COUNT,
                "subscriber did not receive all relay video tracks"
            );
        }

        for publisher in publishers {
            close_with_timeout(publisher).await;
        }
        for ingress in relay_ingress {
            close_with_timeout(ingress).await;
        }
        for egress in relay_egress {
            close_with_timeout(egress).await;
        }
        for subscriber in subscribers {
            close_with_timeout(subscriber).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_lan_peers_exchange_vp8_video_roundtrip() {
        let offerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("native WebRTC peer should initialize");
        let answerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("second native WebRTC peer should initialize");
        let offer = offerer
            .create_offer()
            .await
            .expect("SDP offer should be generated");
        assert!(offer.contains("m=video"));
        assert!(offer.to_ascii_lowercase().contains("vp8"));
        let answer = answerer
            .accept_offer(offer)
            .await
            .expect("answer should be created");
        offerer
            .accept_answer(answer)
            .await
            .expect("answer should be applied");
        offerer
            .wait_until_connected()
            .await
            .expect("offerer should connect");
        answerer
            .wait_until_connected()
            .await
            .expect("answerer should connect");

        // Encode a synthetic 640x480 I420 gradient so the whole pipeline is
        // exercised: encoder -> RTP (payloads large enough to be split into
        // several packets) -> depacketizer -> decoder.
        let (width, height) = (640u32, 480u32);
        let mut encoder = Vp8Encoder::new(width, height, 1_500).expect("encoder should init");
        let y_size = width as usize * height as usize;
        let mut input = vec![0u8; y_size + y_size / 2];
        for row in 0..height {
            for column in 0..width {
                let value = u8::try_from(row * 255 / height.max(1)).unwrap_or(u8::MAX);
                input[row as usize * width as usize + column as usize] = value;
            }
        }
        let bitstream = encoder
            .encode_frame(Duration::ZERO, &input)
            .expect("frame should encode")
            .expect("encoder should emit a frame");
        assert!(bitstream.is_keyframe, "the first frame must be a keyframe");
        let decoded_frame = timeout(Duration::from_secs(5), async {
            for _attempt in 0..3 {
                let mut decoder = Vp8Decoder::new().expect("decoder should init");
                offerer
                    .send_video(&bitstream)
                    .await
                    .expect("video should enter the SRTP track");
                let deadline = Instant::now() + Duration::from_millis(900);
                while Instant::now() < deadline {
                    let Some(packet) = answerer
                        .try_received_video()
                        .expect("remote video queue should remain open")
                    else {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    };
                    if packet.frame.codec != VideoCodec::Vp8 || packet.frame.data.is_empty() {
                        continue;
                    }
                    if let Ok(Some(decoded)) = decoder.decode(&packet.frame) {
                        return Some(decoded);
                    }
                }
            }
            None
        })
        .await
        .expect("encrypted video should arrive")
        .expect("a retransmitted VP8 keyframe should decode");
        assert_eq!(
            (decoded_frame.width, decoded_frame.height),
            (width, height),
            "decoded dimensions must survive the round trip"
        );
        let last_row = decoded_frame.y_stride * (height as usize - 8);
        let last_row_avg: u16 = decoded_frame.y_plane[last_row..]
            .iter()
            .copied()
            .take(64)
            .map(u16::from)
            .sum::<u16>()
            / 64;
        let first_row_avg: u16 = decoded_frame.y_plane[..decoded_frame.y_stride * 8]
            .iter()
            .copied()
            .take(64)
            .map(u16::from)
            .sum::<u16>()
            / 64;
        assert!(
            last_row_avg > first_row_avg,
            "the gradient must survive the round trip (left {first_row_avg}, right {last_row_avg})"
        );

        offerer.close().await.expect("peer should close cleanly");
        answerer.close().await.expect("peer should close cleanly");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_video_assigns_distinct_slots_and_reuses_released_source() {
        let offerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("native WebRTC peer should initialize");
        let answerer = Box::pin(LanPeerConnection::new())
            .await
            .expect("second native WebRTC peer should initialize");
        let offer = offerer
            .create_offer()
            .await
            .expect("SDP offer should be generated");
        let answer = answerer
            .accept_offer(offer)
            .await
            .expect("answer should be created");
        offerer
            .accept_answer(answer)
            .await
            .expect("answer should be applied");
        offerer
            .wait_until_connected()
            .await
            .expect("offerer should connect");
        answerer
            .wait_until_connected()
            .await
            .expect("answerer should connect");

        let width = 320_u32;
        let height = 240_u32;
        let mut encoder = Vp8Encoder::new(width, height, 1_500).expect("encoder should init");
        let y_size = width as usize * height as usize;
        let input = vec![96_u8; y_size + y_size / 2];
        let frame = encoder
            .encode_frame(Duration::ZERO, &input)
            .expect("frame should encode")
            .expect("encoder should emit a frame");

        offerer
            .send_relay_video("publisher-a", &frame)
            .await
            .expect("first relay source should be sent");
        offerer
            .send_relay_video("publisher-b", &frame)
            .await
            .expect("second relay source should be sent");

        let first_tracks = timeout(Duration::from_secs(5), async {
            let mut tracks = HashSet::new();
            while tracks.len() < 2 {
                if let Some(packet) = answerer
                    .try_received_video()
                    .expect("remote video queue should remain open")
                {
                    tracks.insert(packet.track_id);
                } else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            tracks
        })
        .await
        .expect("both relay sources should arrive");
        assert_eq!(
            first_tracks,
            HashSet::from(["nexo-video-1".to_owned(), "nexo-video-2".to_owned()])
        );

        offerer.release_relay_source("publisher-a").await;
        offerer
            .send_relay_video("publisher-c", &frame)
            .await
            .expect("released relay slot should be reusable");
        let reused = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(packet) = answerer
                    .try_received_video()
                    .expect("remote video queue should remain open")
                {
                    break packet.track_id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reused relay source should arrive");
        assert_eq!(reused, "nexo-video-1");

        offerer.close().await.expect("peer should close cleanly");
        answerer.close().await.expect("peer should close cleanly");
    }
}
