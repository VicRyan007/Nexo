use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use futures::StreamExt as _;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, SwarmBuilder, dcutr, identify, identity, kad, mdns,
    multiaddr::Protocol,
    noise, ping, relay,
    request_response::{self, ProtocolSupport},
    swarm::{NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle, dial_opts::DialOpts},
    tcp, yamux,
};
use tokio::sync::{mpsc, oneshot};

use nexo_core::{DeviceIdentity, FileTransferOffer, current_timestamp, peer_sync_token};

use crate::{
    file_transfer::{FILE_TRANSFER_PROTOCOL, FileTransferRequest, FileTransferResponse},
    signalling::{SIGNAL_PROTOCOL, SignalRateLimiter, SignalRequest, SignalResponse},
    sync::{
        MAX_ADVERTISEMENT_ADDRESS_BYTES, MAX_ADVERTISEMENT_PEER_ID_BYTES, PeerAddressAdvertisement,
        SYNC_PROTOCOL, SyncRequest, SyncResponse,
    },
};

const EVENT_BUFFER: usize = 128;
const PROTOCOL_VERSION: &str = "/nexo/0.1.0";
const MAX_ACTIVE_FILE_SOURCES: usize = 2;
const MAX_KNOWN_ADDRESSES_PER_PEER: usize = 8;
const MAX_KNOWN_PEERS: usize = 256;
const MAX_CONCURRENT_DIALS: usize = 8;
const DIAL_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const MAX_DIAL_BACKOFF: Duration = Duration::from_secs(32);
const MAX_RELAY_SERVERS: usize = 4;
const DEFAULT_RELAY_LISTEN_PORT: u16 = 4001;

#[derive(Debug)]
pub enum DiscoveryEvent {
    Listening(Multiaddr),
    NetworkWarning(String),
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
        device_key: [u8; 32],
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
    FileOfferReceived {
        peer_id: PeerId,
        offer: FileTransferOffer,
        channel: request_response::ResponseChannel<FileTransferResponse>,
    },
    FileResponseReceived {
        peer_id: PeerId,
        response: FileTransferResponse,
    },
}

pub type FileOfferResponseChannel = request_response::ResponseChannel<FileTransferResponse>;

pub struct DiscoveryService {
    local_peer_id: PeerId,
    events: mpsc::Receiver<DiscoveryEvent>,
    commands: mpsc::Sender<DiscoveryCommand>,
    shutdown: Option<oneshot::Sender<()>>,
}

#[derive(Clone, Debug)]
struct DiscoveryConfig {
    bootstrap_peers: Vec<(PeerId, Multiaddr)>,
    relay_servers: Vec<(PeerId, Multiaddr)>,
    relay_server_enabled: bool,
    relay_listen_port: u16,
    relay_public_addresses: Vec<Multiaddr>,
    mdns_enabled: bool,
}

impl DiscoveryConfig {
    fn from_environment() -> Self {
        Self {
            bootstrap_peers: configured_bootstrap_peers(),
            relay_servers: configured_relay_servers(),
            relay_server_enabled: relay_server_enabled(),
            relay_listen_port: relay_listen_port(),
            relay_public_addresses: configured_relay_public_addresses(),
            mdns_enabled: mdns_enabled(),
        }
    }
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    dcutr: dcutr::Behaviour,
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    ping: ping::Behaviour,
    relay: relay::client::Behaviour,
    relay_server: relay::Behaviour,
    sync: request_response::cbor::Behaviour<SyncRequest, SyncResponse>,
    signalling: request_response::cbor::Behaviour<SignalRequest, SignalResponse>,
    files: request_response::cbor::Behaviour<FileTransferRequest, FileTransferResponse>,
}

struct DialAttempt {
    next_attempt: Instant,
    failures: u8,
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
    BroadcastFile {
        offer: FileTransferOffer,
        chunks: Vec<nexo_core::FileChunk>,
        peers: Vec<PeerId>,
    },
    RespondFileOffer {
        channel: FileOfferResponseChannel,
        response: FileTransferResponse,
    },
    RequestFileChunk {
        peer_id: PeerId,
        transfer_id: uuid::Uuid,
        chunk_index: u32,
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
        Self::start_with_config(device_identity, DiscoveryConfig::from_environment())
    }

    #[allow(clippy::too_many_lines)]
    fn start_with_config(
        device_identity: &DeviceIdentity,
        config: DiscoveryConfig,
    ) -> Result<Self> {
        let secret = device_identity.secret_key_bytes();
        let local_device_key = device_identity.public_key_bytes();
        let local_key = identity::Keypair::ed25519_from_bytes(secret)
            .map_err(|error| anyhow!("invalid persisted device key: {error}"))?;
        let local_peer_id = local_key.public().to_peer_id();
        let relay_behaviour_config = relay_server_config(config.relay_server_enabled);
        let listen_port = config.relay_listen_port;
        let mut swarm = SwarmBuilder::with_existing_identity(local_key)
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_quic()
            .with_relay_client(noise::Config::new, yamux::Config::default)?
            .with_behaviour(|key, relay| {
                Ok(Behaviour {
                    dcutr: dcutr::Behaviour::new(key.public().to_peer_id()),
                    mdns: if config.mdns_enabled {
                        Some(mdns::tokio::Behaviour::new(
                            mdns::Config::default(),
                            key.public().to_peer_id(),
                        )?)
                    } else {
                        None
                    }
                    .into(),
                    identify: identify::Behaviour::new(identify::Config::new(
                        PROTOCOL_VERSION.into(),
                        key.public(),
                    )),
                    // Kademlia is used as an optional address-discovery layer. Nexo does
                    // not store application records there, so keep its routing and record
                    // caches bounded even when a bootstrap node is configured.
                    kad: {
                        let kbucket_size = NonZeroUsize::new(20)
                            .ok_or_else(|| anyhow!("Kademlia k-bucket size must be non-zero"))?;
                        let mut config = kad::Config::default();
                        config
                            .set_kbucket_size(kbucket_size)
                            .set_query_timeout(Duration::from_secs(30))
                            .set_substreams_timeout(Duration::from_secs(10))
                            .set_periodic_bootstrap_interval(None);

                        let store_config = kad::store::MemoryStoreConfig {
                            max_records: 256,
                            max_value_bytes: 64 * 1024,
                            max_providers_per_key: 16,
                            max_provided_keys: 64,
                        };

                        kad::Behaviour::with_config(
                            key.public().to_peer_id(),
                            kad::store::MemoryStore::with_config(
                                key.public().to_peer_id(),
                                store_config,
                            ),
                            config,
                        )
                    },
                    ping: ping::Behaviour::new(
                        ping::Config::new().with_interval(Duration::from_secs(15)),
                    ),
                    relay,
                    relay_server: relay::Behaviour::new(
                        key.public().to_peer_id(),
                        relay_behaviour_config,
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
                    files: request_response::cbor::Behaviour::with_codec(
                        request_response::cbor::codec::Codec::default()
                            .set_request_size_maximum(128 * 1024)
                            .set_response_size_maximum(128 * 1024),
                        [(
                            StreamProtocol::new(FILE_TRANSFER_PROTOCOL),
                            ProtocolSupport::Full,
                        )],
                        request_response::Config::default()
                            .with_request_timeout(Duration::from_secs(30)),
                    ),
                })
            })?
            .build();

        swarm
            .listen_on(format!("/ip4/0.0.0.0/tcp/{listen_port}").parse()?)
            .context("failed to start the TCP listener")?;
        swarm
            .listen_on(format!("/ip4/0.0.0.0/udp/{listen_port}/quic-v1").parse()?)
            .context("failed to start the QUIC listener")?;

        let (event_tx, events) = mpsc::channel(EVENT_BUFFER);
        let (command_tx, mut commands) = mpsc::channel(EVENT_BUFFER);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let relay_server_enabled = config.relay_server_enabled;
        let relay_public_addresses = config.relay_public_addresses.clone();
        tokio::spawn(async move {
            let mut connected = HashSet::new();
            let mut active_dials = HashSet::new();
            let mut relay_reservations = HashSet::new();
            let mut signal_limiter = SignalRateLimiter::default();
            let mut known_addresses = HashMap::<PeerId, VecDeque<Multiaddr>>::new();
            let mut dial_attempts = HashMap::<PeerId, DialAttempt>::new();
            let mut file_sources: HashMap<uuid::Uuid, Vec<nexo_core::FileChunk>> = HashMap::new();
            let mut local_communities = HashSet::new();
            let mut receiver_epoch = uuid::Uuid::nil();
            let mut dial_retry_interval = tokio::time::interval(DIAL_RETRY_INTERVAL);
            let bootstrap_peers = config.bootstrap_peers;
            let relay_servers = config.relay_servers;
            let relay_peers = relay_servers
                .iter()
                .map(|(peer_id, _)| *peer_id)
                .collect::<HashSet<_>>();
            let mut relay_public_addresses_added = false;
            let kad_enabled = !bootstrap_peers.is_empty();
            for (peer_id, address) in &bootstrap_peers {
                swarm
                    .behaviour_mut()
                    .kad
                    .add_address(peer_id, address.clone());
                remember_peer_address(&mut known_addresses, *peer_id, address.clone());
            }
            for (relay_peer_id, relay_address) in &relay_servers {
                remember_peer_address(&mut known_addresses, *relay_peer_id, relay_address.clone());
            }
            if !bootstrap_peers.is_empty() {
                let _ = swarm.behaviour_mut().kad.bootstrap();
            }
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    _ = dial_retry_interval.tick() => {
                        signal_limiter.prune_idle(Instant::now());
                        let peers = known_addresses.keys().copied().collect::<Vec<_>>();
                        for peer_id in peers {
                            if !connected.contains(&peer_id)
                                && let Some(addresses) = known_addresses.get(&peer_id)
                            {
                                dial_peer_with_backoff(
                                    &mut swarm,
                                    peer_id,
                                    addresses,
                                    &mut dial_attempts,
                                    &mut active_dials,
                                );
                            }
                        }
                    }
                    command = commands.recv() => {
                        let Some(command) = command else { break };
                        match command {
                            DiscoveryCommand::Dial { peer_id, address } => {
                                remember_peer_address(&mut known_addresses, peer_id, address);
                                if let Some(addresses) = known_addresses.get(&peer_id) {
                                    dial_peer_with_backoff(
                                        &mut swarm,
                                        peer_id,
                                        addresses,
                                        &mut dial_attempts,
                                        &mut active_dials,
                                    );
                                }
                            }
                            DiscoveryCommand::SyncAll(request) => {
                                let request = request.with_known_peers(known_peer_advertisements(
                                    &known_addresses,
                                    *swarm.local_peer_id(),
                                    &connected,
                                ));
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
                                let request = request.with_known_peers(known_peer_advertisements(
                                    &known_addresses,
                                    *swarm.local_peer_id(),
                                    &connected,
                                ));
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
                            DiscoveryCommand::BroadcastFile {
                                offer,
                                chunks,
                                peers,
                            } => {
                                let transfer_id = offer.id;
                                if file_sources.len() >= MAX_ACTIVE_FILE_SOURCES
                                    && !file_sources.contains_key(&transfer_id)
                                    && let Some(oldest) = file_sources.keys().next().copied()
                                {
                                    file_sources.remove(&oldest);
                                }
                                file_sources.insert(transfer_id, chunks);
                                for peer_id in peers {
                                    swarm.behaviour_mut().files.send_request(
                                        &peer_id,
                                        FileTransferRequest::Offer(offer.clone()),
                                    );
                                }
                            }
                            DiscoveryCommand::RespondFileOffer { channel, response } => {
                                let _ = swarm.behaviour_mut().files.send_response(channel, response);
                            }
                            DiscoveryCommand::RequestFileChunk {
                                peer_id,
                                transfer_id,
                                chunk_index,
                            } => {
                                swarm.behaviour_mut().files.send_request(
                                    &peer_id,
                                    FileTransferRequest::GetChunk {
                                        transfer_id,
                                        chunk_index,
                                    },
                                );
                            }
                            DiscoveryCommand::UpdateCommunities { tokens, receiver_epoch: epoch, applied } => {
                                local_communities = tokens.into_iter().collect();
                                receiver_epoch = epoch;
                                let _ = applied.send(());
                            }
                        }
                    }
                    event = swarm.select_next_some() => {
                        if relay_server_enabled
                            && let SwarmEvent::NewListenAddr { address, .. } = &event
                        {
                            if relay_public_addresses.is_empty() {
                                swarm.add_external_address(address.clone());
                            } else if !relay_public_addresses_added {
                                for address in &relay_public_addresses {
                                    swarm.add_external_address(address.clone());
                                }
                                relay_public_addresses_added = true;
                            }
                        }
                        if let SwarmEvent::ConnectionEstablished { peer_id, .. } = &event {
                            active_dials.remove(peer_id);
                            dial_attempts.remove(peer_id);
                            if relay_peers.contains(peer_id)
                                && relay_reservations.insert(*peer_id)
                                && let Some((_, relay_address)) = relay_servers
                                    .iter()
                                    .find(|(relay_peer_id, _)| relay_peer_id == peer_id)
                            {
                                let reservation_address = relay_address
                                    .clone()
                                    .with(Protocol::P2p(*peer_id))
                                    .with(Protocol::P2pCircuit);
                                if let Err(error) = swarm.listen_on(reservation_address) {
                                    let _ = event_tx
                                        .send(DiscoveryEvent::NetworkWarning(format!(
                                            "Relay NAT indisponivel em {relay_address}: {error}"
                                        )))
                                        .await;
                                }
                            }
                        }
                        if let SwarmEvent::ConnectionClosed { peer_id, .. } = &event {
                            relay_reservations.remove(peer_id);
                        }
                        if let SwarmEvent::OutgoingConnectionError {
                            peer_id: Some(peer_id), ..
                        } = &event
                        {
                            active_dials.remove(peer_id);
                        }
                        learn_advertised_addresses(&event, &mut known_addresses);
                        if let SwarmEvent::Behaviour(BehaviourEvent::Mdns(
                            mdns::Event::Discovered(peers),
                        )) = &event
                        {
                            for (peer_id, address) in peers {
                                remember_peer_address(
                                    &mut known_addresses,
                                    *peer_id,
                                    address.clone(),
                                );
                                if kad_enabled {
                                    swarm
                                        .behaviour_mut()
                                        .kad
                                        .add_address(peer_id, address.clone());
                                }
                                if let Some(addresses) = known_addresses.get(peer_id) {
                                    dial_peer_with_backoff(
                                        &mut swarm,
                                        *peer_id,
                                        addresses,
                                        &mut dial_attempts,
                                        &mut active_dials,
                                    );
                                }
                            }
                        }
                        if let SwarmEvent::Behaviour(BehaviourEvent::Identify(
                            identify::Event::Received { peer_id, info, .. },
                        )) = &event
                        {
                            for address in &info.listen_addrs {
                                remember_peer_address(
                                    &mut known_addresses,
                                    *peer_id,
                                    address.clone(),
                                );
                                if kad_enabled {
                                    swarm
                                        .behaviour_mut()
                                        .kad
                                        .add_address(peer_id, address.clone());
                                }
                            }
                        }
                        if let SwarmEvent::Behaviour(BehaviourEvent::Kad(
                            kad::Event::RoutablePeer { peer, address },
                        )) = &event
                        {
                            remember_peer_address(&mut known_addresses, *peer, address.clone());
                            if let Some(addresses) = known_addresses.get(peer) {
                                dial_peer_with_backoff(
                                    &mut swarm,
                                    *peer,
                                    addresses,
                                    &mut dial_attempts,
                                    &mut active_dials,
                                );
                            }
                        }
                        let outgoing = handle_event(
                            event,
                            &mut swarm,
                            &mut connected,
                            &relay_peers,
                            &local_communities,
                            local_device_key,
                            receiver_epoch,
                            &file_sources,
                            &mut signal_limiter,
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

    /// Offer a file to selected connected peers. The signed offer is still
    /// authorized by the receiving application before any chunks are requested.
    pub async fn broadcast_file(
        &self,
        offer: FileTransferOffer,
        chunks: Vec<nexo_core::FileChunk>,
        peers: Vec<PeerId>,
    ) -> Result<()> {
        self.commands
            .send(DiscoveryCommand::BroadcastFile {
                offer,
                chunks,
                peers,
            })
            .await
            .context("the discovery service has stopped")
    }

    pub async fn respond_file_offer(
        &self,
        channel: FileOfferResponseChannel,
        response: FileTransferResponse,
    ) -> Result<()> {
        self.commands
            .send(DiscoveryCommand::RespondFileOffer { channel, response })
            .await
            .context("the discovery service has stopped")
    }

    pub async fn request_file_chunk(
        &self,
        peer_id: PeerId,
        transfer_id: uuid::Uuid,
        chunk_index: u32,
    ) -> Result<()> {
        self.commands
            .send(DiscoveryCommand::RequestFileChunk {
                peer_id,
                transfer_id,
                chunk_index,
            })
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

/// Read optional DHT bootstrap peers without making internet access mandatory.
/// Entries use the same authenticated `/p2p/<peer-id>` multiaddr format as invites and are
/// separated by semicolons so IPv6 and future multiaddr components remain unambiguous.
fn configured_bootstrap_peers() -> Vec<(PeerId, Multiaddr)> {
    std::env::var("NEXO_KAD_BOOTSTRAP")
        .ok()
        .map_or_else(Vec::new, |value| parse_bootstrap_peers(&value))
}

fn relay_server_enabled() -> bool {
    relay_server_enabled_value(std::env::var("NEXO_RELAY_SERVER").ok().as_deref())
}

fn relay_server_enabled_value(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn mdns_enabled() -> bool {
    !std::env::var("NEXO_DISABLE_MDNS")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

fn relay_listen_port() -> u16 {
    if !relay_server_enabled() {
        return 0;
    }
    std::env::var("NEXO_RELAY_LISTEN_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_RELAY_LISTEN_PORT)
}

fn relay_server_config(enabled: bool) -> relay::Config {
    let mut config = relay::Config::default();
    if enabled {
        config.max_reservations = 64;
        config.max_reservations_per_peer = 2;
        config.max_circuits = 64;
        config.max_circuits_per_peer = 8;
    } else {
        // Keep the server behaviour present for every node, but make it inert unless the
        // operator explicitly opts into hosting relay traffic.
        config.max_reservations = 0;
        config.max_reservations_per_peer = 0;
        config.max_circuits = 0;
        config.max_circuits_per_peer = 0;
    }
    config
}

/// Read optional public Circuit Relay v2 clients. Entries use the same authenticated
/// `/p2p/<peer-id>` multiaddr format as invitations and are separated by semicolons.
/// The relay is deliberately opt-in: LAN and direct internet paths stay independent of it.
fn configured_relay_servers() -> Vec<(PeerId, Multiaddr)> {
    std::env::var("NEXO_RELAY_SERVERS")
        .ok()
        .map_or_else(Vec::new, |value| parse_relay_servers(&value))
}

fn configured_relay_public_addresses() -> Vec<Multiaddr> {
    std::env::var("NEXO_RELAY_PUBLIC_ADDRESS")
        .ok()
        .map_or_else(Vec::new, |value| {
            value
                .split(';')
                .take(MAX_RELAY_SERVERS)
                .filter_map(|entry| entry.trim().parse::<Multiaddr>().ok())
                .filter(|address| {
                    address.iter().any(|protocol| {
                        matches!(
                            protocol,
                            Protocol::Tcp(_) | Protocol::QuicV1 | Protocol::WebTransport
                        )
                    }) && !address
                        .iter()
                        .any(|protocol| matches!(protocol, Protocol::P2pCircuit))
                })
                .collect()
        })
}

fn parse_bootstrap_peers(value: &str) -> Vec<(PeerId, Multiaddr)> {
    value
        .split(';')
        .take(MAX_KNOWN_PEERS)
        .filter_map(|entry| parse_invite_address(entry.trim()).ok())
        .collect()
}

fn parse_relay_servers(value: &str) -> Vec<(PeerId, Multiaddr)> {
    value
        .split(';')
        .take(MAX_RELAY_SERVERS)
        .filter_map(|entry| parse_invite_address(entry.trim()).ok())
        .collect()
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

fn remember_peer_address(
    known_addresses: &mut HashMap<PeerId, VecDeque<Multiaddr>>,
    peer_id: PeerId,
    address: Multiaddr,
) {
    if !known_addresses.contains_key(&peer_id) && known_addresses.len() >= MAX_KNOWN_PEERS {
        return;
    }
    let addresses = known_addresses.entry(peer_id).or_default();
    if !addresses.contains(&address) {
        addresses.push_back(address);
    }
    while addresses.len() > MAX_KNOWN_ADDRESSES_PER_PEER {
        addresses.pop_front();
    }
}

fn known_peer_advertisements(
    known_addresses: &HashMap<PeerId, VecDeque<Multiaddr>>,
    local_peer_id: PeerId,
    connected: &HashSet<PeerId>,
) -> Vec<PeerAddressAdvertisement> {
    let mut advertisements = known_addresses
        .iter()
        .filter(|(peer_id, _)| **peer_id != local_peer_id && connected.contains(peer_id))
        .map(|(peer_id, addresses)| PeerAddressAdvertisement {
            peer_id: peer_id.to_string(),
            addresses: addresses
                .iter()
                .map(ToString::to_string)
                .filter(|address| {
                    !address.is_empty() && address.len() <= MAX_ADVERTISEMENT_ADDRESS_BYTES
                })
                .collect(),
        })
        .filter(|advertisement| {
            !advertisement.peer_id.is_empty()
                && advertisement.peer_id.len() <= MAX_ADVERTISEMENT_PEER_ID_BYTES
                && !advertisement.addresses.is_empty()
        })
        .collect::<Vec<_>>();
    advertisements.sort_unstable_by(|left, right| left.peer_id.cmp(&right.peer_id));
    advertisements
}

fn learn_advertised_addresses(
    event: &SwarmEvent<BehaviourEvent>,
    known_addresses: &mut HashMap<PeerId, VecDeque<Multiaddr>>,
) {
    let SwarmEvent::Behaviour(BehaviourEvent::Sync(request_response::Event::Message {
        peer,
        message,
        ..
    })) = event
    else {
        return;
    };
    let request_response::Message::Request { request, .. } = message else {
        return;
    };
    if !request.is_within_limits() || peer_id_for_key(request.device_key()) != Some(*peer) {
        return;
    }
    let SyncRequest::Offer { known_peers, .. } = request else {
        return;
    };
    for advertisement in known_peers {
        let Ok(advertised_peer) = advertisement.peer_id.parse::<PeerId>() else {
            continue;
        };
        for raw_address in &advertisement.addresses {
            let Ok(mut address) = raw_address.parse::<Multiaddr>() else {
                continue;
            };
            if address
                .iter()
                .any(|protocol| matches!(protocol, Protocol::P2p(_)))
                || !address.iter().any(|protocol| {
                    matches!(
                        protocol,
                        Protocol::Tcp(_) | Protocol::QuicV1 | Protocol::WebTransport
                    )
                })
            {
                continue;
            }
            address.push(Protocol::P2p(advertised_peer));
            remember_peer_address(known_addresses, advertised_peer, address);
        }
    }
}

fn dial_peer_with_backoff(
    swarm: &mut libp2p::Swarm<Behaviour>,
    peer_id: PeerId,
    addresses: &VecDeque<Multiaddr>,
    attempts: &mut HashMap<PeerId, DialAttempt>,
    active_dials: &mut HashSet<PeerId>,
) {
    if addresses.is_empty()
        || active_dials.len() >= MAX_CONCURRENT_DIALS
        || active_dials.contains(&peer_id)
    {
        return;
    }
    let now = Instant::now();
    let attempt = attempts.entry(peer_id).or_insert(DialAttempt {
        next_attempt: now,
        failures: 0,
    });
    if attempt.next_attempt > now {
        return;
    }
    let options = DialOpts::peer_id(peer_id)
        .addresses(addresses.iter().cloned().collect())
        .build();
    let dial_result = swarm.dial(options);
    attempt.failures = attempt.failures.saturating_add(1);
    attempt.next_attempt = now + dial_backoff(attempt.failures);
    if dial_result.is_ok() {
        active_dials.insert(peer_id);
    }
}

fn dial_backoff(failures: u8) -> Duration {
    let exponent = u32::from(failures.min(5));
    Duration::from_secs(1_u64 << exponent).min(MAX_DIAL_BACKOFF)
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines, deprecated)]
fn handle_event(
    event: SwarmEvent<BehaviourEvent>,
    swarm: &mut libp2p::Swarm<Behaviour>,
    connected: &mut HashSet<PeerId>,
    relay_peers: &HashSet<PeerId>,
    local_communities: &HashSet<[u8; 32]>,
    local_device_key: [u8; 32],
    receiver_epoch: uuid::Uuid,
    file_sources: &HashMap<uuid::Uuid, Vec<nexo_core::FileChunk>>,
    signal_limiter: &mut SignalRateLimiter,
) -> Vec<DiscoveryEvent> {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => vec![DiscoveryEvent::Listening(address)],
        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            connected.insert(peer_id);
            if relay_peers.contains(&peer_id) {
                vec![DiscoveryEvent::NetworkWarning(format!(
                    "Conexao ao relay estabelecida: {peer_id}"
                ))]
            } else if endpoint
                .get_remote_address()
                .iter()
                .any(|protocol| matches!(protocol, Protocol::P2pCircuit))
            {
                vec![
                    DiscoveryEvent::PeerConnected(peer_id),
                    DiscoveryEvent::NetworkWarning(format!(
                        "Conexao relayed estabelecida: {peer_id}"
                    )),
                ]
            } else {
                vec![DiscoveryEvent::PeerConnected(peer_id)]
            }
        }
        SwarmEvent::ConnectionClosed { peer_id, .. } => {
            connected.remove(&peer_id);
            (!relay_peers.contains(&peer_id))
                .then_some(DiscoveryEvent::PeerDisconnected(peer_id))
                .into_iter()
                .collect()
        }
        SwarmEvent::OutgoingConnectionError {
            peer_id: Some(peer_id),
            error,
            ..
        } if relay_peers.contains(&peer_id) => vec![DiscoveryEvent::NetworkWarning(format!(
            "Falha ao conectar ao relay {peer_id}: {error}"
        ))],
        SwarmEvent::Behaviour(BehaviourEvent::Relay(
            relay::client::Event::ReservationReqAccepted { relay_peer_id, .. },
        )) => vec![DiscoveryEvent::NetworkWarning(format!(
            "Reserva relay aceita por {relay_peer_id}"
        ))],
        SwarmEvent::Behaviour(BehaviourEvent::RelayServer(
            relay::Event::ReservationReqAccepted { src_peer_id, .. },
        )) => vec![DiscoveryEvent::NetworkWarning(format!(
            "Relay hospedado aceitou reserva de {src_peer_id}"
        ))],
        SwarmEvent::Behaviour(BehaviourEvent::RelayServer(
            relay::Event::ReservationReqDenied {
                src_peer_id,
                status,
            },
        )) => vec![DiscoveryEvent::NetworkWarning(format!(
            "Relay hospedado recusou reserva de {src_peer_id}: {status:?}"
        ))],
        SwarmEvent::Behaviour(BehaviourEvent::RelayServer(
            relay::Event::ReservationReqAcceptFailed { src_peer_id, error },
        )) => vec![DiscoveryEvent::NetworkWarning(format!(
            "Relay hospedado falhou ao aceitar reserva de {src_peer_id}: {error}"
        ))],
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
                        let _ = swarm.behaviour_mut().sync.send_response(
                            channel,
                            SyncResponse::wanted(local_device_key, receiver_epoch, wanted),
                        );
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
                    device_key,
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
                        device_key,
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
                let allowed = valid_transport
                    && signal_limiter.check_and_record(&request.device_key, Instant::now());
                signal_limiter.prune_idle(Instant::now());
                let accepted = if allowed { request.signals.len() } else { 0 };
                let _ = swarm
                    .behaviour_mut()
                    .signalling
                    .send_response(channel, SignalResponse::received(accepted));
                allowed
                    .then_some(DiscoveryEvent::CallSignalsReceived {
                        peer_id: peer,
                        request,
                    })
                    .into_iter()
                    .collect()
            }
            request_response::Message::Response { .. } => Vec::new(),
        },
        SwarmEvent::Behaviour(BehaviourEvent::Files(request_response::Event::Message {
            peer,
            message,
            ..
        })) => match message {
            request_response::Message::Request {
                request, channel, ..
            } => match request {
                FileTransferRequest::Offer(offer)
                    if offer.verify(current_timestamp()).is_ok()
                        && peer_id_for_key(&offer.author_key) == Some(peer) =>
                {
                    vec![DiscoveryEvent::FileOfferReceived {
                        peer_id: peer,
                        offer,
                        channel,
                    }]
                }
                FileTransferRequest::GetChunk {
                    transfer_id,
                    chunk_index,
                } => {
                    let response = file_sources
                        .get(&transfer_id)
                        .and_then(|chunks| {
                            chunks.iter().find(|chunk| {
                                chunk.transfer_id == transfer_id && chunk.chunk_index == chunk_index
                            })
                        })
                        .cloned()
                        .map_or(FileTransferResponse::ChunkNotFound, |chunk| {
                            FileTransferResponse::Chunk(chunk)
                        });
                    let _ = swarm.behaviour_mut().files.send_response(channel, response);
                    Vec::new()
                }
                FileTransferRequest::Offer(_) => {
                    let _ = swarm.behaviour_mut().files.send_response(
                        channel,
                        FileTransferResponse::OfferRejected {
                            reason: "requisicao de arquivo invalida".to_owned(),
                        },
                    );
                    Vec::new()
                }
            },
            request_response::Message::Response { response, .. } => {
                vec![DiscoveryEvent::FileResponseReceived {
                    peer_id: peer,
                    response,
                }]
            }
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
            known_peers,
        } => SyncRequest::Offer {
            version,
            device_key,
            tokens: tokens
                .iter()
                .map(|token| peer_sync_token(token, &local.to_bytes(), &remote.to_bytes()))
                .collect(),
            known_peers,
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

    #[test]
    fn kad_bootstrap_parser_keeps_only_authenticated_addresses() {
        let first = DeviceIdentity::generate();
        let second = DeviceIdentity::generate();
        let first_peer = peer_id_for_key(&first.public_key_bytes())
            .expect("first identity should map to a peer id");
        let second_peer = peer_id_for_key(&second.public_key_bytes())
            .expect("second identity should map to a peer id");
        let value = format!(
            "/ip4/192.168.1.20/tcp/4242/p2p/{first_peer};not-an-address;/ip4/10.0.0.2/udp/4242/quic-v1/p2p/{second_peer}"
        );
        let peers = parse_bootstrap_peers(&value);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].0, first_peer);
        assert_eq!(peers[1].0, second_peer);
    }

    #[test]
    fn relay_parser_keeps_only_bounded_authenticated_addresses() {
        let first = DeviceIdentity::generate();
        let second = DeviceIdentity::generate();
        let first_peer = peer_id_for_key(&first.public_key_bytes())
            .expect("first identity should map to a peer id");
        let second_peer = peer_id_for_key(&second.public_key_bytes())
            .expect("second identity should map to a peer id");
        let value = format!(
            "/ip4/203.0.113.10/tcp/4001/p2p/{first_peer};not-an-address;/ip4/203.0.113.11/tcp/4001/p2p/{second_peer}"
        );
        let relays = parse_relay_servers(&value);
        assert_eq!(relays.len(), 2);
        assert_eq!(relays[0].0, first_peer);
        assert_eq!(relays[1].0, second_peer);
        assert_eq!(relays[0].1.to_string(), "/ip4/203.0.113.10/tcp/4001");
    }

    #[test]
    fn hosted_relay_mode_requires_explicit_opt_in() {
        assert!(!relay_server_enabled_value(None));
        assert!(!relay_server_enabled_value(Some("false")));
        assert!(relay_server_enabled_value(Some("true")));
        assert!(relay_server_enabled_value(Some(" ON ")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hosted_relay_accepts_a_client_reservation() {
        let server_identity = DeviceIdentity::generate();
        let mut server = DiscoveryService::start_with_config(
            &server_identity,
            DiscoveryConfig {
                bootstrap_peers: Vec::new(),
                relay_servers: Vec::new(),
                relay_server_enabled: true,
                relay_listen_port: 0,
                relay_public_addresses: Vec::new(),
                mdns_enabled: false,
            },
        )
        .expect("relay server should start");

        let relay_address = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = server.next_event().await.expect("server event stream");
                if let DiscoveryEvent::Listening(address) = event
                    && let Some(Protocol::Tcp(port)) = address
                        .iter()
                        .find(|protocol| matches!(protocol, Protocol::Tcp(_)))
                {
                    break format!("/ip4/127.0.0.1/tcp/{port}")
                        .parse::<Multiaddr>()
                        .expect("loopback relay address should parse");
                }
            }
        })
        .await
        .expect("relay server should publish a TCP listener");

        let mut client = DiscoveryService::start_with_config(
            &DeviceIdentity::generate(),
            DiscoveryConfig {
                bootstrap_peers: Vec::new(),
                relay_servers: vec![(server.local_peer_id(), relay_address)],
                relay_server_enabled: false,
                relay_listen_port: 0,
                relay_public_addresses: Vec::new(),
                mdns_enabled: false,
            },
        )
        .expect("relay client should start");

        let reserved = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    event = client.next_event() => {
                        let event = event.expect("client event stream");
                        match event {
                            DiscoveryEvent::Listening(address)
                                if address
                                    .iter()
                                    .any(|protocol| matches!(protocol, Protocol::P2pCircuit)) =>
                            {
                                break true;
                            }
                            DiscoveryEvent::NetworkWarning(message)
                                if message.starts_with("Reserva relay aceita") =>
                            {
                                break true;
                            }
                            DiscoveryEvent::NetworkWarning(message)
                                if message.starts_with("Falha ao conectar ao relay")
                                    || message.starts_with("Relay NAT indisponivel") =>
                            {
                                panic!("relay reservation failed: {message}");
                            }
                            _ => {}
                        }
                    }
                    event = server.next_event() => {
                        let _ = event;
                    }
                }
            }
        })
        .await
        .expect("client should reserve a relay circuit address");

        assert!(reserved);
        drop(client);
        drop(server);
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hosted_relay_dials_a_client_over_a_reserved_circuit() {
        let server_identity = DeviceIdentity::generate();
        let mut server = DiscoveryService::start_with_config(
            &server_identity,
            DiscoveryConfig {
                bootstrap_peers: Vec::new(),
                relay_servers: Vec::new(),
                relay_server_enabled: true,
                relay_listen_port: 0,
                relay_public_addresses: Vec::new(),
                mdns_enabled: false,
            },
        )
        .expect("relay server should start");

        let relay_address = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = server.next_event().await.expect("server event stream");
                if let DiscoveryEvent::Listening(address) = event
                    && let Some(Protocol::Tcp(port)) = address
                        .iter()
                        .find(|protocol| matches!(protocol, Protocol::Tcp(_)))
                {
                    break format!("/ip4/127.0.0.1/tcp/{port}")
                        .parse::<Multiaddr>()
                        .expect("loopback relay address should parse");
                }
            }
        })
        .await
        .expect("relay server should publish a TCP listener");

        let source_identity = DeviceIdentity::generate();
        let source_peer = peer_id_for_key(&source_identity.public_key_bytes())
            .expect("source identity should map to a peer id");
        let mut source_client = DiscoveryService::start_with_config(
            &source_identity,
            DiscoveryConfig {
                bootstrap_peers: Vec::new(),
                relay_servers: vec![(server.local_peer_id(), relay_address.clone())],
                relay_server_enabled: false,
                relay_listen_port: 0,
                relay_public_addresses: Vec::new(),
                mdns_enabled: false,
            },
        )
        .expect("client A should start");

        let source_circuit = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    event = source_client.next_event() => {
                        let event = event.expect("source client event stream");
                        match event {
                            DiscoveryEvent::Listening(address)
                                if address
                                    .iter()
                                    .any(|protocol| matches!(protocol, Protocol::P2pCircuit)) =>
                            {
                                break address;
                            }
                            DiscoveryEvent::NetworkWarning(message)
                                if message.starts_with("Falha ao conectar ao relay")
                                    || message.starts_with("Relay NAT indisponivel") =>
                            {
                                panic!("source relay reservation failed: {message}");
                            }
                            _ => {}
                        }
                    }
                    event = server.next_event() => {
                        let _ = event;
                    }
                }
            }
        })
        .await
        .expect("source should reserve a relay circuit address");

        let dialer_identity = DeviceIdentity::generate();
        let mut dialer_client = DiscoveryService::start_with_config(
            &dialer_identity,
            DiscoveryConfig {
                bootstrap_peers: Vec::new(),
                relay_servers: vec![(server.local_peer_id(), relay_address)],
                relay_server_enabled: false,
                relay_listen_port: 0,
                relay_public_addresses: Vec::new(),
                mdns_enabled: false,
            },
        )
        .expect("client B should start");

        let dialer_circuit = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    event = dialer_client.next_event() => {
                        let event = event.expect("dialer event stream");
                        match event {
                            DiscoveryEvent::Listening(address)
                                if address
                                    .iter()
                                    .any(|protocol| matches!(protocol, Protocol::P2pCircuit)) =>
                            {
                                break true;
                            }
                            DiscoveryEvent::NetworkWarning(message)
                                if message.starts_with("Falha ao conectar ao relay")
                                    || message.starts_with("Relay NAT indisponivel") =>
                            {
                                panic!("dialer relay reservation failed: {message}");
                            }
                            _ => {}
                        }
                    }
                    event = server.next_event() => {
                        let _ = event;
                    }
                }
            }
        })
        .await
        .expect("dialer should reserve a relay circuit address");
        assert!(dialer_circuit);

        dialer_client
            .dial(source_peer, source_circuit)
            .await
            .expect("dialer should queue a relayed dial");

        let relayed = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                tokio::select! {
                    event = dialer_client.next_event() => {
                        let event = event.expect("dialer event stream");
                        match event {
                            DiscoveryEvent::NetworkWarning(message)
                                if message.starts_with("Conexao relayed estabelecida:") =>
                            {
                                break true;
                            }
                            DiscoveryEvent::NetworkWarning(message)
                                if message.starts_with("Falha ao conectar ao relay")
                                    || message.starts_with("Relay NAT indisponivel") =>
                            {
                                panic!("relayed dial failed: {message}");
                            }
                            _ => {}
                        }
                    }
                    event = source_client.next_event() => {
                        let _ = event;
                    }
                    event = server.next_event() => {
                        let _ = event;
                    }
                }
            }
        })
        .await
        .expect("dialer should connect to source through the relay");

        assert!(relayed);
        drop(dialer_client);
        drop(source_client);
        drop(server);
    }

    #[test]
    fn known_peer_addresses_are_deduplicated_and_bounded() {
        let identity = DeviceIdentity::generate();
        let peer = peer_id_for_key(&identity.public_key_bytes())
            .expect("generated identity should map to a peer id");
        let mut known = HashMap::new();
        let address: Multiaddr = "/ip4/192.168.1.20/tcp/4242"
            .parse()
            .expect("address parses");
        remember_peer_address(&mut known, peer, address.clone());
        remember_peer_address(&mut known, peer, address);
        assert_eq!(
            known.get(&peer).expect("peer addresses should exist").len(),
            1
        );
        let max_port = 5000_u16
            + u16::try_from(MAX_KNOWN_ADDRESSES_PER_PEER).expect("address cap fits in a port");
        for port in 5000..=max_port {
            remember_peer_address(
                &mut known,
                peer,
                format!("/ip4/192.168.1.20/tcp/{port}")
                    .parse()
                    .expect("address parses"),
            );
        }
        let addresses = known.get(&peer).expect("peer addresses should exist");
        assert_eq!(addresses.len(), MAX_KNOWN_ADDRESSES_PER_PEER);
        let newest: Multiaddr = format!("/ip4/192.168.1.20/tcp/{max_port}")
            .parse()
            .expect("address parses");
        assert!(addresses.contains(&newest));
    }

    #[test]
    fn dial_backoff_is_exponential_and_capped() {
        assert_eq!(dial_backoff(1), Duration::from_secs(2));
        assert_eq!(dial_backoff(2), Duration::from_secs(4));
        assert_eq!(dial_backoff(5), MAX_DIAL_BACKOFF);
        assert_eq!(dial_backoff(u8::MAX), MAX_DIAL_BACKOFF);
    }
}
