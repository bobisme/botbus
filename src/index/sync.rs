use anyhow::{Context, Result};

use super::fts::SearchIndex;
use crate::core::message::{Message, MessageMeta};
use crate::core::project::{channels_dir, index_path};
use crate::storage::jsonl::read_records_from_offset;

/// Syncs JSONL logs to the FTS index.
pub struct IndexSyncer {
    index: SearchIndex,
}

impl IndexSyncer {
    /// Create a new syncer.
    pub fn new() -> Result<Self> {
        let idx_path = index_path();
        let index = SearchIndex::open(&idx_path)
            .with_context(|| format!("Failed to open index at {}", idx_path.display()))?;

        Ok(Self { index })
    }

    /// Get a reference to the underlying index.
    pub fn index(&self) -> &SearchIndex {
        &self.index
    }

    /// Get a mutable reference to the underlying index.
    pub fn index_mut(&mut self) -> &mut SearchIndex {
        &mut self.index
    }

    /// Sync all channels incrementally.
    pub fn sync_all(&mut self) -> Result<SyncStats> {
        let channels = channels_dir();

        if !channels.exists() {
            return Ok(SyncStats::default());
        }

        let mut stats = SyncStats::default();

        for entry in std::fs::read_dir(&channels)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Some(channel) = path.file_stem().and_then(|s| s.to_str())
            {
                match self.sync_channel(channel) {
                    Ok(count) => {
                        stats.messages_indexed += count;
                        stats.channels_synced += 1;
                    }
                    Err(e) => {
                        stats.errors.push(format!("{}: {}", channel, e));
                    }
                }
            }
        }

        Ok(stats)
    }

    /// Sync a specific channel incrementally.
    ///
    /// When encountering tombstone records, deletes the original message from the
    /// FTS index rather than indexing the tombstone itself.
    pub fn sync_channel(&mut self, channel: &str) -> Result<usize> {
        let path = channels_dir().join(format!("{}.jsonl", channel));

        if !path.exists() {
            return Ok(0);
        }

        let offset = self.index.get_sync_offset(channel)?;
        let (messages, new_offset): (Vec<Message>, u64) = read_records_from_offset(&path, offset)?;

        if messages.is_empty() {
            return Ok(0);
        }

        // Separate tombstones from regular messages
        let mut regular_messages = Vec::new();
        let mut deleted_ids = Vec::new();

        for msg in messages {
            if let Some(MessageMeta::Deleted { target_id, .. }) = &msg.meta {
                deleted_ids.push(target_id.to_string());
            } else {
                regular_messages.push(msg);
            }
        }

        // Delete tombstoned messages from FTS index
        for id in &deleted_ids {
            self.index.delete_message(id)?;
        }

        // Index remaining regular messages
        let count = self.index.index_messages(&regular_messages)?;
        self.index.set_sync_offset(channel, new_offset)?;

        Ok(count)
    }

    /// Rebuild the entire index from scratch.
    ///
    /// This performs a full rebuild:
    /// 1. Reads all messages from all JSONL files
    /// 2. Deduplicates by message ID (ULID)
    /// 3. Sorts by ULID (chronological order)
    /// 4. Clears existing FTS tables
    /// 5. Bulk inserts into FTS index using transactions
    pub fn rebuild(&mut self) -> Result<SyncStats> {
        use std::collections::HashMap;

        let channels = channels_dir();

        if !channels.exists() {
            return Ok(SyncStats::default());
        }

        let mut stats = SyncStats::default();
        let mut messages_by_id: HashMap<String, Message> = HashMap::new();

        // 1. Read all messages from all JSONL files
        for entry in std::fs::read_dir(&channels)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Some(channel) = path.file_stem().and_then(|s| s.to_str())
            {
                stats.channels_synced += 1;

                // Read all messages from this channel (with deletion filtering)
                match crate::core::message::read_messages(&path) {
                    Ok(messages) => {
                        // 2. Deduplicate by message ID
                        for msg in messages {
                            messages_by_id.insert(msg.id.to_string(), msg);
                        }
                    }
                    Err(e) => {
                        stats.errors.push(format!("{}: {}", channel, e));
                    }
                }
            }
        }

        // 3. Sort by ULID (chronological order)
        let mut messages: Vec<Message> = messages_by_id.into_values().collect();
        messages.sort_by_key(|m| m.id);

        // 4. Clear existing FTS tables
        self.index.clear()?;

        // 5. Bulk insert into FTS index
        let count = self.index.index_messages(&messages)?;
        stats.messages_indexed = count;

        // Update sync offsets to the end of each file
        for entry in std::fs::read_dir(&channels)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Some(channel) = path.file_stem().and_then(|s| s.to_str())
            {
                // Get the file size as the new offset
                let metadata = std::fs::metadata(&path)?;
                let offset = metadata.len();
                self.index.set_sync_offset(channel, offset)?;
            }
        }

        Ok(stats)
    }
}

/// Statistics from a sync operation.
#[derive(Debug, Default)]
pub struct SyncStats {
    pub channels_synced: usize,
    pub messages_indexed: usize,
    pub errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    // Most integration tests moved to tests/integration/ since they require
    // global data directory mocking. The reply-edge tests below need the same
    // mocking, and there is nowhere else that can reach `IndexSyncer::rebuild`
    // and the index in one process.

    use super::*;
    use crate::core::message::{Message, MessageMeta};
    use crate::core::project::{DATA_DIR_ENV_VAR, channel_path, ensure_data_dir};
    use crate::storage::jsonl::append_record;
    use chrono::Utc;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TestEnv {
        _dir: TempDir,
    }

    impl TestEnv {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            unsafe {
                env::set_var(DATA_DIR_ENV_VAR, dir.path());
            }
            ensure_data_dir().unwrap();
            Self { _dir: dir }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            unsafe {
                env::remove_var(DATA_DIR_ENV_VAR);
            }
        }
    }

    fn write(channel: &str, msg: &Message) {
        append_record(&channel_path(channel), msg).unwrap();
    }

    /// The index is derived, so a rebuild has to land on the same reply edges
    /// the incremental path produced. If it does not, `rite index rebuild`
    /// becomes a way to lose threads.
    #[test]
    #[serial]
    fn rebuild_reproduces_the_reply_edges_incremental_sync_built() {
        let _env = TestEnv::new();

        let question = Message::new("alice", "general", "who owns review 42?");
        let first = Message::new("bob", "general", "I do").with_reply_to(question.id);
        let second = Message::new("carol", "general", "so do I").with_reply_to(question.id);
        let nested = Message::new("dave", "general", "thanks").with_reply_to(first.id);
        let unrelated = Message::new("erin", "general", "different topic");
        let elsewhere = Message::new("frank", "backend", "deploying").with_reply_to(question.id);

        for msg in [&question, &first, &second, &nested, &unrelated] {
            write("general", msg);
        }
        write("backend", &elsewhere);

        let mut syncer = IndexSyncer::new().unwrap();
        syncer.sync_all().unwrap();

        let parent = question.id.to_string();
        let incremental: Vec<String> = syncer
            .index()
            .replies_to(&parent)
            .unwrap()
            .into_iter()
            .map(|edge| edge.id)
            .collect();
        assert_eq!(incremental.len(), 3, "two here, one from #backend");
        let incremental_total = syncer.index().reply_edge_count().unwrap();
        assert_eq!(incremental_total, 4, "only replies take a row");

        syncer.rebuild().unwrap();

        let rebuilt: Vec<String> = syncer
            .index()
            .replies_to(&parent)
            .unwrap()
            .into_iter()
            .map(|edge| edge.id)
            .collect();
        assert_eq!(rebuilt, incremental, "rebuild must reproduce the edges");
        assert_eq!(
            syncer.index().reply_edge_count().unwrap(),
            incremental_total
        );

        // The reverse direction survives too.
        assert_eq!(
            syncer.index().parent_of(&nested.id.to_string()).unwrap(),
            Some(first.id.to_string())
        );
        assert_eq!(
            syncer.index().parent_of(&unrelated.id.to_string()).unwrap(),
            None
        );

        // And a second rebuild is idempotent, not additive.
        syncer.rebuild().unwrap();
        assert_eq!(
            syncer.index().reply_edge_count().unwrap(),
            incremental_total
        );
    }

    /// A rebuild reads through `read_messages`, which drops tombstoned
    /// records. The deleted message must take its own edge with it, and leave
    /// its children's edges alone so they stay dangling rather than promoted.
    #[test]
    #[serial]
    fn rebuild_drops_edges_for_deleted_messages() {
        let _env = TestEnv::new();

        let question = Message::new("alice", "general", "question");
        let answer = Message::new("bob", "general", "answer").with_reply_to(question.id);
        let nested = Message::new("carol", "general", "follow-up").with_reply_to(answer.id);
        for msg in [&question, &answer, &nested] {
            write("general", msg);
        }

        let mut syncer = IndexSyncer::new().unwrap();
        syncer.sync_all().unwrap();
        assert_eq!(syncer.index().reply_edge_count().unwrap(), 2);

        // Tombstone the middle message.
        let tombstone =
            Message::new("alice", "general", "[message deleted]").with_meta(MessageMeta::Deleted {
                target_id: answer.id,
                deleted_by: "alice".to_string(),
                deleted_at: Utc::now(),
            });
        write("general", &tombstone);

        // Both paths must agree.
        syncer.sync_all().unwrap();
        let after_sync = (
            syncer
                .index()
                .reply_count(&question.id.to_string())
                .unwrap(),
            syncer.index().parent_of(&nested.id.to_string()).unwrap(),
        );

        syncer.rebuild().unwrap();
        let after_rebuild = (
            syncer
                .index()
                .reply_count(&question.id.to_string())
                .unwrap(),
            syncer.index().parent_of(&nested.id.to_string()).unwrap(),
        );

        assert_eq!(after_sync, after_rebuild);
        assert_eq!(after_rebuild.0, 0, "the deleted reply no longer answers");
        assert_eq!(
            after_rebuild.1,
            Some(answer.id.to_string()),
            "the child keeps its anchor and stays dangling"
        );
    }

    /// A channel that never uses threading must add nothing to the table.
    #[test]
    #[serial]
    fn a_flat_channel_stores_no_reply_edges() {
        let _env = TestEnv::new();

        for body in ["one", "two", "three"] {
            write("general", &Message::new("alice", "general", body));
        }

        let mut syncer = IndexSyncer::new().unwrap();
        syncer.sync_all().unwrap();
        assert_eq!(syncer.index().reply_edge_count().unwrap(), 0);

        syncer.rebuild().unwrap();
        assert_eq!(syncer.index().reply_edge_count().unwrap(), 0);
    }
}
