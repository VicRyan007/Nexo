use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::identity::{DeviceIdentity, IdentityError};

pub const DEFAULT_CHUNK_SIZE: u32 = 64 * 1024; // 64 KB per chunk

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TransferStatus {
    Pending,
    Downloading,
    Completed,
    Failed,
}

#[derive(Debug, Error)]
pub enum FileTransferError {
    #[error("identity operation failed: {0}")]
    Identity(#[from] IdentityError),
    #[error("the file transfer signature is invalid")]
    InvalidSignature,
    #[error("the file transfer offer has expired or has a future timestamp")]
    TimestampOutOfRange,
    #[error("the chunk checksum does not match its payload")]
    ChecksumMismatch,
    #[error("the chunk index {0} exceeds the total expected chunks {1}")]
    IndexOutOfBounds(u32, u32),
    #[error("the file size {0} does not match total chunk capacity")]
    InvalidSize(u64),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileTransferOffer {
    pub id: Uuid,
    pub community_id: Uuid,
    pub channel_id: Uuid,
    pub file_name: String,
    pub file_size: u64,
    pub mime_type: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
    pub root_sha256: [u8; 32],
    pub author_key: [u8; 32],
    pub created_at: u64,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileChunk {
    pub transfer_id: Uuid,
    pub chunk_index: u32,
    pub data: Vec<u8>,
    pub chunk_sha256: [u8; 32],
}

impl FileTransferOffer {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        identity: &DeviceIdentity,
        community_id: Uuid,
        channel_id: Uuid,
        file_name: String,
        file_size: u64,
        mime_type: String,
        root_sha256: [u8; 32],
        created_at: u64,
    ) -> Result<Self, FileTransferError> {
        Self::create_with_chunk_size(
            identity,
            community_id,
            channel_id,
            file_name,
            file_size,
            mime_type,
            root_sha256,
            created_at,
            DEFAULT_CHUNK_SIZE,
        )
    }

    /// Create a signed offer with an explicit chunk size. WebRTC data
    /// channels use a smaller value than the libp2p transfer stream because
    /// SCTP messages have a bounded maximum size.
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_chunk_size(
        identity: &DeviceIdentity,
        community_id: Uuid,
        channel_id: Uuid,
        file_name: String,
        file_size: u64,
        mime_type: String,
        root_sha256: [u8; 32],
        created_at: u64,
        chunk_size: u32,
    ) -> Result<Self, FileTransferError> {
        if chunk_size == 0 {
            return Err(FileTransferError::InvalidSize(file_size));
        }
        let total_chunks = if file_size == 0 {
            1
        } else {
            u32::try_from(file_size.div_ceil(u64::from(chunk_size)))
                .map_err(|_| FileTransferError::InvalidSize(file_size))?
        };

        let id = Uuid::new_v4();
        let author_key = identity.public_key_bytes();
        let payload = Self::signable_payload(
            id,
            community_id,
            channel_id,
            &file_name,
            file_size,
            &mime_type,
            chunk_size,
            total_chunks,
            &root_sha256,
            &author_key,
            created_at,
        );
        let signature = identity.sign(&payload).to_vec();

        Ok(Self {
            id,
            community_id,
            channel_id,
            file_name,
            file_size,
            mime_type,
            chunk_size,
            total_chunks,
            root_sha256,
            author_key,
            created_at,
            signature,
        })
    }

    pub fn verify(&self, now: u64) -> Result<(), FileTransferError> {
        if self.created_at > now.saturating_add(300)
            || self.created_at < now.saturating_sub(86400 * 7)
        {
            return Err(FileTransferError::TimestampOutOfRange);
        }
        let verifying_key = VerifyingKey::from_bytes(&self.author_key)
            .map_err(|_| FileTransferError::InvalidSignature)?;
        let signature_bytes: [u8; 64] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| FileTransferError::InvalidSignature)?;
        let signature = Signature::from_bytes(&signature_bytes);
        let payload = Self::signable_payload(
            self.id,
            self.community_id,
            self.channel_id,
            &self.file_name,
            self.file_size,
            &self.mime_type,
            self.chunk_size,
            self.total_chunks,
            &self.root_sha256,
            &self.author_key,
            self.created_at,
        );
        verifying_key
            .verify(&payload, &signature)
            .map_err(|_| FileTransferError::InvalidSignature)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn signable_payload(
        id: Uuid,
        community_id: Uuid,
        channel_id: Uuid,
        file_name: &str,
        file_size: u64,
        mime_type: &str,
        chunk_size: u32,
        total_chunks: u32,
        root_sha256: &[u8; 32],
        author_key: &[u8; 32],
        created_at: u64,
    ) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"NEXO_FILE_OFFER_V1");
        hasher.update(id.as_bytes());
        hasher.update(community_id.as_bytes());
        hasher.update(channel_id.as_bytes());
        hasher.update(file_name.as_bytes());
        hasher.update(file_size.to_be_bytes());
        hasher.update(mime_type.as_bytes());
        hasher.update(chunk_size.to_be_bytes());
        hasher.update(total_chunks.to_be_bytes());
        hasher.update(root_sha256);
        hasher.update(author_key);
        hasher.update(created_at.to_be_bytes());
        hasher.finalize().to_vec()
    }
}

impl FileChunk {
    #[must_use]
    pub fn new(transfer_id: Uuid, chunk_index: u32, data: Vec<u8>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let chunk_sha256: [u8; 32] = hasher.finalize().into();
        Self {
            transfer_id,
            chunk_index,
            data,
            chunk_sha256,
        }
    }

    pub fn verify(&self, expected_total_chunks: u32) -> Result<(), FileTransferError> {
        if self.chunk_index >= expected_total_chunks {
            return Err(FileTransferError::IndexOutOfBounds(
                self.chunk_index,
                expected_total_chunks,
            ));
        }
        let mut hasher = Sha256::new();
        hasher.update(&self.data);
        let actual_hash: [u8; 32] = hasher.finalize().into();
        if actual_hash != self.chunk_sha256 {
            return Err(FileTransferError::ChecksumMismatch);
        }
        Ok(())
    }
}

/// Compute SHA-256 hash of arbitrary bytes.
#[must_use]
pub fn compute_sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_transfer_offer_creates_signs_and_verifies() {
        let identity = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let now = 1_700_000_000;

        let content = b"Hello, P2P file sharing world in Nexo!";
        let root_hash = compute_sha256(content);

        let offer = FileTransferOffer::create(
            &identity,
            community_id,
            channel_id,
            "document.txt".into(),
            content.len() as u64,
            "text/plain".into(),
            root_hash,
            now,
        )
        .expect("offer should be created");

        assert_eq!(offer.total_chunks, 1);
        assert!(offer.verify(now).is_ok());

        // Chunk creation and verification
        let chunk = FileChunk::new(offer.id, 0, content.to_vec());
        assert!(chunk.verify(offer.total_chunks).is_ok());

        // Tampering chunk data fails checksum
        let mut bad_chunk = chunk;
        bad_chunk.data[0] ^= 0xFF;
        assert!(matches!(
            bad_chunk.verify(offer.total_chunks),
            Err(FileTransferError::ChecksumMismatch)
        ));
    }
}
