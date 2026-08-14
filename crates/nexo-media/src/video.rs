//! Encoded video frames for WebRTC transport.
//!
//! [`EncodedVideoFrame`] carries one encoded access unit ready to be
//! packetized into RTP. For VP8 the bytes are a single raw VP8 frame produced
//! by [`crate::Vp8Encoder`]; unlike H.264 there are no parameter sets to carry
//! out of band, so the RTP payloader takes the whole frame as one sample.

use std::time::Duration;

/// The codec an [`EncodedVideoFrame`] was produced with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    /// Google VP8, produced by the software libvpx encoder.
    Vp8,
}

/// One encoded video access unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedVideoFrame {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Media timestamp in seconds since the stream started.
    pub timestamp: Duration,
    /// Raw VP8 frame bytes (intra-frame for keyframes, inter-frame otherwise).
    pub data: Box<[u8]>,
    /// Whether this frame is a keyframe (decodes without any reference frame).
    pub is_keyframe: bool,
}

/// A complete video access unit received from a peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedVideoPacket {
    /// Sequence number of the last RTP packet of the access unit.
    pub sequence_number: u16,
    pub frame: EncodedVideoFrame,
}
