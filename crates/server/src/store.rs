//! Persistence: PostgreSQL in production, an in-memory store for tests and
//! `--ephemeral` runs.

use std::sync::Arc;

use parking_lot::Mutex;
use qchat_proto::{Channel, ChannelId, ChannelKind, ChatMessage, MessageId};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

#[derive(Clone)]
pub enum Store {
    Postgres(PgPool),
    Memory(Arc<Mutex<MemoryStore>>),
}

#[derive(Default)]
pub struct MemoryStore {
    channels: Vec<Channel>,
    messages: Vec<ChatMessage>,
    next_id: MessageId,
}

fn kind_to_i16(kind: ChannelKind) -> i16 {
    match kind {
        ChannelKind::Text => 0,
        ChannelKind::Voice => 1,
    }
}

fn kind_from_i16(v: i16) -> StoreResult<ChannelKind> {
    match v {
        0 => Ok(ChannelKind::Text),
        1 => Ok(ChannelKind::Voice),
        other => Err(StoreError::Corrupt(format!("unknown channel kind {other}"))),
    }
}

impl Store {
    /// Connects to Postgres and applies pending migrations.
    pub async fn connect_postgres(url: &str) -> StoreResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(url)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self::Postgres(pool))
    }

    pub fn memory() -> Self {
        Self::Memory(Arc::new(Mutex::new(MemoryStore {
            next_id: 1,
            ..Default::default()
        })))
    }

    /// Creates the default channels if none exist.
    pub async fn seed_defaults(&self) -> StoreResult<()> {
        if self.list_channels().await?.is_empty() {
            self.create_channel("general", ChannelKind::Text).await?;
            self.create_channel("Lounge", ChannelKind::Voice).await?;
        }
        Ok(())
    }

    pub async fn list_channels(&self) -> StoreResult<Vec<Channel>> {
        match self {
            Store::Postgres(pool) => {
                let rows = sqlx::query(
                    "SELECT id, name, kind, position FROM channels ORDER BY position, created_at",
                )
                .fetch_all(pool)
                .await?;
                rows.into_iter()
                    .map(|r| {
                        Ok(Channel {
                            id: ChannelId(r.try_get::<Uuid, _>("id")?),
                            name: r.try_get("name")?,
                            kind: kind_from_i16(r.try_get("kind")?)?,
                            position: r.try_get("position")?,
                        })
                    })
                    .collect()
            }
            Store::Memory(m) => Ok(m.lock().channels.clone()),
        }
    }

    pub async fn create_channel(&self, name: &str, kind: ChannelKind) -> StoreResult<Channel> {
        let id = ChannelId(Uuid::new_v4());
        match self {
            Store::Postgres(pool) => {
                let position: i32 = sqlx::query_scalar(
                    "INSERT INTO channels (id, name, kind, position)
                     VALUES ($1, $2, $3, (SELECT COALESCE(MAX(position) + 1, 0) FROM channels))
                     RETURNING position",
                )
                .bind(id.0)
                .bind(name)
                .bind(kind_to_i16(kind))
                .fetch_one(pool)
                .await?;
                Ok(Channel {
                    id,
                    name: name.to_owned(),
                    kind,
                    position,
                })
            }
            Store::Memory(m) => {
                let mut m = m.lock();
                let position = i32::try_from(m.channels.len()).unwrap_or(i32::MAX);
                let ch = Channel {
                    id,
                    name: name.to_owned(),
                    kind,
                    position,
                };
                m.channels.push(ch.clone());
                Ok(ch)
            }
        }
    }

    pub async fn insert_message(
        &self,
        channel: ChannelId,
        author: &str,
        content: &str,
    ) -> StoreResult<ChatMessage> {
        match self {
            Store::Postgres(pool) => {
                let row = sqlx::query(
                    "INSERT INTO messages (channel_id, author_name, content)
                     VALUES ($1, $2, $3)
                     RETURNING id, created_at",
                )
                .bind(channel.0)
                .bind(author)
                .bind(content)
                .fetch_one(pool)
                .await?;
                let created: chrono::DateTime<chrono::Utc> = row.try_get("created_at")?;
                Ok(ChatMessage {
                    id: row.try_get("id")?,
                    channel,
                    author: author.to_owned(),
                    content: content.to_owned(),
                    sent_at_ms: created.timestamp_millis(),
                })
            }
            Store::Memory(m) => {
                let mut m = m.lock();
                let msg = ChatMessage {
                    id: m.next_id,
                    channel,
                    author: author.to_owned(),
                    content: content.to_owned(),
                    sent_at_ms: chrono::Utc::now().timestamp_millis(),
                };
                m.next_id += 1;
                m.messages.push(msg.clone());
                Ok(msg)
            }
        }
    }

    /// Returns up to `limit` messages older than `before`, in ascending id order.
    pub async fn history(
        &self,
        channel: ChannelId,
        before: Option<MessageId>,
        limit: u16,
    ) -> StoreResult<Vec<ChatMessage>> {
        let before = before.unwrap_or(MessageId::MAX);
        match self {
            Store::Postgres(pool) => {
                let rows = sqlx::query(
                    "SELECT id, author_name, content, created_at FROM messages
                     WHERE channel_id = $1 AND id < $2
                     ORDER BY id DESC LIMIT $3",
                )
                .bind(channel.0)
                .bind(before)
                .bind(i64::from(limit))
                .fetch_all(pool)
                .await?;
                let mut out = rows
                    .into_iter()
                    .map(|r| {
                        let created: chrono::DateTime<chrono::Utc> = r.try_get("created_at")?;
                        Ok(ChatMessage {
                            id: r.try_get("id")?,
                            channel,
                            author: r.try_get("author_name")?,
                            content: r.try_get("content")?,
                            sent_at_ms: created.timestamp_millis(),
                        })
                    })
                    .collect::<StoreResult<Vec<_>>>()?;
                out.reverse();
                Ok(out)
            }
            Store::Memory(m) => {
                let m = m.lock();
                let mut out: Vec<_> = m
                    .messages
                    .iter()
                    .rev()
                    .filter(|msg| msg.channel == channel && msg.id < before)
                    .take(limit as usize)
                    .cloned()
                    .collect();
                out.reverse();
                Ok(out)
            }
        }
    }
}
