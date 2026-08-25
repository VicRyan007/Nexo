use nexo_core::{CommunityCredential, DirectMessageEnvelope, MlsCommit, SignedMessage};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SYNC_PROTOCOL: &str = "/nexo/sync/0.4.0";
pub const MAX_COMMUNITIES_PER_SYNC: usize = 64;
pub const MAX_MESSAGES_PER_COMMUNITY: usize = 200;
pub const MAX_CREDENTIALS_PER_COMMUNITY: usize = 64;
pub const MAX_CHANNELS_PER_COMMUNITY: usize = 128;
pub const MAX_DIRECT_MESSAGES_PER_COMMUNITY: usize = 200;
pub const MAX_MLS_COMMITS_PER_COMMUNITY: usize = 64;
pub const MAX_ADVERTISED_PEERS: usize = 32;
pub const MAX_ADVERTISEMENTS_PER_PEER: usize = 8;
pub const MAX_ADVERTISEMENT_PEER_ID_BYTES: usize = 128;
pub const MAX_ADVERTISEMENT_ADDRESS_BYTES: usize = 256;
pub const SYNC_VERSION: u8 = 4;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PeerAddressAdvertisement {
    pub peer_id: String,
    pub addresses: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SyncRequest {
    Offer {
        version: u8,
        device_key: [u8; 32],
        tokens: Vec<[u8; 32]>,
        known_peers: Vec<PeerAddressAdvertisement>,
    },
    Batch {
        version: u8,
        device_key: [u8; 32],
        receiver_epoch: Uuid,
        communities: Vec<CommunitySync>,
    },
    Ack {
        version: u8,
        device_key: [u8; 32],
        receiver_epoch: Uuid,
        communities: Vec<CommunityAck>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommunitySync {
    pub community_id: Uuid,
    pub credentials: Vec<CommunityCredential>,
    pub channels: Vec<SyncChannel>,
    pub messages: Vec<SignedMessage>,
    pub direct_messages: Vec<DirectMessageEnvelope>,
    pub mls_commits: Vec<MlsCommit>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncChannel {
    pub id: Uuid,
    pub community_id: Uuid,
    pub name: String,
    pub position: u32,
    pub kind: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommunityAck {
    pub community_id: Uuid,
    pub processed_message_ids: Vec<Uuid>,
    pub processed_direct_message_ids: Vec<Uuid>,
    pub processed_mls_commit_ids: Vec<Uuid>,
    pub request_next: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SyncResponse {
    Wanted {
        version: u8,
        device_key: [u8; 32],
        receiver_epoch: Uuid,
        tokens: Vec<[u8; 32]>,
    },
    Received {
        version: u8,
    },
}

impl SyncRequest {
    #[must_use]
    pub fn offer(device_key: [u8; 32], mut tokens: Vec<[u8; 32]>) -> Self {
        tokens.sort_unstable();
        tokens.dedup();
        tokens.truncate(MAX_COMMUNITIES_PER_SYNC);
        Self::Offer {
            version: SYNC_VERSION,
            device_key,
            tokens,
            known_peers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_known_peers(mut self, mut known_peers: Vec<PeerAddressAdvertisement>) -> Self {
        if let Self::Offer {
            known_peers: current,
            ..
        } = &mut self
        {
            known_peers.retain(|peer| {
                !peer.peer_id.is_empty()
                    && peer.peer_id.len() <= MAX_ADVERTISEMENT_PEER_ID_BYTES
                    && !peer.addresses.is_empty()
            });
            known_peers.truncate(MAX_ADVERTISED_PEERS);
            for peer in &mut known_peers {
                peer.addresses.retain(|address| {
                    !address.is_empty() && address.len() <= MAX_ADVERTISEMENT_ADDRESS_BYTES
                });
                peer.addresses.sort_unstable();
                peer.addresses.dedup();
                peer.addresses.truncate(MAX_ADVERTISEMENTS_PER_PEER);
            }
            known_peers.retain(|peer| !peer.addresses.is_empty());
            *current = known_peers;
        }
        self
    }

    #[must_use]
    pub fn batch(
        device_key: [u8; 32],
        receiver_epoch: Uuid,
        mut communities: Vec<CommunitySync>,
    ) -> Self {
        communities.truncate(MAX_COMMUNITIES_PER_SYNC);
        for community in &mut communities {
            community
                .credentials
                .truncate(MAX_CREDENTIALS_PER_COMMUNITY);
            community.channels.truncate(MAX_CHANNELS_PER_COMMUNITY);
            community.messages.truncate(MAX_MESSAGES_PER_COMMUNITY);
            community
                .direct_messages
                .truncate(MAX_DIRECT_MESSAGES_PER_COMMUNITY);
            community
                .mls_commits
                .truncate(MAX_MLS_COMMITS_PER_COMMUNITY);
        }
        Self::Batch {
            version: SYNC_VERSION,
            device_key,
            receiver_epoch,
            communities,
        }
    }

    #[must_use]
    pub fn device_key(&self) -> &[u8; 32] {
        match self {
            Self::Offer { device_key, .. }
            | Self::Batch { device_key, .. }
            | Self::Ack { device_key, .. } => device_key,
        }
    }

    #[must_use]
    pub fn is_within_limits(&self) -> bool {
        match self {
            Self::Offer {
                version,
                tokens,
                known_peers,
                ..
            } => {
                *version == SYNC_VERSION
                    && tokens.len() <= MAX_COMMUNITIES_PER_SYNC
                    && known_peers.len() <= MAX_ADVERTISED_PEERS
                    && known_peers.iter().all(|peer| {
                        !peer.peer_id.is_empty()
                            && peer.peer_id.len() <= MAX_ADVERTISEMENT_PEER_ID_BYTES
                            && !peer.addresses.is_empty()
                            && peer.addresses.len() <= MAX_ADVERTISEMENTS_PER_PEER
                            && peer.addresses.iter().all(|address| {
                                !address.is_empty()
                                    && address.len() <= MAX_ADVERTISEMENT_ADDRESS_BYTES
                            })
                    })
            }
            Self::Batch {
                version,
                communities,
                ..
            } => {
                *version == SYNC_VERSION
                    && communities.len() <= MAX_COMMUNITIES_PER_SYNC
                    && communities.iter().all(|community| {
                        community.credentials.len() <= MAX_CREDENTIALS_PER_COMMUNITY
                            && community.channels.len() <= MAX_CHANNELS_PER_COMMUNITY
                            && community.messages.len() <= MAX_MESSAGES_PER_COMMUNITY
                            && community.direct_messages.len() <= MAX_DIRECT_MESSAGES_PER_COMMUNITY
                            && community.mls_commits.len() <= MAX_MLS_COMMITS_PER_COMMUNITY
                    })
            }
            Self::Ack {
                version,
                communities,
                ..
            } => {
                *version == SYNC_VERSION
                    && communities.len() <= MAX_COMMUNITIES_PER_SYNC
                    && communities.iter().all(|community| {
                        community.processed_message_ids.len() <= MAX_MESSAGES_PER_COMMUNITY
                            && community.processed_direct_message_ids.len()
                                <= MAX_DIRECT_MESSAGES_PER_COMMUNITY
                            && community.processed_mls_commit_ids.len()
                                <= MAX_MLS_COMMITS_PER_COMMUNITY
                    })
            }
        }
    }

    #[must_use]
    pub fn ack(
        device_key: [u8; 32],
        receiver_epoch: Uuid,
        mut communities: Vec<CommunityAck>,
    ) -> Self {
        communities.truncate(MAX_COMMUNITIES_PER_SYNC);
        for community in &mut communities {
            community
                .processed_message_ids
                .truncate(MAX_MESSAGES_PER_COMMUNITY);
            community
                .processed_direct_message_ids
                .truncate(MAX_DIRECT_MESSAGES_PER_COMMUNITY);
            community
                .processed_mls_commit_ids
                .truncate(MAX_MLS_COMMITS_PER_COMMUNITY);
        }
        Self::Ack {
            version: SYNC_VERSION,
            device_key,
            receiver_epoch,
            communities,
        }
    }
}

impl SyncResponse {
    #[must_use]
    pub fn wanted(device_key: [u8; 32], receiver_epoch: Uuid, mut tokens: Vec<[u8; 32]>) -> Self {
        tokens.truncate(MAX_COMMUNITIES_PER_SYNC);
        Self::Wanted {
            version: SYNC_VERSION,
            device_key,
            receiver_epoch,
            tokens,
        }
    }

    #[must_use]
    pub const fn received() -> Self {
        Self::Received {
            version: SYNC_VERSION,
        }
    }
}
