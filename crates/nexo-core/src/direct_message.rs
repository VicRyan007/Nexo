//! Authenticated one-to-one message envelopes and deterministic conversations.

use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{DeviceIdentity, IdentityError, RatchetMessage};

const ENVELOPE_VERSION: u8 = 1;
const MAX_MESSAGE_AGE_SECONDS: u64 = 5 * 60;

#[derive(Debug, Error)]
pub enum DirectMessageError {
    #[error("direct message envelope version is unsupported")]
    UnsupportedVersion,
    #[error("direct message timestamp is too far in the future")]
    FutureTimestamp,
    #[error("direct message envelope has expired")]
    StaleTimestamp,
    #[error("a direct message cannot be addressed to its sender")]
    SelfRecipient,
    #[error("direct message signature has an invalid length")]
    InvalidSignatureLength,
    #[error("direct message envelope could not be serialized: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("direct message signature is invalid: {0}")]
    Identity(#[from] IdentityError),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DirectSessionHello {
    pub version: u8,
    pub conversation_id: Uuid,
    pub dh_public_key: [u8; 32],
}

impl DirectSessionHello {
    #[must_use]
    pub fn new(conversation_id: Uuid, dh_public_key: [u8; 32]) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            conversation_id,
            dh_public_key,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DirectMessageEnvelope {
    pub version: u8,
    pub id: Uuid,
    pub community_id: Uuid,
    pub conversation_id: Uuid,
    pub sender_key: [u8; 32],
    pub recipient_key: [u8; 32],
    pub ratchet: RatchetMessage,
    pub created_at: u64,
    pub signature: Vec<u8>,
}

#[derive(Serialize)]
struct SignedFields<'a> {
    version: u8,
    id: Uuid,
    community_id: Uuid,
    conversation_id: Uuid,
    sender_key: &'a [u8; 32],
    recipient_key: &'a [u8; 32],
    ratchet: &'a RatchetMessage,
    created_at: u64,
}

impl DirectMessageEnvelope {
    pub fn create(
        identity: &DeviceIdentity,
        community_id: Uuid,
        conversation_id: Uuid,
        recipient_key: [u8; 32],
        ratchet: RatchetMessage,
        created_at: u64,
    ) -> Result<Self, DirectMessageError> {
        let sender_key = identity.public_key_bytes();
        if sender_key == recipient_key {
            return Err(DirectMessageError::SelfRecipient);
        }
        let mut envelope = Self {
            version: ENVELOPE_VERSION,
            id: Uuid::new_v4(),
            community_id,
            conversation_id,
            sender_key,
            recipient_key,
            ratchet,
            created_at,
            signature: Vec::new(),
        };
        envelope.signature = identity.sign(&envelope.signing_bytes()?).to_vec();
        Ok(envelope)
    }

    pub fn verify(&self, now: u64) -> Result<(), DirectMessageError> {
        if self.version != ENVELOPE_VERSION {
            return Err(DirectMessageError::UnsupportedVersion);
        }
        if self.sender_key == self.recipient_key {
            return Err(DirectMessageError::SelfRecipient);
        }
        if self.created_at > now.saturating_add(MAX_MESSAGE_AGE_SECONDS) {
            return Err(DirectMessageError::FutureTimestamp);
        }
        if now.saturating_sub(self.created_at) > MAX_MESSAGE_AGE_SECONDS {
            return Err(DirectMessageError::StaleTimestamp);
        }
        self.verify_signature()
    }

    /// Verify the durable envelope without applying the short transport age window.
    pub fn verify_signature(&self) -> Result<(), DirectMessageError> {
        if self.version != ENVELOPE_VERSION {
            return Err(DirectMessageError::UnsupportedVersion);
        }
        if self.sender_key == self.recipient_key {
            return Err(DirectMessageError::SelfRecipient);
        }
        let signature: [u8; Signature::BYTE_SIZE] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| DirectMessageError::InvalidSignatureLength)?;
        DeviceIdentity::verify(&self.sender_key, &self.signing_bytes()?, &signature)?;
        Ok(())
    }

    fn signing_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&SignedFields {
            version: self.version,
            id: self.id,
            community_id: self.community_id,
            conversation_id: self.conversation_id,
            sender_key: &self.sender_key,
            recipient_key: &self.recipient_key,
            ratchet: &self.ratchet,
            created_at: self.created_at,
        })
    }
}

#[must_use]
pub fn direct_conversation_id(
    community_id: Uuid,
    first_key: [u8; 32],
    second_key: [u8; 32],
) -> Uuid {
    let (first, second) = if first_key <= second_key {
        (first_key, second_key)
    } else {
        (second_key, first_key)
    };
    let mut bytes = Vec::with_capacity(16 + 32 + 32 + 28);
    bytes.extend_from_slice(b"nexo-direct-conversation-v1");
    bytes.extend_from_slice(community_id.as_bytes());
    bytes.extend_from_slice(&first);
    bytes.extend_from_slice(&second);
    Uuid::new_v5(&Uuid::NAMESPACE_OID, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DoubleRatchetSession, public_key_from_private};

    #[test]
    fn envelope_is_signed_and_conversation_is_symmetric() {
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let mut ratchet = DoubleRatchetSession::initialize_initiator(
            [3_u8; 32],
            public_key_from_private([8_u8; 32]),
        );
        let conversation = direct_conversation_id(
            community_id,
            alice.public_key_bytes(),
            bob.public_key_bytes(),
        );
        let envelope = DirectMessageEnvelope::create(
            &alice,
            community_id,
            conversation,
            bob.public_key_bytes(),
            ratchet.encrypt(b"hello"),
            100,
        )
        .expect("envelope should be signed");
        envelope.verify(100).expect("envelope should verify");
        assert_eq!(
            conversation,
            direct_conversation_id(
                community_id,
                bob.public_key_bytes(),
                alice.public_key_bytes()
            )
        );
        assert_eq!(envelope.ratchet.ciphertext.len(), 21);
    }

    #[test]
    fn envelope_rejects_tampering() {
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let mut ratchet = DoubleRatchetSession::initialize_initiator(
            [4_u8; 32],
            public_key_from_private([5_u8; 32]),
        );
        let mut envelope = DirectMessageEnvelope::create(
            &alice,
            Uuid::new_v4(),
            Uuid::new_v4(),
            bob.public_key_bytes(),
            ratchet.encrypt(b"signed"),
            100,
        )
        .expect("envelope should be signed");
        envelope.created_at += 1;
        assert!(envelope.verify(100).is_err());
    }
}
