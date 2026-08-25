use std::time::{Duration, Instant};

use nexo_media::{LanPeerConnection, VideoCodec, VideoDecoder, Vp8Encoder};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_peers_exchange_and_decode_vp8_video() {
    let offer_peer = LanPeerConnection::new_with_video_codec(VideoCodec::Vp8)
        .await
        .expect("offer peer should initialize");
    let answer_peer = LanPeerConnection::new_with_video_codec(VideoCodec::Vp8)
        .await
        .expect("answer peer should initialize");

    let offer_sdp = offer_peer
        .create_offer()
        .await
        .expect("video offer should be created");
    let answer_sdp = answer_peer
        .accept_offer(offer_sdp)
        .await
        .expect("video answer should be created");
    offer_peer
        .accept_answer(answer_sdp)
        .await
        .expect("video answer should be applied");
    offer_peer
        .wait_until_connected()
        .await
        .expect("offer video peer should connect");
    answer_peer
        .wait_until_connected()
        .await
        .expect("answer video peer should connect");

    let mut encoder = Vp8Encoder::new(WIDTH, HEIGHT, 500).expect("VP8 encoder should initialize");
    let i420 = synthetic_i420();
    let mut sent = 0;
    for index in 0..4u64 {
        let timestamp = Duration::from_millis(index * 33);
        if let Some(frame) = encoder
            .encode(timestamp, &i420, index == 0)
            .expect("VP8 frame should encode")
        {
            offer_peer
                .send_video(&frame)
                .await
                .expect("VP8 frame should enter RTP");
            sent += 1;
        }
    }
    assert!(sent > 0, "the encoder should emit at least one frame");

    let deadline = Instant::now() + Duration::from_secs(20);
    let received = loop {
        if let Some(packet) = answer_peer
            .try_received_video()
            .expect("video queue should remain open")
        {
            break packet;
        }
        assert!(Instant::now() < deadline, "VP8 frame should arrive");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    assert_eq!(received.frame.codec, VideoCodec::Vp8);
    assert_eq!(received.frame.width, WIDTH);
    assert_eq!(received.frame.height, HEIGHT);
    assert!(!received.frame.data.is_empty());

    let mut video_decoder = VideoDecoder::new().expect("VP8 decoder should initialize");
    let decoded_frame = video_decoder
        .decode(&received.frame)
        .expect("received VP8 frame should decode")
        .expect("received VP8 frame should produce a decoded image");
    assert_eq!(decoded_frame.width, WIDTH);
    assert_eq!(decoded_frame.height, HEIGHT);
    assert!(!decoded_frame.y_plane.is_empty());

    offer_peer.close().await.expect("offer peer should close");
    answer_peer.close().await.expect("answer peer should close");
}

fn synthetic_i420() -> Vec<u8> {
    let y_size = (WIDTH * HEIGHT) as usize;
    let chroma_size = y_size / 4;
    let mut frame = vec![0u8; y_size + chroma_size * 2];
    let width = usize::try_from(WIDTH).unwrap_or_default();
    for (index, value) in frame[..y_size].iter_mut().enumerate() {
        let column = u8::try_from(index % width).unwrap_or_default();
        *value = 32u8.saturating_add(column);
    }
    frame[y_size..y_size + chroma_size].fill(96);
    frame[y_size + chroma_size..].fill(160);
    frame
}
