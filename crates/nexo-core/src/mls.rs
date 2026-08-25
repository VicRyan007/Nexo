//! MLS-inspired group membership and epoch-key state for Nexo communities.
//!
//! This is Nexo's authenticated control-plane profile, not an RFC 9420 wire-compatible
//! implementation. It keeps the application boundary explicit so a full MLS provider can
//! replace the state machine when interoperable key packages are introduced.

#![allow(clippy::doc_markdown, clippy::cast_possible_truncation)]

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use x25519_dalek::{PublicKey, StaticSecret};

#[derive(Debug, Error)]
pub enum MlsError {
    #[error("member leaf index {0} is out of bounds")]
    InvalidLeafIndex(u32),
    #[error("group epoch mismatch: expected {expected}, got {actual}")]
    EpochMismatch { expected: u64, actual: u64 },
    #[error("unsupported MLS commit version")]
    UnsupportedVersion,
    #[error("MLS commit signature is invalid")]
    InvalidSignature,
    #[error("MLS commit could not be serialized")]
    Serialization,
    #[error("MLS commit has an unauthorized committer")]
    UnauthorizedCommitter,
    #[error("MLS commit has no epoch secret envelope for this member")]
    MissingEpochSecret,
    #[error("MLS recipient public key is invalid")]
    InvalidRecipientKey,
    #[error("MLS epoch secret envelope could not be opened")]
    InvalidEpochSecret,
    #[error("MLS member is already present")]
    DuplicateMember,
    #[error("MLS commit references a member that is not present")]
    MissingMember,
    #[error("MLS commit belongs to another group")]
    WrongGroup,
    #[error("MLS commit operation is not valid for this transition")]
    UnsupportedOperation,
}

/// A member enrolled in an MLS group.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MlsMember {
    pub leaf_index: u32,
    pub public_key: [u8; 32],
    pub device_id: String,
}

/// The local cryptographic state for an MLS group.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MlsGroupState {
    pub group_id: Uuid,
    pub epoch: u64,
    pub members: Vec<MlsMember>,
    pub tree_nodes: Vec<Option<[u8; 32]>>,
    pub epoch_secret: [u8; 32],
    #[serde(default)]
    pub epoch_secrets: Vec<[u8; 32]>,
}

/// Encrypted delivery of a newly generated epoch secret to one remaining
/// member. The recipient key is the Ed25519 identity key used by the group;
/// its X25519 Montgomery form is used only for this sealed control payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MlsSecretEnvelope {
    pub recipient_key: [u8; 32],
    pub ephemeral_public_key: [u8; 32],
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

const MLS_COMMIT_VERSION: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MlsCommitOperation {
    Add {
        device_id: String,
        public_key: [u8; 32],
    },
    Remove {
        leaf_index: u32,
    },
}

/// An authenticated membership transition. It changes the membership epoch
/// and advances the local epoch secret used by community message envelopes.
/// Per-member key-package distribution is still a separate hardening layer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MlsCommit {
    pub id: Uuid,
    pub version: u8,
    pub group_id: Uuid,
    pub epoch: u64,
    pub previous_state_hash: [u8; 32],
    pub committer_key: [u8; 32],
    pub operation: MlsCommitOperation,
    #[serde(default)]
    pub epoch_secret_envelopes: Vec<MlsSecretEnvelope>,
    pub signature: Vec<u8>,
}

#[derive(Serialize)]
struct MlsCommitSignedFields<'a> {
    id: Uuid,
    version: u8,
    group_id: Uuid,
    epoch: u64,
    previous_state_hash: [u8; 32],
    committer_key: [u8; 32],
    operation: &'a MlsCommitOperation,
    epoch_secret_envelopes: &'a [MlsSecretEnvelope],
}

impl MlsGroupState {
    /// Create a new MLS group with the founding creator.
    #[must_use]
    pub fn new(group_id: Uuid, creator_device_id: String, creator_public_key: [u8; 32]) -> Self {
        let mut initial_secret = [0u8; 32];
        let mut hasher = Sha256::new();
        hasher.update(b"nexo-legacy-mls-group-secret-v1");
        hasher.update(group_id.as_bytes());
        hasher.update(creator_public_key);
        initial_secret.copy_from_slice(&hasher.finalize());
        Self::new_with_secret(
            group_id,
            creator_device_id,
            creator_public_key,
            initial_secret,
        )
    }

    /// Create a group from the random secret carried by a signed invitation.
    #[must_use]
    pub fn new_with_secret(
        group_id: Uuid,
        creator_device_id: String,
        creator_public_key: [u8; 32],
        initial_secret: [u8; 32],
    ) -> Self {
        let member = MlsMember {
            leaf_index: 0,
            public_key: creator_public_key,
            device_id: creator_device_id,
        };

        let mut hasher = Sha256::new();
        hasher.update(b"nexo-mls-group-root-v2");
        hasher.update(group_id.as_bytes());
        hasher.update(creator_public_key);
        hasher.update(initial_secret);
        let initial_secret: [u8; 32] = hasher.finalize().into();
        let mut state = Self {
            group_id,
            epoch: 0,
            members: vec![member],
            tree_nodes: vec![Some(creator_public_key)],
            epoch_secret: initial_secret,
            epoch_secrets: vec![initial_secret],
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
        let history_is_complete = usize::try_from(self.epoch)
            .is_ok_and(|epoch| self.epoch_secrets.len() == epoch.saturating_add(1));
        let mut hasher = Sha256::new();
        hasher.update(self.epoch_secret);
        hasher.update(self.epoch.to_be_bytes());
        hasher.update(context_bytes);
        self.epoch_secret = hasher.finalize().into();
        if history_is_complete {
            self.epoch_secrets.push(self.epoch_secret);
        }
    }

    /// Derive an application encryption secret from the current epoch secret.
    #[must_use]
    pub fn derive_application_secret(&self, label: &str) -> [u8; 32] {
        self.derive_application_secret_for_epoch(self.epoch, label)
            .unwrap_or_else(|| Self::derive_application_secret_from(self.epoch_secret, label))
    }

    /// Derive an application secret for a retained historical epoch.
    #[must_use]
    pub fn derive_application_secret_for_epoch(&self, epoch: u64, label: &str) -> Option<[u8; 32]> {
        let secret = self
            .epoch_secrets
            .get(usize::try_from(epoch).ok()?)
            .copied()
            .or_else(|| (epoch == self.epoch).then_some(self.epoch_secret))?;
        Some(Self::derive_application_secret_from(secret, label))
    }

    fn derive_application_secret_from(secret: [u8; 32], label: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(secret);
        hasher.update(label.as_bytes());
        hasher.finalize().into()
    }

    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics only if serialization of this fixed, internally serializable
    /// state fails, which indicates a programming error rather than invalid
    /// user input.
    pub fn state_hash(&self) -> [u8; 32] {
        let bytes = serde_json::to_vec(self).expect("MLS state is serializable");
        Sha256::digest(bytes).into()
    }

    #[must_use]
    pub fn contains_member(&self, public_key: &[u8; 32]) -> bool {
        self.members
            .iter()
            .any(|member| &member.public_key == public_key)
    }

    pub fn apply_commit(&mut self, commit: &MlsCommit) -> Result<(), MlsError> {
        self.apply_commit_internal(commit, false, None)
    }

    /// Apply a commit while opening the private epoch-secret envelope for the
    /// local member. Removal commits without an envelope for the local key
    /// still advance the tree, but leave the removed local state unable to
    /// derive the new group secret.
    pub fn apply_commit_for_identity(
        &mut self,
        commit: &MlsCommit,
        identity: &crate::DeviceIdentity,
    ) -> Result<(), MlsError> {
        self.apply_commit_internal(commit, false, Some(identity))
    }

    /// Apply a self-signed join request after the application has verified the
    /// corresponding membership credential.
    pub fn apply_join_commit(&mut self, commit: &MlsCommit) -> Result<(), MlsError> {
        self.apply_commit_internal(commit, true, None)
    }

    /// Apply a verified add proposal while rebuilding a deterministic
    /// additive membership history. The application must authorize the target
    /// credential before calling this method.
    pub fn apply_add_proposal(&mut self, commit: &MlsCommit) -> Result<(), MlsError> {
        commit.verify_signature()?;
        if commit.group_id != self.group_id {
            return Err(MlsError::WrongGroup);
        }
        let MlsCommitOperation::Add {
            device_id,
            public_key,
        } = &commit.operation
        else {
            return Err(MlsError::UnsupportedOperation);
        };
        let authorized_committer = self.contains_member(&commit.committer_key);
        let self_join = commit.committer_key == *public_key;
        if !authorized_committer && !self_join {
            return Err(MlsError::UnauthorizedCommitter);
        }
        if self.contains_member(public_key) {
            return Err(MlsError::DuplicateMember);
        }
        self.add_member(device_id.clone(), *public_key);
        Ok(())
    }

    fn apply_commit_internal(
        &mut self,
        commit: &MlsCommit,
        allow_self_join: bool,
        identity: Option<&crate::DeviceIdentity>,
    ) -> Result<(), MlsError> {
        commit.verify_signature()?;
        if commit.group_id != self.group_id {
            return Err(MlsError::WrongGroup);
        }
        if commit.epoch != self.epoch.saturating_add(1)
            || commit.previous_state_hash != self.state_hash()
        {
            return Err(MlsError::EpochMismatch {
                expected: self.epoch.saturating_add(1),
                actual: commit.epoch,
            });
        }
        match &commit.operation {
            MlsCommitOperation::Add {
                device_id,
                public_key,
            } => {
                let authorized_committer = self.contains_member(&commit.committer_key);
                let self_join = allow_self_join && commit.committer_key == *public_key;
                if !authorized_committer && !self_join {
                    return Err(MlsError::UnauthorizedCommitter);
                }
                if self.contains_member(public_key) {
                    return Err(MlsError::DuplicateMember);
                }
                self.add_member(device_id.clone(), *public_key);
            }
            MlsCommitOperation::Remove { leaf_index } => {
                if !self.contains_member(&commit.committer_key) {
                    return Err(MlsError::UnauthorizedCommitter);
                }
                self.remove_member(*leaf_index)?;
            }
        }
        if let Some(identity) = identity
            && matches!(commit.operation, MlsCommitOperation::Remove { .. })
            && !commit.epoch_secret_envelopes.is_empty()
        {
            if self.contains_member(&identity.public_key_bytes()) {
                let secret = commit.open_epoch_secret(identity)?;
                self.replace_current_epoch_secret(secret);
            } else {
                // Keep the removed local state structurally useful for the
                // signed revocation record, but do not retain a derivable
                // current group secret.
                self.replace_current_epoch_secret([0; 32]);
            }
        }
        Ok(())
    }

    fn replace_current_epoch_secret(&mut self, secret: [u8; 32]) {
        self.epoch_secret = secret;
        if let Some(current) = self.epoch_secrets.last_mut() {
            *current = secret;
        } else {
            self.epoch_secrets.push(secret);
        }
    }
}

impl MlsCommit {
    pub fn create_add(
        identity: &crate::DeviceIdentity,
        state: &MlsGroupState,
        device_id: String,
        public_key: [u8; 32],
    ) -> Result<Self, MlsError> {
        let mut commit = Self {
            id: Uuid::new_v4(),
            version: MLS_COMMIT_VERSION,
            group_id: state.group_id,
            epoch: state.epoch.saturating_add(1),
            previous_state_hash: state.state_hash(),
            committer_key: identity.public_key_bytes(),
            operation: MlsCommitOperation::Add {
                device_id,
                public_key,
            },
            epoch_secret_envelopes: Vec::new(),
            signature: Vec::new(),
        };
        commit.signature = identity.sign(&commit.signing_bytes()?).to_vec();
        Ok(commit)
    }

    pub fn create_remove(
        identity: &crate::DeviceIdentity,
        state: &MlsGroupState,
        leaf_index: u32,
    ) -> Result<Self, MlsError> {
        if !state.contains_member(&identity.public_key_bytes()) {
            return Err(MlsError::UnauthorizedCommitter);
        }
        if !state
            .members
            .iter()
            .any(|member| member.leaf_index == leaf_index)
        {
            return Err(MlsError::MissingMember);
        }
        let mut new_epoch_secret = [0u8; 32];
        OsRng.fill_bytes(&mut new_epoch_secret);
        let mut recipients = state
            .members
            .iter()
            .filter(|member| member.leaf_index != leaf_index)
            .map(|member| member.public_key)
            .collect::<Vec<_>>();
        recipients.sort_unstable();
        let mut commit = Self {
            id: Uuid::new_v4(),
            version: MLS_COMMIT_VERSION,
            group_id: state.group_id,
            epoch: state.epoch.saturating_add(1),
            previous_state_hash: state.state_hash(),
            committer_key: identity.public_key_bytes(),
            operation: MlsCommitOperation::Remove { leaf_index },
            epoch_secret_envelopes: Vec::new(),
            signature: Vec::new(),
        };
        commit.epoch_secret_envelopes = recipients
            .into_iter()
            .map(|recipient_key| {
                MlsSecretEnvelope::seal(
                    recipient_key,
                    state.group_id,
                    commit.epoch,
                    new_epoch_secret,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        commit.signature = identity.sign(&commit.signing_bytes()?).to_vec();
        Ok(commit)
    }

    pub fn verify_signature(&self) -> Result<(), MlsError> {
        if self.version != MLS_COMMIT_VERSION {
            return Err(MlsError::UnsupportedVersion);
        }
        let signature: [u8; 64] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| MlsError::InvalidSignature)?;
        crate::DeviceIdentity::verify(&self.committer_key, &self.signing_bytes()?, &signature)
            .map_err(|_| MlsError::InvalidSignature)
    }

    fn signing_bytes(&self) -> Result<Vec<u8>, MlsError> {
        serde_json::to_vec(&MlsCommitSignedFields {
            id: self.id,
            version: self.version,
            group_id: self.group_id,
            epoch: self.epoch,
            previous_state_hash: self.previous_state_hash,
            committer_key: self.committer_key,
            operation: &self.operation,
            epoch_secret_envelopes: &self.epoch_secret_envelopes,
        })
        .map_err(|_| MlsError::Serialization)
    }

    fn open_epoch_secret(&self, identity: &crate::DeviceIdentity) -> Result<[u8; 32], MlsError> {
        let recipient_key = identity.public_key_bytes();
        let envelope = self
            .epoch_secret_envelopes
            .iter()
            .find(|envelope| envelope.recipient_key == recipient_key)
            .ok_or(MlsError::MissingEpochSecret)?;
        envelope.open(identity, self.group_id, self.epoch)
    }
}

impl MlsSecretEnvelope {
    fn seal(
        recipient_key: [u8; 32],
        group_id: Uuid,
        epoch: u64,
        epoch_secret: [u8; 32],
    ) -> Result<Self, MlsError> {
        let recipient = VerifyingKey::from_bytes(&recipient_key)
            .map_err(|_| MlsError::InvalidRecipientKey)?
            .to_montgomery();
        let recipient = PublicKey::from(recipient.to_bytes());
        let mut ephemeral_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut ephemeral_bytes);
        let ephemeral_secret = StaticSecret::from(ephemeral_bytes);
        let ephemeral_public_key = PublicKey::from(&ephemeral_secret).to_bytes();
        let shared = ephemeral_secret.diffie_hellman(&recipient).to_bytes();
        let key = envelope_key(shared, group_id, epoch, recipient_key);
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &epoch_secret,
                    aad: &envelope_aad(group_id, epoch, recipient_key),
                },
            )
            .map_err(|_| MlsError::InvalidEpochSecret)?;
        Ok(Self {
            recipient_key,
            ephemeral_public_key,
            nonce,
            ciphertext,
        })
    }

    fn open(
        &self,
        identity: &crate::DeviceIdentity,
        group_id: Uuid,
        epoch: u64,
    ) -> Result<[u8; 32], MlsError> {
        let secret_key = identity.secret_key_bytes();
        let signing_key = SigningKey::from_bytes(&secret_key);
        let private = StaticSecret::from(signing_key.to_scalar_bytes());
        let ephemeral_public = PublicKey::from(self.ephemeral_public_key);
        let shared = private.diffie_hellman(&ephemeral_public).to_bytes();
        let key = envelope_key(shared, group_id, epoch, self.recipient_key);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&self.nonce),
                Payload {
                    msg: &self.ciphertext,
                    aad: &envelope_aad(group_id, epoch, self.recipient_key),
                },
            )
            .map_err(|_| MlsError::InvalidEpochSecret)?;
        plaintext
            .try_into()
            .map_err(|_| MlsError::InvalidEpochSecret)
    }
}

fn envelope_key(shared: [u8; 32], group_id: Uuid, epoch: u64, recipient_key: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"nexo-mls-epoch-secret-envelope-v1");
    hasher.update(shared);
    hasher.update(group_id.as_bytes());
    hasher.update(epoch.to_be_bytes());
    hasher.update(recipient_key);
    hasher.finalize().into()
}

fn envelope_aad(group_id: Uuid, epoch: u64, recipient_key: [u8; 32]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(16 + 8 + 32);
    aad.extend_from_slice(group_id.as_bytes());
    aad.extend_from_slice(&epoch.to_be_bytes());
    aad.extend_from_slice(&recipient_key);
    aad
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceIdentity;

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

    #[test]
    fn signed_join_commit_advances_the_same_membership_epoch() {
        let group_id = Uuid::new_v4();
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let mut alice_state =
            MlsGroupState::new(group_id, "alice".to_owned(), alice.public_key_bytes());
        let commit =
            MlsCommit::create_add(&bob, &alice_state, "bob".to_owned(), bob.public_key_bytes())
                .expect("join commit should be signed");
        alice_state
            .apply_join_commit(&commit)
            .expect("the new member may authenticate its own join");

        assert_eq!(alice_state.epoch, 1);
        assert!(alice_state.contains_member(&bob.public_key_bytes()));
        assert!(commit.verify_signature().is_ok());
        let mut unauthorized_state =
            MlsGroupState::new(group_id, "alice".to_owned(), alice.public_key_bytes());
        assert!(unauthorized_state.apply_commit(&commit).is_err());

        let mut tampered = commit;
        tampered.epoch = 2;
        assert!(tampered.verify_signature().is_err());
    }

    #[test]
    fn signed_remove_commit_revokes_a_member_and_requires_authorization() {
        let group_id = Uuid::new_v4();
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let mut state = MlsGroupState::new(group_id, "alice".to_owned(), alice.public_key_bytes());
        state.add_member("bob".to_owned(), bob.public_key_bytes());
        let commit = MlsCommit::create_remove(&alice, &state, 1).expect("remove should sign");
        state.apply_commit(&commit).expect("remove should apply");
        assert_eq!(state.epoch, 2);
        assert!(!state.contains_member(&bob.public_key_bytes()));
        assert!(matches!(
            MlsCommit::create_remove(&bob, &state, 0),
            Err(MlsError::UnauthorizedCommitter)
        ));
    }

    #[test]
    fn remove_commit_delivers_a_secret_only_to_remaining_members() {
        let group_id = Uuid::new_v4();
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let charlie = DeviceIdentity::generate();
        let mut base = MlsGroupState::new(group_id, "alice".to_owned(), alice.public_key_bytes());
        base.add_member("bob".to_owned(), bob.public_key_bytes());
        base.add_member("charlie".to_owned(), charlie.public_key_bytes());
        let commit = MlsCommit::create_remove(&alice, &base, 1).expect("remove should sign");

        assert_eq!(commit.epoch_secret_envelopes.len(), 2);
        assert!(commit.open_epoch_secret(&bob).is_err());

        let mut alice_state = base.clone();
        let mut charlie_state = base.clone();
        let mut bob_state = base;
        alice_state
            .apply_commit_for_identity(&commit, &alice)
            .expect("founder should open the new epoch secret");
        charlie_state
            .apply_commit_for_identity(&commit, &charlie)
            .expect("remaining member should open the new epoch secret");
        bob_state
            .apply_commit_for_identity(&commit, &bob)
            .expect("removed member should still process the revocation");

        assert_eq!(
            alice_state.derive_application_secret("nexo-media"),
            charlie_state.derive_application_secret("nexo-media")
        );
        assert!(!bob_state.contains_member(&bob.public_key_bytes()));
        assert_ne!(
            bob_state.derive_application_secret("nexo-media"),
            alice_state.derive_application_secret("nexo-media")
        );
    }

    #[test]
    fn concurrent_add_proposals_converge_when_applied_in_same_order() {
        let group_id = Uuid::new_v4();
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let charlie = DeviceIdentity::generate();
        let base = MlsGroupState::new(group_id, "alice".to_owned(), alice.public_key_bytes());
        let bob_commit =
            MlsCommit::create_add(&alice, &base, "bob".to_owned(), bob.public_key_bytes())
                .expect("bob commit should be signed");
        let charlie_commit = MlsCommit::create_add(
            &alice,
            &base,
            "charlie".to_owned(),
            charlie.public_key_bytes(),
        )
        .expect("charlie commit should be signed");

        let mut left = base.clone();
        let mut right = base;
        let mut commits = vec![bob_commit, charlie_commit];
        commits.sort_by_key(|commit| commit.id);
        for commit in &commits {
            left.apply_add_proposal(commit)
                .expect("left state should accept the proposal");
        }
        for commit in &commits {
            right
                .apply_add_proposal(commit)
                .expect("right state should accept the proposal");
        }

        assert_eq!(left, right);
        assert_eq!(left.members.len(), 3);
        assert!(left.contains_member(&bob.public_key_bytes()));
        assert!(left.contains_member(&charlie.public_key_bytes()));
    }
}
