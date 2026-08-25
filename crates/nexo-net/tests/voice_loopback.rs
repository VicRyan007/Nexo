use std::{f32::consts::TAU, path::PathBuf, time::Duration};

use nexo_core::{
    CallNegotiationRole, CallSignal, CallSignalKind, CommunityCredential, DeviceIdentity,
    NetworkInvite, call_negotiation_role, current_timestamp,
};
use nexo_media::{
    AudioFrame, LanPeerConnection, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, VoiceDecoder, VoiceEncoder,
};
use nexo_net::{DiscoveryEvent, DiscoveryService, SignalRequest};
use nexo_store::LocalStore;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn two_authorized_nodes_negotiate_and_exchange_voice() {
    let now = current_timestamp();
    let alice = DeviceIdentity::generate();
    let bob = DeviceIdentity::generate();
    let invite = NetworkInvite::create(&alice, "Voice integration", Vec::new(), now, 600)
        .expect("invite should be created");
    let alice_credential = CommunityCredential::claim(&alice, invite.clone(), now)
        .expect("alice credential should be claimed");
    let bob_credential = CommunityCredential::claim(&bob, invite.clone(), now)
        .expect("bob credential should be claimed");
    let alice_path = temporary_database("alice");
    let bob_path = temporary_database("bob");
    let mut alice_store = LocalStore::open(&alice_path).expect("alice store should open");
    let mut bob_store = LocalStore::open(&bob_path).expect("bob store should open");
    alice_store
        .create_community(invite.network_id, &invite.network_name, now)
        .expect("alice community should exist");
    bob_store
        .create_community(invite.network_id, &invite.network_name, now)
        .expect("bob community should exist");
    alice_store
        .save_credential(&alice_credential)
        .expect("alice credential should persist");
    bob_store
        .save_credential(&bob_credential)
        .expect("bob credential should persist");
    alice_store
        .authorize_member(invite.network_id, &alice.public_key_bytes(), now)
        .expect("alice should authorize herself");
    bob_store
        .authorize_member(invite.network_id, &bob.public_key_bytes(), now)
        .expect("bob should authorize himself");
    alice_store
        .import_credential(&bob_credential, now)
        .expect("alice should import bob membership");
    bob_store
        .import_credential(&alice_credential, now)
        .expect("bob should import alice membership");

    let mut alice_network = DiscoveryService::start(&alice).expect("alice network should start");
    let mut bob_network = DiscoveryService::start(&bob).expect("bob network should start");
    let alice_address = listening_address(&mut alice_network).await;
    let alice_peer = alice_network.local_peer_id();
    connect_nodes(
        &mut alice_network,
        &mut bob_network,
        alice_peer,
        alice_address,
    )
    .await;

    let alice_role = call_negotiation_role(&alice.public_key_bytes(), &bob.public_key_bytes())
        .expect("distinct identities need roles");
    let call_id = Uuid::new_v4();
    let (offer_identity, answer_identity, offer_store, answer_store, offer_network, answer_network) =
        if alice_role == CallNegotiationRole::Offerer {
            (
                &alice,
                &bob,
                &alice_store,
                &bob_store,
                &mut alice_network,
                &mut bob_network,
            )
        } else {
            (
                &bob,
                &alice,
                &bob_store,
                &alice_store,
                &mut bob_network,
                &mut alice_network,
            )
        };

    let presence = CallSignal::create(
        offer_identity,
        invite.network_id,
        call_id,
        1,
        CallSignalKind::ParticipantState,
        "join".to_owned(),
        now,
    )
    .expect("presence should be signed");
    send_signal(offer_network, answer_network, presence.clone()).await;
    assert!(
        answer_store
            .accept_call_signal(&presence, current_timestamp())
            .expect("presence should be authorized")
    );

    let offer_peer = LanPeerConnection::new()
        .await
        .expect("offer WebRTC peer should initialize");
    let answer_peer = LanPeerConnection::new()
        .await
        .expect("answer WebRTC peer should initialize");
    let offer_sdp = offer_peer
        .create_offer()
        .await
        .expect("real audio offer should be created");
    let offer_signal = CallSignal::create(
        offer_identity,
        invite.network_id,
        call_id,
        2,
        CallSignalKind::Offer,
        offer_sdp,
        current_timestamp(),
    )
    .expect("offer should be signed");
    send_signal(offer_network, answer_network, offer_signal.clone()).await;
    assert!(
        answer_store
            .accept_call_signal(&offer_signal, current_timestamp())
            .expect("offer should be authorized")
    );
    let answer_sdp = answer_peer
        .accept_offer(offer_signal.payload)
        .await
        .expect("real audio answer should be created");

    let answer_signal = CallSignal::create(
        answer_identity,
        invite.network_id,
        call_id,
        1,
        CallSignalKind::Answer,
        answer_sdp,
        current_timestamp(),
    )
    .expect("answer should be signed");
    send_signal(answer_network, offer_network, answer_signal.clone()).await;
    assert!(
        offer_store
            .accept_call_signal(&answer_signal, current_timestamp())
            .expect("answer should be authorized")
    );
    offer_peer
        .accept_answer(answer_signal.payload)
        .await
        .expect("answer should be applied");
    offer_peer
        .wait_until_connected()
        .await
        .expect("offer peer should connect");
    answer_peer
        .wait_until_connected()
        .await
        .expect("answer peer should connect");

    let input = synthetic_voice_frame();
    let mut encoder = VoiceEncoder::new().expect("encoder should initialize");
    let packet = encoder.encode(&input).expect("voice should encode");
    offer_peer
        .send_audio(&packet)
        .await
        .expect("voice should enter SRTP");
    // Waits below use generous wall-clock budgets because `cargo test
    // --workspace` runs several heavy integration tests concurrently (the
    // two-instance app test saturates the CPU for ~40s), which can stall the
    // libp2p/QUIC handshake and SRTP delivery. Timeouts only bound the wait;
    // the assertions themselves are unchanged.
    let received = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(frame) = answer_peer
                .try_received_audio()
                .expect("audio queue should remain open")
            {
                break frame;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("voice should arrive");
    let mut decoder = VoiceDecoder::new().expect("decoder should initialize");
    let output = decoder
        .decode(&received.frame)
        .expect("voice should decode");
    assert!(output.samples.iter().any(|sample| sample.abs() > 0.001));

    offer_peer.close().await.expect("offer peer should close");
    answer_peer.close().await.expect("answer peer should close");
    drop(alice_store);
    drop(bob_store);
    remove_database(&alice_path);
    remove_database(&bob_path);
}

async fn send_signal(
    sender: &mut DiscoveryService,
    receiver: &mut DiscoveryService,
    signal: CallSignal,
) {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let _ = sender
                .signal_peer(
                    receiver.local_peer_id(),
                    SignalRequest::new(signal.author_key, vec![signal.clone()]),
                )
                .await;
            if let Some(request) = wait_call_signal(receiver, sender.local_peer_id()).await {
                assert_eq!(request.signals, vec![signal]);
                return;
            }
        }
    })
    .await
    .expect("signed call signal should arrive");
}

async fn listening_address(service: &mut DiscoveryService) -> libp2p::Multiaddr {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(DiscoveryEvent::Listening(address)) = service.next_event().await
                && address
                    .iter()
                    .any(|protocol| matches!(protocol, libp2p::multiaddr::Protocol::Tcp(_)))
            {
                return address;
            }
        }
    })
    .await
    .expect("listener should start")
}

/// Connect `initiator` to `responder` and wait until both sides observe the link.
///
/// Under the parallel `cargo test --workspace` run the CPU is saturated by the
/// two-instance app test for ~40s, so a single libp2p dial can exceed the
/// default connection timeout and never surface `PeerConnected`. Re-dialing
/// until both sides connect keeps the assertion meaningful without betting on a
/// fixed wall-clock budget.
async fn connect_nodes(
    responder: &mut DiscoveryService,
    initiator: &mut DiscoveryService,
    responder_peer: libp2p::PeerId,
    responder_address: libp2p::Multiaddr,
) {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let _ = initiator
                .dial(responder_peer, responder_address.clone())
                .await;
            let initiator_ok = peer_connected(initiator, responder_peer).await;
            let responder_ok = peer_connected(responder, initiator.local_peer_id()).await;
            if initiator_ok && responder_ok {
                return;
            }
        }
    })
    .await
    .expect("nodes should connect");
}

async fn peer_connected(service: &mut DiscoveryService, expected_peer: libp2p::PeerId) -> bool {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if matches!(
                service.next_event().await,
                Some(DiscoveryEvent::PeerConnected(peer_id)) if peer_id == expected_peer
            ) {
                return;
            }
        }
    })
    .await
    .is_ok()
}

async fn wait_call_signal(
    service: &mut DiscoveryService,
    expected_peer: libp2p::PeerId,
) -> Option<SignalRequest> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(DiscoveryEvent::CallSignalsReceived { peer_id, request }) =
                service.next_event().await
                && peer_id == expected_peer
            {
                return request;
            }
        }
    })
    .await
    .ok()
}

fn synthetic_voice_frame() -> AudioFrame {
    AudioFrame {
        samples: (0..OPUS_FRAME_SAMPLES)
            .map(|index| {
                #[allow(clippy::cast_precision_loss)]
                let time = index as f32 / OPUS_SAMPLE_RATE as f32;
                (TAU * 440.0 * time).sin() * 0.25
            })
            .collect(),
        sample_rate: OPUS_SAMPLE_RATE,
    }
}

fn temporary_database(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nexo-voice-{label}-{}.sqlite3", Uuid::new_v4()))
}

fn remove_database(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
}
