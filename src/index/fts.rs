use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;

use super::schema::init_schema;
use crate::core::message::Message;

/// Escape a string for safe use in FTS5 queries.
///
/// FTS5 has special characters that need escaping:
/// - Double quotes: used for phrase queries
/// - Asterisk: used for prefix queries
/// - Parentheses: used for grouping
/// - AND, OR, NOT: boolean operators
/// - NEAR: proximity operator
/// - Colon: column filter
///
/// We wrap the term in double quotes and escape any internal quotes.
fn escape_fts5_term(term: &str) -> String {
    // Escape double quotes by doubling them, then wrap in quotes
    let escaped = term.replace('"', "\"\"");
    format!("\"{}\"", escaped)
}

/// A search result from the FTS index.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub channel: String,
    pub agent: String,
    pub body: String,
    pub ts: String,
    pub rank: f64,
}

/// A reply edge read back out of the index.
#[derive(Debug, Clone, Serialize)]
pub struct ReplyEdge {
    /// The replying message.
    pub id: String,
    /// The message it answers.
    pub reply_to: String,
    pub channel: String,
    pub ts: String,
}

/// Full-text search index backed by SQLite FTS5.
pub struct SearchIndex {
    conn: Connection,
}

impl SearchIndex {
    /// Open or create a search index at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open index: {}", path.display()))?;

        // Enable WAL mode for better concurrency
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .with_context(|| "Failed to set WAL mode")?;

        init_schema(&conn)?;

        Ok(Self { conn })
    }

    /// Create an in-memory index (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().with_context(|| "Failed to open in-memory index")?;

        init_schema(&conn)?;

        Ok(Self { conn })
    }

    /// Index a message.
    pub fn index_message(&self, msg: &Message) -> Result<()> {
        let id = msg.id.to_string();
        let ts = msg.ts.to_rfc3339();

        self.conn
            .execute(
                "INSERT OR REPLACE INTO messages_fts (id, channel, agent, body, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, msg.channel, msg.agent, msg.body, ts],
            )
            .with_context(|| "Failed to insert into FTS")?;

        index_reply_edge(&self.conn, msg)?;

        Ok(())
    }

    /// Index multiple messages in a transaction.
    pub fn index_messages(&mut self, messages: &[Message]) -> Result<usize> {
        if messages.is_empty() {
            return Ok(0);
        }

        let tx = self.conn.transaction()?;

        for msg in messages {
            let id = msg.id.to_string();
            let ts = msg.ts.to_rfc3339();

            tx.execute(
                "INSERT OR REPLACE INTO messages_fts (id, channel, agent, body, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, msg.channel, msg.agent, msg.body, ts],
            )?;

            index_reply_edge(&tx, msg)?;
        }

        tx.commit()?;

        Ok(messages.len())
    }

    /// Direct replies to `parent_id`, oldest first.
    ///
    /// ULIDs sort chronologically, so ordering by `id` is creation order and
    /// needs no timestamp comparison across machines.
    ///
    /// This is the startup half of `rite wait --reply-to`: the tail of a
    /// channel catches replies that arrive after the wait begins, and this
    /// catches the one that arrived a moment before it, without reading a
    /// single channel file.
    pub fn replies_to(&self, parent_id: &str) -> Result<Vec<ReplyEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, reply_to, channel, ts FROM message_replies WHERE reply_to = ?1 ORDER BY id",
        )?;

        let replies = stmt
            .query_map(params![parent_id], |row| {
                Ok(ReplyEdge {
                    id: row.get(0)?,
                    reply_to: row.get(1)?,
                    channel: row.get(2)?,
                    ts: row.get(3)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(replies)
    }

    /// How many messages answer `parent_id`.
    pub fn reply_count(&self, parent_id: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM message_replies WHERE reply_to = ?1",
            params![parent_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// The message `id` answers, if the index knows of one.
    ///
    /// `None` means either "not a reply" or "not indexed yet". Callers that
    /// must tell those apart read the JSONL, which is the source of truth.
    pub fn parent_of(&self, id: &str) -> Result<Option<String>> {
        let parent: Option<String> = self
            .conn
            .query_row(
                "SELECT reply_to FROM message_replies WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .ok();
        Ok(parent)
    }

    /// Total reply edges held by the index.
    pub fn reply_edge_count(&self) -> Result<usize> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM message_replies", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Search for messages matching a query.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, channel, agent, body, ts, bm25(messages_fts) as rank
            FROM messages_fts
            WHERE messages_fts MATCH ?1
            ORDER BY rank
            LIMIT ?2
            "#,
        )?;

        let results = stmt
            .query_map(params![query, limit as i64], |row| {
                Ok(SearchResult {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    agent: row.get(2)?,
                    body: row.get(3)?,
                    ts: row.get(4)?,
                    rank: row.get(5)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Search within a specific channel.
    pub fn search_channel(
        &self,
        query: &str,
        channel: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        // Combine query with channel filter using FTS5 AND syntax
        // Escape the channel name to prevent FTS5 injection
        let fts_query = format!("{} AND channel:{}", query, escape_fts5_term(channel));
        self.search(&fts_query, limit)
    }

    /// Search messages from a specific agent.
    pub fn search_from(&self, query: &str, agent: &str, limit: usize) -> Result<Vec<SearchResult>> {
        // Combine query with agent filter using FTS5 AND syntax
        // Escape the agent name to prevent FTS5 injection
        let fts_query = format!("{} AND agent:{}", query, escape_fts5_term(agent));
        self.search(&fts_query, limit)
    }

    /// Search within a specific channel and from a specific agent.
    pub fn search_channel_from(
        &self,
        query: &str,
        channel: &str,
        agent: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let fts_query = format!(
            "{} AND channel:{} AND agent:{}",
            query,
            escape_fts5_term(channel),
            escape_fts5_term(agent)
        );
        self.search(&fts_query, limit)
    }

    /// Get sync offset for a channel.
    pub fn get_sync_offset(&self, channel: &str) -> Result<u64> {
        let offset: Option<i64> = self
            .conn
            .query_row(
                "SELECT offset FROM sync_state WHERE channel = ?1",
                params![channel],
                |row| row.get(0),
            )
            .ok();

        Ok(offset.unwrap_or(0) as u64)
    }

    /// Set sync offset for a channel.
    pub fn set_sync_offset(&self, channel: &str, offset: u64) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_state (channel, offset, last_sync) VALUES (?1, ?2, ?3)",
            params![channel, offset as i64, now],
        )?;
        Ok(())
    }

    /// Get the total number of indexed messages.
    pub fn message_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Delete a specific message from the index by its ULID ID.
    ///
    /// Drops the message's own reply edge as well. Edges *pointing at* it are
    /// left alone on purpose: its children are still real messages, and losing
    /// their anchor would turn them into roots, which is exactly the silent
    /// promotion the thread walk exists to prevent. They resolve as
    /// [`crate::core::thread::RootKind::MissingParent`] instead.
    pub fn delete_message(&self, id: &str) -> Result<bool> {
        let changes = self
            .conn
            .execute("DELETE FROM messages_fts WHERE id = ?1", params![id])
            .with_context(|| format!("Failed to delete message {} from FTS", id))?;

        self.conn
            .execute("DELETE FROM message_replies WHERE id = ?1", params![id])
            .with_context(|| format!("Failed to delete reply edge for {}", id))?;

        Ok(changes > 0)
    }

    /// Clear all messages from the index.
    pub fn clear(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM messages_fts", [])
            .with_context(|| "Failed to clear FTS index")?;
        self.conn
            .execute("DELETE FROM message_replies", [])
            .with_context(|| "Failed to clear reply edges")?;
        Ok(())
    }
}

/// Record `msg`'s reply edge, if it has one.
///
/// Uses [`Message::parent_id`], so a message that anchors to itself stores no
/// edge — a self-edge here would make `replies_to(x)` return `x` and spin any
/// consumer that walks the result.
fn index_reply_edge(conn: &Connection, msg: &Message) -> Result<()> {
    let Some(parent) = msg.parent_id() else {
        return Ok(());
    };

    conn.execute(
        "INSERT OR REPLACE INTO message_replies (id, reply_to, channel, ts) VALUES (?1, ?2, ?3, ?4)",
        params![
            msg.id.to_string(),
            parent.to_string(),
            msg.channel,
            msg.ts.to_rfc3339()
        ],
    )
    .with_context(|| format!("Failed to index reply edge for {}", msg.id))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ulid::Ulid;

    #[test]
    fn test_escape_fts5_term() {
        // Simple term
        assert_eq!(escape_fts5_term("hello"), "\"hello\"");

        // Term with double quotes
        assert_eq!(escape_fts5_term("say \"hello\""), "\"say \"\"hello\"\"\"");

        // Term with FTS5 operators (should be neutralized by quoting)
        assert_eq!(escape_fts5_term("foo AND bar"), "\"foo AND bar\"");
        assert_eq!(escape_fts5_term("foo OR bar"), "\"foo OR bar\"");
        assert_eq!(escape_fts5_term("NOT foo"), "\"NOT foo\"");

        // Term with special characters
        assert_eq!(escape_fts5_term("prefix*"), "\"prefix*\"");
        assert_eq!(escape_fts5_term("(grouped)"), "\"(grouped)\"");
        assert_eq!(escape_fts5_term("col:value"), "\"col:value\"");
    }

    fn make_message(channel: &str, agent: &str, body: &str) -> Message {
        Message {
            ts: Utc::now(),
            id: Ulid::new(),
            agent: agent.to_string(),
            channel: channel.to_string(),
            body: body.to_string(),
            mentions: vec![],
            labels: vec![],
            reply_to: None,
            attachments: vec![],
            meta: None,
        }
    }

    #[test]
    fn test_index_and_search() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        let messages = vec![
            make_message("general", "Alice", "Hello world"),
            make_message("general", "Bob", "Working on authentication"),
            make_message("backend", "Alice", "Fixed the bug in auth module"),
        ];

        index.index_messages(&messages).unwrap();

        // Search for "auth" in body field
        let results = index.search("body:auth*", 10).unwrap();
        assert_eq!(results.len(), 2);

        // Search in specific channel
        let results = index.search_channel("body:auth*", "backend", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].channel, "backend");
    }

    #[test]
    fn test_search_from_agent() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        let messages = vec![
            make_message("general", "Alice", "Hello from Alice"),
            make_message("general", "Bob", "Hello from Bob"),
        ];

        index.index_messages(&messages).unwrap();

        let results = index.search_from("body:Hello", "Alice", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent, "Alice");
    }

    #[test]
    fn test_search_channel_from_agent() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        let messages = vec![
            make_message("general", "Alice", "Investigating auth"),
            make_message("general", "Bob", "Investigating auth"),
            make_message("backend", "Alice", "Investigating auth"),
        ];

        index.index_messages(&messages).unwrap();

        let results = index
            .search_channel_from("body:Investigating", "general", "Alice", 10)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].channel, "general");
        assert_eq!(results[0].agent, "Alice");
    }

    #[test]
    fn test_sync_offset() {
        let index = SearchIndex::open_in_memory().unwrap();

        assert_eq!(index.get_sync_offset("general").unwrap(), 0);

        index.set_sync_offset("general", 1234).unwrap();
        assert_eq!(index.get_sync_offset("general").unwrap(), 1234);
    }

    #[test]
    fn test_reply_edges_are_queryable_by_parent() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        let parent = make_message("general", "Alice", "Review 42 please");
        let first = make_message("general", "Bob", "on it").with_reply_to(parent.id);
        let second = make_message("general", "Carol", "done").with_reply_to(parent.id);
        let unrelated = make_message("general", "Dave", "unrelated");

        index
            .index_messages(&[
                parent.clone(),
                first.clone(),
                second.clone(),
                unrelated.clone(),
            ])
            .unwrap();

        let replies = index.replies_to(&parent.id.to_string()).unwrap();
        assert_eq!(replies.len(), 2);
        // ULID order is creation order.
        let mut ids = vec![first.id.to_string(), second.id.to_string()];
        ids.sort();
        assert_eq!(
            replies.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            ids
        );
        assert_eq!(replies[0].channel, "general");

        assert_eq!(index.reply_count(&parent.id.to_string()).unwrap(), 2);
        assert_eq!(index.reply_count(&unrelated.id.to_string()).unwrap(), 0);

        assert_eq!(
            index.parent_of(&first.id.to_string()).unwrap(),
            Some(parent.id.to_string())
        );
        assert_eq!(index.parent_of(&unrelated.id.to_string()).unwrap(), None);

        // Only replies take a row.
        assert_eq!(index.reply_edge_count().unwrap(), 2);
    }

    #[test]
    fn test_reply_edge_indexing_is_idempotent() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        let parent = make_message("general", "Alice", "question");
        let reply = make_message("general", "Bob", "answer").with_reply_to(parent.id);

        index
            .index_messages(&[parent.clone(), reply.clone()])
            .unwrap();
        index
            .index_messages(&[parent.clone(), reply.clone()])
            .unwrap();
        index.index_message(&reply).unwrap();

        assert_eq!(index.reply_count(&parent.id.to_string()).unwrap(), 1);
    }

    #[test]
    fn test_self_referencing_message_stores_no_edge() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        let mut looped = make_message("general", "Alice", "me");
        looped.reply_to = Some(looped.id);

        index.index_messages(&[looped.clone()]).unwrap();

        assert!(index.replies_to(&looped.id.to_string()).unwrap().is_empty());
        assert_eq!(index.reply_edge_count().unwrap(), 0);
    }

    #[test]
    fn test_delete_drops_own_edge_but_keeps_children() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        let root = make_message("general", "Alice", "question");
        let middle = make_message("general", "Bob", "answer").with_reply_to(root.id);
        let leaf = make_message("general", "Carol", "follow-up").with_reply_to(middle.id);

        index
            .index_messages(&[root.clone(), middle.clone(), leaf.clone()])
            .unwrap();

        index.delete_message(&middle.id.to_string()).unwrap();

        // The deleted message no longer answers anything…
        assert!(index.replies_to(&root.id.to_string()).unwrap().is_empty());
        // …but its child keeps its anchor, so it stays a dangling child rather
        // than being promoted to a root.
        assert_eq!(
            index.parent_of(&leaf.id.to_string()).unwrap(),
            Some(middle.id.to_string())
        );
    }

    #[test]
    fn test_clear_drops_reply_edges() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        let parent = make_message("general", "Alice", "question");
        let reply = make_message("general", "Bob", "answer").with_reply_to(parent.id);
        index.index_messages(&[parent, reply]).unwrap();
        assert_eq!(index.reply_edge_count().unwrap(), 1);

        index.clear().unwrap();
        assert_eq!(index.reply_edge_count().unwrap(), 0);
    }

    #[test]
    fn test_message_count() {
        let mut index = SearchIndex::open_in_memory().unwrap();

        assert_eq!(index.message_count().unwrap(), 0);

        let messages = vec![
            make_message("general", "Alice", "One"),
            make_message("general", "Bob", "Two"),
        ];

        index.index_messages(&messages).unwrap();
        assert_eq!(index.message_count().unwrap(), 2);
    }
}
