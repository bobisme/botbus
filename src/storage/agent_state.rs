//! Per-agent state storage.
//!
//! Each agent has its own state file at `.rite/agents/<AgentName>/state.json`
//! to avoid conflicts when multiple agents share a project.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Per-agent state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    /// Read cursor offsets per channel (byte offsets into JSONL files)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub read_offsets: HashMap<String, u64>,

    /// Last read message ID per channel (for robustness across compaction)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub last_read_ids: HashMap<String, String>,

    /// Last read timestamp per channel
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub last_read_times: HashMap<String, DateTime<Utc>>,

    /// Channels this agent is subscribed to
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscribed_channels: Vec<String>,

    /// Timestamp of the agent's most recent heartbeat.
    ///
    /// Written on every `rite` invocation that resolves this agent's
    /// identity (see `record_heartbeat` and `main.rs`), so it doubles as a
    /// "last seen alive" marker. Used by `crate::core::presence` to derive
    /// whether the agent is currently live, without the agent having to
    /// remember to set or clear anything itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat: Option<DateTime<Utc>>,
}

impl AgentState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Get the path to an agent's state file.
pub fn agent_state_path(project_root: &Path, agent_name: &str) -> PathBuf {
    project_root
        .join(".rite")
        .join("agents")
        .join(agent_name)
        .join("state.json")
}

/// Manager for per-agent state with file locking.
pub struct AgentStateManager {
    path: PathBuf,
}

impl AgentStateManager {
    /// Create a new manager for the given agent.
    pub fn new(project_root: &Path, agent_name: &str) -> Self {
        Self {
            path: agent_state_path(project_root, agent_name),
        }
    }

    /// Create from an explicit path (for testing).
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Load the agent's state from disk.
    pub fn load(&self) -> Result<AgentState> {
        if !self.path.exists() {
            return Ok(AgentState::default());
        }

        let file = File::open(&self.path)
            .with_context(|| format!("Failed to open agent state: {}", self.path.display()))?;

        file.lock_shared()
            .with_context(|| "Failed to acquire shared lock on agent state")?;

        let mut contents = String::new();
        let mut reader = std::io::BufReader::new(&file);
        reader
            .read_to_string(&mut contents)
            .with_context(|| "Failed to read agent state")?;

        if contents.trim().is_empty() {
            return Ok(AgentState::default());
        }

        let state: AgentState =
            serde_json::from_str(&contents).with_context(|| "Failed to parse agent state")?;

        Ok(state)
    }

    /// Save the agent's state to disk.
    pub fn save(&self, state: &AgentState) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path)
            .with_context(|| {
                format!(
                    "Failed to open agent state for writing: {}",
                    self.path.display()
                )
            })?;

        file.lock_exclusive()
            .with_context(|| "Failed to acquire exclusive lock on agent state")?;

        let mut file_ref = &file;
        file_ref.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;

        let json = serde_json::to_string_pretty(state)
            .with_context(|| "Failed to serialize agent state")?;

        let mut writer = std::io::BufWriter::new(&file);
        writer
            .write_all(json.as_bytes())
            .with_context(|| "Failed to write agent state")?;

        writer.flush()?;
        file.sync_all()?;

        Ok(())
    }

    /// Update the state atomically using a closure.
    ///
    /// Holds an exclusive lock across the entire read-modify-write operation
    /// to prevent race conditions between concurrent updates.
    pub fn update<F>(&self, f: F) -> Result<AgentState>
    where
        F: FnOnce(&mut AgentState),
    {
        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        // Open file with read+write, creating if needed
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .with_context(|| {
                format!(
                    "Failed to open agent state for update: {}",
                    self.path.display()
                )
            })?;

        // Acquire exclusive lock - held for entire read-modify-write
        file.lock_exclusive()
            .with_context(|| "Failed to acquire exclusive lock on agent state")?;

        // Read current contents
        let mut contents = String::new();
        let mut reader = std::io::BufReader::new(&file);
        reader
            .read_to_string(&mut contents)
            .with_context(|| "Failed to read agent state")?;

        // Parse state (default if empty/missing)
        let mut state: AgentState = if contents.trim().is_empty() {
            AgentState::default()
        } else {
            serde_json::from_str(&contents).with_context(|| "Failed to parse agent state")?
        };

        // Apply the update
        f(&mut state);

        // Serialize
        let json = serde_json::to_string_pretty(&state)
            .with_context(|| "Failed to serialize agent state")?;

        // Truncate file and write back (file position is at end after read)
        use std::io::Seek;
        let mut file_ref = &file;
        file_ref.seek(std::io::SeekFrom::Start(0))?;
        file.set_len(0)?;

        let mut writer = std::io::BufWriter::new(&file);
        writer
            .write_all(json.as_bytes())
            .with_context(|| "Failed to write agent state")?;

        writer.flush()?;
        file.sync_all()?;

        // Lock released on drop
        Ok(state)
    }

    /// Get the read offset for a channel.
    pub fn get_read_offset(&self, channel: &str) -> Result<u64> {
        Ok(self.load()?.read_offsets.get(channel).copied().unwrap_or(0))
    }

    /// Set the read offset for a channel.
    pub fn set_read_offset(&self, channel: &str, offset: u64) -> Result<()> {
        self.update(|s| {
            s.read_offsets.insert(channel.to_string(), offset);
        })?;
        Ok(())
    }

    /// Get the last read message ID for a channel.
    pub fn get_last_read_id(&self, channel: &str) -> Result<Option<String>> {
        Ok(self.load()?.last_read_ids.get(channel).cloned())
    }

    /// Set the last read message ID for a channel.
    pub fn set_last_read_id(&self, channel: &str, id: &str) -> Result<()> {
        self.update(|s| {
            s.last_read_ids.insert(channel.to_string(), id.to_string());
        })?;
        Ok(())
    }

    /// Mark a channel as read up to a specific offset and message ID.
    pub fn mark_read(&self, channel: &str, offset: u64, last_id: Option<&str>) -> Result<()> {
        self.update(|s| {
            s.read_offsets.insert(channel.to_string(), offset);
            s.last_read_times.insert(channel.to_string(), Utc::now());
            if let Some(id) = last_id {
                s.last_read_ids.insert(channel.to_string(), id.to_string());
            } else {
                s.last_read_ids.remove(channel);
            }
        })?;
        Ok(())
    }

    /// Get read cursor info for a channel.
    pub fn get_read_cursor(&self, channel: &str) -> Result<ReadCursor> {
        let state = self.load()?;
        Ok(ReadCursor {
            offset: state.read_offsets.get(channel).copied().unwrap_or(0),
            last_id: state.last_read_ids.get(channel).cloned(),
            last_time: state.last_read_times.get(channel).copied(),
        })
    }

    /// Subscribe to a channel.
    pub fn subscribe(&self, channel: &str) -> Result<bool> {
        let mut added = false;
        self.update(|s| {
            if !s.subscribed_channels.contains(&channel.to_string()) {
                s.subscribed_channels.push(channel.to_string());
                added = true;
            }
        })?;
        Ok(added)
    }

    /// Unsubscribe from a channel.
    pub fn unsubscribe(&self, channel: &str) -> Result<bool> {
        let mut removed = false;
        self.update(|s| {
            if let Some(pos) = s.subscribed_channels.iter().position(|c| c == channel) {
                s.subscribed_channels.remove(pos);
                removed = true;
            }
        })?;
        Ok(removed)
    }

    /// Get list of subscribed channels.
    pub fn get_subscribed_channels(&self) -> Result<Vec<String>> {
        Ok(self.load()?.subscribed_channels)
    }

    /// Record a heartbeat for this agent at the current time.
    ///
    /// Returns the timestamp that was recorded, so callers can reuse it
    /// (e.g. for logging) without a second clock read.
    pub fn record_heartbeat(&self) -> Result<DateTime<Utc>> {
        let now = Utc::now();
        self.update(|s| {
            s.last_heartbeat = Some(now);
        })?;
        Ok(now)
    }

    /// Get this agent's most recent heartbeat timestamp, if any has ever
    /// been recorded.
    pub fn get_last_heartbeat(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(self.load()?.last_heartbeat)
    }
}

/// Read cursor information for a channel.
#[derive(Debug, Clone, Default)]
pub struct ReadCursor {
    /// Byte offset into the channel file
    pub offset: u64,
    /// Last read message ID (ULID)
    pub last_id: Option<String>,
    /// Timestamp when mark-read was called
    pub last_time: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn test_agent_state_roundtrip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("agent_state.json");
        let manager = AgentStateManager::from_path(&path);

        let mut state = AgentState::new();
        state.read_offsets.insert("general".to_string(), 1234);
        state
            .last_read_ids
            .insert("general".to_string(), "01ABC".to_string());

        manager.save(&state).unwrap();
        let loaded = manager.load().unwrap();

        assert_eq!(loaded.read_offsets.get("general"), Some(&1234));
        assert_eq!(
            loaded.last_read_ids.get("general"),
            Some(&"01ABC".to_string())
        );
    }

    #[test]
    fn test_load_nonexistent() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nonexistent.json");
        let manager = AgentStateManager::from_path(&path);

        let state = manager.load().unwrap();
        assert!(state.read_offsets.is_empty());
    }

    #[test]
    fn test_set_read_offset() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let manager = AgentStateManager::from_path(&path);

        manager.set_read_offset("general", 5678).unwrap();
        assert_eq!(manager.get_read_offset("general").unwrap(), 5678);
    }

    #[test]
    fn test_mark_read() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let manager = AgentStateManager::from_path(&path);

        manager.mark_read("general", 9999, Some("01XYZ")).unwrap();

        let cursor = manager.get_read_cursor("general").unwrap();
        assert_eq!(cursor.offset, 9999);
        assert_eq!(cursor.last_id, Some("01XYZ".to_string()));
        assert!(cursor.last_time.is_some());
    }

    #[test]
    fn test_mark_read_without_last_id_clears_stale_id() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let manager = AgentStateManager::from_path(&path);

        manager.mark_read("general", 9999, Some("01XYZ")).unwrap();
        manager.mark_read("general", 0, None).unwrap();

        let cursor = manager.get_read_cursor("general").unwrap();
        assert_eq!(cursor.offset, 0);
        assert_eq!(cursor.last_id, None);
        assert!(cursor.last_time.is_some());
    }

    #[test]
    fn test_per_agent_isolation() {
        let temp = TempDir::new().unwrap();
        let project = temp.path();

        let agent1 = AgentStateManager::new(project, "Agent1");
        let agent2 = AgentStateManager::new(project, "Agent2");

        agent1.set_read_offset("general", 100).unwrap();
        agent2.set_read_offset("general", 200).unwrap();

        // Each agent has its own offset
        assert_eq!(agent1.get_read_offset("general").unwrap(), 100);
        assert_eq!(agent2.get_read_offset("general").unwrap(), 200);
    }

    /// Stress test for concurrent agent state updates (bd-k7r).
    ///
    /// Spawns multiple threads that concurrently increment a read offset.
    /// Verifies that no updates are lost due to race conditions.
    #[test]
    fn test_concurrent_agent_state_updates() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("agent_state.json");
        let manager = Arc::new(AgentStateManager::from_path(&path));

        // Initialize the counter
        manager
            .update(|s| {
                s.read_offsets.insert("counter".to_string(), 0);
            })
            .unwrap();

        const NUM_THREADS: usize = 10;
        const INCREMENTS_PER_THREAD: usize = 50;

        let mut handles = Vec::new();
        for _ in 0..NUM_THREADS {
            let manager = Arc::clone(&manager);
            let handle = std::thread::spawn(move || {
                for _ in 0..INCREMENTS_PER_THREAD {
                    manager
                        .update(|s| {
                            let current = s.read_offsets.get("counter").copied().unwrap_or(0);
                            s.read_offsets.insert("counter".to_string(), current + 1);
                        })
                        .unwrap();
                }
            });
            handles.push(handle);
        }

        // Wait for all threads to complete
        for handle in handles {
            handle.join().expect("Thread panicked");
        }

        // Verify no updates were lost
        let state = manager.load().unwrap();
        let final_count = state.read_offsets.get("counter").copied().unwrap_or(0);
        let expected = (NUM_THREADS * INCREMENTS_PER_THREAD) as u64;

        assert_eq!(
            final_count, expected,
            "Expected {} increments but got {} - updates were lost!",
            expected, final_count
        );
    }

    #[test]
    fn test_subscribe_channel() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let manager = AgentStateManager::from_path(&path);

        // Subscribe to a channel
        let added = manager.subscribe("general").unwrap();
        assert!(added);

        // Verify subscription
        let channels = manager.get_subscribed_channels().unwrap();
        assert_eq!(channels, vec!["general"]);

        // Subscribe to same channel again (should be idempotent)
        let added = manager.subscribe("general").unwrap();
        assert!(!added);

        let channels = manager.get_subscribed_channels().unwrap();
        assert_eq!(channels, vec!["general"]);
    }

    #[test]
    fn test_unsubscribe_channel() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let manager = AgentStateManager::from_path(&path);

        // Subscribe to channels
        manager.subscribe("general").unwrap();
        manager.subscribe("dev").unwrap();

        // Unsubscribe from one
        let removed = manager.unsubscribe("general").unwrap();
        assert!(removed);

        let channels = manager.get_subscribed_channels().unwrap();
        assert_eq!(channels, vec!["dev"]);

        // Unsubscribe from non-existent (should be idempotent)
        let removed = manager.unsubscribe("general").unwrap();
        assert!(!removed);
    }

    #[test]
    fn test_remove_channel_from_agent_states() {
        let temp = TempDir::new().unwrap();
        let manager = AgentStateManager::new(temp.path(), "test-agent");

        manager.subscribe("old-channel").unwrap();
        manager.set_read_offset("old-channel", 100).unwrap();
        manager
            .set_last_read_id("old-channel", "test-id-123")
            .unwrap();
        manager
            .mark_read("old-channel", 100, Some("test-id-123"))
            .unwrap();

        let updated = remove_channel_from_agent_states(temp.path(), "old-channel").unwrap();
        assert_eq!(updated, 1);

        let state = manager.load().unwrap();
        assert!(
            !state
                .subscribed_channels
                .contains(&"old-channel".to_string())
        );
        assert_eq!(state.read_offsets.get("old-channel"), None);
        assert_eq!(state.last_read_ids.get("old-channel"), None);
        assert_eq!(state.last_read_times.get("old-channel"), None);
    }

    #[test]
    fn test_record_and_get_heartbeat() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let manager = AgentStateManager::from_path(&path);

        assert_eq!(manager.get_last_heartbeat().unwrap(), None);

        let recorded = manager.record_heartbeat().unwrap();
        let loaded = manager.get_last_heartbeat().unwrap();
        assert_eq!(loaded, Some(recorded));
    }

    #[test]
    fn test_heartbeat_overwrites_previous() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let manager = AgentStateManager::from_path(&path);

        let first = manager.record_heartbeat().unwrap();
        // Force a distinguishable second timestamp regardless of clock resolution.
        manager
            .update(|s| {
                s.last_heartbeat = Some(first + chrono::Duration::seconds(5));
            })
            .unwrap();

        let loaded = manager.get_last_heartbeat().unwrap().unwrap();
        assert!(loaded > first);
    }

    #[test]
    fn test_multiple_subscriptions() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let manager = AgentStateManager::from_path(&path);

        // Subscribe to multiple channels
        manager.subscribe("general").unwrap();
        manager.subscribe("dev").unwrap();
        manager.subscribe("ops").unwrap();

        let channels = manager.get_subscribed_channels().unwrap();
        assert_eq!(channels.len(), 3);
        assert!(channels.contains(&"general".to_string()));
        assert!(channels.contains(&"dev".to_string()));
        assert!(channels.contains(&"ops".to_string()));
    }
}

/// Rename a channel in all agent state files.
///
/// This function scans all agent directories and updates any references to the old channel name
/// in their state files. It updates:
/// - Keys in `read_offsets` HashMap
/// - Keys in `last_read_ids` HashMap
/// - Keys in `last_read_times` HashMap
/// - Entries in `subscribed_channels` Vec
///
/// Returns the number of agent states that were updated.
pub fn rename_channel_in_agent_states(
    project_root: &Path,
    old_name: &str,
    new_name: &str,
) -> Result<usize> {
    let agents_dir = project_root.join(".rite").join("agents");

    // If agents directory doesn't exist, no states to update
    if !agents_dir.exists() {
        return Ok(0);
    }

    let mut updated_count = 0;

    // Iterate through all agent directories
    let entries = std::fs::read_dir(&agents_dir)
        .with_context(|| format!("Failed to read agents directory: {}", agents_dir.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| "Failed to read agent directory entry")?;
        let agent_path = entry.path();

        // Only process directories
        if !agent_path.is_dir() {
            continue;
        }

        let agent_name = match agent_path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };

        let manager = AgentStateManager::new(project_root, agent_name);
        let mut state = manager.load()?;
        let mut changed = false;

        // Update read_offsets
        if let Some(offset) = state.read_offsets.remove(old_name) {
            state.read_offsets.insert(new_name.to_string(), offset);
            changed = true;
        }

        // Update last_read_ids
        if let Some(last_id) = state.last_read_ids.remove(old_name) {
            state.last_read_ids.insert(new_name.to_string(), last_id);
            changed = true;
        }

        // Update last_read_times
        if let Some(last_time) = state.last_read_times.remove(old_name) {
            state
                .last_read_times
                .insert(new_name.to_string(), last_time);
            changed = true;
        }

        // Update subscribed_channels
        if let Some(pos) = state.subscribed_channels.iter().position(|c| c == old_name) {
            state.subscribed_channels[pos] = new_name.to_string();
            changed = true;
        }

        // Save if anything changed
        if changed {
            manager.save(&state)?;
            updated_count += 1;
        }
    }

    Ok(updated_count)
}

/// Remove a channel from all agent state files.
///
/// This removes:
/// - Keys in `read_offsets`
/// - Keys in `last_read_ids`
/// - Keys in `last_read_times`
/// - Entries in `subscribed_channels`
///
/// Returns the number of agent states that were updated.
pub fn remove_channel_from_agent_states(project_root: &Path, channel: &str) -> Result<usize> {
    let agents_dir = project_root.join(".rite").join("agents");

    if !agents_dir.exists() {
        return Ok(0);
    }

    let mut updated_count = 0;
    let entries = std::fs::read_dir(&agents_dir)
        .with_context(|| format!("Failed to read agents directory: {}", agents_dir.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| "Failed to read agent directory entry")?;
        let agent_path = entry.path();

        if !agent_path.is_dir() {
            continue;
        }

        let agent_name = match agent_path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };

        let manager = AgentStateManager::new(project_root, agent_name);
        let mut state = manager.load()?;
        let mut changed = false;

        changed |= state.read_offsets.remove(channel).is_some();
        changed |= state.last_read_ids.remove(channel).is_some();
        changed |= state.last_read_times.remove(channel).is_some();

        let before_len = state.subscribed_channels.len();
        state.subscribed_channels.retain(|c| c != channel);
        changed |= state.subscribed_channels.len() != before_len;

        if changed {
            manager.save(&state)?;
            updated_count += 1;
        }
    }

    Ok(updated_count)
}
