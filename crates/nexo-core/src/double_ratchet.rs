//! A small, authenticated Double Ratchet session for one-to-one messages.
//!
//! X25519 provides the DH ratchet and ChaCha20-Poly1305 provides authenticated
//! encryption for each message key. The wire type is intentionally independent
//! from storage and transport so callers can persist or deliver it as needed.

#![allow(clippy::missing_panics_doc)]

use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::AeadInPlace};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

const NONCE_LEN: usize = 12;
const AUTH_TAG_LEN: usize = 16;

#[derive(Debug, Eq, Error, PartialEq)]
pub enum RatchetError {
    #[error("cryptographic key exchange failed")]
    KeyExchangeFailed,
    #[error("message decryption failed or message was tampered")]
    DecryptionFailed,
    #[error("message sequence number is invalid or duplicate")]
    DuplicateOrOutOfOrder,
}

/// A ciphertext message produced by the Double Ratchet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RatchetMessage {
    pub dh_public_key: [u8; 32],
    pub sequence_number: u32,
    pub previous_chain_length: u32,
    pub ciphertext: Vec<u8>,
}

/// Serializable session checkpoint used to resume a DM after restarting Nexo.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DoubleRatchetState {
    pub dh_our_private: [u8; 32],
    pub dh_our_public: [u8; 32],
    pub dh_remote_public: Option<[u8; 32]>,
    pub root_key: [u8; 32],
    pub send_chain_key: Option<[u8; 32]>,
    pub recv_chain_key: Option<[u8; 32]>,
    pub send_seq: u32,
    pub recv_seq: u32,
    pub prev_send_chain_len: u32,
}

/// Return the X25519 public key corresponding to a private key.
#[must_use]
pub fn public_key_from_private(private_key: [u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(private_key)).to_bytes()
}

/// Derive a stable first responder key from a community secret and identity.
/// The initiator's first DH key remains random; only the responder bootstrap
/// needs to be discoverable before decrypting the first message.
#[must_use]
pub fn derive_initial_private(
    shared_master_secret: [u8; 32],
    identity_public: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"nexo-double-ratchet-initial-private-v2");
    hasher.update(shared_master_secret);
    hasher.update(identity_public);
    hasher.finalize().into()
}

fn kdf_ck(chain_key: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut next_hasher = Sha256::new();
    next_hasher.update(b"nexo-double-ratchet-chain-next-v2");
    next_hasher.update(chain_key);
    let next_chain_key: [u8; 32] = next_hasher.finalize().into();

    let mut message_hasher = Sha256::new();
    message_hasher.update(b"nexo-double-ratchet-message-key-v2");
    message_hasher.update(chain_key);
    let message_key: [u8; 32] = message_hasher.finalize().into();

    (next_chain_key, message_key)
}

fn kdf_rk(root_key: &[u8; 32], dh_shared: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut root_hasher = Sha256::new();
    root_hasher.update(b"nexo-double-ratchet-root-v2");
    root_hasher.update(root_key);
    root_hasher.update(dh_shared);
    let next_root_key: [u8; 32] = root_hasher.finalize().into();

    let mut chain_hasher = Sha256::new();
    chain_hasher.update(b"nexo-double-ratchet-dh-chain-v2");
    chain_hasher.update(root_key);
    chain_hasher.update(dh_shared);
    let next_chain_key: [u8; 32] = chain_hasher.finalize().into();

    (next_root_key, next_chain_key)
}

fn compute_dh(our_private: [u8; 32], remote_public: [u8; 32]) -> [u8; 32] {
    StaticSecret::from(our_private)
        .diffie_hellman(&PublicKey::from(remote_public))
        .to_bytes()
}

fn nonce(sequence_number: u32) -> [u8; NONCE_LEN] {
    let mut value = [0u8; NONCE_LEN];
    value[..4].copy_from_slice(b"NEXO");
    value[8..].copy_from_slice(&sequence_number.to_be_bytes());
    value
}

fn associated_data(
    dh_public_key: &[u8; 32],
    sequence_number: u32,
    previous_chain_length: u32,
) -> [u8; 40] {
    let mut value = [0u8; 40];
    value[..32].copy_from_slice(dh_public_key);
    value[32..36].copy_from_slice(&sequence_number.to_be_bytes());
    value[36..].copy_from_slice(&previous_chain_length.to_be_bytes());
    value
}

fn encrypt_payload(
    message_key: &[u8; 32],
    dh_public_key: &[u8; 32],
    sequence_number: u32,
    previous_chain_length: u32,
    plaintext: &[u8],
) -> Vec<u8> {
    let mut ciphertext = plaintext.to_vec();
    let cipher = ChaCha20Poly1305::new(message_key.into());
    cipher
        .encrypt_in_place(
            Nonce::from_slice(&nonce(sequence_number)),
            &associated_data(dh_public_key, sequence_number, previous_chain_length),
            &mut ciphertext,
        )
        .expect("ChaCha20-Poly1305 encryption cannot fail for an in-memory buffer");
    ciphertext
}

fn decrypt_payload(
    message_key: &[u8; 32],
    dh_public_key: &[u8; 32],
    sequence_number: u32,
    previous_chain_length: u32,
    ciphertext: &[u8],
) -> Result<Vec<u8>, RatchetError> {
    if ciphertext.len() < AUTH_TAG_LEN {
        return Err(RatchetError::DecryptionFailed);
    }
    let mut plaintext = ciphertext.to_vec();
    let cipher = ChaCha20Poly1305::new(message_key.into());
    cipher
        .decrypt_in_place(
            Nonce::from_slice(&nonce(sequence_number)),
            &associated_data(dh_public_key, sequence_number, previous_chain_length),
            &mut plaintext,
        )
        .map_err(|_| RatchetError::DecryptionFailed)?;
    Ok(plaintext)
}

/// A Double Ratchet session state machine between two authenticated peers.
#[derive(Debug)]
pub struct DoubleRatchetSession {
    dh_our_private: [u8; 32],
    dh_our_public: [u8; 32],
    dh_remote_public: Option<[u8; 32]>,
    root_key: [u8; 32],
    send_chain_key: Option<[u8; 32]>,
    recv_chain_key: Option<[u8; 32]>,
    send_seq: u32,
    recv_seq: u32,
    prev_send_chain_len: u32,
}

impl DoubleRatchetSession {
    /// Initialize an initiator with a shared master secret and responder key.
    #[must_use]
    pub fn initialize_initiator(
        shared_master_secret: [u8; 32],
        remote_dh_public: [u8; 32],
    ) -> Self {
        let mut our_private = [0u8; 32];
        let mut rng = rand::rngs::OsRng;
        rng.fill_bytes(&mut our_private);
        let our_public = public_key_from_private(our_private);
        let dh_shared = compute_dh(our_private, remote_dh_public);
        let (root_key, send_chain_key) = kdf_rk(&shared_master_secret, &dh_shared);

        Self {
            dh_our_private: our_private,
            dh_our_public: our_public,
            dh_remote_public: Some(remote_dh_public),
            root_key,
            send_chain_key: Some(send_chain_key),
            recv_chain_key: None,
            send_seq: 0,
            recv_seq: 0,
            prev_send_chain_len: 0,
        }
    }

    /// Initialize a responder with its private initial DH key.
    #[must_use]
    pub fn initialize_responder(
        shared_master_secret: [u8; 32],
        our_initial_private: [u8; 32],
    ) -> Self {
        let our_public = public_key_from_private(our_initial_private);
        Self {
            dh_our_private: our_initial_private,
            dh_our_public: our_public,
            dh_remote_public: None,
            root_key: shared_master_secret,
            send_chain_key: None,
            recv_chain_key: None,
            send_seq: 0,
            recv_seq: 0,
            prev_send_chain_len: 0,
        }
    }

    /// Return the current DH public key for a session hello.
    #[must_use]
    pub fn dh_public_key(&self) -> [u8; 32] {
        self.dh_our_public
    }

    /// Whether this side currently has a sending chain available.
    #[must_use]
    pub fn can_encrypt(&self) -> bool {
        self.send_chain_key.is_some()
    }

    /// Export all ratchet state needed for an offline/restart checkpoint.
    #[must_use]
    pub fn state(&self) -> DoubleRatchetState {
        DoubleRatchetState {
            dh_our_private: self.dh_our_private,
            dh_our_public: self.dh_our_public,
            dh_remote_public: self.dh_remote_public,
            root_key: self.root_key,
            send_chain_key: self.send_chain_key,
            recv_chain_key: self.recv_chain_key,
            send_seq: self.send_seq,
            recv_seq: self.recv_seq,
            prev_send_chain_len: self.prev_send_chain_len,
        }
    }

    /// Restore a ratchet from a validated local checkpoint.
    #[must_use]
    pub fn from_state(state: &DoubleRatchetState) -> Self {
        Self {
            dh_our_private: state.dh_our_private,
            dh_our_public: state.dh_our_public,
            dh_remote_public: state.dh_remote_public,
            root_key: state.root_key,
            send_chain_key: state.send_chain_key,
            recv_chain_key: state.recv_chain_key,
            send_seq: state.send_seq,
            recv_seq: state.recv_seq,
            prev_send_chain_len: state.prev_send_chain_len,
        }
    }

    /// Encrypt a plaintext message with the current sending chain key.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> RatchetMessage {
        let chain_key = self.send_chain_key.expect("send chain key initialized");
        let (next_chain_key, message_key) = kdf_ck(&chain_key);
        self.send_chain_key = Some(next_chain_key);

        let sequence_number = self.send_seq;
        self.send_seq = self.send_seq.saturating_add(1);
        let ciphertext = encrypt_payload(
            &message_key,
            &self.dh_our_public,
            sequence_number,
            self.prev_send_chain_len,
            plaintext,
        );

        RatchetMessage {
            dh_public_key: self.dh_our_public,
            sequence_number,
            previous_chain_length: self.prev_send_chain_len,
            ciphertext,
        }
    }

    /// Decrypt an incoming message, rejecting duplicates and reordering.
    pub fn decrypt(&mut self, message: &RatchetMessage) -> Result<Vec<u8>, RatchetError> {
        if self.dh_remote_public != Some(message.dh_public_key) {
            self.dh_step(message.dh_public_key);
        }
        if message.sequence_number != self.recv_seq {
            return Err(RatchetError::DuplicateOrOutOfOrder);
        }
        let chain_key = self.recv_chain_key.ok_or(RatchetError::KeyExchangeFailed)?;
        let (next_chain_key, message_key) = kdf_ck(&chain_key);
        let plaintext = decrypt_payload(
            &message_key,
            &message.dh_public_key,
            message.sequence_number,
            message.previous_chain_length,
            &message.ciphertext,
        )?;
        self.recv_chain_key = Some(next_chain_key);
        self.recv_seq = self.recv_seq.saturating_add(1);
        Ok(plaintext)
    }

    fn dh_step(&mut self, new_remote_dh: [u8; 32]) {
        self.prev_send_chain_len = self.send_seq;
        self.send_seq = 0;
        self.recv_seq = 0;
        self.dh_remote_public = Some(new_remote_dh);

        let dh_recv_shared = compute_dh(self.dh_our_private, new_remote_dh);
        let (root_key, recv_chain_key) = kdf_rk(&self.root_key, &dh_recv_shared);
        self.root_key = root_key;
        self.recv_chain_key = Some(recv_chain_key);

        let mut next_private = [0u8; 32];
        let mut rng = rand::rngs::OsRng;
        rng.fill_bytes(&mut next_private);
        self.dh_our_private = next_private;
        self.dh_our_public = public_key_from_private(next_private);

        let dh_send_shared = compute_dh(next_private, new_remote_dh);
        let (root_key, send_chain_key) = kdf_rk(&self.root_key, &dh_send_shared);
        self.root_key = root_key;
        self.send_chain_key = Some(send_chain_key);
    }
}

impl Drop for DoubleRatchetSession {
    fn drop(&mut self) {
        self.dh_our_private.zeroize();
        self.root_key.zeroize();
        if let Some(chain_key) = self.send_chain_key.as_mut() {
            chain_key.zeroize();
        }
        if let Some(chain_key) = self.recv_chain_key.as_mut() {
            chain_key.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_ratchet_encrypts_and_decrypts_across_rounds() {
        let master_secret = [0x55u8; 32];
        let bob_initial_priv = [0x33u8; 32];
        let bob_initial_pub = public_key_from_private(bob_initial_priv);

        let mut alice = DoubleRatchetSession::initialize_initiator(master_secret, bob_initial_pub);
        let mut bob = DoubleRatchetSession::initialize_responder(master_secret, bob_initial_priv);

        let msg1 = alice.encrypt(b"Hello Bob! Secret DM");
        assert_eq!(
            bob.decrypt(&msg1).expect("Bob decrypts the first message"),
            b"Hello Bob! Secret DM"
        );

        let msg2 = bob.encrypt(b"Hi Alice! Forward secrecy active");
        assert_eq!(
            alice
                .decrypt(&msg2)
                .expect("Alice decrypts the ratcheted reply"),
            b"Hi Alice! Forward secrecy active"
        );

        let msg3 = alice.encrypt(b"Third message with ratcheted keys");
        assert_eq!(
            bob.decrypt(&msg3).expect("Bob decrypts the third message"),
            b"Third message with ratcheted keys"
        );
    }

    #[test]
    fn double_ratchet_rejects_tampering_and_duplicate_delivery() {
        let master_secret = [0x77u8; 32];
        let bob_private = [0x11u8; 32];
        let mut alice = DoubleRatchetSession::initialize_initiator(
            master_secret,
            public_key_from_private(bob_private),
        );
        let mut bob = DoubleRatchetSession::initialize_responder(master_secret, bob_private);
        let mut message = alice.encrypt(b"authenticated");
        let original = message.clone();
        message.ciphertext[0] ^= 1;
        assert_eq!(bob.decrypt(&message), Err(RatchetError::DecryptionFailed));
        assert_eq!(
            bob.decrypt(&original)
                .expect("original ciphertext remains valid"),
            b"authenticated"
        );

        let message = alice.encrypt(b"once");
        assert_eq!(
            bob.decrypt(&message).expect("first delivery succeeds"),
            b"once"
        );
        assert_eq!(
            bob.decrypt(&message),
            Err(RatchetError::DuplicateOrOutOfOrder)
        );
    }
}
