use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{DeviceIdentity, IdentityError, InviteError, NetworkInvite};

const CREDENTIAL_VERSION: u8 = 1;
const MAX_CLOCK_SKEW_SECONDS: u64 = 5 * 60;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommunityCredential {
    pub version: u8,
    pub invite: NetworkInvite,
    pub member_key: [u8; 32],
    pub accepted_at: u64,
    pub signature: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum MembershipError {
    #[error("the membership credential version is unsupported")]
    UnsupportedVersion,
    #[error("the membership claim was created outside the invitation lifetime")]
    OutsideInviteLifetime,
    #[error("the membership claim timestamp is too far in the future")]
    FutureTimestamp,
    #[error("the membership signature has an invalid length")]
    InvalidSignatureLength,
    #[error("the membership credential could not be serialized: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("the invitation is invalid: {0}")]
    Invite(#[from] InviteError),
    #[error("the membership signature is invalid: {0}")]
    Identity(#[from] IdentityError),
}

#[derive(Serialize)]
struct SignedFields<'a> {
    version: u8,
    invite: &'a NetworkInvite,
    member_key: &'a [u8; 32],
    accepted_at: u64,
}

impl CommunityCredential {
    pub fn claim(
        identity: &DeviceIdentity,
        invite: NetworkInvite,
        accepted_at: u64,
    ) -> Result<Self, MembershipError> {
        invite.verify(accepted_at)?;
        let mut credential = Self {
            version: CREDENTIAL_VERSION,
            invite,
            member_key: identity.public_key_bytes(),
            accepted_at,
            signature: Vec::new(),
        };
        credential.signature = identity.sign(&credential.signing_bytes()?).to_vec();
        Ok(credential)
    }

    pub fn verify(&self, now: u64) -> Result<(), MembershipError> {
        if self.version != CREDENTIAL_VERSION {
            return Err(MembershipError::UnsupportedVersion);
        }
        if self.accepted_at < self.invite.created_at || self.accepted_at > self.invite.expires_at {
            return Err(MembershipError::OutsideInviteLifetime);
        }
        if self.accepted_at > now.saturating_add(MAX_CLOCK_SKEW_SECONDS) {
            return Err(MembershipError::FutureTimestamp);
        }
        self.invite.verify(self.accepted_at)?;
        let signature: [u8; 64] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| MembershipError::InvalidSignatureLength)?;
        DeviceIdentity::verify(&self.member_key, &self.signing_bytes()?, &signature)?;
        Ok(())
    }

    fn signing_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&SignedFields {
            version: self.version,
            invite: &self.invite,
            member_key: &self.member_key,
            accepted_at: self.accepted_at,
        })
    }
}

#[must_use]
pub fn community_sync_token(invite: &NetworkInvite) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"nexo-community-sync-v1");
    digest.update(invite.network_id.as_bytes());
    digest.update(invite.nonce.as_bytes());
    digest.finalize().into()
}

#[must_use]
pub fn peer_sync_token(
    community_token: &[u8; 32],
    local_peer: &[u8],
    remote_peer: &[u8],
) -> [u8; 32] {
    let (first, second) = if local_peer <= remote_peer {
        (local_peer, remote_peer)
    } else {
        (remote_peer, local_peer)
    };
    let mut digest = Sha256::new();
    digest.update(b"nexo-peer-sync-v1");
    digest.update(community_token);
    digest.update((first.len() as u64).to_be_bytes());
    digest.update(first);
    digest.update((second.len() as u64).to_be_bytes());
    digest.update(second);
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkInvite;

    #[test]
    fn credential_binds_invite_to_member_key() {
        let inviter = DeviceIdentity::generate();
        let member = DeviceIdentity::generate();
        let invite = NetworkInvite::create(&inviter, "Amigos", Vec::new(), 100, 600)
            .expect("invite should be created");
        let credential =
            CommunityCredential::claim(&member, invite, 120).expect("credential should be claimed");
        credential
            .verify(900)
            .expect("credential remains valid after invite expiry");
    }

    #[test]
    fn credential_rejects_member_key_tampering() {
        let inviter = DeviceIdentity::generate();
        let member = DeviceIdentity::generate();
        let invite = NetworkInvite::create(&inviter, "Amigos", Vec::new(), 100, 600)
            .expect("invite should be created");
        let mut credential =
            CommunityCredential::claim(&member, invite, 120).expect("credential should be claimed");
        credential.member_key = DeviceIdentity::generate().public_key_bytes();
        assert!(credential.verify(120).is_err());
    }

    #[test]
    fn peer_token_is_symmetric_and_peer_specific() {
        let community = [7_u8; 32];
        assert_eq!(
            peer_sync_token(&community, b"alice", b"bob"),
            peer_sync_token(&community, b"bob", b"alice")
        );
        assert_ne!(
            peer_sync_token(&community, b"alice", b"bob"),
            peer_sync_token(&community, b"alice", b"carol")
        );
    }
}
