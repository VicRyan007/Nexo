use nexo_core::{CommunityCredential, SignedMessage};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SYNC_PROTOCOL: &str = "/nexo/sync/0.2.0";
pub const MAX_COMMUNITIES_PER_SYNC: usize = 64;
pub const MAX_MESSAGES_PER_COMMUNITY: usize = 200;
pub const MAX_CREDENTIALS_PER_COMMUNITY: usize = 64;
pub const SYNC_VERSION: u8 = 2;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SyncRequest {
    Offer {
        version: u8,
        device_key: [u8; 32],
        tokens: Vec<[u8; 32]>,
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
    pub messages: Vec<SignedMessage>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommunityAck {
    pub community_id: Uuid,
    pub processed_message_ids: Vec<Uuid>,
    pub request_next: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SyncResponse {
    Wanted {
        version: u8,
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
        }
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
            community.messages.truncate(MAX_MESSAGES_PER_COMMUNITY);
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
                version, tokens, ..
            } => *version == SYNC_VERSION && tokens.len() <= MAX_COMMUNITIES_PER_SYNC,
            Self::Batch {
                version,
                communities,
                ..
            } => {
                *version == SYNC_VERSION
                    && communities.len() <= MAX_COMMUNITIES_PER_SYNC
                    && communities.iter().all(|community| {
                        community.credentials.len() <= MAX_CREDENTIALS_PER_COMMUNITY
                            && community.messages.len() <= MAX_MESSAGES_PER_COMMUNITY
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
    pub fn wanted(receiver_epoch: Uuid, mut tokens: Vec<[u8; 32]>) -> Self {
        tokens.truncate(MAX_COMMUNITIES_PER_SYNC);
        Self::Wanted {
            version: SYNC_VERSION,
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
