use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::AeadInPlace};
use rand::RngCore as _;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

const NONCE_LEN: usize = 12;
const SEQUENCE_LEN: usize = 8;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = NONCE_LEN + SEQUENCE_LEN;

#[derive(Debug, Error)]
pub enum MediaCryptoError {
    #[error("encrypted frame length is too short")]
    FrameTooShort,
    #[error("media authentication tag verification failed")]
    AuthenticationFailed,
    #[error("sequence number mismatch")]
    SequenceMismatch,
}

/// End-to-end media protection above WebRTC's transport encryption.
///
/// The relay only sees this authenticated ciphertext. ChaCha20-Poly1305 is
/// used as the proven AEAD primitive. Each frame carries a fresh random nonce
/// in its authenticated header, while the sequence remains available for
/// replay/order checks.
pub struct MediaFrameCipher {
    key: [u8; 32],
}

impl MediaFrameCipher {
    /// Derive a call-scoped key from the authenticated community secret.
    #[must_use]
    pub fn derive(call_id: Uuid, call_secret: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"nexo-e2e-media-chacha20poly1305-v2");
        hasher.update(call_id.as_bytes());
        hasher.update(call_secret);
        Self {
            key: hasher.finalize().into(),
        }
    }

    /// Encrypt a media frame as `[12-byte nonce | 8-byte sequence | AEAD]`.
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn encrypt(&self, payload: &[u8], sequence: u64) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let mut header = [0u8; HEADER_LEN];
        header[..NONCE_LEN].copy_from_slice(&nonce);
        header[NONCE_LEN..].copy_from_slice(&sequence.to_be_bytes());
        let mut ciphertext = payload.to_vec();
        let cipher = ChaCha20Poly1305::new((&self.key).into());
        cipher
            .encrypt_in_place(Nonce::from_slice(&nonce), &header, &mut ciphertext)
            .expect("ChaCha20-Poly1305 encryption cannot fail for an in-memory buffer");

        let mut encrypted = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        encrypted.extend_from_slice(&header);
        encrypted.extend_from_slice(&ciphertext);
        encrypted
    }

    /// Decrypt and authenticate a frame, requiring its expected sequence.
    pub fn decrypt(
        &self,
        data: &[u8],
        expected_sequence: u64,
    ) -> Result<Vec<u8>, MediaCryptoError> {
        if data.len() < HEADER_LEN + TAG_LEN {
            return Err(MediaCryptoError::FrameTooShort);
        }
        let (header, ciphertext) = data.split_at(HEADER_LEN);
        let nonce: [u8; NONCE_LEN] = header[..NONCE_LEN]
            .try_into()
            .map_err(|_| MediaCryptoError::FrameTooShort)?;
        let sequence_bytes: [u8; SEQUENCE_LEN] = header[NONCE_LEN..]
            .try_into()
            .map_err(|_| MediaCryptoError::FrameTooShort)?;
        let sequence = u64::from_be_bytes(sequence_bytes);
        if sequence != expected_sequence {
            return Err(MediaCryptoError::SequenceMismatch);
        }
        let mut plaintext = ciphertext.to_vec();
        let cipher = ChaCha20Poly1305::new((&self.key).into());
        cipher
            .decrypt_in_place(Nonce::from_slice(&nonce), header, &mut plaintext)
            .map_err(|_| MediaCryptoError::AuthenticationFailed)?;
        Ok(plaintext)
    }

    /// Read an authenticated envelope sequence before decryption.
    pub fn sequence(data: &[u8]) -> Result<u64, MediaCryptoError> {
        if data.len() < HEADER_LEN + TAG_LEN {
            return Err(MediaCryptoError::FrameTooShort);
        }
        let sequence_bytes: [u8; SEQUENCE_LEN] = data[NONCE_LEN..HEADER_LEN]
            .try_into()
            .map_err(|_| MediaCryptoError::FrameTooShort)?;
        Ok(u64::from_be_bytes(sequence_bytes))
    }
}

impl Drop for MediaFrameCipher {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_cipher_encrypt_decrypt_roundtrip() {
        let call_id = Uuid::new_v4();
        let cipher = MediaFrameCipher::derive(call_id, b"super-secret-call-passphrase");
        let original_payload = b"Hello Nexo E2E Encrypted Audio Frame";
        let encrypted = cipher.encrypt(original_payload, 42);

        assert_ne!(encrypted, original_payload);
        assert_eq!(
            MediaFrameCipher::sequence(&encrypted).expect("sequence is present"),
            42
        );
        assert_eq!(
            cipher
                .decrypt(&encrypted, 42)
                .expect("ciphertext authenticates"),
            original_payload
        );
    }

    #[test]
    fn media_cipher_rejects_tampered_payload_and_header() {
        let cipher = MediaFrameCipher::derive(Uuid::new_v4(), b"secret");
        let mut encrypted = cipher.encrypt(b"unaltered payload", 1);
        encrypted[HEADER_LEN] ^= 0xFF;
        assert!(matches!(
            cipher.decrypt(&encrypted, 1),
            Err(MediaCryptoError::AuthenticationFailed)
        ));

        let mut header_tampered = cipher.encrypt(b"payload", 2);
        header_tampered[0] ^= 0xFF;
        assert!(matches!(
            cipher.decrypt(&header_tampered, 2),
            Err(MediaCryptoError::AuthenticationFailed)
        ));
    }

    #[test]
    fn media_cipher_uses_a_fresh_nonce_for_repeated_sequences() {
        let cipher = MediaFrameCipher::derive(Uuid::new_v4(), b"secret");
        let first = cipher.encrypt(b"same payload", 7);
        let second = cipher.encrypt(b"same payload", 7);

        assert_ne!(&first[..NONCE_LEN], &second[..NONCE_LEN]);
        assert_eq!(
            cipher
                .decrypt(&first, 7)
                .expect("first frame authenticates"),
            b"same payload"
        );
        assert_eq!(
            cipher
                .decrypt(&second, 7)
                .expect("second frame authenticates"),
            b"same payload"
        );
    }

    #[test]
    fn media_cipher_rejects_wrong_key() {
        let call_id = Uuid::new_v4();
        let cipher1 = MediaFrameCipher::derive(call_id, b"key1");
        let cipher2 = MediaFrameCipher::derive(call_id, b"key2");
        let encrypted = cipher1.encrypt(b"secret message", 100);

        assert!(matches!(
            cipher2.decrypt(&encrypted, 100),
            Err(MediaCryptoError::AuthenticationFailed)
        ));
    }

    #[test]
    fn media_cipher_rejects_wrong_sequence() {
        let cipher = MediaFrameCipher::derive(Uuid::new_v4(), b"key");
        let encrypted = cipher.encrypt(b"payload", 50);

        assert!(matches!(
            cipher.decrypt(&encrypted, 51),
            Err(MediaCryptoError::SequenceMismatch)
        ));
    }
}
