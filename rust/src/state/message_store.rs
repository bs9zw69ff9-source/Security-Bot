//! Message store (`message_store` table).
//!
//! Discord never sends a message's content with its delete event, and the
//! gateway cache that used to fill that gap is empty after every restart, so
//! a message sent before a restart and deleted after it logged as "content not
//! cached". Every guild message is written here as it arrives instead, kept
//! current through edits, and read back when it is deleted or edited.
//!
//! Rows are pruned by age so the file doesn't grow forever: live messages
//! after `LIVE_RETENTION_DAYS`, deleted ones (the evidence) after
//! `DELETED_RETENTION_DAYS`.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serenity::model::channel::{Attachment, Message};

use crate::common::db;

pub const LIVE_RETENTION_DAYS: i64 = 14;
pub const DELETED_RETENTION_DAYS: i64 = 30;

const DAY_MS: i64 = 86_400_000;

/// An attachment as it was when the message was stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAttachment {
    pub filename: String,
    pub url: String,
    pub size: u32,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl From<&Attachment> for StoredAttachment {
    fn from(a: &Attachment) -> Self {
        StoredAttachment {
            filename: a.filename.clone(),
            url: a.url.clone(),
            size: a.size,
            content_type: a.content_type.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub message_id: u64,
    pub guild_id: u64,
    pub channel_id: u64,
    pub author_id: u64,
    pub author_tag: String,
    pub author_avatar: String,
    pub content: String,
    pub attachments: Vec<StoredAttachment>,
    pub created_at: i64,
    pub deleted_at: Option<i64>,
}

impl StoredMessage {
    pub fn from_message(m: &Message, guild_id: u64) -> Self {
        StoredMessage {
            message_id: m.id.get(),
            guild_id,
            channel_id: m.channel_id.get(),
            author_id: m.author.id.get(),
            author_tag: m.author.tag(),
            author_avatar: m.author.face(),
            content: m.content.clone(),
            attachments: m.attachments.iter().map(StoredAttachment::from).collect(),
            created_at: m.timestamp.unix_timestamp() * 1000,
            deleted_at: None,
        }
    }
}

pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_store (
            message_id    INTEGER PRIMARY KEY,
            guild_id      INTEGER NOT NULL,
            channel_id    INTEGER NOT NULL,
            author_id     INTEGER NOT NULL,
            author_tag    TEXT NOT NULL,
            author_avatar TEXT NOT NULL,
            content       TEXT NOT NULL,
            attachments   TEXT NOT NULL,
            created_at    INTEGER NOT NULL,
            deleted_at    INTEGER
        );
        CREATE INDEX IF NOT EXISTS message_store_created ON message_store (created_at);
        CREATE INDEX IF NOT EXISTS message_store_deleted ON message_store (deleted_at);
        CREATE INDEX IF NOT EXISTS message_store_guild ON message_store (guild_id, message_id);",
    )
}

// Snowflakes are well inside i64, which is what SQLite stores integers as.
fn sql_id(id: u64) -> i64 {
    id as i64
}

fn insert(conn: &Connection, m: &StoredMessage) -> rusqlite::Result<()> {
    let attachments = serde_json::to_string(&m.attachments).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT INTO message_store
            (message_id, guild_id, channel_id, author_id, author_tag, author_avatar, content, attachments, created_at, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(message_id) DO UPDATE SET
            content = excluded.content,
            attachments = excluded.attachments,
            author_tag = excluded.author_tag,
            author_avatar = excluded.author_avatar",
        params![
            sql_id(m.message_id),
            sql_id(m.guild_id),
            sql_id(m.channel_id),
            sql_id(m.author_id),
            m.author_tag,
            m.author_avatar,
            m.content,
            attachments,
            m.created_at,
            m.deleted_at,
        ],
    )?;
    Ok(())
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    let attachments: String = row.get(7)?;
    Ok(StoredMessage {
        message_id: row.get::<_, i64>(0)? as u64,
        guild_id: row.get::<_, i64>(1)? as u64,
        channel_id: row.get::<_, i64>(2)? as u64,
        author_id: row.get::<_, i64>(3)? as u64,
        author_tag: row.get(4)?,
        author_avatar: row.get(5)?,
        content: row.get(6)?,
        attachments: serde_json::from_str(&attachments).unwrap_or_default(),
        created_at: row.get(8)?,
        deleted_at: row.get(9)?,
    })
}

const COLUMNS: &str =
    "message_id, guild_id, channel_id, author_id, author_tag, author_avatar, content, attachments, created_at, deleted_at";

// Every lookup is scoped to the guild the event came from. Message ids are
// globally unique, so this never changes an answer; it makes it impossible for
// one guild's event to read back another guild's message.
fn get_in(conn: &Connection, guild_id: u64, message_id: u64) -> Option<StoredMessage> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM message_store WHERE message_id = ?1 AND guild_id = ?2"),
        params![sql_id(message_id), sql_id(guild_id)],
        row_to_message,
    )
    .optional()
    .unwrap_or(None)
}

fn update_content_in(conn: &Connection, guild_id: u64, message_id: u64, content: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE message_store SET content = ?3 WHERE message_id = ?1 AND guild_id = ?2",
        params![sql_id(message_id), sql_id(guild_id), content],
    )
}

fn mark_deleted_in(conn: &Connection, guild_id: u64, message_id: u64, now: i64) -> Option<StoredMessage> {
    if let Err(e) = conn.execute(
        "UPDATE message_store SET deleted_at = ?3 WHERE message_id = ?1 AND guild_id = ?2 AND deleted_at IS NULL",
        params![sql_id(message_id), sql_id(guild_id), now],
    ) {
        eprintln!("⚠️ [{guild_id}] couldn't mark message {message_id} deleted: {e}");
    }
    get_in(conn, guild_id, message_id)
}

fn prune_in(conn: &Connection, now: i64) -> usize {
    let live_cutoff = now - LIVE_RETENTION_DAYS * DAY_MS;
    let deleted_cutoff = now - DELETED_RETENTION_DAYS * DAY_MS;
    conn.execute(
        "DELETE FROM message_store
         WHERE (deleted_at IS NULL AND created_at < ?1)
            OR (deleted_at IS NOT NULL AND deleted_at < ?2)",
        params![live_cutoff, deleted_cutoff],
    )
    .unwrap_or(0)
}

/// Record a new message.
pub fn store(m: &StoredMessage) {
    db::with_conn(|conn| {
        if let Err(e) = insert(conn, m) {
            eprintln!("⚠️ couldn't store message {}: {e}", m.message_id);
        }
    });
}

/// Look a message up.
pub fn get(guild_id: u64, message_id: u64) -> Option<StoredMessage> {
    db::with_conn(|conn| get_in(conn, guild_id, message_id))
}

/// Keep the stored copy current after an edit.
pub fn update_content(guild_id: u64, message_id: u64, content: &str) {
    db::with_conn(|conn| {
        if let Err(e) = update_content_in(conn, guild_id, message_id, content) {
            eprintln!("⚠️ [{guild_id}] couldn't update stored message {message_id}: {e}");
        }
    });
}

/// Stamp a message as deleted (so it is kept for the longer retention) and
/// return what it said.
pub fn mark_deleted(guild_id: u64, message_id: u64) -> Option<StoredMessage> {
    let now = crate::common::config::now_ms();
    db::with_conn(|conn| mark_deleted_in(conn, guild_id, message_id, now))
}

/// Drop rows past their retention. Returns how many went.
pub fn prune() -> usize {
    let now = crate::common::config::now_ms();
    db::with_conn(|conn| prune_in(conn, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        create_schema(&c).unwrap();
        c
    }

    fn msg(id: u64, content: &str, created_at: i64) -> StoredMessage {
        StoredMessage {
            message_id: id,
            guild_id: 10,
            channel_id: 20,
            author_id: 30,
            author_tag: "someone".into(),
            author_avatar: "https://cdn.example/a.png".into(),
            content: content.into(),
            attachments: vec![StoredAttachment {
                filename: "cat.png".into(),
                url: "https://cdn.example/cat.png".into(),
                size: 2048,
                content_type: Some("image/png".into()),
            }],
            created_at,
            deleted_at: None,
        }
    }

    #[test]
    fn a_message_survives_a_round_trip() {
        let c = conn();
        let m = msg(1_234_567_890_123_456_789, "hello", 1_000);
        insert(&c, &m).unwrap();
        assert_eq!(get_in(&c, 10, m.message_id), Some(m));
    }

    #[test]
    fn edits_replace_the_content_and_deletes_are_stamped() {
        let c = conn();
        insert(&c, &msg(1, "before", 1_000)).unwrap();
        update_content_in(&c, 10, 1, "after").unwrap();
        let deleted = mark_deleted_in(&c, 10, 1, 5_000).unwrap();
        assert_eq!(deleted.content, "after");
        assert_eq!(deleted.deleted_at, Some(5_000));
        // A second delete event doesn't move the stamp.
        assert_eq!(mark_deleted_in(&c, 10, 1, 9_000).unwrap().deleted_at, Some(5_000));
    }

    #[test]
    fn unknown_messages_are_none() {
        let c = conn();
        assert_eq!(mark_deleted_in(&c, 10, 42, 1), None);
    }

    #[test]
    fn pruning_keeps_deleted_messages_longer() {
        let c = conn();
        let now = 100 * DAY_MS;
        insert(&c, &msg(1, "old live", now - (LIVE_RETENTION_DAYS + 1) * DAY_MS)).unwrap();
        insert(&c, &msg(2, "recent live", now - DAY_MS)).unwrap();
        insert(&c, &msg(3, "old but deleted recently", now - (LIVE_RETENTION_DAYS + 1) * DAY_MS)).unwrap();
        mark_deleted_in(&c, 10, 3, now - DAY_MS);
        insert(&c, &msg(4, "deleted long ago", now - 60 * DAY_MS)).unwrap();
        mark_deleted_in(&c, 10, 4, now - (DELETED_RETENTION_DAYS + 1) * DAY_MS);

        assert_eq!(prune_in(&c, now), 2);
        assert!(get_in(&c, 10, 1).is_none());
        assert!(get_in(&c, 10, 2).is_some());
        assert!(get_in(&c, 10, 3).is_some());
        assert!(get_in(&c, 10, 4).is_none());
    }

    /// An event from another guild can't read, edit or delete this guild's
    /// stored message, even given its id.
    #[test]
    fn lookups_are_scoped_to_the_guild() {
        let c = conn();
        insert(&c, &msg(7, "guild ten's secret", 1_000)).unwrap();
        assert!(get_in(&c, 99, 7).is_none());
        assert_eq!(update_content_in(&c, 99, 7, "overwritten").unwrap(), 0);
        assert!(mark_deleted_in(&c, 99, 7, 5_000).is_none());
        let still = get_in(&c, 10, 7).unwrap();
        assert_eq!(still.content, "guild ten's secret");
        assert_eq!(still.deleted_at, None);
    }
}
