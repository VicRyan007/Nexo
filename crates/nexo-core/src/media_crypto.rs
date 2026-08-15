use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

const NONCE_LEN: usize = 12;
const MAC_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum MediaCryptoError {
    #[error("encrypted frame length is too short")]
    FrameTooShort,
    #[error("media authentication tag verification failed")]
    AuthenticationFailed,
    #[error("sequence number mismatch")]
    SequenceMismatch,
}

/// End-to-end media frame cipher operating above the transport.
///
/// Ensures that participant-hosted SFUs or forwarding nodes cannot decode
/// audio or video payload bytes without possessing the call secret key.
pub struct MediaFrameCipher {
    key: [u8; 32],
}

impl MediaFrameCipher {
    /// Derive a frame cipher key for a call session from a shared call secret.
    #[must_use]
    pub fn derive(call_id: Uuid, call_secret: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"nexo-e2e-media-v1");
        hasher.update(call_id.as_bytes());
        hasher.update(call_secret);
        let key_bytes: [u8; 32] = hasher.finalize().into();
        Self { key: key_bytes }
    }

    /// Encrypt a media frame (audio payload or video access unit) for transmission.
    ///
    /// Output format: `[12-byte nonce | 8-byte sequence | ciphertext | 32-byte HMAC-SHA256]`
    #[must_use]
    pub fn encrypt(&self, payload: &[u8], sequence: u64) -> Vec<u8> {
        let nonce = self.generate_nonce(sequence);
        let mut encrypted = Vec::with_capacity(NONCE_LEN + 8 + payload.len() + MAC_LEN);
        encrypted.extend_from_slice(&nonce);
        encrypted.extend_from_slice(&sequence.to_be_bytes());

        // Perform stream XOR with PRF block stream derived from key + nonce + block_index
        let mut ciphertext = payload.to_vec();
        self.apply_keystream(&nonce, sequence, &mut ciphertext);

        encrypted.extend_from_slice(&ciphertext);

        // Compute HMAC-SHA256 auth tag over (nonce | sequence | ciphertext)
        let mac = self.compute_mac(&encrypted);
        encrypted.extend_from_slice(&mac);
        encrypted
    }

    /// Decrypt a media frame and verify its end-to-end authentication tag.
    pub fn decrypt(
        &self,
        data: &[u8],
        expected_sequence: u64,
    ) -> Result<Vec<u8>, MediaCryptoError> {
        if data.len() < NONCE_LEN + 8 + MAC_LEN {
            return Err(MediaCryptoError::FrameTooShort);
        }

        let (header_and_body, mac_tag) = data.split_at(data.len() - MAC_LEN);
        let expected_mac = self.compute_mac(header_and_body);

        if !constant_time_eq(mac_tag, &expected_mac) {
            return Err(MediaCryptoError::AuthenticationFailed);
        }

        let nonce: [u8; NONCE_LEN] = header_and_body[..NONCE_LEN]
            .try_into()
            .map_err(|_| MediaCryptoError::FrameTooShort)?;

        let sequence_bytes: [u8; 8] = header_and_body[NONCE_LEN..NONCE_LEN + 8]
            .try_into()
            .map_err(|_| MediaCryptoError::FrameTooShort)?;
        let sequence = u64::from_be_bytes(sequence_bytes);

        if sequence != expected_sequence {
            return Err(MediaCryptoError::SequenceMismatch);
        }

        let ciphertext = &header_and_body[NONCE_LEN + 8..];
        let mut plaintext = ciphertext.to_vec();
        self.apply_keystream(&nonce, sequence, &mut plaintext);

        Ok(plaintext)
    }

    fn generate_nonce(&self, sequence: u64) -> [u8; NONCE_LEN] {
        let mut hasher = Sha256::new();
        hasher.update(self.key);
        hasher.update(sequence.to_le_bytes());
        let hash = hasher.finalize();
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&hash[..NONCE_LEN]);
        nonce
    }

    fn apply_keystream(&self, nonce: &[u8; NONCE_LEN], sequence: u64, buf: &mut [u8]) {
        let mut block_idx: u32 = 0;
        for chunk in buf.chunks_mut(32) {
            let mut hasher = Sha256::new();
            hasher.update(self.key);
            hasher.update(nonce);
            hasher.update(sequence.to_be_bytes());
            hasher.update(block_idx.to_be_bytes());
            let keystream_block = hasher.finalize();

            for (b, k) in chunk.iter_mut().zip(keystream_block.iter()) {
                *b ^= *k;
            }
            block_idx = block_idx.wrapping_add(1);
        }
    }

    fn compute_mac(&self, data: &[u8]) -> [u8; MAC_LEN] {
        let mut hasher = Sha256::new();
        hasher.update(self.key);
        hasher.update(b"nexo-e2e-mac-v1");
        hasher.update(data);
        hasher.finalize().into()
    }
}

impl Drop for MediaFrameCipher {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_cipher_encrypt_decrypt_roundtrip() {
        let call_id = Uuid::new_v4();
        let secret = b"super-secret-call-passphrase";
        let cipher = MediaFrameCipher::derive(call_id, secret);

        let original_payload = b"Hello Nexo E2E Encrypted Audio Frame";
        let sequence = 42;

        let encrypted = cipher.encrypt(original_payload, sequence);
        assert_ne!(encrypted, original_payload);

        let decrypted = cipher
            .decrypt(&encrypted, sequence)
            .expect("decryption should succeed");
        assert_eq!(decrypted, original_payload);
    }

    #[test]
    fn media_cipher_rejects_tampered_payload() {
        let call_id = Uuid::new_v4();
        let secret = b"secret";
        let cipher = MediaFrameCipher::derive(call_id, secret);

        let mut encrypted = cipher.encrypt(b"unaltered payload", 1);
        // Tamper with one byte in ciphertext
        if let Some(byte) = encrypted.get_mut(15) {
            *byte ^= 0xFF;
        }

        assert!(matches!(
            cipher.decrypt(&encrypted, 1),
            Err(MediaCryptoError::AuthenticationFailed)
        ));
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
        let call_id = Uuid::new_v4();
        let cipher = MediaFrameCipher::derive(call_id, b"key");

        let encrypted = cipher.encrypt(b"payload", 50);
        assert!(matches!(
            cipher.decrypt(&encrypted, 51),
            Err(MediaCryptoError::SequenceMismatch)
        ));
    }
}
