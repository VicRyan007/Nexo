use std::time::Duration;

use nexo_core::{
    CallSignal, CallSignalKind, CommunityCredential, DeviceIdentity, NetworkInvite, SignedMessage,
    community_sync_token,
};
use nexo_net::{
    CommunityAck, CommunitySync, DiscoveryEvent, DiscoveryService, SignalRequest, SyncRequest,
};
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn two_nodes_exchange_a_signed_batch_over_loopback() {
    let alice = DeviceIdentity::generate();
    let bob = DeviceIdentity::generate();
    let invite = NetworkInvite::create(&alice, "Amigos", Vec::new(), 100, 600)
        .expect("invite should be created");
    let alice_credential = CommunityCredential::claim(&alice, invite.clone(), 110)
        .expect("credential should be claimed");
    let bob_credential = CommunityCredential::claim(&bob, invite.clone(), 110)
        .expect("credential should be claimed");
    let channel_id = Uuid::new_v5(
        &Uuid::from_u128(0x3a6b_9561_66fd_4f9e_8bb4_1cf2_e033_ea97),
        invite.network_id.as_bytes(),
    );
    let message = SignedMessage::create(
        &alice,
        invite.network_id,
        channel_id,
        "mensagem em loopback".to_owned(),
        120,
    )
    .expect("message should be created");
    let token = community_sync_token(&invite);
    let alice_epoch = Uuid::new_v4();
    let bob_epoch = Uuid::new_v4();

    let mut alice_net = DiscoveryService::start(&alice).expect("alice network should start");
    let mut bob_net = DiscoveryService::start(&bob).expect("bob network should start");
    alice_net
        .update_communities(alice_epoch, vec![token])
        .await
        .expect("alice communities should update");
    bob_net
        .update_communities(bob_epoch, vec![token])
        .await
        .expect("bob communities should update");

    let alice_address = listening_address(&mut alice_net).await;
    bob_net
        .dial(alice_net.local_peer_id(), alice_address)
        .await
        .expect("bob should dial alice");
    wait_connected(&mut bob_net).await;
    bob_net
        .sync_peer(
            alice_net.local_peer_id(),
            SyncRequest::offer(bob.public_key_bytes(), vec![token]),
        )
        .await
        .expect("bob should offer communities");

    let wanted = wait_wanted(&mut bob_net).await;
    assert_eq!(wanted, vec![token]);
    let batch = SyncRequest::batch(
        bob.public_key_bytes(),
        alice_epoch,
        vec![CommunitySync {
            community_id: invite.network_id,
            credentials: vec![alice_credential, bob_credential],
            channels: Vec::new(),
            messages: vec![message.clone()],
            direct_messages: Vec::new(),
            mls_commits: Vec::new(),
            has_more: false,
        }],
    );
    assert!(matches!(
        &batch,
        SyncRequest::Batch { communities, .. } if communities.len() == 1
    ));
    bob_net
        .sync_peer(alice_net.local_peer_id(), batch)
        .await
        .expect("bob should send batch");

    let received = wait_received(&mut alice_net, bob_net.local_peer_id()).await;
    let SyncRequest::Batch { communities, .. } = received else {
        panic!("expected batch");
    };
    assert_eq!(communities.len(), 1);
    assert_eq!(communities[0].messages, vec![message]);

    alice_net
        .sync_peer(
            bob_net.local_peer_id(),
            SyncRequest::ack(
                alice.public_key_bytes(),
                alice_epoch,
                vec![CommunityAck {
                    community_id: invite.network_id,
                    processed_message_ids: vec![communities[0].messages[0].id],
                    processed_direct_message_ids: Vec::new(),
                    processed_mls_commit_ids: Vec::new(),
                    request_next: false,
                }],
            ),
        )
        .await
        .expect("alice should acknowledge batch");
    let acknowledged = wait_acknowledged(&mut bob_net, alice_net.local_peer_id()).await;
    assert!(matches!(acknowledged, SyncRequest::Ack { .. }));

    let signal = CallSignal::create(
        &alice,
        invite.network_id,
        Uuid::new_v4(),
        1,
        CallSignalKind::Offer,
        "v=0\r\n".into(),
        120,
    )
    .expect("call signal should be signed");
    alice_net
        .signal_peer(
            bob_net.local_peer_id(),
            SignalRequest::new(alice.public_key_bytes(), vec![signal.clone()]),
        )
        .await
        .expect("alice should send call signalling");
    let received_signal = wait_call_signal(&mut bob_net, alice_net.local_peer_id()).await;
    assert_eq!(received_signal.signals, vec![signal]);
}

async fn wait_call_signal(
    service: &mut DiscoveryService,
    expected_peer: libp2p::PeerId,
) -> SignalRequest {
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
    .expect("call signal should arrive")
}

async fn wait_acknowledged(
    service: &mut DiscoveryService,
    expected_peer: libp2p::PeerId,
) -> SyncRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(DiscoveryEvent::SyncAcknowledged { peer_id, request }) =
                service.next_event().await
                && peer_id == expected_peer
            {
                return request;
            }
        }
    })
    .await
    .expect("batch acknowledgement should arrive")
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

async fn wait_connected(service: &mut DiscoveryService) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(
                service.next_event().await,
                Some(DiscoveryEvent::PeerConnected(_))
            ) {
                return;
            }
        }
    })
    .await
    .expect("nodes should connect");
}

async fn wait_wanted(service: &mut DiscoveryService) -> Vec<[u8; 32]> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(DiscoveryEvent::SyncWanted { tokens, .. }) = service.next_event().await {
                return tokens;
            }
        }
    })
    .await
    .expect("sync offer should be accepted")
}

async fn wait_received(
    service: &mut DiscoveryService,
    expected_peer: libp2p::PeerId,
) -> SyncRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(DiscoveryEvent::SyncReceived { peer_id, request }) =
                service.next_event().await
                && peer_id == expected_peer
            {
                return request;
            }
        }
    })
    .await
    .expect("batch should arrive")
}
