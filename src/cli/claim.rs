use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use colored::Colorize;
use globset::{Glob, GlobSetBuilder};
use serde::Serialize;
use std::path::Path;
use tracing::instrument;

use super::OutputFormat;
use crate::core::claim::FileClaim;
use crate::core::identity::{require_agent, resolve_agent};
use crate::core::message::{Message, MessageMeta};
use crate::core::presence::{self, Presence};
use crate::core::project::{channel_path, claims_path, data_dir};
use crate::storage::jsonl::{append_if, append_record, read_records};
use crate::sync::auto_commit::{auto_commit_after_claim, auto_commit_after_release};

/// Check if a pattern looks like a URI (has a scheme like "bead://", "db://", etc.)
fn is_uri(pattern: &str) -> bool {
    // URIs have a scheme followed by "://"
    // Common schemes: bead://, db://, port://, file://, http://, etc.
    if let Some(colon_pos) = pattern.find(':') {
        // Check that scheme is alphanumeric and followed by //
        let scheme = &pattern[..colon_pos];
        let after_colon = &pattern[colon_pos..];
        return !scheme.is_empty()
            && scheme.chars().all(|c| c.is_ascii_alphanumeric())
            && after_colon.starts_with("://");
    }
    false
}

/// Expand a claim pattern to an absolute, canonicalized path.
/// Relative patterns are expanded from current working directory.
/// Glob wildcards are preserved.
/// URIs (patterns with schemes like "bead://") are passed through unchanged.
///
/// # Security
/// Canonicalizes the base path portion to resolve symlinks and normalize
/// path components like `.` and `..`, preventing path confusion attacks.
fn expand_pattern(pattern: &str) -> String {
    // URIs pass through unchanged - they're not file paths
    if is_uri(pattern) {
        return pattern.to_string();
    }
    // Get cwd for expansion
    let cwd = std::env::current_dir().unwrap_or_default();

    // Split into base (canonicalizable) and glob suffix
    let (base, suffix) = if let Some(idx) = pattern.find("**") {
        let base_end = pattern[..idx].rfind('/').map(|i| i + 1).unwrap_or(idx);
        (&pattern[..base_end], &pattern[base_end..])
    } else if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        // Simple glob - find last / before any glob char
        let glob_start = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
        let base_end = pattern[..glob_start].rfind('/').map(|i| i + 1).unwrap_or(0);
        (&pattern[..base_end], &pattern[base_end..])
    } else {
        // No glob - entire path is base
        (pattern, "")
    };

    // Build absolute base path
    let abs_base = if base.starts_with('/') {
        Path::new(base).to_path_buf()
    } else if base.is_empty() {
        cwd.clone()
    } else {
        cwd.join(base)
    };

    // Try to canonicalize the base path (resolve symlinks, normalize ..)
    // If it fails (e.g., path doesn't exist yet), just use the cleaned absolute path
    let canonical_base = abs_base.canonicalize().unwrap_or_else(|_| abs_base.clone());

    // Recombine with glob suffix
    if suffix.is_empty() {
        canonical_base.to_string_lossy().to_string()
    } else {
        let base_str = canonical_base.to_string_lossy();
        let base_str = base_str.trim_end_matches('/');
        let suffix_str = suffix.trim_start_matches('/');
        format!("{}/{}", base_str, suffix_str)
    }
}

/// Display a claim pattern as-is (absolute paths for clarity).
/// URIs are displayed unchanged.
fn display_pattern(pattern: &str) -> String {
    // Always show full pattern for clarity - no relative path conversion
    pattern.to_string()
}

pub struct ClaimOptions {
    pub patterns: Vec<String>,
    pub ttl: u64,
    pub message: Option<String>,
    /// Extend TTL on existing claims matching this pattern
    pub extend: Option<String>,
    pub agent: Option<String>,
}

/// Claim files for editing.
#[instrument(skip(options), fields(pattern_count = options.patterns.len(), ttl = options.ttl, extend = options.extend.is_some()))]
pub fn claim(options: ClaimOptions) -> Result<()> {
    let agent_name = require_agent(options.agent.as_deref())?;

    // Handle --extend option: extend TTL on existing claims
    if let Some(extend_pattern) = &options.extend {
        let expanded = expand_pattern(extend_pattern);
        return extend_claims(&expanded, options.ttl, &agent_name);
    }

    // Expand patterns to absolute paths
    let expanded_patterns: Vec<String> =
        options.patterns.iter().map(|p| expand_pattern(p)).collect();

    // Create the claim with expanded (absolute) patterns and optional message
    let claim = FileClaim::with_message(
        &agent_name,
        expanded_patterns.clone(),
        options.ttl,
        options.message.clone(),
    );

    // Atomic check-and-stake: fail if ANY active claim (including our own) holds overlapping patterns
    // This prevents double-firing hooks and ensures claim semantics are strict
    let patterns_for_check = expanded_patterns.clone();
    let acquired = append_if(&claims_path(), &claim, |existing_claims| {
        let now = Utc::now();

        // Build active claims map (latest state per claim ID)
        let mut active: std::collections::HashMap<ulid::Ulid, &FileClaim> =
            std::collections::HashMap::new();
        for c in existing_claims {
            active.insert(c.id, c);
        }

        // Check for ANY overlapping active claim (including our own)
        for c in active.values() {
            // Skip inactive or expired
            if !c.active || c.expires_at < now {
                continue;
            }

            // Check for pattern overlap
            for new_pat in &patterns_for_check {
                for existing_pat in &c.patterns {
                    if patterns_overlap(new_pat, existing_pat) {
                        return false; // Conflict - don't append
                    }
                }
            }
        }

        true // No conflicts - safe to append
    })
    .with_context(|| "Failed to stake claim")?;

    if !acquired {
        // Re-read claims to provide helpful error message
        let claims: Vec<FileClaim> = read_records(&claims_path())?;
        let conflicts = check_conflicts_including_self(&expanded_patterns, &claims);

        let now = Utc::now();
        eprintln!("{}", "Error: Conflict with existing claim(s)".red().bold());
        eprintln!();

        for (pattern, holder, expires) in &conflicts {
            let remaining_secs = (*expires - now).num_seconds().max(0);
            let remaining_mins = (remaining_secs / 60).max(0);

            eprintln!("  Pattern: {}", pattern.cyan());
            eprintln!("  Claimed by: {}", holder.yellow());
            eprintln!("  Expires at: {}", expires.to_rfc3339().dimmed());
            eprintln!(
                "  Time remaining: {}s ({}m)",
                remaining_secs, remaining_mins
            );
            eprintln!();
        }

        if !conflicts.is_empty() {
            // Get the first conflicting agent for suggestions
            let first_holder = &conflicts[0].1;
            let first_expires = &conflicts[0].2;
            let wait_secs = (*first_expires - now).num_seconds().max(0);

            eprintln!("{}", "Options:".bold());
            eprintln!();
            eprintln!("1. {} Wait for claim to expire:", "Wait:".green());
            eprintln!(
                "   {}",
                format!(
                    "sleep {} && rite claim {}",
                    wait_secs + 5,
                    options.patterns.join(" ")
                )
                .dimmed()
            );
            eprintln!();

            if first_holder != &agent_name {
                eprintln!(
                    "2. {} Ask holder to release or narrow their claim:",
                    "Communicate:".green()
                );
                eprintln!(
                    "   {}",
                    format!(
                        "rite send @{} \"Can you release {}? I need to work on it\"",
                        first_holder,
                        options.patterns.join(", ")
                    )
                    .dimmed()
                );
            } else {
                eprintln!(
                    "2. {} Release your existing claim first:",
                    "Release:".green()
                );
                eprintln!(
                    "   {}",
                    format!("rite release {}", options.patterns.join(" ")).dimmed()
                );
            }
        }

        anyhow::bail!("Cannot claim - conflicts with existing claims");
    }

    // Post message to #claims (use absolute patterns for clarity)
    let display_patterns: Vec<String> = expanded_patterns.clone();
    let body = if let Some(msg) = &options.message {
        format!("Claimed {} ({})", display_patterns.join(", "), msg)
    } else {
        format!("Claimed {}", display_patterns.join(", "))
    };

    let claim_msg = Message::new(&agent_name, "claims", &body).with_meta(MessageMeta::Claim {
        patterns: display_patterns.clone(),
        ttl_secs: options.ttl,
    });

    append_record(&channel_path("claims"), &claim_msg)?;
    auto_commit_after_claim(&data_dir(), &display_patterns);

    // Output (use absolute patterns for clarity)
    println!(
        "{} Claimed {} pattern(s) for {}",
        "Success:".green(),
        display_patterns.len(),
        format_duration(options.ttl)
    );
    for pattern in &display_patterns {
        println!("  {}", pattern.cyan());
    }

    Ok(())
}

/// Extend TTL on existing claims matching the given pattern.
fn extend_claims(pattern: &str, ttl: u64, agent_name: &str) -> Result<()> {
    let all_claims: Vec<FileClaim> = read_records(&claims_path())?;
    let now = Utc::now();

    // Build active claims map
    let mut active: std::collections::HashMap<ulid::Ulid, FileClaim> =
        std::collections::HashMap::new();
    for claim in all_claims {
        active.insert(claim.id, claim);
    }

    let mut extended_count = 0;

    for claim in active.values() {
        // Only extend our own claims
        if claim.agent != agent_name {
            continue;
        }

        // Skip inactive or expired
        if !claim.active || claim.expires_at < now {
            continue;
        }

        // Check if any pattern matches
        let matches = claim.patterns.iter().any(|p| {
            p == pattern
                || p.contains(pattern)
                || pattern.contains(p)
                || patterns_overlap(p, pattern)
        });

        if matches {
            // Create extended claim (new record with same ID but new expiry)
            let extended = claim.extend(ttl);
            append_record(&claims_path(), &extended)?;
            extended_count += 1;

            // Post system message for claim extension
            let display_patterns: Vec<String> =
                claim.patterns.iter().map(|p| display_pattern(p)).collect();
            let body = format!(
                "Claim extended: {} by {} (expires in {})",
                display_patterns.join(", "),
                agent_name,
                format_duration(ttl)
            );
            let msg =
                Message::new(agent_name, "claims", &body).with_meta(MessageMeta::ClaimExtended {
                    patterns: display_patterns,
                    ttl_secs: ttl,
                });
            append_record(&channel_path("claims"), &msg)?;
        }
    }

    if extended_count == 0 {
        println!("No matching claims to extend.");
    } else {
        println!(
            "{} Extended {} claim(s) for {}",
            "Success:".green(),
            extended_count,
            format_duration(ttl)
        );
    }

    Ok(())
}

/// Output for claims list.
#[derive(Debug, Serialize)]
pub struct ClaimsOutput {
    pub claims: Vec<ClaimInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ClaimInfo {
    pub agent: String,
    pub patterns: Vec<String>,
    pub active: bool,
    pub expires_at: DateTime<Utc>,
    pub expires_in_secs: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// True when the claim's holder has a lapsed presence heartbeat (see
    /// `crate::core::presence`) — i.e. the holder is presumed gone, not
    /// just quiet. This is a REPORT only: rite never auto-releases another
    /// agent's claim on the strength of this field. A claim holder with no
    /// heartbeat history at all (predates this feature) reports `false`
    /// here rather than a false positive.
    pub stale: bool,
}

/// Parse a time specification (absolute or relative).
/// Supports:
/// - Relative: "2h", "30m", "1d", "2h ago"
/// - Absolute: "2026-01-28", "2026-01-28T12:00:00Z", "2026-01-28 12:00:00"
fn parse_time_spec(s: &str) -> Result<DateTime<Utc>> {
    // Remove " ago" suffix if present
    let s = s.trim().strip_suffix(" ago").unwrap_or(s).trim();

    // Try parsing as relative time (e.g., "2h", "30m", "1d")
    if s.len() >= 2 {
        let unit = s.chars().last().unwrap();
        let number_part = &s[..s.len() - 1];

        if let Ok(amount) = number_part.parse::<i64>() {
            let duration = match unit {
                's' => Some(chrono::Duration::seconds(amount)),
                'm' => Some(chrono::Duration::minutes(amount)),
                'h' => Some(chrono::Duration::hours(amount)),
                'd' => Some(chrono::Duration::days(amount)),
                _ => None,
            };

            if let Some(dur) = duration {
                return Ok(Utc::now() - dur);
            }
        }
    }

    // Try parsing as RFC3339
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Try parsing as just a date
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0).unwrap();
        return Ok(DateTime::from_naive_utc_and_offset(dt, Utc));
    }

    // Try parsing as date + time
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(DateTime::from_naive_utc_and_offset(dt, Utc));
    }

    anyhow::bail!("Could not parse time: {}", s)
}

/// List active file claims.
pub fn claims(
    format: OutputFormat,
    show_all: bool,
    mine_only: bool,
    limit: Option<usize>,
    since: Option<String>,
    agent: Option<&str>,
) -> Result<()> {
    let current_agent = resolve_agent(agent).unwrap_or_default();

    let all_claims: Vec<FileClaim> = read_records(&claims_path())?;

    // Build active claims map (latest state per claim ID)
    let mut active: std::collections::HashMap<ulid::Ulid, FileClaim> =
        std::collections::HashMap::new();
    for claim in all_claims {
        active.insert(claim.id, claim);
    }

    // Parse --since time if provided
    let since_time = if let Some(ref since_str) = since {
        Some(parse_time_spec(since_str)?)
    } else {
        None
    };

    // Filter and sort
    let now = Utc::now();
    let mut claims_list: Vec<_> = active
        .values()
        .filter(|c| {
            if mine_only && c.agent != current_agent {
                return false;
            }
            if !show_all && (!c.active || c.expires_at < now) {
                return false;
            }
            // Filter by --since if provided
            if let Some(since_dt) = since_time
                && c.ts < since_dt
            {
                return false;
            }
            true
        })
        .collect();

    // Sort by creation time (most recent first) when using --since or --limit
    // Otherwise sort by expiration time
    if since.is_some() || limit.is_some() {
        claims_list.sort_by_key(|claim| std::cmp::Reverse(claim.ts));
    } else {
        claims_list.sort_by_key(|claim| claim.expires_at);
    }

    // Apply limit if provided
    if let Some(n) = limit {
        claims_list.truncate(n);
    }

    // Prepare structured output. Cache presence lookups per agent - multiple
    // claims are commonly held by the same agent, and each lookup is a
    // small file read.
    let mut presence_cache: std::collections::HashMap<String, Presence> =
        std::collections::HashMap::new();
    let claim_infos: Vec<ClaimInfo> = claims_list
        .iter()
        .map(|c| {
            let presence = *presence_cache
                .entry(c.agent.clone())
                .or_insert_with(|| presence::agent_presence(&c.agent));
            ClaimInfo {
                agent: c.agent.clone(),
                patterns: c.patterns.clone(),
                active: c.active,
                expires_at: c.expires_at,
                expires_in_secs: (c.expires_at - now).num_seconds(),
                message: c.message.clone(),
                stale: presence.is_stale(),
            }
        })
        .collect();

    // Build advice
    let mut advice = Vec::new();
    if !claim_infos.is_empty() && !current_agent.is_empty() {
        // Suggest releasing claims if the agent has any
        let has_agent_claims = claim_infos.iter().any(|c| c.agent == current_agent);
        if has_agent_claims {
            advice.push("rite claims release --all".to_string());
        }
    }

    match format {
        OutputFormat::Json => {
            let output = ClaimsOutput {
                claims: claim_infos,
                advice,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
            return Ok(());
        }
        OutputFormat::Pretty => {
            if claims_list.is_empty() {
                println!("No active claims.");
                return Ok(());
            }

            // Group claims by type: files vs URI schemes
            let mut file_claims: Vec<_> = Vec::new();
            let mut uri_claims: std::collections::HashMap<String, Vec<_>> =
                std::collections::HashMap::new();

            for claim in &claims_list {
                let first_pattern = claim.patterns.first().map(|s| s.as_str()).unwrap_or("");
                if is_uri(first_pattern) {
                    let scheme = first_pattern
                        .split("://")
                        .next()
                        .unwrap_or("uri")
                        .to_string();
                    uri_claims.entry(scheme).or_default().push(claim);
                } else {
                    file_claims.push(claim);
                }
            }

            let format_claim = |claim: &&FileClaim, current_agent: &str, now: DateTime<Utc>| {
                let remaining = (claim.expires_at - now).num_minutes();
                let status = if claim.expires_at < now {
                    "expired".red()
                } else if !claim.active {
                    "released".dimmed()
                } else {
                    format!("expires in {}m", remaining).green()
                };

                let agent_display = if claim.agent == current_agent {
                    claim.agent.cyan().bold()
                } else {
                    claim.agent.yellow().normal()
                };

                // Display patterns relative to cwd when possible
                let display_patterns: Vec<String> =
                    claim.patterns.iter().map(|p| display_pattern(p)).collect();

                // Presence is reported, never used to change lock behavior -
                // a stale claim still shows its normal expiry status above.
                let stale_marker = if presence::agent_presence(&claim.agent).is_stale() {
                    " [STALE - holder unreachable]".red().to_string()
                } else {
                    String::new()
                };

                println!(
                    "  {:<16} {:<30} {}{}",
                    agent_display,
                    display_patterns.join(", ").dimmed(),
                    status,
                    stale_marker
                );
            };

            // Display file claims first
            if !file_claims.is_empty() {
                println!("{}", "File Claims:".bold());
                for claim in &file_claims {
                    format_claim(claim, &current_agent, now);
                }
            }

            // Display URI claims grouped by scheme
            let mut schemes: Vec<_> = uri_claims.keys().collect();
            schemes.sort();
            for scheme in schemes {
                if let Some(claims) = uri_claims.get(scheme) {
                    println!("{}", format!("{} Claims:", scheme).bold());
                    for claim in claims {
                        format_claim(claim, &current_agent, now);
                    }
                }
            }

            // Fallback if we had claims but no groupings (shouldn't happen)
            if file_claims.is_empty() && uri_claims.is_empty() {
                println!("{}", "Active Claims:".bold());
                for claim in &claims_list {
                    format_claim(claim, &current_agent, now);
                }
            }
        }
        OutputFormat::Text => {
            for claim in &claims_list {
                let display_patterns: Vec<String> =
                    claim.patterns.iter().map(|p| display_pattern(p)).collect();
                let remaining_mins = (claim.expires_at - now).num_minutes().max(0);
                let stale = presence::agent_presence(&claim.agent).is_stale();
                let reason = claim
                    .message
                    .as_ref()
                    .map(|m| format!("  \"{}\"", m))
                    .unwrap_or_default();

                println!(
                    "{}  {}  {}m remaining  stale:{}{}",
                    display_patterns.join(", "),
                    claim.agent,
                    remaining_mins,
                    stale,
                    reason
                );
            }
        }
    }

    Ok(())
}

/// Release file claims.
#[instrument(skip(patterns, agent), fields(pattern_count = patterns.len(), release_all))]
pub fn release(patterns: Vec<String>, release_all: bool, agent: Option<&str>) -> Result<()> {
    let agent_name = require_agent(agent)?;

    // Expand release patterns to absolute paths
    let expanded_patterns: Vec<String> = patterns.iter().map(|p| expand_pattern(p)).collect();

    let all_claims: Vec<FileClaim> = read_records(&claims_path())?;

    // Build active claims map
    let mut active: std::collections::HashMap<ulid::Ulid, FileClaim> =
        std::collections::HashMap::new();
    for claim in all_claims {
        active.insert(claim.id, claim);
    }

    let now = Utc::now();
    let mut released_count = 0;

    for claim in active.values() {
        // Only release our own claims
        if claim.agent != agent_name {
            continue;
        }

        // Skip inactive or expired
        if !claim.active || claim.expires_at < now {
            continue;
        }

        // Check if we should release this one
        let should_release = release_all
            || expanded_patterns.is_empty()
            || expanded_patterns.iter().any(|p| claim.patterns.contains(p));

        if should_release {
            let release_record = claim.release();
            append_record(&claims_path(), &release_record)?;
            released_count += 1;

            // Post release message (use absolute patterns for clarity)
            let display_patterns: Vec<String> =
                claim.patterns.iter().map(|p| display_pattern(p)).collect();
            let msg = Message::new(
                &agent_name,
                "claims",
                format!("Released {}", display_patterns.join(", ")),
            )
            .with_meta(MessageMeta::Release {
                patterns: display_patterns,
            });
            append_record(&channel_path("claims"), &msg)?;
        }
    }

    if released_count == 0 {
        println!("No claims to release.");
    } else {
        auto_commit_after_release(&data_dir(), "claims");
        println!(
            "{} Released {} claim(s)",
            "Success:".green(),
            released_count
        );
    }

    Ok(())
}

/// Output from check-claim command.
#[derive(Debug, Serialize)]
pub struct CheckClaimOutput {
    /// The file/pattern that was checked
    pub path: String,
    /// Whether the path is safe to edit (no conflicts)
    pub safe: bool,
    /// Conflicting claims (if any)
    pub conflicts: Vec<ClaimConflict>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ClaimConflict {
    /// Agent who holds the claim
    pub agent: String,
    /// The pattern that matches
    pub pattern: String,
    /// When the claim expires
    pub expires_at: DateTime<Utc>,
    /// Seconds until expiration
    pub expires_in_secs: i64,
    /// True when the blocking agent's presence heartbeat has lapsed - a
    /// caller can use this to tell "blocked by a live agent" (false, go
    /// coordinate) from "blocked by a corpse" (true, holder is presumed
    /// gone) apart. Report only: the conflict still stands either way,
    /// nothing here releases the blocking claim.
    pub stale: bool,
}

/// Check if a file/pattern conflicts with existing claims.
/// Returns Ok(true) if safe, Ok(false) if conflict exists.
/// Exits with code 1 on conflict for easy shell scripting.
pub fn check_claim(path: String, format: OutputFormat, agent: Option<&str>) -> Result<bool> {
    let current_agent = resolve_agent(agent).unwrap_or_default();
    let now = Utc::now();

    // Expand path to absolute for matching against stored claims
    let expanded_path = expand_pattern(&path);

    let all_claims: Vec<FileClaim> = read_records(&claims_path()).unwrap_or_default();

    // Build active claims map
    let mut active: std::collections::HashMap<ulid::Ulid, FileClaim> =
        std::collections::HashMap::new();
    for claim in all_claims {
        active.insert(claim.id, claim);
    }

    // Find conflicts
    let mut conflicts = Vec::new();
    for claim in active.values() {
        // Skip our own claims
        if claim.agent == current_agent {
            continue;
        }

        // Skip inactive or expired
        if !claim.active || claim.expires_at < now {
            continue;
        }

        // Check if our path matches any of their patterns
        for pattern in &claim.patterns {
            if path_matches_pattern(&expanded_path, pattern) {
                conflicts.push(ClaimConflict {
                    agent: claim.agent.clone(),
                    pattern: display_pattern(pattern), // Show relative when possible
                    expires_at: claim.expires_at,
                    expires_in_secs: (claim.expires_at - now).num_seconds(),
                    stale: presence::agent_presence(&claim.agent).is_stale(),
                });
            }
        }
    }

    let safe = conflicts.is_empty();

    let output = CheckClaimOutput {
        path: path.clone(),
        safe,
        conflicts,
        advice: vec![], // Informational command, no advice needed
    };

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Pretty => {
            if safe {
                println!("{} {} is safe to edit", "✓".green(), path.cyan());
            } else {
                println!("{} {} has conflicts:", "✗".red(), path.cyan());
                for conflict in &output.conflicts {
                    let expires = format_duration(conflict.expires_in_secs as u64);
                    let stale_marker = if conflict.stale {
                        " [STALE - holder unreachable]".red().to_string()
                    } else {
                        String::new()
                    };
                    println!(
                        "  {} claimed {} (expires in {}){}",
                        conflict.agent.yellow(),
                        conflict.pattern.dimmed(),
                        expires,
                        stale_marker
                    );
                }
            }
        }
        OutputFormat::Text => {
            if safe {
                println!("unclaimed  {}", path);
            } else {
                for conflict in &output.conflicts {
                    let expires = format_duration(conflict.expires_in_secs as u64);
                    println!(
                        "claimed  {}  {}  \"expires in {}\"  stale:{}",
                        conflict.agent, path, expires, conflict.stale
                    );
                }
            }
        }
    }

    Ok(safe)
}

/// Check if a specific path/URI matches a glob pattern.
/// For URIs, uses prefix matching with wildcard support.
fn path_matches_pattern(path: &str, pattern: &str) -> bool {
    // Handle URI patterns
    if is_uri(pattern) || is_uri(path) {
        return uri_matches_pattern(path, pattern);
    }

    // Try glob matching for file paths
    if let Ok(glob) = Glob::new(pattern) {
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        if let Ok(set) = builder.build()
            && set.is_match(path)
        {
            return true;
        }
    }

    // Also check prefix matching for directory patterns
    let pattern_base = pattern
        .split("**")
        .next()
        .unwrap_or(pattern)
        .trim_end_matches('/');
    if !pattern_base.is_empty() && path.starts_with(pattern_base) {
        return true;
    }

    false
}

/// Check if a URI matches a URI pattern.
/// Supports exact match and wildcard suffix (e.g., "bead://project/*" matches "bead://project/bd-123")
fn uri_matches_pattern(uri: &str, pattern: &str) -> bool {
    // Exact match
    if uri == pattern {
        return true;
    }

    // Wildcard pattern: "scheme://path/*" matches "scheme://path/anything"
    if pattern.ends_with("/*") {
        let prefix = &pattern[..pattern.len() - 1]; // Remove trailing *
        if uri.starts_with(prefix) {
            return true;
        }
    }

    // Also support "**" suffix for consistency with file globs
    if pattern.ends_with("/**") {
        let prefix = &pattern[..pattern.len() - 2]; // Remove trailing **
        if uri.starts_with(prefix) {
            return true;
        }
    }

    // Prefix matching: if pattern is a prefix of URI (without wildcards)
    // e.g., "bead://project" matches "bead://project/bd-123"
    if !pattern.contains('*') && uri.starts_with(pattern) {
        // Ensure we're matching at a boundary (/ or end)
        let remainder = &uri[pattern.len()..];
        if remainder.is_empty() || remainder.starts_with('/') {
            return true;
        }
    }

    false
}

/// Check for conflicts including the agent's own claims.
/// Used when we want to fail if ANY claim (including our own) holds the pattern.
fn check_conflicts_including_self(
    new_patterns: &[String],
    claims: &[FileClaim],
) -> Vec<(String, String, chrono::DateTime<Utc>)> {
    let mut conflicts = Vec::new();
    let now = Utc::now();

    // Build active claims map
    let mut active: std::collections::HashMap<ulid::Ulid, &FileClaim> =
        std::collections::HashMap::new();
    for claim in claims {
        active.insert(claim.id, claim);
    }

    for claim in active.values() {
        // Skip inactive or expired
        if !claim.active || claim.expires_at < now {
            continue;
        }

        // Check for overlaps (including our own claims)
        for new_pat in new_patterns {
            for existing_pat in &claim.patterns {
                if patterns_overlap(new_pat, existing_pat) {
                    conflicts.push((new_pat.clone(), claim.agent.clone(), claim.expires_at));
                }
            }
        }
    }

    conflicts
}

fn patterns_overlap(a: &str, b: &str) -> bool {
    // Handle URI patterns
    if is_uri(a) || is_uri(b) {
        return uri_patterns_overlap(a, b);
    }

    // Build glob matchers for file patterns
    let mut builder_a = GlobSetBuilder::new();
    let mut builder_b = GlobSetBuilder::new();

    if let Ok(glob) = Glob::new(a) {
        builder_a.add(glob);
    }
    if let Ok(glob) = Glob::new(b) {
        builder_b.add(glob);
    }

    let set_a = builder_a.build().ok();
    let set_b = builder_b.build().ok();

    // Simple heuristic: if either pattern matches the other as a path, they overlap
    // This is not perfect but handles common cases

    // Check if b matches a as a literal path
    if let Some(ref set) = set_b
        && set.is_match(a)
    {
        return true;
    }

    // Check if a matches b as a literal path
    if let Some(ref set) = set_a
        && set.is_match(b)
    {
        return true;
    }

    // Check for common prefix (simple heuristic)
    let a_base = a.split("**").next().unwrap_or(a).trim_end_matches('/');
    let b_base = b.split("**").next().unwrap_or(b).trim_end_matches('/');

    if !a_base.is_empty()
        && !b_base.is_empty()
        && (a_base.starts_with(b_base) || b_base.starts_with(a_base))
    {
        return true;
    }

    false
}

/// Check if two URI patterns overlap.
fn uri_patterns_overlap(a: &str, b: &str) -> bool {
    // Different schemes can't overlap
    let scheme_a = a.split("://").next().unwrap_or("");
    let scheme_b = b.split("://").next().unwrap_or("");
    if scheme_a != scheme_b {
        return false;
    }

    // agent:// claims use hierarchy for parent/subagent relationships.
    // agent://root and agent://root/sub are distinct presence claims that coexist.
    // Only exact matches conflict (handled below via uri_matches_pattern).
    if scheme_a == "agent" {
        return a == b;
    }

    // Check if either matches the other
    if uri_matches_pattern(a, b) || uri_matches_pattern(b, a) {
        return true;
    }

    // Check prefix overlap (without wildcards)
    let base_a = a.trim_end_matches('*').trim_end_matches('/');
    let base_b = b.trim_end_matches('*').trim_end_matches('/');

    if !base_a.is_empty()
        && !base_b.is_empty()
        && (base_a.starts_with(base_b) || base_b.starts_with(base_a))
    {
        return true;
    }

    false
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_patterns_overlap() {
        assert!(patterns_overlap("src/**/*.rs", "src/main.rs"));
        assert!(patterns_overlap("src/auth/**", "src/auth/login.rs"));
        assert!(!patterns_overlap("src/api/**", "tests/**"));
    }

    #[test]
    fn test_path_matches_pattern() {
        assert!(path_matches_pattern("src/main.rs", "src/**"));
        assert!(path_matches_pattern("src/auth/login.rs", "src/auth/**"));
        assert!(path_matches_pattern("Cargo.toml", "*.toml"));
        assert!(!path_matches_pattern("tests/foo.rs", "src/**"));
    }

    #[test]
    fn test_expand_pattern() {
        // Absolute paths pass through unchanged
        assert_eq!(expand_pattern("/home/user/src/**"), "/home/user/src/**");

        // Relative paths get expanded (depends on cwd, so just check it's absolute)
        let expanded = expand_pattern("src/**");
        assert!(
            expanded.starts_with('/'),
            "Should be absolute: {}",
            expanded
        );
        assert!(
            expanded.ends_with("src/**"),
            "Should preserve glob: {}",
            expanded
        );
    }

    #[test]
    fn test_display_pattern() {
        // All patterns are displayed as-is (absolute paths for clarity)
        assert_eq!(
            display_pattern("/some/other/path/**"),
            "/some/other/path/**"
        );
        assert_eq!(display_pattern("/home/user/src/**"), "/home/user/src/**");

        // URIs pass through unchanged
        assert_eq!(
            display_pattern("bead://project/bd-123"),
            "bead://project/bd-123"
        );
    }

    // === URI claim tests ===

    #[test]
    fn test_is_uri() {
        // Valid URIs
        assert!(is_uri("bead://project/bd-123"));
        assert!(is_uri("db://myapp/users"));
        assert!(is_uri("port://8080"));
        assert!(is_uri("file:///home/user/file.txt"));
        assert!(is_uri("http://example.com"));

        // Not URIs (file paths)
        assert!(!is_uri("src/main.rs"));
        assert!(!is_uri("/home/user/src/**"));
        assert!(!is_uri("*.toml"));
        assert!(!is_uri("src:file.rs")); // No ://
    }

    #[test]
    fn test_expand_pattern_uri() {
        // URIs pass through unchanged
        assert_eq!(expand_pattern("bead://rite/bd-123"), "bead://rite/bd-123");
        assert_eq!(expand_pattern("db://myapp/users"), "db://myapp/users");
        assert_eq!(expand_pattern("port://8080"), "port://8080");
    }

    #[test]
    fn test_display_pattern_uri() {
        // URIs display as-is
        assert_eq!(display_pattern("bead://rite/bd-123"), "bead://rite/bd-123");
        assert_eq!(display_pattern("db://myapp/*"), "db://myapp/*");
    }

    #[test]
    fn test_uri_matches_pattern() {
        // Exact match
        assert!(uri_matches_pattern(
            "bead://rite/bd-123",
            "bead://rite/bd-123"
        ));

        // Wildcard suffix
        assert!(uri_matches_pattern("bead://rite/bd-123", "bead://rite/*"));
        assert!(uri_matches_pattern("db://myapp/users", "db://myapp/*"));

        // Double-star wildcard
        assert!(uri_matches_pattern("bead://rite/bd-123", "bead://rite/**"));

        // Prefix matching
        assert!(uri_matches_pattern("bead://rite/bd-123", "bead://rite"));

        // Non-matches
        assert!(!uri_matches_pattern("bead://other/bd-123", "bead://rite/*"));
        assert!(!uri_matches_pattern("db://myapp/users", "bead://myapp/*"));
    }

    #[test]
    fn test_uri_patterns_overlap() {
        // Same URI
        assert!(uri_patterns_overlap(
            "bead://rite/bd-123",
            "bead://rite/bd-123"
        ));

        // Wildcard overlaps specific
        assert!(uri_patterns_overlap("bead://rite/*", "bead://rite/bd-123"));
        assert!(uri_patterns_overlap("bead://rite/bd-123", "bead://rite/*"));

        // Different schemes don't overlap
        assert!(!uri_patterns_overlap(
            "bead://rite/bd-123",
            "db://rite/bd-123"
        ));

        // Different paths don't overlap
        assert!(!uri_patterns_overlap(
            "bead://project-a/bd-123",
            "bead://project-b/bd-456"
        ));

        // agent:// claims: parent and subagent don't overlap
        assert!(!uri_patterns_overlap(
            "agent://leader",
            "agent://leader/worker-1"
        ));
        assert!(!uri_patterns_overlap(
            "agent://leader/worker-1",
            "agent://leader"
        ));

        // agent:// claims: different subagents don't overlap
        assert!(!uri_patterns_overlap(
            "agent://leader/worker-1",
            "agent://leader/worker-2"
        ));

        // agent:// claims: exact match still overlaps
        assert!(uri_patterns_overlap("agent://leader", "agent://leader"));
        assert!(uri_patterns_overlap(
            "agent://leader/worker-1",
            "agent://leader/worker-1"
        ));
    }

    #[test]
    fn test_path_matches_pattern_mixed() {
        // File paths don't match URIs
        assert!(!path_matches_pattern("src/main.rs", "bead://rite/*"));
        assert!(!path_matches_pattern("bead://rite/bd-123", "src/**"));
    }

    // Integration tests that need project setup are moved to tests/integration/
    // since they require the global data directory to be mocked
}
