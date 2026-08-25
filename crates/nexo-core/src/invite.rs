use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{DeviceIdentity, IdentityError};

const PREFIX: &str = "NEXO1.";
const VERSION: u8 = 1;
const MAX_CODE_BYTES: usize = 16 * 1024;
const MAX_NAME_BYTES: usize = 80;
const MAX_ADDRESSES: usize = 8;
const MAX_ADDRESS_BYTES: usize = 256;
const MAX_LIFETIME_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkInvite {
    pub version: u8,
    pub network_id: Uuid,
    pub network_name: String,
    pub inviter_key: String,
    pub addresses: Vec<String>,
    pub created_at: u64,
    pub expires_at: u64,
    pub nonce: String,
    #[serde(default)]
    pub group_secret: Option<String>,
    pub signature: String,
}

#[derive(Debug, Error)]
pub enum InviteError {
    #[error("the invitation format is invalid")]
    InvalidFormat,
    #[error("the invitation contains invalid data")]
    InvalidData,
    #[error("the invitation signature is invalid")]
    InvalidSignature,
    #[error("the invitation has expired")]
    Expired,
    #[error("the invitation lifetime is not allowed")]
    Lifetime,
}

#[derive(Serialize)]
struct SignedFields<'a> {
    version: u8,
    network_id: Uuid,
    network_name: &'a str,
    inviter_key: &'a str,
    addresses: &'a [String],
    created_at: u64,
    expires_at: u64,
    nonce: &'a str,
    group_secret: &'a Option<String>,
}

#[derive(Serialize)]
struct LegacySignedFields<'a> {
    version: u8,
    network_id: Uuid,
    network_name: &'a str,
    inviter_key: &'a str,
    addresses: &'a [String],
    created_at: u64,
    expires_at: u64,
    nonce: &'a str,
}

impl NetworkInvite {
    pub fn create(
        identity: &DeviceIdentity,
        network_name: impl Into<String>,
        addresses: Vec<String>,
        now: u64,
        lifetime_seconds: u64,
    ) -> Result<Self, InviteError> {
        let network_name = network_name.into();
        validate_fields(&network_name, &addresses)?;
        if lifetime_seconds == 0 || lifetime_seconds > MAX_LIFETIME_SECONDS {
            return Err(InviteError::Lifetime);
        }

        let mut nonce = [0_u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let mut group_secret = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut group_secret);
        let mut invite = Self {
            version: VERSION,
            network_id: Uuid::new_v4(),
            network_name,
            inviter_key: identity.public_key_text(),
            addresses,
            created_at: now,
            expires_at: now
                .checked_add(lifetime_seconds)
                .ok_or(InviteError::Lifetime)?,
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            group_secret: Some(URL_SAFE_NO_PAD.encode(group_secret)),
            signature: String::new(),
        };
        invite.signature = URL_SAFE_NO_PAD.encode(identity.sign(&invite.signing_bytes()?));
        Ok(invite)
    }

    pub fn decode_and_verify(code: &str, now: u64) -> Result<Self, InviteError> {
        if code.len() > MAX_CODE_BYTES || !code.starts_with(PREFIX) {
            return Err(InviteError::InvalidFormat);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(&code[PREFIX.len()..])
            .map_err(|_| InviteError::InvalidFormat)?;
        let invite: Self =
            serde_json::from_slice(&bytes).map_err(|_| InviteError::InvalidFormat)?;
        invite.verify(now)?;
        Ok(invite)
    }

    pub fn verify(&self, now: u64) -> Result<(), InviteError> {
        if self.version != VERSION {
            return Err(InviteError::InvalidData);
        }
        validate_fields(&self.network_name, &self.addresses)?;
        if self.expires_at <= self.created_at
            || self.expires_at - self.created_at > MAX_LIFETIME_SECONDS
        {
            return Err(InviteError::Lifetime);
        }
        if self.created_at > now.saturating_add(5 * 60) {
            return Err(InviteError::Lifetime);
        }
        if now > self.expires_at {
            return Err(InviteError::Expired);
        }
        if let Some(secret) = &self.group_secret {
            let secret = URL_SAFE_NO_PAD
                .decode(secret)
                .map_err(|_| InviteError::InvalidData)?;
            if secret.len() != 32 {
                return Err(InviteError::InvalidData);
            }
        }

        let public_key: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&self.inviter_key)
            .map_err(|_| InviteError::InvalidData)?
            .try_into()
            .map_err(|_| InviteError::InvalidData)?;
        let signature: [u8; 64] = URL_SAFE_NO_PAD
            .decode(&self.signature)
            .map_err(|_| InviteError::InvalidSignature)?
            .try_into()
            .map_err(|_| InviteError::InvalidSignature)?;
        let signed = DeviceIdentity::verify(&public_key, &self.signing_bytes()?, &signature);
        if signed.is_ok() || self.group_secret.is_some() {
            return signed.map_err(map_identity_error);
        }
        DeviceIdentity::verify(&public_key, &self.legacy_signing_bytes()?, &signature)
            .map_err(map_identity_error)
    }

    pub fn encode(&self) -> Result<String, InviteError> {
        let json = serde_json::to_vec(self).map_err(|_| InviteError::InvalidData)?;
        Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json)))
    }

    #[must_use]
    pub fn group_secret_bytes(&self) -> Option<[u8; 32]> {
        self.group_secret
            .as_deref()
            .and_then(|secret| URL_SAFE_NO_PAD.decode(secret).ok())
            .and_then(|secret| secret.try_into().ok())
    }

    fn signing_bytes(&self) -> Result<Vec<u8>, InviteError> {
        serde_json::to_vec(&SignedFields {
            version: self.version,
            network_id: self.network_id,
            network_name: &self.network_name,
            inviter_key: &self.inviter_key,
            addresses: &self.addresses,
            created_at: self.created_at,
            expires_at: self.expires_at,
            nonce: &self.nonce,
            group_secret: &self.group_secret,
        })
        .map_err(|_| InviteError::InvalidData)
    }

    fn legacy_signing_bytes(&self) -> Result<Vec<u8>, InviteError> {
        serde_json::to_vec(&LegacySignedFields {
            version: self.version,
            network_id: self.network_id,
            network_name: &self.network_name,
            inviter_key: &self.inviter_key,
            addresses: &self.addresses,
            created_at: self.created_at,
            expires_at: self.expires_at,
            nonce: &self.nonce,
        })
        .map_err(|_| InviteError::InvalidData)
    }
}

fn validate_fields(name: &str, addresses: &[String]) -> Result<(), InviteError> {
    if name.trim().is_empty()
        || name.len() > MAX_NAME_BYTES
        || addresses.len() > MAX_ADDRESSES
        || addresses.iter().any(|address| {
            address.is_empty() || address.len() > MAX_ADDRESS_BYTES || !address.starts_with('/')
        })
    {
        return Err(InviteError::InvalidData);
    }
    Ok(())
}

fn map_identity_error(_: IdentityError) -> InviteError {
    InviteError::InvalidSignature
}

#[must_use]
pub fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_round_trip_and_tamper_detection() {
        let identity = DeviceIdentity::generate();
        let invite = NetworkInvite::create(
            &identity,
            "Amigos",
            vec!["/ip4/192.168.1.10/udp/43191/quic-v1".into()],
            1_000,
            600,
        )
        .expect("invite should be created");
        let code = invite.encode().expect("invite should encode");
        let decoded = NetworkInvite::decode_and_verify(&code, 1_100)
            .expect("invite should decode and verify");
        assert_eq!(decoded.network_id, invite.network_id);
        assert_eq!(decoded.group_secret_bytes(), invite.group_secret_bytes());
        assert!(decoded.group_secret_bytes().is_some());

        let mut tampered = decoded;
        tampered.network_name = "Outra rede".into();
        assert!(tampered.verify(1_100).is_err());
    }

    #[test]
    fn expired_invite_is_rejected() {
        let invite =
            NetworkInvite::create(&DeviceIdentity::generate(), "LAN", Vec::new(), 1_000, 60)
                .expect("invite should be created");
        assert!(matches!(invite.verify(1_061), Err(InviteError::Expired)));
    }

    #[test]
    fn legacy_invite_without_group_secret_remains_verifiable() {
        let identity = DeviceIdentity::generate();
        let mut invite = NetworkInvite::create(&identity, "LAN", Vec::new(), 1_000, 60)
            .expect("invite should be created");
        invite.group_secret = None;
        invite.signature = URL_SAFE_NO_PAD.encode(
            identity.sign(
                &invite
                    .legacy_signing_bytes()
                    .expect("legacy fields should serialize"),
            ),
        );
        invite.verify(1_001).expect("legacy invite should verify");
    }
}
