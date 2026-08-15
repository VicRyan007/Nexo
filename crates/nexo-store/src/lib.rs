use std::path::Path;

use nexo_core::{
    CallSignal, CommunityCredential, FileTransferOffer, MessageError, SignedMessage,
    community_sync_token,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use thiserror::Error;
use uuid::Uuid;

const DEFAULT_CHANNEL_NAME: &str = "geral";
const DEFAULT_CHANNEL_NAMESPACE: Uuid = Uuid::from_u128(0x3a6b_9561_66fd_4f9e_8bb4_1cf2_e033_ea97);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Community {
    pub id: Uuid,
    pub name: String,
    pub default_channel_id: Uuid,
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
             CREATE TABLE IF NOT EXISTS call_signals_seen (
                 id TEXT PRIMARY KEY NOT NULL,
                 community_id TEXT NOT NULL,
                 call_id TEXT NOT NULL,
                 author_key BLOB NOT NULL,
                 sequence INTEGER NOT NULL,
                 received_at INTEGER NOT NULL
             );
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

    pub fn authorize_member(
        &self,
        community_id: Uuid,
        public_key: &[u8; 32],
        authorized_at: u64,
    ) -> Result<(), StoreError> {
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
            "DELETE FROM credentials WHERE community_id = ?1 AND member_key = ?2",
            params![community_id.to_string(), member_key],
        )?;
        Ok(deleted_member > 0)
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
        let community = self
            .community(community_id)?
            .ok_or_else(|| StoreError::InvalidData("unknown community".to_owned()))?;
        let fetch = limit.min(500).saturating_add(1);
        let fetch =
            i64::try_from(fetch).map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let mut statement = self.connection.prepare(
            "SELECT id, version, community_id, channel_id, author_key, body, created_at, signature
             FROM messages m
             WHERE channel_id = ?1 AND NOT EXISTS (
                 SELECT 1 FROM sync_deliveries d
                 WHERE d.peer_id = ?2 AND d.receiver_epoch = ?3
                   AND d.community_id = ?4 AND d.message_id = m.id
             )
             ORDER BY created_at, id LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                community.default_channel_id.to_string(),
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

    pub fn import_messages_accepted(
        &self,
        community_id: Uuid,
        messages: &[SignedMessage],
        now: u64,
    ) -> Result<(Vec<Uuid>, usize), StoreError> {
        let mut accepted = Vec::new();
        let mut inserted = 0;
        for message in messages {
            if message.community_id != community_id {
                continue;
            }
            match self.insert_message(message, now) {
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
        message.verify(now)?;
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
    use nexo_core::{CommunityCredential, DeviceIdentity, NetworkInvite};

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
}
