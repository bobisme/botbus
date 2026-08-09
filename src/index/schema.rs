use anyhow::{Context, Result};
use rusqlite::Connection;

/// SQL schema for the FTS index.
pub const SCHEMA: &str = r#"
-- Messages FTS table (stores content for search results)
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    id,
    channel,
    agent,
    body,
    ts
);

-- Sync state tracking
CREATE TABLE IF NOT EXISTS sync_state (
    channel TEXT PRIMARY KEY,
    offset INTEGER NOT NULL DEFAULT 0,
    last_sync TEXT NOT NULL
);

-- Reply edges: one row per message that answers another.
--
-- Messages without a reply anchor are not stored, so a flat channel adds
-- nothing here and the table stays proportional to the replies, not to the
-- traffic. `reply_to` carries the index because the hot question is "who
-- answered X", asked by `rite wait --reply-to` on startup and by the TUI when
-- it draws reply counts. `id` is the primary key, which answers the reverse
-- ("what does X answer") for free while making re-indexing the same message
-- idempotent.
--
-- Derived and rebuildable like every other table here: `rite index rebuild`
-- clears it and replays every channel.
CREATE TABLE IF NOT EXISTS message_replies (
    id TEXT PRIMARY KEY,
    reply_to TEXT NOT NULL,
    channel TEXT NOT NULL,
    ts TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_message_replies_parent
    ON message_replies (reply_to, id);
"#;

/// Initialize the database schema.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)
        .with_context(|| "Failed to initialize FTS schema")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_schema() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Verify tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"messages_fts".to_string()));
        assert!(tables.contains(&"sync_state".to_string()));
        assert!(tables.contains(&"message_replies".to_string()));
    }

    /// `init_schema` runs on every open, including against a database created
    /// before `message_replies` existed. It must add the table instead of
    /// failing, or an upgrade would take the index offline.
    #[test]
    fn test_init_schema_is_idempotent_and_upgrades_old_databases() {
        let conn = Connection::open_in_memory().unwrap();

        // A pre-threading index: FTS and sync state only.
        conn.execute_batch(
            r#"
            CREATE VIRTUAL TABLE messages_fts USING fts5(id, channel, agent, body, ts);
            CREATE TABLE sync_state (
                channel TEXT PRIMARY KEY,
                offset INTEGER NOT NULL DEFAULT 0,
                last_sync TEXT NOT NULL
            );
            "#,
        )
        .unwrap();

        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_replies", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(indexes.contains(&"idx_message_replies_parent".to_string()));
    }
}
