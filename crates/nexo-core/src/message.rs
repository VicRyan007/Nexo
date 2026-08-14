use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{DeviceIdentity, IdentityError};

const MESSAGE_VERSION: u8 = 1;
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_CLOCK_SKEW_SECONDS: u64 = 5 * 60;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedMessage {
    pub version: u8,
    pub id: Uuid,
    pub community_id: Uuid,
    pub channel_id: Uuid,
    pub author_key: [u8; 32],
    pub body: String,
    pub created_at: u64,
    pub signature: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum MessageError {
    #[error("the message body is empty")]
    EmptyBody,
    #[error("the message body exceeds {MAX_BODY_BYTES} bytes")]
    BodyTooLarge,
    #[error("the message version is unsupported")]
    UnsupportedVersion,
    #[error("the message timestamp is too far in the future")]
    FutureTimestamp,
    #[error("the message signature has an invalid length")]
    InvalidSignatureLength,
    #[error("the message could not be serialized: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("the message signature is invalid: {0}")]
    Identity(#[from] IdentityError),
}

#[derive(Serialize)]
struct SignedFields<'a> {
    version: u8,
    id: Uuid,
    community_id: Uuid,
    channel_id: Uuid,
    author_key: &'a [u8; 32],
    body: &'a str,
    created_at: u64,
}

impl SignedMessage {
    pub fn create(
        identity: &DeviceIdentity,
        community_id: Uuid,
        channel_id: Uuid,
        body: String,
        created_at: u64,
    ) -> Result<Self, MessageError> {
        validate_body(&body)?;
        let mut message = Self {
            version: MESSAGE_VERSION,
            id: Uuid::new_v4(),
            community_id,
            channel_id,
            author_key: identity.public_key_bytes(),
            body,
            created_at,
            signature: Vec::new(),
        };
        message.signature = identity.sign(&message.signing_bytes()?).to_vec();
        Ok(message)
    }

    pub fn verify(&self, now: u64) -> Result<(), MessageError> {
        if self.version != MESSAGE_VERSION {
            return Err(MessageError::UnsupportedVersion);
        }
        validate_body(&self.body)?;
        if self.created_at > now.saturating_add(MAX_CLOCK_SKEW_SECONDS) {
            return Err(MessageError::FutureTimestamp);
        }
        let signature: [u8; Signature::BYTE_SIZE] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| MessageError::InvalidSignatureLength)?;
        DeviceIdentity::verify(&self.author_key, &self.signing_bytes()?, &signature)?;
        Ok(())
    }

    fn signing_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&SignedFields {
            version: self.version,
            id: self.id,
            community_id: self.community_id,
            channel_id: self.channel_id,
            author_key: &self.author_key,
            body: &self.body,
            created_at: self.created_at,
        })
    }
}

fn validate_body(body: &str) -> Result<(), MessageError> {
    if body.trim().is_empty() {
        return Err(MessageError::EmptyBody);
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(MessageError::BodyTooLarge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_message_round_trip() {
        let identity = DeviceIdentity::generate();
        let message = SignedMessage::create(
            &identity,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "ola, nexo".to_owned(),
            100,
        )
        .expect("message should be created");
        message.verify(100).expect("message should verify");

        let encoded = serde_json::to_vec(&message).expect("message should serialize");
        let decoded: SignedMessage =
            serde_json::from_slice(&encoded).expect("message should deserialize");
        assert_eq!(decoded, message);
    }

    #[test]
    fn rejects_tampered_message() {
        let identity = DeviceIdentity::generate();
        let mut message = SignedMessage::create(
            &identity,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "original".to_owned(),
            100,
        )
        .expect("message should be created");
        message.body = "alterada".to_owned();
        assert!(message.verify(100).is_err());
    }
}
