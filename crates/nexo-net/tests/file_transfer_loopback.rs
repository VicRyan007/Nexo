use std::time::Duration;

use nexo_core::{DeviceIdentity, FileChunk, FileTransferOffer, current_timestamp};
use nexo_net::{DiscoveryEvent, DiscoveryService, FileTransferResponse};
use uuid::Uuid;

#[tokio::test]
async fn two_nodes_exchange_a_signed_file_and_chunk_over_loopback() {
    let alice = DeviceIdentity::generate();
    let bob = DeviceIdentity::generate();
    let community_id = Uuid::new_v4();
    let channel_id = Uuid::new_v4();
    let payload = b"arquivo local-first".to_vec();
    let offer = FileTransferOffer::create(
        &alice,
        community_id,
        channel_id,
        "nota.txt".to_owned(),
        payload.len() as u64,
        "text/plain".to_owned(),
        nexo_core::compute_sha256(&payload),
        current_timestamp(),
    )
    .expect("offer should be created");
    let chunk = FileChunk::new(offer.id, 0, payload.clone());

    let mut alice_net = DiscoveryService::start(&alice).expect("alice network should start");
    let mut bob_net = DiscoveryService::start(&bob).expect("bob network should start");
    let alice_address = listening_address(&mut alice_net).await;
    bob_net
        .dial(alice_net.local_peer_id(), alice_address)
        .await
        .expect("bob should dial alice");
    wait_connected(&mut bob_net).await;
    wait_connected(&mut alice_net).await;

    alice_net
        .broadcast_file(
            offer.clone(),
            vec![chunk.clone()],
            vec![bob_net.local_peer_id()],
        )
        .await
        .expect("alice should offer the file");

    let (peer_id, channel) = wait_offer(&mut bob_net, alice_net.local_peer_id()).await;
    assert_eq!(peer_id, alice_net.local_peer_id());
    bob_net
        .respond_file_offer(
            channel,
            FileTransferResponse::OfferAccepted {
                transfer_id: offer.id,
            },
        )
        .await
        .expect("bob should accept the offer");
    assert!(matches!(
        wait_response(&mut alice_net, bob_net.local_peer_id()).await,
        FileTransferResponse::OfferAccepted { transfer_id } if transfer_id == offer.id
    ));

    bob_net
        .request_file_chunk(alice_net.local_peer_id(), offer.id, 0)
        .await
        .expect("bob should request the first chunk");
    assert!(matches!(
        wait_response(&mut bob_net, alice_net.local_peer_id()).await,
        FileTransferResponse::Chunk(received) if received == chunk
    ));
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

async fn wait_offer(
    service: &mut DiscoveryService,
    expected_peer: libp2p::PeerId,
) -> (libp2p::PeerId, nexo_net::FileOfferResponseChannel) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(DiscoveryEvent::FileOfferReceived {
                peer_id, channel, ..
            }) = service.next_event().await
                && peer_id == expected_peer
            {
                return (peer_id, channel);
            }
        }
    })
    .await
    .expect("file offer should arrive")
}

async fn wait_response(
    service: &mut DiscoveryService,
    expected_peer: libp2p::PeerId,
) -> FileTransferResponse {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(DiscoveryEvent::FileResponseReceived { peer_id, response }) =
                service.next_event().await
                && peer_id == expected_peer
            {
                return response;
            }
        }
    })
    .await
    .expect("file response should arrive")
}
