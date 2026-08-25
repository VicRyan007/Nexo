use std::path::Path;

use nexo_core::{
    CallSignal, CommunityCredential, DirectMessageEnvelope, FileTransferOffer, MessageError,
    MlsCommit, MlsGroupState, SignedMessage, community_sync_token, current_timestamp,
    direct_conversation_id,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use thiserror::Error;
use uuid::Uuid;

const DEFAULT_CHANNEL_NAME: &str = "geral";
const DEFAULT_CHANNEL_NAMESPACE: Uuid = Uuid::from_u128(0x3a6b_9561_66fd_4f9e_8bb4_1cf2_e033_ea97);
const CHANNEL_NAMESPACE: Uuid = Uuid::from_u128(0x5c5a_1f6a_0e46_4a0b_8f08_0d1e_7a55_2b4c);

fn member_device_id(public_key: &[u8; 32]) -> String {
    let mut value = String::with_capacity(16);
    for byte in &public_key[..8] {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Community {
    pub id: Uuid,
    pub name: String,
    pub default_channel_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ChannelKind {
    Text,
    Voice,
}

impl ChannelKind {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Voice => "voice",
        }
    }
}

impl std::str::FromStr for ChannelKind {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("voice") {
            Ok(Self::Voice)
        } else {
            Ok(Self::Text)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Channel {
    pub id: Uuid,
    pub community_id: Uuid,
    pub name: String,
    pub position: u32,
    pub kind: ChannelKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredFileTransfer {
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
    pub local_path: Option<String>,
    pub status: String,
    pub downloaded_chunks: u32,
    pub created_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredDirectMessage {
    pub envelope: DirectMessageEnvelope,
    pub body: String,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database operation failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("stored data is invalid: {0}")]
    InvalidData(String),
    #[error("message validation failed: {0}")]
    InvalidMessage(#[from] MessageError),
    #[error("the message channel does not belong to its community")]
    ChannelMismatch,
    #[error("the message author is not authorized in this community")]
    UnauthorizedAuthor,
}

pub struct LocalStore {
    connection: Connection,
}

impl LocalStore {
    #[allow(clippy::too_many_lines)]
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        }
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS communities (
                 id TEXT PRIMARY KEY NOT NULL,
                 name TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS channels (
                 id TEXT PRIMARY KEY NOT NULL,
                 community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 UNIQUE(community_id, name)
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id TEXT PRIMARY KEY NOT NULL,
                 version INTEGER NOT NULL DEFAULT 1,
                 community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
                 author_key BLOB NOT NULL,
                 body TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 signature BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS members (
                 community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 public_key BLOB NOT NULL,
                 authorized_at INTEGER NOT NULL,
                 PRIMARY KEY(community_id, public_key)
             );
             CREATE TABLE IF NOT EXISTS revoked_members (
                 community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 public_key BLOB NOT NULL,
                 revoked_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY(community_id, public_key)
             );
             CREATE TABLE IF NOT EXISTS credentials (
                 community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 member_key BLOB NOT NULL,
                 credential_json TEXT NOT NULL,
                 PRIMARY KEY(community_id, member_key)
             );
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY NOT NULL,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sync_deliveries (
                 peer_id TEXT NOT NULL,
                 receiver_epoch TEXT NOT NULL,
                 community_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 PRIMARY KEY(peer_id, receiver_epoch, community_id, message_id)
             );
             CREATE TABLE IF NOT EXISTS sync_pending (
                 peer_id TEXT NOT NULL,
                 receiver_epoch TEXT NOT NULL,
                 community_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 PRIMARY KEY(peer_id, receiver_epoch, community_id, message_id)
             );
             CREATE TABLE IF NOT EXISTS direct_sync_deliveries (
                 peer_id TEXT NOT NULL,
                 receiver_epoch TEXT NOT NULL,
                 community_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 PRIMARY KEY(peer_id, receiver_epoch, community_id, message_id)
             );
             CREATE TABLE IF NOT EXISTS direct_sync_pending (
                 peer_id TEXT NOT NULL,
                 receiver_epoch TEXT NOT NULL,
                 community_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 PRIMARY KEY(peer_id, receiver_epoch, community_id, message_id)
             );
              CREATE TABLE IF NOT EXISTS call_signals_seen (
                  id TEXT PRIMARY KEY NOT NULL,
                  community_id TEXT NOT NULL,
                  call_id TEXT NOT NULL,
                  author_key BLOB NOT NULL,
                  sequence INTEGER NOT NULL,
                  received_at INTEGER NOT NULL
              );
              CREATE INDEX IF NOT EXISTS call_signals_seen_received_at
                  ON call_signals_seen(received_at);
             CREATE TABLE IF NOT EXISTS direct_messages (
                 id TEXT PRIMARY KEY NOT NULL,
                 community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 conversation_id TEXT NOT NULL,
                 sender_key BLOB NOT NULL,
                 recipient_key BLOB NOT NULL,
                 body TEXT NOT NULL,
                 envelope_json TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS direct_messages_timeline
                 ON direct_messages(conversation_id, created_at, id);
             CREATE TABLE IF NOT EXISTS mls_groups (
                 community_id TEXT PRIMARY KEY NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 state_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mls_commits (
                 id TEXT PRIMARY KEY NOT NULL,
                 community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 epoch INTEGER NOT NULL,
                 commit_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS mls_commits_epoch
                 ON mls_commits(community_id, epoch, id);
             CREATE TABLE IF NOT EXISTS file_transfers (
                 id TEXT PRIMARY KEY NOT NULL,
                 community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
                 channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
                 file_name TEXT NOT NULL,
                 file_size INTEGER NOT NULL,
                 mime_type TEXT NOT NULL,
                 chunk_size INTEGER NOT NULL,
                 total_chunks INTEGER NOT NULL,
                 root_sha256 BLOB NOT NULL,
                 author_key BLOB NOT NULL,
                 local_path TEXT,
                 status TEXT NOT NULL,
                 downloaded_chunks INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS file_chunks_saved (
                 transfer_id TEXT NOT NULL REFERENCES file_transfers(id) ON DELETE CASCADE,
                 chunk_index INTEGER NOT NULL,
                 PRIMARY KEY(transfer_id, chunk_index)
             );
             CREATE INDEX IF NOT EXISTS messages_timeline
                 ON messages(channel_id, created_at, id);",
        )?;
        if !has_column(&connection, "messages", "version")? {
            connection.execute(
                "ALTER TABLE messages ADD COLUMN version INTEGER NOT NULL DEFAULT 1",
                [],
            )?;
        }
        if !has_column(&connection, "channels", "kind")? {
            connection.execute(
                "ALTER TABLE channels ADD COLUMN kind TEXT NOT NULL DEFAULT 'text'",
                [],
            )?;
        }
        migrate_credentials(&mut connection)?;
        connection.execute(
            "INSERT OR IGNORE INTO metadata(key, value) VALUES ('database_epoch', ?1)",
            [Uuid::new_v4().to_string()],
        )?;
        Ok(Self { connection })
    }

    pub fn database_epoch(&self) -> Result<Uuid, StoreError> {
        let value = self.connection.query_row(
            "SELECT value FROM metadata WHERE key = 'database_epoch'",
            [],
            |row| row.get::<_, String>(0),
        )?;
        parse_uuid(&value)
    }

    /// Retrieve a persisted key-value setting from metadata table.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>, StoreError> {
        let mut stmt = self
            .connection
            .prepare("SELECT value FROM metadata WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Store or update a persisted key-value setting in metadata table.
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO metadata(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn create_community(
        &mut self,
        id: Uuid,
        name: &str,
        created_at: u64,
    ) -> Result<Community, StoreError> {
        let channel_id = default_channel_id(id);
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO communities(id, name, created_at) VALUES (?1, ?2, ?3)",
            params![id.to_string(), name, to_i64(created_at)?],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO channels(id, community_id, name, position)
             VALUES (?1, ?2, ?3, 0)",
            params![channel_id.to_string(), id.to_string(), DEFAULT_CHANNEL_NAME],
        )?;
        transaction.commit()?;
        self.community(id)?.ok_or_else(|| {
            StoreError::InvalidData("community was not available after insertion".to_owned())
        })
    }

    /// Create a new channel in a community.
    pub fn create_channel(
        &self,
        community_id: Uuid,
        name: &str,
        kind: ChannelKind,
    ) -> Result<Channel, StoreError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(StoreError::InvalidData(
                "channel name cannot be empty".to_owned(),
            ));
        }
        if let Some(existing) = self
            .channels(community_id)?
            .into_iter()
            .find(|channel| channel.name.eq_ignore_ascii_case(name))
        {
            if existing.kind != kind {
                self.connection.execute(
                    "UPDATE channels SET kind = ?1 WHERE id = ?2",
                    params![kind.as_str(), existing.id.to_string()],
                )?;
                return Ok(Channel { kind, ..existing });
            }
            return Ok(existing);
        }
        let channel_key = format!("{}:{}", community_id, name.to_ascii_lowercase());
        let channel_id = Uuid::new_v5(&CHANNEL_NAMESPACE, channel_key.as_bytes());
        let max_pos: i64 = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM channels WHERE community_id = ?1",
                [community_id.to_string()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let pos = u32::try_from(max_pos.max(0)).unwrap_or(0);
        self.connection.execute(
            "INSERT INTO channels(id, community_id, name, position, kind)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(community_id, name) DO UPDATE SET kind = excluded.kind",
            params![
                channel_id.to_string(),
                community_id.to_string(),
                name,
                pos,
                kind.as_str()
            ],
        )?;
        Ok(Channel {
            id: channel_id,
            community_id,
            name: name.to_owned(),
            position: pos,
            kind,
        })
    }

    /// List all channels for a given community.
    pub fn channels(&self, community_id: Uuid) -> Result<Vec<Channel>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, community_id, name, position, kind
             FROM channels WHERE community_id = ?1 ORDER BY position, name",
        )?;
        let rows = statement.query_map([community_id.to_string()], |row| {
            let id_str: String = row.get(0)?;
            let comm_str: String = row.get(1)?;
            let kind_str: String = row.get(4)?;
            Ok(Channel {
                id: parse_uuid(&id_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                community_id: parse_uuid(&comm_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                name: row.get(2)?,
                position: u32::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                kind: kind_str.parse().unwrap_or(ChannelKind::Text),
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Import a channel definition while preserving its stable identifier.
    /// Channel metadata is bounded by the sync protocol and messages still
    /// require a valid community signature before they are stored.
    pub fn import_channel(&self, channel: &Channel) -> Result<(), StoreError> {
        if self.community(channel.community_id)?.is_none() {
            return Err(StoreError::InvalidData(
                "unknown channel community".to_owned(),
            ));
        }
        self.connection.execute(
            "INSERT INTO channels(id, community_id, name, position, kind)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name, position = excluded.position, kind = excluded.kind",
            params![
                channel.id.to_string(),
                channel.community_id.to_string(),
                channel.name,
                i64::from(channel.position),
                channel.kind.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn authorize_member(
        &self,
        community_id: Uuid,
        public_key: &[u8; 32],
        authorized_at: u64,
    ) -> Result<(), StoreError> {
        if self.is_revoked_member(community_id, public_key)? {
            return Err(StoreError::InvalidData(
                "member is revoked in this community".to_owned(),
            ));
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO members(community_id, public_key, authorized_at)
             VALUES (?1, ?2, ?3)",
            params![
                community_id.to_string(),
                public_key.as_slice(),
                to_i64(authorized_at)?
            ],
        )?;
        Ok(())
    }

    /// Load or create the local MLS membership state. Existing databases are
    /// bootstrapped deterministically from their authorized member set so all
    /// peers converge before the first exchanged commit.
    pub fn ensure_mls_group(
        &self,
        community_id: Uuid,
        founder_device_id: String,
        founder_key: [u8; 32],
        member_keys: &[[u8; 32]],
    ) -> Result<MlsGroupState, StoreError> {
        let mut initial_secret = [0u8; 32];
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"nexo-legacy-mls-group-secret-v1");
        hasher.update(community_id.as_bytes());
        hasher.update(founder_key);
        initial_secret.copy_from_slice(&hasher.finalize());
        self.ensure_mls_group_with_secret(
            community_id,
            founder_device_id,
            founder_key,
            member_keys,
            initial_secret,
        )
    }

    pub fn ensure_mls_group_with_secret(
        &self,
        community_id: Uuid,
        founder_device_id: String,
        founder_key: [u8; 32],
        member_keys: &[[u8; 32]],
        initial_secret: [u8; 32],
    ) -> Result<MlsGroupState, StoreError> {
        if let Some(state) = self.mls_group(community_id)? {
            return Ok(state);
        }
        let mut state = MlsGroupState::new_with_secret(
            community_id,
            founder_device_id,
            founder_key,
            initial_secret,
        );
        let mut additional = member_keys
            .iter()
            .copied()
            .filter(|key| *key != founder_key)
            .collect::<Vec<_>>();
        additional.sort_unstable();
        for key in additional {
            state.add_member(member_device_id(&key), key);
        }
        self.save_mls_group(&state)?;
        Ok(state)
    }

    pub fn mls_group(&self, community_id: Uuid) -> Result<Option<MlsGroupState>, StoreError> {
        let value = self
            .connection
            .query_row(
                "SELECT state_json FROM mls_groups WHERE community_id = ?1",
                [community_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        value
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))
            })
            .transpose()
    }

    pub fn save_mls_group(&self, state: &MlsGroupState) -> Result<(), StoreError> {
        let state_json = serde_json::to_string(state)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO mls_groups(community_id, state_json)
             VALUES (?1, ?2)
             ON CONFLICT(community_id) DO UPDATE SET state_json = excluded.state_json",
            params![state.group_id.to_string(), state_json],
        )?;
        Ok(())
    }

    pub fn save_mls_commit(&self, commit: &MlsCommit) -> Result<bool, StoreError> {
        commit
            .verify_signature()
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let commit_json = serde_json::to_string(commit)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO mls_commits(id, community_id, epoch, commit_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                commit.id.to_string(),
                commit.group_id.to_string(),
                i64::try_from(commit.epoch).map_err(|_| {
                    StoreError::InvalidData("MLS epoch exceeds SQLite integer range".to_owned())
                })?,
                commit_json
            ],
        )?;
        Ok(inserted > 0)
    }

    pub fn has_mls_commit(&self, commit_id: Uuid) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM mls_commits WHERE id = ?1)",
                [commit_id.to_string()],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn mls_commits(&self, community_id: Uuid) -> Result<Vec<MlsCommit>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT commit_json FROM mls_commits
             WHERE community_id = ?1 ORDER BY epoch, id",
        )?;
        let rows =
            statement.query_map([community_id.to_string()], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let value = row?;
            serde_json::from_str(&value).map_err(|error| StoreError::InvalidData(error.to_string()))
        })
        .collect()
    }

    pub fn is_authorized_member(
        &self,
        community_id: Uuid,
        public_key: &[u8; 32],
    ) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM members WHERE community_id = ?1 AND public_key = ?2
                 )",
                params![community_id.to_string(), public_key.as_slice()],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn authorized_members(&self, community_id: Uuid) -> Result<Vec<[u8; 32]>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT public_key FROM members WHERE community_id = ?1 ORDER BY public_key",
        )?;
        let rows =
            statement.query_map([community_id.to_string()], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|row| {
            row?.try_into().map_err(|value: Vec<u8>| {
                StoreError::InvalidData(format!(
                    "member public key has {} bytes instead of 32",
                    value.len()
                ))
            })
        })
        .collect()
    }

    pub fn accept_call_signal(&self, signal: &CallSignal, now: u64) -> Result<bool, StoreError> {
        signal
            .verify(now)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        if !self.is_authorized_member(signal.community_id, &signal.author_key)? {
            return Err(StoreError::UnauthorizedAuthor);
        }
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO call_signals_seen(
                 id, community_id, call_id, author_key, sequence, received_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                signal.id.to_string(),
                signal.community_id.to_string(),
                signal.call_id.to_string(),
                signal.author_key.as_slice(),
                to_i64(signal.sequence)?,
                to_i64(now)?
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Store the decrypted local view while retaining the signed ciphertext envelope.
    pub fn record_direct_message(
        &self,
        envelope: &DirectMessageEnvelope,
        body: &str,
        local_key: &[u8; 32],
        _now: u64,
    ) -> Result<bool, StoreError> {
        envelope
            .verify_signature()
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        if self.community(envelope.community_id)?.is_none()
            || !self.is_authorized_member(envelope.community_id, &envelope.sender_key)?
            || !self.is_authorized_member(envelope.community_id, &envelope.recipient_key)?
            || (local_key != &envelope.sender_key && local_key != &envelope.recipient_key)
        {
            return Err(StoreError::UnauthorizedAuthor);
        }
        if envelope.conversation_id
            != direct_conversation_id(
                envelope.community_id,
                envelope.sender_key,
                envelope.recipient_key,
            )
        {
            return Err(StoreError::InvalidData(
                "direct message conversation id is invalid".to_owned(),
            ));
        }
        let envelope_json = serde_json::to_string(envelope)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO direct_messages(
                 id, community_id, conversation_id, sender_key, recipient_key,
                 body, envelope_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                envelope.id.to_string(),
                envelope.community_id.to_string(),
                envelope.conversation_id.to_string(),
                envelope.sender_key.as_slice(),
                envelope.recipient_key.as_slice(),
                body,
                envelope_json,
                to_i64(envelope.created_at)?,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn direct_messages(
        &self,
        conversation_id: Uuid,
        limit: usize,
        _now: u64,
    ) -> Result<Vec<StoredDirectMessage>, StoreError> {
        let limit = i64::try_from(limit.min(500))
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let mut statement = self.connection.prepare(
            "SELECT envelope_json, body FROM (
                 SELECT id, envelope_json, body, created_at FROM direct_messages
                 WHERE conversation_id = ?1
                 ORDER BY created_at DESC, id DESC LIMIT ?2
             ) ORDER BY created_at, id",
        )?;
        let rows = statement.query_map(params![conversation_id.to_string(), limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let Ok((envelope_json, body)) = row else {
                continue;
            };
            let Ok(envelope) = serde_json::from_str::<DirectMessageEnvelope>(&envelope_json) else {
                continue;
            };
            if envelope.conversation_id == conversation_id && envelope.verify_signature().is_ok() {
                messages.push(StoredDirectMessage { envelope, body });
            }
        }
        Ok(messages)
    }

    /// Delete call signals recorded before `older_than_timestamp` to prevent
    /// unbounded table growth.
    pub fn prune_old_call_signals(&self, older_than_timestamp: u64) -> Result<usize, StoreError> {
        let deleted = self.connection.execute(
            "DELETE FROM call_signals_seen WHERE received_at < ?1",
            params![to_i64(older_than_timestamp)?],
        )?;
        Ok(deleted)
    }

    /// Revokes authorization for `member_key` in `community_id`.
    pub fn revoke_member(&self, community_id: Uuid, member_key: &[u8]) -> Result<bool, StoreError> {
        let deleted_member = self.connection.execute(
            "DELETE FROM members WHERE community_id = ?1 AND public_key = ?2",
            params![community_id.to_string(), member_key],
        )?;
        self.connection.execute(
            "INSERT OR IGNORE INTO revoked_members(community_id, public_key, revoked_at)
             VALUES (?1, ?2, ?3)",
            params![
                community_id.to_string(),
                member_key,
                to_i64(current_timestamp())?
            ],
        )?;
        Ok(deleted_member > 0)
    }

    pub fn is_revoked_member(
        &self,
        community_id: Uuid,
        member_key: &[u8; 32],
    ) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM revoked_members
                     WHERE community_id = ?1 AND public_key = ?2
                 )",
                params![community_id.to_string(), member_key.as_slice()],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    /// Record a file transfer offer into the local database.
    pub fn record_file_offer(
        &self,
        offer: &FileTransferOffer,
        local_path: Option<&str>,
        status: &str,
        now: u64,
    ) -> Result<bool, StoreError> {
        offer
            .verify(now)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        if !self.is_authorized_member(offer.community_id, &offer.author_key)? {
            return Err(StoreError::UnauthorizedAuthor);
        }
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO file_transfers(
                 id, community_id, channel_id, file_name, file_size, mime_type,
                 chunk_size, total_chunks, root_sha256, author_key, local_path,
                 status, downloaded_chunks, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                offer.id.to_string(),
                offer.community_id.to_string(),
                offer.channel_id.to_string(),
                offer.file_name,
                to_i64(offer.file_size)?,
                offer.mime_type,
                to_i64(u64::from(offer.chunk_size))?,
                to_i64(u64::from(offer.total_chunks))?,
                offer.root_sha256.as_slice(),
                offer.author_key.as_slice(),
                local_path,
                status,
                0,
                to_i64(offer.created_at)?
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Record a downloaded chunk for a file transfer and update `downloaded_chunks` count.
    pub fn record_chunk_received(
        &self,
        transfer_id: Uuid,
        chunk_index: u32,
    ) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO file_chunks_saved(transfer_id, chunk_index)
             VALUES (?1, ?2)",
            params![transfer_id.to_string(), to_i64(u64::from(chunk_index))?],
        )?;
        let count: i64 = transaction.query_row(
            "SELECT count(1) FROM file_chunks_saved WHERE transfer_id = ?1",
            [transfer_id.to_string()],
            |row| row.get(0),
        )?;
        let count_u32 =
            u32::try_from(count).map_err(|error| StoreError::InvalidData(error.to_string()))?;
        transaction.execute(
            "UPDATE file_transfers SET downloaded_chunks = ?1 WHERE id = ?2",
            params![count, transfer_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(count_u32)
    }

    /// Retrieve a file transfer by id.
    pub fn file_transfer(
        &self,
        transfer_id: Uuid,
    ) -> Result<Option<StoredFileTransfer>, StoreError> {
        self.connection
            .query_row(
                "SELECT id, community_id, channel_id, file_name, file_size, mime_type,
                        chunk_size, total_chunks, root_sha256, author_key, local_path,
                        status, downloaded_chunks, created_at
                 FROM file_transfers WHERE id = ?1",
                [transfer_id.to_string()],
                |row| {
                    let id_str: String = row.get(0)?;
                    let comm_str: String = row.get(1)?;
                    let chan_str: String = row.get(2)?;
                    let root_sha_vec: Vec<u8> = row.get(8)?;
                    let author_vec: Vec<u8> = row.get(9)?;
                    let root_sha256: [u8; 32] = root_sha_vec
                        .try_into()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    let author_key: [u8; 32] = author_vec
                        .try_into()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;

                    Ok(StoredFileTransfer {
                        id: parse_uuid(&id_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                        community_id: parse_uuid(&comm_str)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        channel_id: parse_uuid(&chan_str)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        file_name: row.get(3)?,
                        file_size: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                        mime_type: row.get(5)?,
                        chunk_size: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
                        total_chunks: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
                        root_sha256,
                        author_key,
                        local_path: row.get(10)?,
                        status: row.get(11)?,
                        downloaded_chunks: u32::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
                        created_at: u64::try_from(row.get::<_, i64>(13)?).unwrap_or(0),
                    })
                },
            )
            .optional()
            .map_err(StoreError::Database)
    }

    /// List all file transfers for a channel.
    pub fn file_transfers_in_channel(
        &self,
        channel_id: Uuid,
    ) -> Result<Vec<StoredFileTransfer>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, community_id, channel_id, file_name, file_size, mime_type,
                    chunk_size, total_chunks, root_sha256, author_key, local_path,
                    status, downloaded_chunks, created_at
             FROM file_transfers WHERE channel_id = ?1 ORDER BY created_at",
        )?;
        let rows = statement.query_map([channel_id.to_string()], |row| {
            let id_str: String = row.get(0)?;
            let comm_str: String = row.get(1)?;
            let chan_str: String = row.get(2)?;
            let root_sha_vec: Vec<u8> = row.get(8)?;
            let author_vec: Vec<u8> = row.get(9)?;
            let root_sha256: [u8; 32] = root_sha_vec
                .try_into()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let author_key: [u8; 32] = author_vec
                .try_into()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;

            Ok(StoredFileTransfer {
                id: parse_uuid(&id_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                community_id: parse_uuid(&comm_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                channel_id: parse_uuid(&chan_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                file_name: row.get(3)?,
                file_size: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                mime_type: row.get(5)?,
                chunk_size: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
                total_chunks: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
                root_sha256,
                author_key,
                local_path: row.get(10)?,
                status: row.get(11)?,
                downloaded_chunks: u32::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
                created_at: u64::try_from(row.get::<_, i64>(13)?).unwrap_or(0),
            })
        })?;
        let mut transfers = Vec::new();
        for row in rows {
            transfers.push(row?);
        }
        Ok(transfers)
    }

    /// Update status and optional local path of a file transfer.
    pub fn update_file_transfer_status(
        &self,
        transfer_id: Uuid,
        status: &str,
        local_path: Option<&str>,
    ) -> Result<(), StoreError> {
        if let Some(path) = local_path {
            self.connection.execute(
                "UPDATE file_transfers SET status = ?1, local_path = ?2 WHERE id = ?3",
                params![status, path, transfer_id.to_string()],
            )?;
        } else {
            self.connection.execute(
                "UPDATE file_transfers SET status = ?1 WHERE id = ?2",
                params![status, transfer_id.to_string()],
            )?;
        }
        Ok(())
    }

    pub fn save_credential(&self, credential: &CommunityCredential) -> Result<(), StoreError> {
        if self.is_revoked_member(credential.invite.network_id, &credential.member_key)? {
            return Err(StoreError::InvalidData(
                "member is revoked in this community".to_owned(),
            ));
        }
        let credential_json = serde_json::to_string(credential)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO credentials(community_id, member_key, credential_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(community_id, member_key)
             DO UPDATE SET credential_json = excluded.credential_json",
            params![
                credential.invite.network_id.to_string(),
                credential.member_key.as_slice(),
                credential_json
            ],
        )?;
        Ok(())
    }

    pub fn credentials(&self, community_id: Uuid) -> Result<Vec<CommunityCredential>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT credential_json FROM credentials
             WHERE community_id = ?1 ORDER BY member_key",
        )?;
        let rows =
            statement.query_map([community_id.to_string()], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let value = row?;
            serde_json::from_str(&value).map_err(|error| StoreError::InvalidData(error.to_string()))
        })
        .collect()
    }

    pub fn import_credential(
        &self,
        credential: &CommunityCredential,
        now: u64,
    ) -> Result<(), StoreError> {
        credential
            .verify(now)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let community_id = credential.invite.network_id;
        let known = self.community(community_id)?.is_some();
        if !known
            || !self.accepts_sync_token(community_id, community_sync_token(&credential.invite))?
        {
            return Err(StoreError::InvalidData(
                "unknown community credential".to_owned(),
            ));
        }
        if self.is_revoked_member(community_id, &credential.member_key)? {
            return Err(StoreError::InvalidData(
                "member is revoked in this community".to_owned(),
            ));
        }
        self.authorize_member(
            credential.invite.network_id,
            &credential.member_key,
            credential.accepted_at,
        )?;
        self.save_credential(credential)
    }

    pub fn sync_tokens(&self) -> Result<Vec<(Uuid, [u8; 32])>, StoreError> {
        let mut tokens = Vec::new();
        for community in self.communities()? {
            if let Some(credential) = self.credentials(community.id)?.into_iter().next() {
                tokens.push((community.id, community_sync_token(&credential.invite)));
            }
        }
        Ok(tokens)
    }

    pub fn accepts_sync_token(
        &self,
        community_id: Uuid,
        token: [u8; 32],
    ) -> Result<bool, StoreError> {
        Ok(self
            .credentials(community_id)?
            .iter()
            .any(|credential| community_sync_token(&credential.invite) == token))
    }

    pub fn sync_messages(
        &self,
        community_id: Uuid,
        limit: usize,
        now: u64,
    ) -> Result<Vec<SignedMessage>, StoreError> {
        let community = self
            .community(community_id)?
            .ok_or_else(|| StoreError::InvalidData("unknown community".to_owned()))?;
        self.messages(community.default_channel_id, limit, now)
    }

    pub fn sync_page(
        &self,
        peer_id: &str,
        receiver_epoch: Uuid,
        community_id: Uuid,
        limit: usize,
        now: u64,
    ) -> Result<(Vec<SignedMessage>, bool), StoreError> {
        let _community = self
            .community(community_id)?
            .ok_or_else(|| StoreError::InvalidData("unknown community".to_owned()))?;
        let fetch = limit.min(500).saturating_add(1);
        let fetch =
            i64::try_from(fetch).map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let mut statement = self.connection.prepare(
            "SELECT id, version, community_id, channel_id, author_key, body, created_at, signature
             FROM messages m
            WHERE m.community_id = ?1 AND NOT EXISTS (
                 SELECT 1 FROM sync_deliveries d
                 WHERE d.peer_id = ?2 AND d.receiver_epoch = ?3
                   AND d.community_id = ?4 AND d.message_id = m.id
             )
             ORDER BY created_at, id LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                community_id.to_string(),
                peer_id,
                receiver_epoch.to_string(),
                community_id.to_string(),
                fetch
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                ))
            },
        )?;
        let mut messages = Vec::new();
        for row in rows {
            let (id, version, community_id, channel_id, author_key, body, created_at, signature) =
                row?;
            let message = stored_message(
                &id,
                version,
                &community_id,
                &channel_id,
                author_key,
                body,
                created_at,
                signature,
            )?;
            if message.verify(now).is_ok() {
                messages.push(message);
            }
        }
        let has_more = messages.len() > limit;
        messages.truncate(limit);
        Ok((messages, has_more))
    }

    /// Return encrypted direct-message envelopes addressed to one peer that
    /// have not yet been acknowledged for this database epoch.
    pub fn sync_direct_page(
        &self,
        peer_id: &str,
        receiver_epoch: Uuid,
        community_id: Uuid,
        recipient_key: &[u8; 32],
        limit: usize,
    ) -> Result<(Vec<DirectMessageEnvelope>, bool), StoreError> {
        let _community = self
            .community(community_id)?
            .ok_or_else(|| StoreError::InvalidData("unknown community".to_owned()))?;
        let fetch = limit.min(500).saturating_add(1);
        let fetch =
            i64::try_from(fetch).map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let mut statement = self.connection.prepare(
            "SELECT id, envelope_json FROM direct_messages dm
             WHERE dm.community_id = ?1 AND dm.recipient_key = ?2
               AND NOT EXISTS (
                   SELECT 1 FROM direct_sync_deliveries d
                   WHERE d.peer_id = ?3 AND d.receiver_epoch = ?4
                     AND d.community_id = ?5 AND d.message_id = dm.id
               )
             ORDER BY dm.created_at, dm.id LIMIT ?6",
        )?;
        let rows = statement.query_map(
            params![
                community_id.to_string(),
                recipient_key.as_slice(),
                peer_id,
                receiver_epoch.to_string(),
                community_id.to_string(),
                fetch,
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut envelopes = Vec::new();
        for row in rows {
            let (_id, value) = row?;
            let envelope = serde_json::from_str::<DirectMessageEnvelope>(&value)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?;
            if envelope.community_id == community_id
                && envelope.recipient_key == *recipient_key
                && envelope.conversation_id
                    == direct_conversation_id(
                        community_id,
                        envelope.sender_key,
                        envelope.recipient_key,
                    )
                && envelope.verify_signature().is_ok()
            {
                envelopes.push(envelope);
            }
        }
        let has_more = envelopes.len() > limit;
        envelopes.truncate(limit);
        Ok((envelopes, has_more))
    }

    /// Return authenticated MLS commits that have not yet been acknowledged
    /// by this peer and database epoch.
    pub fn sync_mls_page(
        &self,
        peer_id: &str,
        receiver_epoch: Uuid,
        community_id: Uuid,
        limit: usize,
    ) -> Result<(Vec<MlsCommit>, bool), StoreError> {
        let _community = self
            .community(community_id)?
            .ok_or_else(|| StoreError::InvalidData("unknown community".to_owned()))?;
        let fetch = limit.min(500).saturating_add(1);
        let fetch =
            i64::try_from(fetch).map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let mut statement = self.connection.prepare(
            "SELECT commit_json FROM mls_commits mc
             WHERE mc.community_id = ?1 AND NOT EXISTS (
                 SELECT 1 FROM sync_deliveries d
                 WHERE d.peer_id = ?2 AND d.receiver_epoch = ?3
                   AND d.community_id = ?1 AND d.message_id = mc.id
             )
             ORDER BY mc.epoch, mc.id LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                community_id.to_string(),
                peer_id,
                receiver_epoch.to_string(),
                fetch
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut commits = Vec::new();
        for row in rows {
            let value = row?;
            let commit = serde_json::from_str::<MlsCommit>(&value)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?;
            if commit.group_id == community_id && commit.verify_signature().is_ok() {
                commits.push(commit);
            }
        }
        let has_more = commits.len() > limit;
        commits.truncate(limit);
        Ok((commits, has_more))
    }

    pub fn record_pending_direct(
        &self,
        peer_id: &str,
        receiver_epoch: Uuid,
        community_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<(), StoreError> {
        for message_id in message_ids {
            self.connection.execute(
                "INSERT OR IGNORE INTO direct_sync_pending(
                     peer_id, receiver_epoch, community_id, message_id
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    peer_id,
                    receiver_epoch.to_string(),
                    community_id.to_string(),
                    message_id.to_string()
                ],
            )?;
        }
        Ok(())
    }

    pub fn acknowledge_pending_direct(
        &self,
        peer_id: &str,
        receiver_epoch: Uuid,
        community_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<usize, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut accepted = 0;
        for message_id in message_ids {
            let pending = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM direct_sync_pending
                     WHERE peer_id = ?1 AND receiver_epoch = ?2
                       AND community_id = ?3 AND message_id = ?4
                 )",
                params![
                    peer_id,
                    receiver_epoch.to_string(),
                    community_id.to_string(),
                    message_id.to_string()
                ],
                |row| row.get::<_, bool>(0),
            )?;
            if !pending {
                continue;
            }
            transaction.execute(
                "INSERT OR IGNORE INTO direct_sync_deliveries(
                     peer_id, receiver_epoch, community_id, message_id
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    peer_id,
                    receiver_epoch.to_string(),
                    community_id.to_string(),
                    message_id.to_string()
                ],
            )?;
            transaction.execute(
                "DELETE FROM direct_sync_pending
                 WHERE peer_id = ?1 AND receiver_epoch = ?2
                   AND community_id = ?3 AND message_id = ?4",
                params![
                    peer_id,
                    receiver_epoch.to_string(),
                    community_id.to_string(),
                    message_id.to_string()
                ],
            )?;
            accepted += 1;
        }
        transaction.commit()?;
        Ok(accepted)
    }

    pub fn record_pending(
        &self,
        peer_id: &str,
        receiver_epoch: Uuid,
        community_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<(), StoreError> {
        for message_id in message_ids {
            self.connection.execute(
                "INSERT OR IGNORE INTO sync_pending(
                     peer_id, receiver_epoch, community_id, message_id
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    peer_id,
                    receiver_epoch.to_string(),
                    community_id.to_string(),
                    message_id.to_string()
                ],
            )?;
        }
        Ok(())
    }

    pub fn acknowledge_pending(
        &self,
        peer_id: &str,
        receiver_epoch: Uuid,
        community_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<usize, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut accepted = 0;
        for message_id in message_ids {
            let pending = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sync_pending
                     WHERE peer_id = ?1 AND receiver_epoch = ?2
                       AND community_id = ?3 AND message_id = ?4
                 )",
                params![
                    peer_id,
                    receiver_epoch.to_string(),
                    community_id.to_string(),
                    message_id.to_string()
                ],
                |row| row.get::<_, bool>(0),
            )?;
            if !pending {
                continue;
            }
            transaction.execute(
                "INSERT OR IGNORE INTO sync_deliveries(
                     peer_id, receiver_epoch, community_id, message_id
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    peer_id,
                    receiver_epoch.to_string(),
                    community_id.to_string(),
                    message_id.to_string()
                ],
            )?;
            transaction.execute(
                "DELETE FROM sync_pending
                 WHERE peer_id = ?1 AND receiver_epoch = ?2
                   AND community_id = ?3 AND message_id = ?4",
                params![
                    peer_id,
                    receiver_epoch.to_string(),
                    community_id.to_string(),
                    message_id.to_string()
                ],
            )?;
            accepted += 1;
        }
        transaction.commit()?;
        Ok(accepted)
    }

    pub fn import_messages(
        &self,
        community_id: Uuid,
        messages: &[SignedMessage],
        now: u64,
    ) -> Result<usize, StoreError> {
        Ok(self
            .import_messages_accepted(community_id, messages, now)?
            .1)
    }

    pub fn import_messages_with_mls(
        &self,
        community_id: Uuid,
        messages: &[SignedMessage],
        mls_state: Option<&MlsGroupState>,
        now: u64,
    ) -> Result<usize, StoreError> {
        Ok(self
            .import_messages_accepted_with_mls(community_id, messages, mls_state, now)?
            .1)
    }

    pub fn import_messages_accepted(
        &self,
        community_id: Uuid,
        messages: &[SignedMessage],
        now: u64,
    ) -> Result<(Vec<Uuid>, usize), StoreError> {
        self.import_messages_accepted_with_mls(community_id, messages, None, now)
    }

    pub fn import_messages_accepted_with_mls(
        &self,
        community_id: Uuid,
        messages: &[SignedMessage],
        mls_state: Option<&MlsGroupState>,
        now: u64,
    ) -> Result<(Vec<Uuid>, usize), StoreError> {
        let mut accepted = Vec::new();
        let mut inserted = 0;
        for message in messages {
            if message.community_id != community_id {
                continue;
            }
            match self.insert_message_with_mls(message, mls_state, now) {
                Ok(true) => {
                    inserted += 1;
                    accepted.push(message.id);
                }
                Ok(false) => accepted.push(message.id),
                Err(
                    StoreError::InvalidMessage(_)
                    | StoreError::ChannelMismatch
                    | StoreError::UnauthorizedAuthor,
                ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok((accepted, inserted))
    }

    pub fn communities(&self) -> Result<Vec<Community>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT c.id, c.name, ch.id
             FROM communities c
             JOIN channels ch ON ch.community_id = c.id AND ch.position = 0
             ORDER BY c.created_at, c.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (id, name, channel_id) = row?;
            Ok(Community {
                id: parse_uuid(&id)?,
                name,
                default_channel_id: parse_uuid(&channel_id)?,
            })
        })
        .collect()
    }

    pub fn insert_message(&self, message: &SignedMessage, now: u64) -> Result<bool, StoreError> {
        self.insert_message_with_mls(message, None, now)
    }

    pub fn insert_message_with_mls(
        &self,
        message: &SignedMessage,
        mls_state: Option<&MlsGroupState>,
        now: u64,
    ) -> Result<bool, StoreError> {
        message.verify(now)?;
        if message.version == 2 {
            let Some(mls_state) = mls_state else {
                return Err(StoreError::InvalidData(
                    "encrypted community message has no MLS state".to_owned(),
                ));
            };
            message.decrypt_body(mls_state)?;
        }
        let community_id = message.community_id.to_string();
        let channel_owner: Option<String> = self
            .connection
            .query_row(
                "SELECT community_id FROM channels WHERE id = ?1",
                [message.channel_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if channel_owner.as_deref() != Some(community_id.as_str()) {
            return Err(StoreError::ChannelMismatch);
        }
        let authorized = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM members WHERE community_id = ?1 AND public_key = ?2
             )",
            params![community_id, message.author_key.as_slice()],
            |row| row.get::<_, bool>(0),
        )?;
        if !authorized {
            return Err(StoreError::UnauthorizedAuthor);
        }
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO messages(
                 id, version, community_id, channel_id, author_key, body, created_at, signature
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message.id.to_string(),
                message.version,
                community_id,
                message.channel_id.to_string(),
                message.author_key.as_slice(),
                message.body,
                to_i64(message.created_at)?,
                message.signature,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn messages(
        &self,
        channel_id: Uuid,
        limit: usize,
        now: u64,
    ) -> Result<Vec<SignedMessage>, StoreError> {
        let limit = i64::try_from(limit.min(500))
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let mut statement = self.connection.prepare(
            "SELECT id, version, community_id, channel_id, author_key, body, created_at, signature
             FROM (
                 SELECT id, version, community_id, channel_id, author_key, body, created_at, signature
                 FROM messages WHERE channel_id = ?1
                 ORDER BY created_at DESC, id DESC LIMIT ?2
             ) ORDER BY created_at, id",
        )?;
        let rows = statement.query_map(params![channel_id.to_string(), limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Vec<u8>>(7)?,
            ))
        })?;
        let mut valid = Vec::new();
        for row in rows {
            let Ok((
                id,
                version,
                community_id,
                channel_id,
                author_key,
                body,
                created_at,
                signature,
            )) = row
            else {
                continue;
            };
            let Ok(message) = stored_message(
                &id,
                version,
                &community_id,
                &channel_id,
                author_key,
                body,
                created_at,
                signature,
            ) else {
                continue;
            };
            if message.verify(now).is_ok() {
                valid.push(message);
            }
        }
        Ok(valid)
    }

    fn community(&self, id: Uuid) -> Result<Option<Community>, StoreError> {
        self.connection
            .query_row(
                "SELECT c.id, c.name, ch.id FROM communities c
                 JOIN channels ch ON ch.community_id = c.id AND ch.position = 0
                 WHERE c.id = ?1",
                [id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .map(|(id, name, channel_id)| {
                Ok(Community {
                    id: parse_uuid(&id)?,
                    name,
                    default_channel_id: parse_uuid(&channel_id)?,
                })
            })
            .transpose()
    }
}

fn default_channel_id(community_id: Uuid) -> Uuid {
    Uuid::new_v5(&DEFAULT_CHANNEL_NAMESPACE, community_id.as_bytes())
}

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn migrate_credentials(connection: &mut Connection) -> Result<(), StoreError> {
    if has_column(connection, "credentials", "member_key")? {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    let legacy = {
        let mut statement = transaction.prepare("SELECT credential_json FROM credentials")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.filter_map(Result::ok).collect::<Vec<_>>()
    };
    transaction.execute_batch(
        "ALTER TABLE credentials RENAME TO credentials_legacy;
         CREATE TABLE credentials (
             community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
             member_key BLOB NOT NULL,
             credential_json TEXT NOT NULL,
             PRIMARY KEY(community_id, member_key)
         );",
    )?;
    for value in legacy {
        let Ok(credential) = serde_json::from_str::<CommunityCredential>(&value) else {
            continue;
        };
        transaction.execute(
            "INSERT OR IGNORE INTO credentials(community_id, member_key, credential_json)
             VALUES (?1, ?2, ?3)",
            params![
                credential.invite.network_id.to_string(),
                credential.member_key.as_slice(),
                value
            ],
        )?;
    }
    transaction.execute("DROP TABLE credentials_legacy", [])?;
    transaction.pragma_update(None, "user_version", 1)?;
    transaction.commit()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stored_message(
    id: &str,
    version: i64,
    community_id: &str,
    channel_id: &str,
    author_key: Vec<u8>,
    body: String,
    created_at: i64,
    signature: Vec<u8>,
) -> Result<SignedMessage, StoreError> {
    Ok(SignedMessage {
        version: u8::try_from(version)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?,
        id: parse_uuid(id)?,
        community_id: parse_uuid(community_id)?,
        channel_id: parse_uuid(channel_id)?,
        author_key: author_key
            .try_into()
            .map_err(|_| StoreError::InvalidData("invalid author key".to_owned()))?,
        body,
        created_at: u64::try_from(created_at)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?,
        signature,
    })
}

fn parse_uuid(value: &str) -> Result<Uuid, StoreError> {
    Uuid::parse_str(value).map_err(|error| StoreError::InvalidData(error.to_string()))
}

fn to_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|error| StoreError::InvalidData(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_core::{
        CommunityCredential, DeviceIdentity, DoubleRatchetSession, MlsCommit, NetworkInvite,
        direct_conversation_id, public_key_from_private,
    };

    #[test]
    fn direct_messages_are_persisted_once_and_survive_signal_expiry() {
        let path = std::env::temp_dir().join(format!("nexo-dm-{}.sqlite3", Uuid::new_v4()));
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let mut store = LocalStore::open(&path).expect("store should open");
        let community = store
            .create_community(community_id, "DM", 100)
            .expect("community should exist");
        store
            .authorize_member(community_id, &alice.public_key_bytes(), 100)
            .expect("alice should be authorized");
        store
            .authorize_member(community_id, &bob.public_key_bytes(), 100)
            .expect("bob should be authorized");
        let mut ratchet = DoubleRatchetSession::initialize_initiator(
            [9_u8; 32],
            public_key_from_private([31_u8; 32]),
        );
        let conversation_id = direct_conversation_id(
            community_id,
            alice.public_key_bytes(),
            bob.public_key_bytes(),
        );
        let envelope = DirectMessageEnvelope::create(
            &alice,
            community_id,
            conversation_id,
            bob.public_key_bytes(),
            ratchet.encrypt(b"persisted"),
            100,
        )
        .expect("envelope should exist");
        assert!(
            store
                .record_direct_message(&envelope, "persisted", &alice.public_key_bytes(), 100)
                .expect("message should be stored")
        );
        assert!(
            !store
                .record_direct_message(&envelope, "persisted", &alice.public_key_bytes(), 100)
                .expect("duplicate should be ignored")
        );
        let messages = store
            .direct_messages(conversation_id, 50, 100 + 3600)
            .expect("messages should load");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "persisted");
        assert_eq!(messages[0].envelope.community_id, community.id);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn community_and_signed_messages_survive_reopen() {
        let path = std::env::temp_dir().join(format!("nexo-store-{}.sqlite3", Uuid::new_v4()));
        let identity = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let channel_id;
        {
            let mut store = LocalStore::open(&path).expect("store should open");
            let community = store
                .create_community(community_id, "Teste", 100)
                .expect("community should be created");
            channel_id = community.default_channel_id;
            store
                .authorize_member(community_id, &identity.public_key_bytes(), 100)
                .expect("member should be authorized");
            let message = SignedMessage::create(
                &identity,
                community_id,
                channel_id,
                "mensagem persistente".to_owned(),
                101,
            )
            .expect("message should be created");
            assert!(
                store
                    .insert_message(&message, 101)
                    .expect("insert should work")
            );
            assert!(
                !store
                    .insert_message(&message, 101)
                    .expect("duplicate is idempotent")
            );
        }
        let store = LocalStore::open(&path).expect("store should reopen");
        let messages = store
            .messages(channel_id, 100, 101)
            .expect("messages should load");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "mensagem persistente");
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mls_epoch_and_commit_history_survive_reopen() {
        let path = std::env::temp_dir().join(format!("nexo-mls-{}.sqlite3", Uuid::new_v4()));
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        {
            let mut store = LocalStore::open(&path).expect("store should open");
            store
                .create_community(community_id, "MLS", 100)
                .expect("community should be created");
            store
                .authorize_member(community_id, &alice.public_key_bytes(), 100)
                .expect("alice should be authorized");
            store
                .authorize_member(community_id, &bob.public_key_bytes(), 101)
                .expect("bob should be authorized");
            let mut state = store
                .ensure_mls_group(
                    community_id,
                    "alice".to_owned(),
                    alice.public_key_bytes(),
                    &[alice.public_key_bytes()],
                )
                .expect("MLS group should initialize");
            let commit =
                MlsCommit::create_add(&alice, &state, "bob".to_owned(), bob.public_key_bytes())
                    .expect("commit should be signed");
            state
                .apply_commit(&commit)
                .expect("commit should advance the group");
            store
                .save_mls_commit(&commit)
                .expect("commit should persist");
            store.save_mls_group(&state).expect("state should persist");
            assert_eq!(state.epoch, 1);
        }

        let store = LocalStore::open(&path).expect("store should reopen");
        let state = store
            .mls_group(community_id)
            .expect("MLS group should load")
            .expect("MLS group should exist");
        let commits = store
            .mls_commits(community_id)
            .expect("MLS commits should load");
        assert_eq!(state.epoch, 1);
        assert!(state.contains_member(&bob.public_key_bytes()));
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].epoch, state.epoch);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn default_channel_is_deterministic_and_unauthorized_messages_are_rejected() {
        let path = std::env::temp_dir().join(format!("nexo-store-{}.sqlite3", Uuid::new_v4()));
        let community_id = Uuid::new_v4();
        let identity = DeviceIdentity::generate();
        let mut store = LocalStore::open(&path).expect("store should open");
        let first = store
            .create_community(community_id, "Teste", 100)
            .expect("community should be created");
        let second = store
            .create_community(community_id, "Teste", 100)
            .expect("community should be idempotent");
        assert_eq!(first.default_channel_id, second.default_channel_id);
        let message = SignedMessage::create(
            &identity,
            community_id,
            first.default_channel_id,
            "sem autorizacao".to_owned(),
            101,
        )
        .expect("message should be created");
        assert!(matches!(
            store.insert_message(&message, 101),
            Err(StoreError::UnauthorizedAuthor)
        ));
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn authorized_member_keys_are_returned_without_duplicates() {
        let path = std::env::temp_dir().join(format!("nexo-store-{}.sqlite3", Uuid::new_v4()));
        let mut store = LocalStore::open(&path).expect("store should open");
        let community_id = Uuid::new_v4();
        store
            .create_community(community_id, "Teste", 99)
            .expect("community should be created");
        let first = DeviceIdentity::generate().public_key_bytes();
        let second = DeviceIdentity::generate().public_key_bytes();
        store
            .authorize_member(community_id, &first, 100)
            .expect("first member should be authorized");
        store
            .authorize_member(community_id, &second, 101)
            .expect("second member should be authorized");
        store
            .authorize_member(community_id, &first, 102)
            .expect("duplicate authorization should be idempotent");
        let members = store
            .authorized_members(community_id)
            .expect("authorized members should load");
        assert_eq!(members.len(), 2);
        assert!(members.contains(&first));
        assert!(members.contains(&second));
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn two_independent_databases_converge_from_member_credentials() {
        let alice_path =
            std::env::temp_dir().join(format!("nexo-alice-{}.sqlite3", Uuid::new_v4()));
        let bob_path = std::env::temp_dir().join(format!("nexo-bob-{}.sqlite3", Uuid::new_v4()));
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let invite = NetworkInvite::create(&alice, "Amigos", Vec::new(), 100, 600)
            .expect("invite should be created");
        let alice_credential = CommunityCredential::claim(&alice, invite.clone(), 110)
            .expect("alice credential should be claimed");
        let bob_credential = CommunityCredential::claim(&bob, invite.clone(), 120)
            .expect("bob credential should be claimed");

        let mut alice_store = LocalStore::open(&alice_path).expect("alice store should open");
        let alice_community = alice_store
            .create_community(invite.network_id, &invite.network_name, 100)
            .expect("alice community should be created");
        alice_store
            .authorize_member(invite.network_id, &alice.public_key_bytes(), 110)
            .expect("alice should be authorized");
        alice_store
            .save_credential(&alice_credential)
            .expect("alice credential should save");
        let alice_message = SignedMessage::create(
            &alice,
            invite.network_id,
            alice_community.default_channel_id,
            "mensagem da alice".to_owned(),
            130,
        )
        .expect("alice message should be created");
        alice_store
            .insert_message(&alice_message, 130)
            .expect("alice message should be stored");

        let mut bob_store = LocalStore::open(&bob_path).expect("bob store should open");
        bob_store
            .create_community(invite.network_id, &invite.network_name, 120)
            .expect("bob community should be created");
        bob_store
            .authorize_member(invite.network_id, &bob.public_key_bytes(), 120)
            .expect("bob should be authorized");
        bob_store
            .save_credential(&bob_credential)
            .expect("bob credential should save");

        bob_store
            .import_credential(&alice_credential, 900)
            .expect("alice membership remains valid after invite expiry");
        assert_eq!(
            bob_store
                .import_messages(invite.network_id, &[alice_message], 900)
                .expect("alice message should import"),
            1
        );
        let bob_messages = bob_store
            .sync_messages(invite.network_id, 100, 900)
            .expect("bob history should load");
        assert_eq!(bob_messages.len(), 1);
        assert_eq!(bob_messages[0].body, "mensagem da alice");

        drop(alice_store);
        drop(bob_store);
        let _ = std::fs::remove_file(alice_path);
        let _ = std::fs::remove_file(bob_path);
    }

    #[test]
    fn delivery_pages_converge_and_include_late_old_messages() {
        let path = std::env::temp_dir().join(format!("nexo-pages-{}.sqlite3", Uuid::new_v4()));
        let identity = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let peer = "peer-b";
        let epoch = Uuid::new_v4();
        let mut store = LocalStore::open(&path).expect("store should open");
        let community = store
            .create_community(community_id, "Paginas", 1)
            .expect("community should be created");
        store
            .authorize_member(community_id, &identity.public_key_bytes(), 1)
            .expect("member should be authorized");
        for index in 0..205_u64 {
            let message = SignedMessage::create(
                &identity,
                community_id,
                community.default_channel_id,
                format!("mensagem {index}"),
                10 + index,
            )
            .expect("message should be created");
            store
                .insert_message(&message, 1_000)
                .expect("message should be inserted");
        }

        let (first, first_more) = store
            .sync_page(peer, epoch, community_id, 200, 1_000)
            .expect("first page should load");
        assert_eq!(first.len(), 200);
        assert!(first_more);
        let first_ids = first.iter().map(|message| message.id).collect::<Vec<_>>();
        store
            .record_pending(peer, epoch, community_id, &first_ids)
            .expect("first page should be pending");
        store
            .acknowledge_pending(peer, epoch, community_id, &first_ids)
            .expect("first page should be acknowledged");
        let (second, second_more) = store
            .sync_page(peer, epoch, community_id, 200, 1_000)
            .expect("second page should load");
        assert_eq!(second.len(), 5);
        assert!(!second_more);
        let second_ids = second.iter().map(|message| message.id).collect::<Vec<_>>();
        store
            .record_pending(peer, epoch, community_id, &second_ids)
            .expect("second page should be pending");
        store
            .acknowledge_pending(peer, epoch, community_id, &second_ids)
            .expect("second page should be acknowledged");

        let late = SignedMessage::create(
            &identity,
            community_id,
            community.default_channel_id,
            "mensagem atrasada".to_owned(),
            2,
        )
        .expect("late message should be created");
        store
            .insert_message(&late, 1_000)
            .expect("late message should be inserted");
        let (late_page, has_more) = store
            .sync_page(peer, epoch, community_id, 200, 1_000)
            .expect("late page should load");
        assert_eq!(late_page, vec![late]);
        assert!(!has_more);

        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unsolicited_delivery_receipts_are_ignored() {
        let path = std::env::temp_dir().join(format!("nexo-receipts-{}.sqlite3", Uuid::new_v4()));
        let store = LocalStore::open(&path).expect("store should open");
        let accepted = store
            .acknowledge_pending(
                "unknown-peer",
                Uuid::new_v4(),
                Uuid::new_v4(),
                &[Uuid::new_v4()],
            )
            .expect("unsolicited receipt should be handled");
        assert_eq!(accepted, 0);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn call_signals_require_membership_and_are_replay_safe() {
        use nexo_core::{CallSignalKind, current_timestamp};

        let path = std::env::temp_dir().join(format!("nexo-call-{}.sqlite3", Uuid::new_v4()));
        let member = DeviceIdentity::generate();
        let outsider = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let now = current_timestamp();
        let mut store = LocalStore::open(&path).expect("store should open");
        store
            .create_community(community_id, "Chamada", now)
            .expect("community should be created");
        store
            .authorize_member(community_id, &member.public_key_bytes(), now)
            .expect("member should be authorized");
        let member_signal = CallSignal::create(
            &member,
            community_id,
            Uuid::new_v4(),
            1,
            CallSignalKind::Offer,
            "v=0".into(),
            now,
        )
        .expect("member signal should be signed");
        assert!(
            store
                .accept_call_signal(&member_signal, now)
                .expect("first signal should be accepted")
        );
        assert!(
            !store
                .accept_call_signal(&member_signal, now)
                .expect("replay should be ignored")
        );
        let outsider_signal = CallSignal::create(
            &outsider,
            community_id,
            Uuid::new_v4(),
            1,
            CallSignalKind::Offer,
            "v=0".into(),
            now,
        )
        .expect("outsider signal should be signed");
        assert!(matches!(
            store.accept_call_signal(&outsider_signal, now),
            Err(StoreError::UnauthorizedAuthor)
        ));
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn call_signals_pruning_and_member_revocation_works() {
        use nexo_core::{CallSignalKind, current_timestamp};

        let path = std::env::temp_dir().join(format!("nexo-prune-{}.sqlite3", Uuid::new_v4()));
        let member = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let now = current_timestamp();
        let mut store = LocalStore::open(&path).expect("store should open");
        store
            .create_community(community_id, "Seguranca", now)
            .expect("community should be created");
        store
            .authorize_member(community_id, &member.public_key_bytes(), now)
            .expect("member should be authorized");

        let signal = CallSignal::create(
            &member,
            community_id,
            Uuid::new_v4(),
            1,
            CallSignalKind::Offer,
            "v=0".into(),
            now,
        )
        .expect("signal should be signed");

        assert!(
            store
                .accept_call_signal(&signal, now)
                .expect("signal should be accepted")
        );
        // Prune older than now + 10s should delete the recorded signal
        let pruned = store
            .prune_old_call_signals(now + 10)
            .expect("prune should succeed");
        assert_eq!(pruned, 1);

        // Revoke member
        assert!(
            store
                .revoke_member(community_id, &member.public_key_bytes())
                .expect("revocation should succeed")
        );
        assert!(
            !store
                .is_authorized_member(community_id, &member.public_key_bytes())
                .expect("member should not be authorized")
        );
        assert!(
            store
                .is_revoked_member(community_id, &member.public_key_bytes())
                .expect("revocation marker should persist")
        );
        assert!(
            store
                .authorize_member(community_id, &member.public_key_bytes(), now)
                .is_err()
        );

        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_transfer_offer_and_chunk_tracking_works() {
        use nexo_core::{FileTransferOffer, compute_sha256, current_timestamp};

        let path = std::env::temp_dir().join(format!("nexo-files-{}.sqlite3", Uuid::new_v4()));
        let member = DeviceIdentity::generate();
        let community_id = Uuid::new_v4();
        let now = current_timestamp();
        let mut store = LocalStore::open(&path).expect("store should open");
        let community = store
            .create_community(community_id, "Arquivos", now)
            .expect("community should be created");
        store
            .authorize_member(community_id, &member.public_key_bytes(), now)
            .expect("member should be authorized");

        let content = b"Sample binary payload transferred chunk by chunk";
        let root_hash = compute_sha256(content);

        let offer = FileTransferOffer::create(
            &member,
            community_id,
            community.default_channel_id,
            "project_spec.pdf".into(),
            content.len() as u64,
            "application/pdf".into(),
            root_hash,
            now,
        )
        .expect("offer should be created");

        assert!(
            store
                .record_file_offer(&offer, Some("/tmp/project_spec.pdf"), "completed", now)
                .expect("offer should be saved")
        );

        let fetched = store
            .file_transfer(offer.id)
            .expect("fetch should succeed")
            .expect("transfer should exist");

        assert_eq!(fetched.file_name, "project_spec.pdf");
        assert_eq!(fetched.file_size, content.len() as u64);
        assert_eq!(fetched.status, "completed");

        // Test chunk recording
        let count = store
            .record_chunk_received(offer.id, 0)
            .expect("chunk recorded");
        assert_eq!(count, 1);

        let list = store
            .file_transfers_in_channel(community.default_channel_id)
            .expect("channel file list should succeed");
        assert_eq!(list.len(), 1);

        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn multiple_channels_text_and_voice_management_works() {
        use nexo_core::current_timestamp;

        let path = std::env::temp_dir().join(format!("nexo-channels-{}.sqlite3", Uuid::new_v4()));
        let community_id = Uuid::new_v4();
        let now = current_timestamp();
        let mut store = LocalStore::open(&path).expect("store should open");
        let community = store
            .create_community(community_id, "Comunidade Multicanais", now)
            .expect("community should be created");

        // 1. Initial channels contains default text channel "geral"
        let initial_channels = store
            .channels(community.id)
            .expect("channels list succeeds");
        assert_eq!(initial_channels.len(), 1);
        assert_eq!(initial_channels[0].name, "geral");
        assert_eq!(initial_channels[0].kind, ChannelKind::Text);

        // 2. Add another text channel and two voice channels
        let c_anuncios = store
            .create_channel(community.id, "anuncios", ChannelKind::Text)
            .expect("anuncios created");
        let c_anuncios_again = store
            .create_channel(community.id, "Anuncios", ChannelKind::Voice)
            .expect("existing channel creation is idempotent");
        assert_eq!(c_anuncios.id, c_anuncios_again.id);
        assert_eq!(c_anuncios_again.kind, ChannelKind::Voice);
        let c_voz1 = store
            .create_channel(community.id, "Sala de Voz 1", ChannelKind::Voice)
            .expect("voz 1 created");
        let c_voz2 = store
            .create_channel(community.id, "Sala de Jogos", ChannelKind::Voice)
            .expect("voz 2 created");

        assert_eq!(c_anuncios.kind, ChannelKind::Text);
        assert_eq!(c_voz1.kind, ChannelKind::Voice);
        assert_eq!(c_voz2.kind, ChannelKind::Voice);

        let all_channels = store
            .channels(community.id)
            .expect("channels list succeeds");
        assert_eq!(all_channels.len(), 4);

        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn custom_channel_ids_converge_across_independent_stores() {
        let mut first = LocalStore::open(Path::new(":memory:")).expect("first store opens");
        let mut second = LocalStore::open(Path::new(":memory:")).expect("second store opens");
        let community_id = Uuid::new_v4();
        first
            .create_community(community_id, "Convergencia", 100)
            .expect("first community is created");
        second
            .create_community(community_id, "Convergencia", 100)
            .expect("second community is created");

        let first_channel = first
            .create_channel(community_id, "Anuncios", ChannelKind::Text)
            .expect("first channel is created");
        let second_channel = second
            .create_channel(community_id, "anuncios", ChannelKind::Text)
            .expect("second channel is created");

        assert_eq!(first_channel.id, second_channel.id);
        assert_eq!(first_channel.name, "Anuncios");
        assert_eq!(second_channel.name, "anuncios");
    }
}
