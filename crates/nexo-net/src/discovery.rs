use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result, anyhow};
use futures::StreamExt as _;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, SwarmBuilder, identify, identity, mdns,
    multiaddr::Protocol,
    noise, ping,
    request_response::{self, ProtocolSupport},
    swarm::{NetworkBehaviour, SwarmEvent, dial_opts::DialOpts},
    tcp, yamux,
};
use tokio::sync::{mpsc, oneshot};

use nexo_core::{DeviceIdentity, peer_sync_token};

use crate::{
    signalling::{SIGNAL_PROTOCOL, SignalRequest, SignalResponse},
    sync::{SYNC_PROTOCOL, SyncRequest, SyncResponse},
};

const EVENT_BUFFER: usize = 128;
const PROTOCOL_VERSION: &str = "/nexo/0.1.0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryEvent {
    Listening(Multiaddr),
    PeerFound {
        peer_id: PeerId,
        address: Multiaddr,
    },
    PeerExpired {
        peer_id: PeerId,
        address: Multiaddr,
    },
    PeerConnected(PeerId),
    PeerDisconnected(PeerId),
    SyncWanted {
        peer_id: PeerId,
        receiver_epoch: uuid::Uuid,
        tokens: Vec<[u8; 32]>,
    },
    SyncReceived {
        peer_id: PeerId,
        request: SyncRequest,
    },
    SyncAcknowledged {
        peer_id: PeerId,
        request: SyncRequest,
    },
    CallSignalsReceived {
        peer_id: PeerId,
        request: SignalRequest,
    },
}

pub struct DiscoveryService {
    local_peer_id: PeerId,
    events: mpsc::Receiver<DiscoveryEvent>,
    commands: mpsc::Sender<DiscoveryCommand>,
    shutdown: Option<oneshot::Sender<()>>,
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    sync: request_response::cbor::Behaviour<SyncRequest, SyncResponse>,
    signalling: request_response::cbor::Behaviour<SignalRequest, SignalResponse>,
}

enum DiscoveryCommand {
    Dial {
        peer_id: PeerId,
        address: Multiaddr,
    },
    SyncAll(SyncRequest),
    SyncPeer {
        peer_id: PeerId,
        request: SyncRequest,
    },
    SignalPeer {
        peer_id: PeerId,
        request: SignalRequest,
    },
    UpdateCommunities {
        tokens: Vec<[u8; 32]>,
        receiver_epoch: uuid::Uuid,
        applied: oneshot::Sender<()>,
    },
}

impl DiscoveryService {
    #[allow(clippy::too_many_lines)]
    pub fn start(device_identity: &DeviceIdentity) -> Result<Self> {
        let secret = device_identity.secret_key_bytes();
        let local_key = identity::Keypair::ed25519_from_bytes(secret)
            .map_err(|error| anyhow!("invalid persisted device key: {error}"))?;
        let local_peer_id = local_key.public().to_peer_id();
        let mut swarm = SwarmBuilder::with_existing_identity(local_key)
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_quic()
            .with_behaviour(|key| {
                Ok(Behaviour {
                    mdns: mdns::tokio::Behaviour::new(
                        mdns::Config::default(),
                        key.public().to_peer_id(),
                    )?,
                    identify: identify::Behaviour::new(identify::Config::new(
                        PROTOCOL_VERSION.into(),
                        key.public(),
                    )),
                    ping: ping::Behaviour::new(
                        ping::Config::new().with_interval(Duration::from_secs(15)),
                    ),
                    sync: request_response::cbor::Behaviour::with_codec(
                        request_response::cbor::codec::Codec::default()
                            .set_request_size_maximum(512 * 1024)
                            .set_response_size_maximum(512 * 1024),
                        [(StreamProtocol::new(SYNC_PROTOCOL), ProtocolSupport::Full)],
                        request_response::Config::default()
                            .with_request_timeout(Duration::from_secs(15)),
                    ),
                    signalling: request_response::cbor::Behaviour::with_codec(
                        request_response::cbor::codec::Codec::default()
                            .set_request_size_maximum(512 * 1024)
                            .set_response_size_maximum(1024),
                        [(StreamProtocol::new(SIGNAL_PROTOCOL), ProtocolSupport::Full)],
                        request_response::Config::default()
                            .with_request_timeout(Duration::from_secs(10)),
                    ),
                })
            })?
            .build();

        swarm
            .listen_on("/ip4/0.0.0.0/tcp/0".parse()?)
            .context("failed to start the TCP listener")?;
        swarm
            .listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)
            .context("failed to start the QUIC listener")?;

        let (event_tx, events) = mpsc::channel(EVENT_BUFFER);
        let (command_tx, mut commands) = mpsc::channel(EVENT_BUFFER);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut connected = HashSet::new();
            let mut local_communities = HashSet::new();
            let mut receiver_epoch = uuid::Uuid::nil();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    command = commands.recv() => {
                        let Some(command) = command else { break };
                        match command {
                            DiscoveryCommand::Dial { peer_id, address } => {
                                let options = DialOpts::peer_id(peer_id)
                                    .addresses(vec![address])
                                    .build();
                                let _ = swarm.dial(options);
                            }
                            DiscoveryCommand::SyncAll(request) => {
                                for peer_id in &connected {
                                    let request = request_for_peer(
                                        request.clone(),
                                        swarm.local_peer_id(),
                                        peer_id,
                                    );
                                    swarm.behaviour_mut().sync.send_request(peer_id, request);
                                }
                            }
                            DiscoveryCommand::SyncPeer { peer_id, request } => {
                                let request = request_for_peer(
                                    request,
                                    swarm.local_peer_id(),
                                    &peer_id,
                                );
                                swarm.behaviour_mut().sync.send_request(&peer_id, request);
                            }
                            DiscoveryCommand::SignalPeer { peer_id, request } => {
                                swarm.behaviour_mut().signalling.send_request(&peer_id, request);
                            }
                            DiscoveryCommand::UpdateCommunities { tokens, receiver_epoch: epoch, applied } => {
                                local_communities = tokens.into_iter().collect();
                                receiver_epoch = epoch;
                                let _ = applied.send(());
                            }
                        }
                    }
                    event = swarm.select_next_some() => {
                        if let SwarmEvent::Behaviour(BehaviourEvent::Mdns(
                            mdns::Event::Discovered(peers),
                        )) = &event
                        {
                            for (peer_id, address) in peers {
                                let options = DialOpts::peer_id(*peer_id)
                                    .addresses(vec![address.clone()])
                                    .build();
                                let _ = swarm.dial(options);
                            }
                        }
                        let outgoing = handle_event(
                            event,
                            &mut swarm,
                            &mut connected,
                            &local_communities,
                            receiver_epoch,
                        );
                        for event in outgoing {
                            if event_tx.send(event).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            local_peer_id,
            events,
            commands: command_tx,
            shutdown: Some(shutdown_tx),
        })
    }

    #[must_use]
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    pub async fn next_event(&mut self) -> Option<DiscoveryEvent> {
        self.events.recv().await
    }

    pub async fn sync_all(&self, request: SyncRequest) -> Result<()> {
        self.commands
            .send(DiscoveryCommand::SyncAll(request))
            .await
            .context("the discovery service has stopped")
    }

    pub async fn dial(&self, peer_id: PeerId, address: Multiaddr) -> Result<()> {
        self.commands
            .send(DiscoveryCommand::Dial { peer_id, address })
            .await
            .context("the discovery service has stopped")
    }

    pub async fn dial_invite_address(&self, value: &str) -> Result<()> {
        let (peer_id, address) = parse_invite_address(value)?;
        self.dial(peer_id, address).await
    }

    pub async fn sync_peer(&self, peer_id: PeerId, request: SyncRequest) -> Result<()> {
        self.commands
            .send(DiscoveryCommand::SyncPeer { peer_id, request })
            .await
            .context("the discovery service has stopped")
    }

    pub async fn signal_peer(&self, peer_id: PeerId, request: SignalRequest) -> Result<()> {
        self.commands
            .send(DiscoveryCommand::SignalPeer { peer_id, request })
            .await
            .context("the discovery service has stopped")
    }

    pub async fn update_communities(
        &self,
        receiver_epoch: uuid::Uuid,
        tokens: Vec<[u8; 32]>,
    ) -> Result<()> {
        let (applied, confirmation) = oneshot::channel();
        self.commands
            .send(DiscoveryCommand::UpdateCommunities {
                tokens,
                receiver_epoch,
                applied,
            })
            .await
            .context("the discovery service has stopped")?;
        confirmation
            .await
            .context("the discovery service stopped before applying communities")
    }
}

fn parse_invite_address(value: &str) -> Result<(PeerId, Multiaddr)> {
    let mut address: Multiaddr = value
        .parse()
        .with_context(|| format!("invalid invitation address: {value}"))?;
    let Some(Protocol::P2p(peer_id)) = address.pop() else {
        return Err(anyhow!("invitation address has no peer identity"));
    };
    if !address.iter().any(|protocol| {
        matches!(
            protocol,
            Protocol::Tcp(_) | Protocol::QuicV1 | Protocol::WebTransport
        )
    }) {
        return Err(anyhow!("invitation address has no supported transport"));
    }
    Ok((peer_id, address))
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

#[allow(clippy::too_many_lines)]
fn handle_event(
    event: SwarmEvent<BehaviourEvent>,
    swarm: &mut libp2p::Swarm<Behaviour>,
    connected: &mut HashSet<PeerId>,
    local_communities: &HashSet<[u8; 32]>,
    receiver_epoch: uuid::Uuid,
) -> Vec<DiscoveryEvent> {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => vec![DiscoveryEvent::Listening(address)],
        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
            connected.insert(peer_id);
            vec![DiscoveryEvent::PeerConnected(peer_id)]
        }
        SwarmEvent::ConnectionClosed { peer_id, .. } => {
            connected.remove(&peer_id);
            vec![DiscoveryEvent::PeerDisconnected(peer_id)]
        }
        SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(peers))) => peers
            .into_iter()
            .map(|(peer_id, address)| DiscoveryEvent::PeerFound { peer_id, address })
            .collect(),
        SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Expired(peers))) => peers
            .into_iter()
            .map(|(peer_id, address)| DiscoveryEvent::PeerExpired { peer_id, address })
            .collect(),
        SwarmEvent::Behaviour(BehaviourEvent::Sync(request_response::Event::Message {
            peer,
            message,
            ..
        })) => match message {
            request_response::Message::Request {
                request, channel, ..
            } => {
                if !request.is_within_limits()
                    || peer_id_for_key(request.device_key()) != Some(peer)
                {
                    let _ = swarm
                        .behaviour_mut()
                        .sync
                        .send_response(channel, SyncResponse::received());
                    return Vec::new();
                }
                match &request {
                    SyncRequest::Offer { tokens, .. } => {
                        let local_peer = swarm.local_peer_id().to_bytes();
                        let remote_peer = peer.to_bytes();
                        let wanted = tokens
                            .iter()
                            .filter(|offered| {
                                local_communities.iter().any(|local| {
                                    peer_sync_token(local, &local_peer, &remote_peer) == **offered
                                })
                            })
                            .copied()
                            .collect();
                        let _ = swarm
                            .behaviour_mut()
                            .sync
                            .send_response(channel, SyncResponse::wanted(receiver_epoch, wanted));
                        Vec::new()
                    }
                    SyncRequest::Batch { .. } => {
                        let _ = swarm
                            .behaviour_mut()
                            .sync
                            .send_response(channel, SyncResponse::received());
                        vec![DiscoveryEvent::SyncReceived {
                            peer_id: peer,
                            request,
                        }]
                    }
                    SyncRequest::Ack { .. } => {
                        let _ = swarm
                            .behaviour_mut()
                            .sync
                            .send_response(channel, SyncResponse::received());
                        vec![DiscoveryEvent::SyncAcknowledged {
                            peer_id: peer,
                            request,
                        }]
                    }
                }
            }
            request_response::Message::Response { response, .. } => match response {
                SyncResponse::Wanted {
                    version,
                    receiver_epoch,
                    tokens,
                } if version == crate::sync::SYNC_VERSION
                    && tokens.len() <= crate::sync::MAX_COMMUNITIES_PER_SYNC =>
                {
                    let local_peer = swarm.local_peer_id().to_bytes();
                    let remote_peer = peer.to_bytes();
                    let tokens = local_communities
                        .iter()
                        .filter(|local| {
                            tokens.contains(&peer_sync_token(local, &local_peer, &remote_peer))
                        })
                        .copied()
                        .collect();
                    vec![DiscoveryEvent::SyncWanted {
                        peer_id: peer,
                        receiver_epoch,
                        tokens,
                    }]
                }
                SyncResponse::Received { .. } | SyncResponse::Wanted { .. } => Vec::new(),
            },
        },
        SwarmEvent::Behaviour(BehaviourEvent::Signalling(request_response::Event::Message {
            peer,
            message,
            ..
        })) => match message {
            request_response::Message::Request {
                request, channel, ..
            } => {
                let valid_transport = request.is_within_limits()
                    && peer_id_for_key(&request.device_key) == Some(peer);
                let accepted = if valid_transport {
                    request.signals.len()
                } else {
                    0
                };
                let _ = swarm
                    .behaviour_mut()
                    .signalling
                    .send_response(channel, SignalResponse::received(accepted));
                valid_transport
                    .then_some(DiscoveryEvent::CallSignalsReceived {
                        peer_id: peer,
                        request,
                    })
                    .into_iter()
                    .collect()
            }
            request_response::Message::Response { .. } => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn request_for_peer(request: SyncRequest, local: &PeerId, remote: &PeerId) -> SyncRequest {
    match request {
        SyncRequest::Offer {
            version,
            device_key,
            tokens,
        } => SyncRequest::Offer {
            version,
            device_key,
            tokens: tokens
                .iter()
                .map(|token| peer_sync_token(token, &local.to_bytes(), &remote.to_bytes()))
                .collect(),
        },
        other @ (SyncRequest::Batch { .. } | SyncRequest::Ack { .. }) => other,
    }
}

fn peer_id_for_key(public_key: &[u8; 32]) -> Option<PeerId> {
    let ed25519 = identity::ed25519::PublicKey::try_from_bytes(public_key).ok()?;
    Some(identity::PublicKey::from(ed25519).to_peer_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_offer_matches_from_both_sides() {
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let alice_peer = peer_id_for_key(&alice.public_key_bytes())
            .expect("generated identity should map to a peer id");
        let bob_peer = peer_id_for_key(&bob.public_key_bytes())
            .expect("generated identity should map to a peer id");
        let raw = [7_u8; 32];
        let SyncRequest::Offer { tokens, .. } = request_for_peer(
            SyncRequest::offer(bob.public_key_bytes(), vec![raw]),
            &bob_peer,
            &alice_peer,
        ) else {
            unreachable!();
        };
        assert_eq!(
            tokens[0],
            peer_sync_token(&raw, &alice_peer.to_bytes(), &bob_peer.to_bytes())
        );
    }

    #[test]
    fn invitation_address_extracts_authenticated_peer() {
        let identity = DeviceIdentity::generate();
        let peer = peer_id_for_key(&identity.public_key_bytes())
            .expect("generated identity should map to a peer id");
        let value = format!("/ip4/192.168.1.20/tcp/4242/p2p/{peer}");
        let (parsed_peer, address) =
            parse_invite_address(&value).expect("invitation address should parse");
        assert_eq!(parsed_peer, peer);
        assert_eq!(address.to_string(), "/ip4/192.168.1.20/tcp/4242");
        assert!(parse_invite_address("/ip4/192.168.1.20/tcp/4242").is_err());
    }
}
