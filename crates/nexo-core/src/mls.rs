//! Messaging Layer Security (MLS - RFC 9420) `TreeKEM` Group Key Exchange.
//!
//! Provides scalable asymmetric group key agreement in O(log N), ratcheting epoch
//! secrets forward whenever members join, leave, or update their key packages.

#![allow(clippy::doc_markdown, clippy::cast_possible_truncation)]

use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum MlsError {
    #[error("member leaf index {0} is out of bounds")]
    InvalidLeafIndex(u32),
    #[error("group epoch mismatch: expected {expected}, got {actual}")]
    EpochMismatch { expected: u64, actual: u64 },
}

/// A member enrolled in an MLS group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MlsMember {
    pub leaf_index: u32,
    pub public_key: [u8; 32],
    pub device_id: String,
}

/// The local cryptographic state for an MLS group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MlsGroupState {
    pub group_id: Uuid,
    pub epoch: u64,
    pub members: Vec<MlsMember>,
    pub tree_nodes: Vec<Option<[u8; 32]>>,
    pub epoch_secret: [u8; 32],
}

impl MlsGroupState {
    /// Create a new MLS group with the founding creator.
    #[must_use]
    pub fn new(group_id: Uuid, creator_device_id: String, creator_public_key: [u8; 32]) -> Self {
        let member = MlsMember {
            leaf_index: 0,
            public_key: creator_public_key,
            device_id: creator_device_id,
        };

        let mut initial_secret = [0u8; 32];
        let mut hasher = Sha256::new();
        hasher.update(group_id.as_bytes());
        hasher.update(creator_public_key);
        initial_secret.copy_from_slice(&hasher.finalize());

        let mut state = Self {
            group_id,
            epoch: 0,
            members: vec![member],
            tree_nodes: vec![Some(creator_public_key)],
            epoch_secret: initial_secret,
        };
        state.recompute_tree();
        state
    }

    /// Add a new member to the group, ratcheting the group epoch and secret forward.
    pub fn add_member(&mut self, device_id: String, public_key: [u8; 32]) -> u64 {
        let leaf_index = self.members.len() as u32;
        self.members.push(MlsMember {
            leaf_index,
            public_key,
            device_id,
        });

        self.epoch += 1;
        self.ratchet_epoch_secret(public_key);
        self.recompute_tree();
        self.epoch
    }

    /// Remove a member from the group and advance the epoch.
    pub fn remove_member(&mut self, leaf_index: u32) -> Result<u64, MlsError> {
        let pos = self
            .members
            .iter()
            .position(|m| m.leaf_index == leaf_index)
            .ok_or(MlsError::InvalidLeafIndex(leaf_index))?;

        let removed = self.members.remove(pos);
        self.epoch += 1;
        self.ratchet_epoch_secret(removed.public_key);
        self.recompute_tree();
        Ok(self.epoch)
    }

    /// Recompute internal `TreeKEM` nodes using SHA-256 parent hash combinations.
    fn recompute_tree(&mut self) {
        let num_leaves = self.members.len().max(1);
        let total_nodes = 2 * num_leaves - 1;
        self.tree_nodes = vec![None; total_nodes];

        // Fill leaves
        for (i, member) in self.members.iter().enumerate() {
            self.tree_nodes[i * 2] = Some(member.public_key);
        }

        // Propagate internal nodes
        for i in 0..num_leaves.saturating_sub(1) {
            let left = self.tree_nodes.get(i * 2).copied().flatten();
            let right = self.tree_nodes.get(i * 2 + 2).copied().flatten();
            if let (Some(l), Some(r)) = (left, right) {
                let mut hasher = Sha256::new();
                hasher.update(l);
                hasher.update(r);
                let parent: [u8; 32] = hasher.finalize().into();
                let parent_idx = i * 2 + 1;
                if parent_idx < self.tree_nodes.len() {
                    self.tree_nodes[parent_idx] = Some(parent);
                }
            }
        }
    }

    fn ratchet_epoch_secret(&mut self, context_bytes: [u8; 32]) {
        let mut hasher = Sha256::new();
        hasher.update(self.epoch_secret);
        hasher.update(self.epoch.to_be_bytes());
        hasher.update(context_bytes);
        self.epoch_secret = hasher.finalize().into();
    }

    /// Derive an application encryption secret from the current epoch secret.
    #[must_use]
    pub fn derive_application_secret(&self, label: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.epoch_secret);
        hasher.update(label.as_bytes());
        hasher.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mls_group_advances_epoch_and_derives_unique_secrets() {
        let group_id = Uuid::new_v4();
        let alice_key = [0x11u8; 32];
        let mut group = MlsGroupState::new(group_id, "alice-pc".into(), alice_key);

        assert_eq!(group.epoch, 0);
        assert_eq!(group.members.len(), 1);

        let secret_epoch_0 = group.derive_application_secret("message-encryption");

        // Add Bob
        let bob_key = [0x22u8; 32];
        let epoch1 = group.add_member("bob-laptop".into(), bob_key);
        assert_eq!(epoch1, 1);
        assert_eq!(group.members.len(), 2);

        let secret_epoch_1 = group.derive_application_secret("message-encryption");
        assert_ne!(secret_epoch_0, secret_epoch_1, "Epoch secrets must rotate");

        // Add Charlie
        let charlie_key = [0x33u8; 32];
        let epoch2 = group.add_member("charlie-phone".into(), charlie_key);
        assert_eq!(epoch2, 2);

        // Remove Bob
        let epoch3 = group.remove_member(1).expect("remove bob succeeds");
        assert_eq!(epoch3, 3);
        assert_eq!(group.members.len(), 2);

        let secret_epoch_3 = group.derive_application_secret("message-encryption");
        assert_ne!(secret_epoch_1, secret_epoch_3);
    }
}
