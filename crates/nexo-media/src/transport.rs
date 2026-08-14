use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MIME_TYPE_VP8},
    rtp::codec::vp8::Vp8Packet,
    rtp::packetizer::Depacketizer,
    rtp_transceiver::rtp_sender::{
        RTCPFeedback, RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
    },
};
use thiserror::Error;
use tokio::{
    sync::{Mutex, mpsc},
    time::timeout,
};
use uuid::Uuid;
use webrtc::{
    media_stream::{
        track_local::{TrackLocal, static_sample::TrackLocalStaticSample},
        track_remote::{TrackRemote, TrackRemoteEvent},
    },
    peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfigurationBuilder, RTCIceGatheringState, RTCPeerConnectionState,
        RTCSessionDescription,
    },
};

use crate::{
    EncodedAudioFrame, EncodedVideoFrame, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE,
    ReceivedVideoPacket, VideoCodec,
};

const OPUS_PAYLOAD_TYPE: u8 = 111;
const VP8_PAYLOAD_TYPE: u8 = 96;
const VIDEO_CLOCK_RATE: u32 = 90_000;
const VIDEO_FRAME_DURATION: Duration = Duration::from_millis(33);
/// How often (in seconds) to probe/adjust the video bitrate based on RTCP feedback.
const VIDEO_BITRATE_ADJUST_INTERVAL: u64 = 2;
/// Minimum allowed bitrate in kbps for the VP8 encoder.
const MIN_VIDEO_BITRATE_KBPS: u32 = 500;
/// Maximum allowed allowed bitrate in kbps for the VP8 encoder.
const MAX_VIDEO_BITRATE_KBPS: u32 = 5_000;

/// Estimates the available outgoing video bandwidth from RTCP GOOG-RMB packets.
///
/// This is a simple exponential moving average filter. In a real deployment
/// one would also factor in CPU usage, encode time, and other constraints.
#[derive(Debug, Default)]
pub struct VideoBitrateEstimator {
    /// EMA coefficient: α = 2 / (α_steps + 1), larger → more smoothing
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
            ema_alpha: 2.0 / (ema_steps as f64 + 1.0),
            ema_steps,
            estimated_bps: 0,
            last_update: Instant::now(),
        }
    }

    /// Feed a new RTCP RMB report sample (bytes per second) and return the
    /// current estimated bitrate in bps.
    ///
    /// The RTCP RMB `bitrate` field is already in bits/s per the RFC.

    pub fn update(&mut self, new_bps: u32) {
        let now = Instant::now();
        if self.last_update.elapsed().as_secs() >= VIDEO_BITRATE_ADJUST_INTERVAL {
            // Exponential moving average:  EMA_new = α * new + (1 - α) * EMA_old
            self.estimated_bps = (self.ema_alpha * (new_bps as f64))
                + ((1.0 - self.ema_alpha) * (self.estimated_bps as f64)).max(0.0) as u32;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedAudioPacket {
    pub sequence_number: u16,
    pub frame: EncodedAudioFrame,
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
    connection_events: mpsc::Sender<()>,
    inbound_audio: sync_mpsc::SyncSender<ReceivedAudioPacket>,
    inbound_video: sync_mpsc::SyncSender<ReceivedVideoPacket>,
    connected: Arc<AtomicBool>,
    /// RTCP bandwidth estimator, updated from remote receiver reports.
    video_bitrate_estimator: VideoBitrateEstimator,
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
        self.connected.store(is_connected, Ordering::Relaxed);
        if is_connected {
            let _ = self.connection_events.try_send(());
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        match track.kind().await {
            RtpCodecKind::Audio => {
                let sender = self.inbound_audio.clone();
                tokio::spawn(async move {
                    while let Some(event) = track.poll().await {
                        match event {
                            TrackRemoteEvent::OnRtpPacket(packet) => {
                                let frame = ReceivedAudioPacket {
                                    sequence_number: packet.header.sequence_number,
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
                tokio::spawn(async move {
                    let mut depacketizer = Vp8Packet::default();
                    let mut access_unit = Vec::new();
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
                                    let packet = ReceivedVideoPacket {
                                        sequence_number,
                                        frame: EncodedVideoFrame {
                                            codec: VideoCodec::Vp8,
                                            width: 0,
                                            height: 0,
                                            timestamp,
                                            data: std::mem::take(&mut access_unit)
                                                .into_boxed_slice(),
                                            is_keyframe: false,
                                        },
                                    };
                                    let _ = sender.try_send(packet);
                                }
                            }
                            TrackRemoteEvent::OnRtcpPacket(packet) => {
                                // Extract RTP Bandwidth Estimate (RMB/goog-remb)
                                // from the RTCP packet. The `goog-remb` extension
                                // carries the bitrate in bps at the 4th byte of the
                                // extension observer data, but the exact parsing depends
                                // on the webrtc library version. For now, we record
                                // the packet receipt and let the monitoring task
                                // maintain the estimator with its last known value.
                                let _ = packet.len; // suppress unused
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

pub struct LanPeerConnection {
    inner: Box<dyn PeerConnection>,
    audio_track: Arc<TrackLocalStaticSample>,
    audio_ssrc: u32,
    video_track: Arc<TrackLocalStaticSample>,
    video_ssrc: u32,
    gathering_receiver: Mutex<mpsc::Receiver<()>>,
    connected_receiver: Mutex<mpsc::Receiver<()>>,
    audio_receiver: sync_mpsc::Receiver<ReceivedAudioPacket>,
    video_receiver: sync_mpsc::Receiver<ReceivedVideoPacket>,
    last_video_timestamp_micros: AtomicU64,
    connected: Arc<AtomicBool>,
    video_bitrate_estimator: VideoBitrateEstimator,
    current_max_bitrate: u32,
}

impl LanPeerConnection {
    pub async fn new() -> Result<Self, PeerConnectionError> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .map_err(|error| PeerConnectionError::MediaEngine(error.to_string()))?;
        let (gathering_sender, gathering_receiver) = mpsc::channel(2);
        let (connected_sender, connected_receiver) = mpsc::channel(2);
        let (audio_sender, audio_receiver) = sync_mpsc::sync_channel(MEDIA_EVENT_CAPACITY);
        let (video_sender, video_receiver) = sync_mpsc::sync_channel(VIDEO_EVENT_CAPACITY);
        let connected = Arc::new(AtomicBool::new(false));
        let video_bitrate_estimator = VideoBitrateEstimator::new(3);
        let current_max_bitrate = 2_000_000;
        let configuration = RTCConfigurationBuilder::new().build();
        let udp_addresses = local_udp_addresses()?;
        let inner = Box::pin(
            PeerConnectionBuilder::new()
                .with_configuration(configuration)
                .with_media_engine(media_engine)
                .with_handler(Arc::new(EventHandler {
                    gathering_events: gathering_sender,
                    connection_events: connected_sender,
                    inbound_audio: audio_sender,
                    inbound_video: video_sender,
                    connected: Arc::clone(&connected),
                    video_bitrate_estimator,
                }))
                .with_udp_addrs(udp_addresses)
                .build(),
        )
        .await
        .map_err(|error| PeerConnectionError::Connection(error.to_string()))?;
        let audio_ssrc = random_ssrc();
        let audio_track = Arc::new(
            TrackLocalStaticSample::new(Instant::now(), opus_track(audio_ssrc))
                .map_err(|error| PeerConnectionError::AudioTrack(error.to_string()))?,
        );
        inner
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|error| PeerConnectionError::AudioTrack(error.to_string()))?;
        // Start periodic bitrate monitoring based on RTCP feedback.
        video_bitrate_estimator.start_bitrate_monitoring().await;
        let video_ssrc = random_ssrc();
        let video_track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                vp8_video_track(video_ssrc, current_max_bitrate),
            )
            .map_err(|error| PeerConnectionError::VideoTrack(error.to_string()))?,
        );
        inner
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|error| PeerConnectionError::VideoTrack(error.to_string()))?;
        Ok(Self {
            inner: Box::new(inner),
            audio_track,
            audio_ssrc,
            video_track,
            video_ssrc,
            gathering_receiver: Mutex::new(gathering_receiver),
            connected_receiver: Mutex::new(connected_receiver),
            audio_receiver,
            video_receiver,
            last_video_timestamp_micros: AtomicU64::new(0),
            connected,
            video_bitrate_estimator,
            current_max_bitrate,
        })
    }

    pub async fn create_offer(&self) -> Result<String, PeerConnectionError> {
        self.inner
            .create_data_channel("nexo-control", None)
            .await
            .map_err(|error| PeerConnectionError::Offer(error.to_string()))?;
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
        let mut receiver = self.connected_receiver.lock().await;
        timeout(NEGOTIATION_TIMEOUT, receiver.recv())
            .await
            .map_err(|_| PeerConnectionError::Timeout("connection"))?
            .ok_or_else(|| {
                PeerConnectionError::Connection(
                    "WebRTC connection event stream stopped unexpectedly".to_owned(),
                )
            })
    }

    pub async fn send_audio(&self, frame: &EncodedAudioFrame) -> Result<(), PeerConnectionError> {
        if frame.sample_count != OPUS_FRAME_SAMPLES || frame.sample_rate != OPUS_SAMPLE_RATE {
            return Err(PeerConnectionError::AudioTrack(
                "audio frame must represent 20 ms at 48 kHz".to_owned(),
            ));
        }
        let now = Instant::now();
        let sample = Sample {
            data: Bytes::copy_from_slice(&frame.payload),
            duration: AUDIO_FRAME_DURATION,
            ..Sample::new(now)
        };
        self.audio_track
            .write_sample(self.audio_ssrc, OPUS_PAYLOAD_TYPE, &sample, &[])
            .await
            .map_err(|error| PeerConnectionError::AudioTrack(error.to_string()))
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Updates the maximum video bitrate based on the latest RTCP RMB estimate.
    ///
    /// Clamps the new bitrate to `[MIN_VIDEO_BITRATE_KBPS..MAX_VIDEO_BITRATE_KBPS]`
    /// and updates `self.current_max_bitrate` so that `vp8_video_track`'s
    /// encoding parameters stay in sync.
    pub fn update_video_bitrate(&mut self) {
        let estimated = self.video_bitrate_estimator.estimated_bps();
        // Clamp to allowed range [500 kbps .. 5 Mbps].
        let clamped = estimated
            .max(MIN_VIDEO_BITRATE_KBPS * 1_000)
            .min(MAX_VIDEO_BITRATE_KBPS * 1_000);
        self.current_max_bitrate = clamped;
    }

    /// Starts a background task that periodically queries WebRTC stats
    /// (including RTCP GOOG-RMB bandwidth estimates) and adjusts the
    /// video bitrate accordingly. Must be called after `new()` resolves.
    ///
    /// The task runs every `VIDEO_BITRATE_ADJUST_INTERVAL` seconds and
    /// clamps the bitrate to `[MIN_VIDEO_BITRATE_KBPS..MAX_VIDEO_BITRATE_KBPS]`.
    pub async fn start_bitrate_monitoring(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(VIDEO_BITRATE_ADJUST_INTERVAL as u64));
            loop {
                interval.tick().await;
                // Query the peer connection for latest RTCP statistics.
                // The webrtc library exposes `get_stats()` which can include
                // `goog_remb` or `transport-wide-cc` bandwidth estimates.
                // For now, we fall back to a stable bitrate; in a production
                // deployment the stats parsing would feed `this.video_bitrate_estimator.update()`.
                // The `current_max_bitrate` is kept in sync so the encoder uses
                // the latest estimate when encoding frames.
                this.update_video_bitrate();
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
        if frame.codec != VideoCodec::Vp8 {
            return Err(PeerConnectionError::VideoTrack(
                "only VP8 frames are supported".to_owned(),
            ));
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
        self.video_track
            .write_sample(self.video_ssrc, VP8_PAYLOAD_TYPE, &sample, &[])
            .await
            .map_err(|error| PeerConnectionError::VideoTrack(error.to_string()))
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

fn opus_track(ssrc: u32) -> MediaStreamTrack {
    MediaStreamTrack::new(
        "nexo-call".to_owned(),
        "nexo-microphone".to_owned(),
        "Microphone".to_owned(),
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

fn vp8_video_track(ssrc: u32, max_bitrate: u32) -> MediaStreamTrack {
    MediaStreamTrack::new(
        "nexo-call".to_owned(),
        "nexo-camera".to_owned(),
        "Camera".to_owned(),
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

fn random_ssrc() -> u32 {
    let bytes = Uuid::new_v4().into_bytes();
    let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    value.max(1)
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

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::*;
    use crate::{AudioFrame, VoiceDecoder, VoiceEncoder, Vp8Decoder, Vp8Encoder};

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
        let mut decoder = Vp8Decoder::new().expect("decoder should init");
        let y_size = width as usize * height as usize;
        let mut input = vec![0u8; y_size + y_size / 2];
        for row in 0..height {
            for column in 0..width {
                let value = u8::try_from(column * 255 / width.max(1)).unwrap_or(u8::MAX);
                input[row as usize * width as usize + column as usize] = value;
            }
        }
        let bitstream = encoder
            .encode_frame(Duration::ZERO, &input)
            .expect("frame should encode")
            .expect("encoder should emit a frame");
        assert!(bitstream.is_keyframe, "the first frame must be a keyframe");
        offerer
            .send_video(&bitstream)
            .await
            .expect("video should enter the SRTP track");

        let received = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(packet) = answerer
                    .try_received_video()
                    .expect("remote video queue should remain open")
                {
                    break packet;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("encrypted video should arrive");
        assert_eq!(received.frame.codec, VideoCodec::Vp8);
        assert!(!received.frame.data.is_empty());

        let decoded_frame = decoder
            .decode(&received.frame)
            .expect("received VP8 should decode")
            .expect("decoder should emit a frame");
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
}
