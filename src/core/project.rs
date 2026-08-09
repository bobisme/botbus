//! Global path helpers for Rite data storage.
//!
//! Rite uses XDG Base Directory specification for storage:
//! - Data: `$XDG_DATA_HOME/rite/` (default: `~/.local/share/rite/`)
//!
//! For testing, set `RITE_DATA_DIR` to override the data directory.
//!
//! Directory structure:
//! ```text
//! ~/.local/share/rite/
//!   channels/
//!     general.jsonl
//!     <channel>.jsonl
//!   claims.jsonl
//!   state.json
//!   index.sqlite
//! ```

use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::env;
use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Environment variable to override the data directory (for testing).
pub const DATA_DIR_ENV_VAR: &str = "RITE_DATA_DIR";

/// Environment variable to override the cache directory (for testing).
pub const CACHE_DIR_ENV_VAR: &str = "RITE_CACHE_DIR";

/// Get the Rite data directory.
///
/// Checks in order:
/// 1. `RITE_DATA_DIR` environment variable (for testing)
/// 2. XDG data directory (`$XDG_DATA_HOME/rite/`)
/// 3. Fallback: `~/.local/share/rite/`
///
/// Note: This function reads the env var each time, so tests can set different
/// values for different test cases.
pub fn data_dir() -> PathBuf {
    // 1. Check env var override (for testing)
    if let Ok(dir) = env::var(DATA_DIR_ENV_VAR)
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }

    // 2. Try XDG-compliant path
    if let Some(proj_dirs) = ProjectDirs::from("", "", "rite") {
        return proj_dirs.data_dir().to_path_buf();
    }

    // 3. Fallback: ~/.local/share/rite/
    if let Some(user_dirs) = directories::UserDirs::new() {
        return user_dirs
            .home_dir()
            .join(".local")
            .join("share")
            .join("rite");
    }

    // Last resort: current directory (not ideal, but won't panic)
    PathBuf::from(".rite")
}

/// Get the Rite cache directory.
///
/// Checks in order:
/// 1. `RITE_CACHE_DIR` environment variable (for testing)
/// 2. XDG cache directory (`$XDG_CACHE_HOME/rite/`)
/// 3. Fallback: `~/.cache/rite/`
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = env::var(CACHE_DIR_ENV_VAR)
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }

    if let Some(proj_dirs) = ProjectDirs::from("", "", "rite") {
        return proj_dirs.cache_dir().to_path_buf();
    }

    if let Some(user_dirs) = directories::UserDirs::new() {
        return user_dirs.home_dir().join(".cache").join("rite");
    }

    PathBuf::from(".rite_cache")
}

/// Ensure the cache directory exists (owner-only permissions on Unix).
pub fn ensure_cache_dir() -> Result<PathBuf> {
    let dir = cache_dir();
    let created = !dir.exists();

    fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create cache dir: {}", dir.display()))?;

    #[cfg(unix)]
    if created {
        let permissions = fs::Permissions::from_mode(0o700);
        fs::set_permissions(&dir, permissions)
            .with_context(|| format!("Failed to set permissions on: {}", dir.display()))?;
    }

    Ok(dir)
}

/// Ensure the data directory and subdirectories exist.
///
/// Creates:
/// - Data directory (with restricted permissions: 0700 on Unix)
/// - Channels subdirectory
///
/// # Security
/// The data directory is created with restrictive permissions (owner-only)
/// to protect message history and agent state from other users.
pub fn ensure_data_dir() -> Result<PathBuf> {
    let dir = data_dir();
    let created = !dir.exists();

    fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create data dir: {}", dir.display()))?;

    // Set restrictive permissions on the data directory (Unix only)
    #[cfg(unix)]
    if created {
        // 0700 = rwx------ (owner only)
        let permissions = fs::Permissions::from_mode(0o700);
        fs::set_permissions(&dir, permissions)
            .with_context(|| format!("Failed to set permissions on: {}", dir.display()))?;
    }

    fs::create_dir_all(channels_dir()).with_context(|| {
        format!(
            "Failed to create channels dir: {}",
            channels_dir().display()
        )
    })?;
    Ok(dir)
}

/// Get the channels directory path.
pub fn channels_dir() -> PathBuf {
    data_dir().join("channels")
}

/// Get the path to a specific channel file.
///
/// # Security
/// Validates channel name to prevent path traversal attacks.
/// Rejects names containing path separators or parent directory references.
pub fn channel_path(channel: &str) -> PathBuf {
    // Defense-in-depth: reject path traversal attempts even if CLI validated
    debug_assert!(
        !channel.contains('/') && !channel.contains('\\') && !channel.contains(".."),
        "channel_path called with unsafe channel name: {}",
        channel
    );

    // Sanitize: remove any path separators (should never happen if CLI validates)
    let safe_channel = channel.replace(['/', '\\'], "").replace("..", "");

    channels_dir().join(format!("{}.jsonl", safe_channel))
}

/// Get the claims.jsonl path.
pub fn claims_path() -> PathBuf {
    data_dir().join("claims.jsonl")
}

/// Get the statuses.jsonl path.
pub fn statuses_path() -> PathBuf {
    data_dir().join("statuses.jsonl")
}

/// Get the state.json path.
pub fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

/// Get the index.sqlite path.
pub fn index_path() -> PathBuf {
    data_dir().join("index.sqlite")
}

/// Get the hooks.jsonl path (hook definitions).
pub fn hooks_path() -> PathBuf {
    data_dir().join("hooks.jsonl")
}

/// Get the hooks_audit.jsonl path (hook firing audit log).
pub fn hooks_audit_path() -> PathBuf {
    data_dir().join("hooks_audit.jsonl")
}

/// Get the hook_queue.jsonl path (triggers batched behind a spawn lease).
///
/// Additive: a rite that predates spawn leases never opens this file, so its
/// presence in a synced data directory is inert for older builds.
pub fn hook_queue_path() -> PathBuf {
    data_dir().join("hook_queue.jsonl")
}

/// Get the Telegram config path in the cache directory.
pub fn telegram_config_path() -> PathBuf {
    cache_dir().join("telegram.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    #[test]
    fn test_data_dir_not_empty() {
        let dir = data_dir();
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    #[serial]
    fn test_data_dir_override() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_str().unwrap();

        // Set override
        unsafe {
            env::set_var(DATA_DIR_ENV_VAR, temp_path);
        }

        let dir = data_dir();
        assert_eq!(dir, temp.path());

        // Clean up
        unsafe {
            env::remove_var(DATA_DIR_ENV_VAR);
        }
    }

    #[test]
    #[serial]
    fn test_cache_dir_override() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_str().unwrap();

        unsafe {
            env::set_var(CACHE_DIR_ENV_VAR, temp_path);
        }

        let dir = cache_dir();
        assert_eq!(dir, temp.path());

        unsafe {
            env::remove_var(CACHE_DIR_ENV_VAR);
        }
    }

    #[test]
    #[serial]
    fn test_telegram_config_path() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_str().unwrap();

        unsafe {
            env::set_var(CACHE_DIR_ENV_VAR, temp_path);
        }

        let path = telegram_config_path();
        assert!(path.ends_with("telegram.json"));
        assert!(path.starts_with(temp.path()));

        unsafe {
            env::remove_var(CACHE_DIR_ENV_VAR);
        }
    }

    #[test]
    #[serial]
    fn test_channels_dir_is_subdir() {
        let base = data_dir();
        let channels = channels_dir();
        assert!(channels.starts_with(&base));
        assert!(channels.ends_with("channels"));
    }

    #[test]
    #[serial]
    fn test_channel_path() {
        let base = data_dir();
        let path = channel_path("general");
        assert!(path.ends_with("general.jsonl"));
        assert!(path.starts_with(&base));
    }

    #[test]
    #[serial]
    fn test_claims_path() {
        let base = data_dir();
        let path = claims_path();
        assert!(path.ends_with("claims.jsonl"));
        assert!(path.starts_with(&base));
    }

    #[test]
    #[serial]
    fn test_state_path() {
        let base = data_dir();
        let path = state_path();
        assert!(path.ends_with("state.json"));
        assert!(path.starts_with(&base));
    }

    #[test]
    #[serial]
    fn test_index_path() {
        let base = data_dir();
        let path = index_path();
        assert!(path.ends_with("index.sqlite"));
        assert!(path.starts_with(&base));
    }
}
