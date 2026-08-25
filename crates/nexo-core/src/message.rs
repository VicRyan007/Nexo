use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use ed25519_dalek::Signature;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{DeviceIdentity, IdentityError};

const MESSAGE_VERSION: u8 = 1;
const ENCRYPTED_MESSAGE_VERSION: u8 = 2;
const ENCRYPTED_BODY_PREFIX: &str = "NEXO-MSG2";
const MESSAGE_NONCE_BYTES: usize = 12;
const MESSAGE_TAG_BYTES: usize = 16;
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_WIRE_BODY_BYTES: usize = 32 * 1024;
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
    #[error("the encrypted message envelope is malformed")]
    MalformedEncryptedBody,
    #[error("the encrypted message belongs to another community or epoch")]
    WrongEncryptionContext,
    #[error("the encrypted message could not be authenticated")]
    DecryptionFailed,
    #[error("the decrypted message is not valid UTF-8")]
    InvalidUtf8,
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

#[derive(Serialize)]
struct EncryptedMessageAssociatedData {
    id: Uuid,
    community_id: Uuid,
    channel_id: Uuid,
    author_key: [u8; 32],
    epoch: u64,
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

    /// Create a community message whose wire body is an authenticated
    /// ChaCha20-Poly1305 envelope derived from the current MLS epoch.
    pub fn create_encrypted(
        identity: &DeviceIdentity,
        community_id: Uuid,
        channel_id: Uuid,
        body: &str,
        created_at: u64,
        group: &crate::MlsGroupState,
    ) -> Result<Self, MessageError> {
        validate_body(body)?;
        if group.group_id != community_id {
            return Err(MessageError::WrongEncryptionContext);
        }
        let mut message = Self {
            version: ENCRYPTED_MESSAGE_VERSION,
            id: Uuid::new_v4(),
            community_id,
            channel_id,
            author_key: identity.public_key_bytes(),
            body: String::new(),
            created_at,
            signature: Vec::new(),
        };
        let associated_data = message.associated_data(group.epoch)?;
        let key = group.derive_application_secret("community-message");
        let mut nonce = [0_u8; MESSAGE_NONCE_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new((&key).into());
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: body.as_bytes(),
                    aad: &associated_data,
                },
            )
            .map_err(|_| MessageError::DecryptionFailed)?;
        let mut encoded = String::with_capacity(
            ENCRYPTED_BODY_PREFIX.len() + 1 + 20 + 1 + (nonce.len() + ciphertext.len()) * 2,
        );
        encoded.push_str(ENCRYPTED_BODY_PREFIX);
        encoded.push('.');
        encoded.push_str(&group.epoch.to_string());
        encoded.push('.');
        let mut envelope = Vec::with_capacity(nonce.len() + ciphertext.len());
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&ciphertext);
        encoded.push_str(&URL_SAFE_NO_PAD.encode(envelope));
        if encoded.len() > MAX_WIRE_BODY_BYTES {
            return Err(MessageError::BodyTooLarge);
        }
        message.body = encoded;
        message.signature = identity.sign(&message.signing_bytes()?).to_vec();
        Ok(message)
    }

    pub fn verify(&self, now: u64) -> Result<(), MessageError> {
        if self.version != MESSAGE_VERSION && self.version != ENCRYPTED_MESSAGE_VERSION {
            return Err(MessageError::UnsupportedVersion);
        }
        if self.version == ENCRYPTED_MESSAGE_VERSION {
            if self.body.len() > MAX_WIRE_BODY_BYTES {
                return Err(MessageError::BodyTooLarge);
            }
            parse_encrypted_body(&self.body)?;
        } else {
            validate_body(&self.body)?;
        }
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

    /// Decrypt the body using the matching persisted community epoch.
    pub fn decrypt_body(&self, group: &crate::MlsGroupState) -> Result<String, MessageError> {
        if self.version == MESSAGE_VERSION {
            return Ok(self.body.clone());
        }
        if self.version != ENCRYPTED_MESSAGE_VERSION || group.group_id != self.community_id {
            return Err(MessageError::WrongEncryptionContext);
        }
        let (epoch, envelope) = parse_encrypted_body(&self.body)?;
        let key = group
            .derive_application_secret_for_epoch(epoch, "community-message")
            .ok_or(MessageError::WrongEncryptionContext)?;
        let nonce: [u8; MESSAGE_NONCE_BYTES] = envelope[..MESSAGE_NONCE_BYTES]
            .try_into()
            .map_err(|_| MessageError::MalformedEncryptedBody)?;
        let associated_data = self.associated_data(epoch)?;
        let cipher = ChaCha20Poly1305::new((&key).into());
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &envelope[MESSAGE_NONCE_BYTES..],
                    aad: &associated_data,
                },
            )
            .map_err(|_| MessageError::DecryptionFailed)?;
        let plaintext = String::from_utf8(plaintext).map_err(|_| MessageError::InvalidUtf8)?;
        validate_body(&plaintext)?;
        Ok(plaintext)
    }

    fn associated_data(&self, epoch: u64) -> Result<Vec<u8>, MessageError> {
        serde_json::to_vec(&EncryptedMessageAssociatedData {
            id: self.id,
            community_id: self.community_id,
            channel_id: self.channel_id,
            author_key: self.author_key,
            epoch,
            created_at: self.created_at,
        })
        .map_err(MessageError::Serialization)
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

fn parse_encrypted_body(body: &str) -> Result<(u64, Vec<u8>), MessageError> {
    let mut parts = body.splitn(3, '.');
    if parts.next() != Some(ENCRYPTED_BODY_PREFIX) {
        return Err(MessageError::MalformedEncryptedBody);
    }
    let epoch = parts
        .next()
        .ok_or(MessageError::MalformedEncryptedBody)?
        .parse::<u64>()
        .map_err(|_| MessageError::MalformedEncryptedBody)?;
    let envelope = URL_SAFE_NO_PAD
        .decode(parts.next().ok_or(MessageError::MalformedEncryptedBody)?)
        .map_err(|_| MessageError::MalformedEncryptedBody)?;
    if envelope.len() < MESSAGE_NONCE_BYTES + MESSAGE_TAG_BYTES {
        return Err(MessageError::MalformedEncryptedBody);
    }
    Ok((epoch, envelope))
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
    use crate::MlsGroupState;

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

    #[test]
    fn encrypted_message_round_trips_and_hides_plaintext() {
        let identity = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let group = MlsGroupState::new(
            community_id,
            "founder".to_owned(),
            identity.public_key_bytes(),
        );
        let message = SignedMessage::create_encrypted(
            &identity,
            community_id,
            channel_id,
            "segredo da comunidade",
            100,
            &group,
        )
        .expect("encrypted message should be created");

        assert_eq!(message.version, ENCRYPTED_MESSAGE_VERSION);
        assert!(!message.body.contains("segredo"));
        message
            .verify(100)
            .expect("encrypted envelope should verify");
        assert_eq!(
            message
                .decrypt_body(&group)
                .expect("message should decrypt"),
            "segredo da comunidade"
        );
        let wrong_secret_group = MlsGroupState::new_with_secret(
            community_id,
            "founder".to_owned(),
            identity.public_key_bytes(),
            [0x99; 32],
        );
        assert!(matches!(
            message.decrypt_body(&wrong_secret_group),
            Err(MessageError::DecryptionFailed)
        ));
    }

    #[test]
    fn encrypted_message_rejects_tampering_wrong_group_and_keeps_old_epoch() {
        let identity = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let mut group = MlsGroupState::new(
            community_id,
            "founder".to_owned(),
            identity.public_key_bytes(),
        );
        let message = SignedMessage::create_encrypted(
            &identity,
            community_id,
            channel_id,
            "mensagem anterior",
            100,
            &group,
        )
        .expect("encrypted message should be created");
        let mut tampered = message.clone();
        tampered.body.push('x');
        assert!(tampered.verify(100).is_err());

        group.add_member("new-device".to_owned(), [0x44; 32]);
        assert_eq!(
            message.decrypt_body(&group).expect("old epoch is retained"),
            "mensagem anterior"
        );

        let wrong_group = MlsGroupState::new(Uuid::new_v4(), "other".to_owned(), [0x22; 32]);
        assert!(matches!(
            message.decrypt_body(&wrong_group),
            Err(MessageError::WrongEncryptionContext)
        ));
    }
}
