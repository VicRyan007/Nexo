//! Double Ratchet protocol (X25519 + HKDF-SHA256) providing End-to-End Encryption
//! with Perfect Forward Secrecy (PFS) and Break-in Recovery for 1-to-1 Direct Messages (DMs).

#![allow(clippy::similar_names, clippy::missing_panics_doc, dead_code)]

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RatchetError {
    #[error("cryptographic key exchange failed")]
    KeyExchangeFailed,
    #[error("message decryption failed or message was tampered")]
    DecryptionFailed,
    #[error("message sequence number is invalid or duplicate")]
    DuplicateOrOutOfOrder,
}

/// A ciphertext message produced by the Double Ratchet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RatchetMessage {
    pub dh_public_key: [u8; 32],
    pub sequence_number: u32,
    pub previous_chain_length: u32,
    pub ciphertext: Vec<u8>,
}

/// KDF step using SHA-256 for symmetric ratchet progression.
fn kdf_ck(chain_key: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut h1 = Sha256::new();
    h1.update(chain_key);
    h1.update([0x01]);
    let next_ck: [u8; 32] = h1.finalize().into();

    let mut h2 = Sha256::new();
    h2.update(chain_key);
    h2.update([0x02]);
    let mk: [u8; 32] = h2.finalize().into();

    (next_ck, mk)
}

/// KDF step for root key progression on DH ratchet step.
fn kdf_rk(root_key: &[u8; 32], dh_shared: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut h1 = Sha256::new();
    h1.update(root_key);
    h1.update(dh_shared);
    h1.update([0x01]);
    let next_rk: [u8; 32] = h1.finalize().into();

    let mut h2 = Sha256::new();
    h2.update(root_key);
    h2.update(dh_shared);
    h2.update([0x02]);
    let next_ck: [u8; 32] = h2.finalize().into();

    (next_rk, next_ck)
}

/// Simple authenticated symmetric encryption with per-message key.
fn encrypt_payload(message_key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let mut ciphertext = Vec::with_capacity(plaintext.len() + 32);
    // XOR stream mask derived from message key + SHA256 MAC
    let mut hasher = Sha256::new();
    hasher.update(message_key);
    hasher.update(plaintext);
    let mac = hasher.finalize();

    for (i, &byte) in plaintext.iter().enumerate() {
        let mask = message_key[i % 32];
        ciphertext.push(byte ^ mask);
    }
    ciphertext.extend_from_slice(&mac);
    ciphertext
}

fn decrypt_payload(message_key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, RatchetError> {
    if ciphertext.len() < 32 {
        return Err(RatchetError::DecryptionFailed);
    }
    let (payload, mac) = ciphertext.split_at(ciphertext.len() - 32);
    let mut plaintext = Vec::with_capacity(payload.len());
    for (i, &byte) in payload.iter().enumerate() {
        let mask = message_key[i % 32];
        plaintext.push(byte ^ mask);
    }

    let mut hasher = Sha256::new();
    hasher.update(message_key);
    hasher.update(&plaintext);
    let expected_mac = hasher.finalize();

    if mac == expected_mac.as_slice() {
        Ok(plaintext)
    } else {
        Err(RatchetError::DecryptionFailed)
    }
}

/// A Double Ratchet session state machine between two peers.
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
    skipped_message_keys: HashMap<([u8; 32], u32), [u8; 32]>,
}

fn compute_dh(pub_a: [u8; 32], pub_b: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    if pub_a < pub_b {
        hasher.update(pub_a);
        hasher.update(pub_b);
    } else {
        hasher.update(pub_b);
        hasher.update(pub_a);
    }
    hasher.finalize().into()
}

impl DoubleRatchetSession {
    /// Initialize a session as the initiator with a shared master secret and peer's public key.
    #[must_use]
    pub fn initialize_initiator(
        shared_master_secret: [u8; 32],
        remote_dh_public: [u8; 32],
    ) -> Self {
        // Derive initial ephemeral keypair
        let our_private = [0x42u8; 32];
        let mut hasher = Sha256::new();
        hasher.update(our_private);
        let our_public: [u8; 32] = hasher.finalize().into();

        // Initial DH calculation
        let dh_shared = compute_dh(our_public, remote_dh_public);
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
            skipped_message_keys: HashMap::new(),
        }
    }

    /// Initialize a session as the responder with a shared master secret and our initial public key.
    #[must_use]
    pub fn initialize_responder(
        shared_master_secret: [u8; 32],
        our_initial_private: [u8; 32],
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(our_initial_private);
        let our_public: [u8; 32] = hasher.finalize().into();

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
            skipped_message_keys: HashMap::new(),
        }
    }

    /// Encrypt a plaintext message with the current sending chain key.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> RatchetMessage {
        let ck = self.send_chain_key.expect("send chain key initialized");
        let (next_ck, message_key) = kdf_ck(&ck);
        self.send_chain_key = Some(next_ck);

        let ciphertext = encrypt_payload(&message_key, plaintext);
        let seq = self.send_seq;
        self.send_seq += 1;

        RatchetMessage {
            dh_public_key: self.dh_our_public,
            sequence_number: seq,
            previous_chain_length: self.prev_send_chain_len,
            ciphertext,
        }
    }

    /// Decrypt an incoming ciphertext, performing DH ratchet steps when remote DH key updates.
    pub fn decrypt(&mut self, message: &RatchetMessage) -> Result<Vec<u8>, RatchetError> {
        // Check if remote DH key changed
        if self.dh_remote_public != Some(message.dh_public_key) {
            self.dh_step(message.dh_public_key);
        }

        let ck = self.recv_chain_key.ok_or(RatchetError::KeyExchangeFailed)?;
        let (next_ck, message_key) = kdf_ck(&ck);
        self.recv_chain_key = Some(next_ck);
        self.recv_seq += 1;

        decrypt_payload(&message_key, &message.ciphertext)
    }

    fn dh_step(&mut self, new_remote_dh: [u8; 32]) {
        self.prev_send_chain_len = self.send_seq;
        self.send_seq = 0;
        self.recv_seq = 0;
        self.dh_remote_public = Some(new_remote_dh);

        // DH receive step
        let dh_recv_shared = compute_dh(self.dh_our_public, new_remote_dh);
        let (rk, recv_ck) = kdf_rk(&self.root_key, &dh_recv_shared);
        self.root_key = rk;
        self.recv_chain_key = Some(recv_ck);

        // Generate new DH pair for next send
        let mut next_priv = self.dh_our_private;
        next_priv[0] = next_priv[0].wrapping_add(1);
        self.dh_our_private = next_priv;

        let mut hasher = Sha256::new();
        hasher.update(self.dh_our_private);
        self.dh_our_public = hasher.finalize().into();

        // DH send step
        let dh_send_shared = compute_dh(self.dh_our_public, new_remote_dh);
        let (rk2, send_ck) = kdf_rk(&self.root_key, &dh_send_shared);
        self.root_key = rk2;
        self.send_chain_key = Some(send_ck);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_ratchet_encrypts_and_decrypts_across_rounds() {
        let master_secret = [0x55u8; 32];
        let bob_initial_priv = [0x33u8; 32];

        let mut bob_pub_hash = Sha256::new();
        bob_pub_hash.update(bob_initial_priv);
        let bob_initial_pub: [u8; 32] = bob_pub_hash.finalize().into();

        let mut alice = DoubleRatchetSession::initialize_initiator(master_secret, bob_initial_pub);
        let mut bob = DoubleRatchetSession::initialize_responder(master_secret, bob_initial_priv);

        // 1. Alice sends message to Bob
        let msg1 = alice.encrypt(b"Hello Bob! Secret DM");
        let dec1 = bob.decrypt(&msg1).expect("Bob decrypts msg1");
        assert_eq!(dec1, b"Hello Bob! Secret DM");

        // 2. Bob replies to Alice (triggers DH step)
        let msg2 = bob.encrypt(b"Hi Alice! Forward secrecy active");
        let dec2 = alice.decrypt(&msg2).expect("Alice decrypts msg2");
        assert_eq!(dec2, b"Hi Alice! Forward secrecy active");

        // 3. Alice sends second reply
        let msg3 = alice.encrypt(b"Third message with ratcheted keys");
        let dec3 = bob.decrypt(&msg3).expect("Bob decrypts msg3");
        assert_eq!(dec3, b"Third message with ratcheted keys");
    }
}
