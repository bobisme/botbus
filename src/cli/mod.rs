use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

pub mod agents;
pub mod agentsmd;
pub mod channels;
pub mod claim;
pub mod doctor;
pub mod format;
pub mod history;
pub mod hooks;
pub mod inbox;
pub mod index;
pub mod init;
pub mod mark_read;
pub mod mentions;
pub mod messages;
pub mod names;
pub mod search;
pub mod send;
pub mod status;
pub mod statuses;
pub mod subscribe;
pub mod sync;
pub mod telegram;
pub mod tldr;
pub mod ui;
pub mod wait;
pub mod watch;
pub mod whoami;

/// Output format for structured data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable colored text
    Pretty,
    /// Concise text for AI agents
    #[value(alias = "toon")]
    Text,
    /// JSON - standard machine-readable format
    Json,
}

#[derive(Parser)]
#[command(
    name = "rite",
    author,
    version,
    about = "Chat-oriented coordination for AI coding agents",
    long_about = None,
    after_help = tldr::QUICK_REFERENCE
)]
pub struct Cli {
    /// Agent identity (default: from RITE_AGENT env var)
    #[arg(short, long, global = true, env = "RITE_AGENT")]
    pub agent: Option<String>,

    /// Suppress non-essential output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Increase verbosity
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Hidden alias for --format json (agents frequently guess this)
    #[arg(long, global = true, hide = true)]
    pub json: bool,

    /// Output format: pretty (default for TTY), text (default for pipes), or json
    #[arg(long, global = true, value_enum)]
    pub format: Option<OutputFormat>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize the Rite data directory
    Init,

    /// Check environment health and configuration
    Doctor,

    /// Generate a random agent name (kebab-case)
    GenerateName,

    /// Display current agent identity
    Whoami {
        /// Suggest agent name as <project>-<suffix>
        #[arg(long)]
        suggest_project_suffix: Option<String>,
    },

    /// Show quick command reference
    Tldr,

    /// Send a message to a channel or agent
    #[command(alias = "post")]
    Send {
        /// Channel name or @agent for DM
        target: String,

        /// Message content
        message: String,

        /// Attach metadata (JSON)
        #[arg(long)]
        meta: Option<String>,

        /// Add label(s) to the message (can be used multiple times)
        #[arg(short = 'L', long = "label", action = clap::ArgAction::Append)]
        labels: Vec<String>,

        /// Attach file(s) (can be used multiple times)
        #[arg(long = "attach", action = clap::ArgAction::Append)]
        attachments: Vec<String>,

        /// Don't fire hooks for this message
        #[arg(long)]
        no_hooks: bool,
    },

    /// View message history
    #[command(aliases = &["read", "show"])]
    History {
        /// Channel to view (default: general)
        channel: Option<String>,

        /// Channel to view (named alternative to positional)
        #[arg(short = 'c', long = "channel", hide = true)]
        channel_named: Option<String>,

        /// Number of messages (default: 50)
        #[arg(short = 'n', long, alias = "limit", default_value = "50")]
        count: usize,

        /// Follow mode (like tail -f)
        #[arg(short, long)]
        follow: bool,

        /// Exit follow mode after N seconds
        #[arg(long)]
        timeout: Option<u64>,

        /// Exit follow mode after receiving N new messages
        #[arg(long)]
        follow_count: Option<usize>,

        /// Messages after this time
        #[arg(long)]
        since: Option<String>,

        /// Messages before this time
        #[arg(long)]
        before: Option<String>,

        /// Filter by sender
        #[arg(long)]
        from: Option<String>,

        /// Filter by label (can be used multiple times - messages must have ANY of the labels)
        #[arg(short = 'L', long = "label", action = clap::ArgAction::Append)]
        labels: Vec<String>,

        /// Read messages after this byte offset (for incremental reading)
        #[arg(long)]
        after_offset: Option<u64>,

        /// Read messages after this message ID (ULID)
        #[arg(long)]
        after_id: Option<String>,

        /// Show offset info for next read
        #[arg(long)]
        show_offset: bool,

        /// Output format (default: text for history)
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
    },

    /// Stream new messages in real-time
    Watch {
        /// Channel to watch (default: all)
        #[arg(short, long)]
        channel: Option<String>,

        /// Watch all channels
        #[arg(long)]
        all: bool,
    },

    /// Manage channels
    #[command(aliases = &["list-channels", "ls"])]
    Channels {
        #[command(subcommand)]
        command: Option<ChannelsCommands>,
    },

    /// List agents (derived from message history)
    #[command(alias = "list-agents")]
    Agents {
        /// Only show recently active agents
        #[arg(long)]
        active: bool,
    },

    /// Full-text search messages
    Search {
        /// Search query (supports FTS5 syntax)
        query: String,

        /// Limit to channel
        #[arg(short, long)]
        channel: Option<String>,

        /// Max results (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        count: usize,

        /// Filter by sender
        #[arg(long)]
        from: Option<String>,
    },

    /// Manage file claims (advisory locks)
    Claims {
        #[command(subcommand)]
        command: ClaimsCommands,
    },

    /// Launch the terminal UI
    Ui {
        /// Start in this channel
        #[arg(short, long)]
        channel: Option<String>,
    },

    /// Mark a channel as read (for incremental reading)
    MarkRead {
        /// Channel to mark as read
        channel: String,

        /// Explicit byte offset (default: current end of file)
        #[arg(long)]
        offset: Option<u64>,

        /// Explicit last message ID
        #[arg(long)]
        last_id: Option<String>,
    },

    /// Show unread messages (uses stored read cursor)
    Inbox {
        /// Specific channel(s) to check (default: DMs only)
        #[arg(short, long, action = clap::ArgAction::Append)]
        channels: Vec<String>,

        /// Check all channels
        #[arg(long)]
        all: bool,

        /// Maximum total messages to show across all channels (default: 10)
        #[arg(short = 'n', long, default_value = "10")]
        count: usize,

        /// Maximum messages to show per channel
        #[arg(long)]
        limit_per_channel: Option<usize>,

        /// Mark as read after displaying
        #[arg(long)]
        mark_read: bool,

        /// Check all channels for @mentions of current agent
        #[arg(long)]
        mentions: bool,

        /// Only show the count of unread messages (no message content)
        #[arg(long)]
        count_only: bool,

        /// Output format (default: text for inbox)
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
    },

    /// Stream @mentions of this agent across all channels
    Mentions {
        #[command(subcommand)]
        command: MentionsCommands,
    },

    /// Show status overview
    Status,

    /// Wait for a message (blocking, with optional timeout)
    Wait {
        /// Wait for @mentions of current agent from any channel
        #[arg(long)]
        mentions: bool,

        /// Wait for messages in specific channel(s)
        #[arg(short, long, action = clap::ArgAction::Append)]
        channels: Vec<String>,

        /// Wait for messages with specific label(s) (can be used multiple times)
        #[arg(short = 'L', long = "label", action = clap::ArgAction::Append)]
        labels: Vec<String>,

        /// Wait only for messages from this agent
        #[arg(long)]
        from: Option<String>,

        /// Timeout in seconds (0 = no timeout)
        #[arg(short, long, default_value = "0")]
        timeout: u64,
    },

    /// Manage AGENTS.md workflow instructions
    #[command(name = "agentsmd")]
    AgentsMd {
        #[command(subcommand)]
        command: AgentsMdCommands,
    },

    /// Manage channel subscriptions
    Subscriptions {
        #[command(subcommand)]
        command: SubscriptionsCommands,
    },

    /// Manage channel hooks (trigger commands on messages)
    Hooks {
        #[command(subcommand)]
        command: HooksCommands,
    },

    /// Manage agent statuses (presence + status message)
    Statuses {
        #[command(subcommand)]
        command: StatusesCommands,
    },

    /// Run the Telegram bridge (headless bot)
    Telegram,

    /// Message operations
    Messages {
        #[command(subcommand)]
        command: MessagesCommands,
    },

    /// Git-based multi-machine sync
    Sync {
        #[command(subcommand)]
        command: Option<SyncCommands>,
    },

    /// Manage search index
    Index {
        #[command(subcommand)]
        command: IndexCommands,
    },
}

const MENTIONS_FOLLOW_HELP: &str = "\
Examples:
  # Machine consumer: one JSON object per line, flushed as it is produced.
  # Mentions from every channel, plus this agent's own DMs.
  rite mentions follow --agent my-agent --format json

  # Mentions only — suppress this agent's DMs
  rite mentions follow --agent my-agent --format json --no-dms

  # Only review traffic
  rite mentions follow --agent my-agent --format json -L review

  # Human view
  rite mentions follow --agent my-agent --format pretty

Agent workflow:
  # One long-lived process replaces one watcher per channel. Read stdout
  # line by line; each line is a complete record:
  #   {\"route\":\"mention\",\"channel\":\"rite\",\"reply_target\":\"rite\",\"message\":{...}}
  # Reply with:  rite send <reply_target> \"...\"";

#[derive(Subcommand)]
pub enum MentionsCommands {
    /// Stream messages mentioning this agent, plus its DMs, across all channels
    ///
    /// Emits one record per matching message and runs until killed. With
    /// `--format json` the stream is JSONL: exactly one JSON object per line,
    /// flushed as it is produced. There is no closing envelope because the
    /// stream has no end — each line is the envelope. `--format text` emits one
    /// two-space-delimited line per record (id, route, channel, agent, body);
    /// `--format pretty` is the human view.
    ///
    /// Every record carries a `route` discriminator saying why it was
    /// forwarded: "mention" (the message's mentions include this agent) or "dm"
    /// (a direct message this agent is a party to). It also carries
    /// `reply_target`, the argument to pass to `rite send`.
    ///
    /// This agent's own DMs are always delivered — a DM is the most direct form
    /// of address there is. Pass --no-dms for a mentions-only stream.
    ///
    /// Channels that already exist are seeded at their current end of file, so
    /// startup does not replay history. Channels created after startup are read
    /// from the beginning, so a channel whose first message is the mention is
    /// not missed.
    ///
    /// Mention matching is case-insensitive. DM privacy is absolute: a mention
    /// never routes a message out of a DM this agent is not a party to. This
    /// agent's own messages are never echoed back.
    #[command(after_help = MENTIONS_FOLLOW_HELP)]
    Follow {
        /// Suppress this agent's own DMs (default: DMs are streamed)
        #[arg(long)]
        no_dms: bool,

        /// Only stream messages with this label (repeatable; matches any)
        #[arg(short = 'L', long = "label", action = clap::ArgAction::Append)]
        labels: Vec<String>,

        /// Stop after N seconds (default: run until killed)
        #[arg(long)]
        timeout: Option<u64>,

        /// Stop after N records (default: uncapped)
        #[arg(short = 'n', long)]
        count: Option<usize>,
    },
}

#[derive(Subcommand)]
pub enum MessagesCommands {
    /// Get a message by ID
    Get {
        /// Message ID (ULID)
        id: String,
    },

    /// Delete a message by ID (appends a tombstone)
    Delete {
        /// Message ID (ULID) to delete
        id: String,

        /// Skip interactive confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum ChannelsCommands {
    /// List all channels
    List {
        /// Only show channels you've participated in (sent or mentioned)
        #[arg(long)]
        mine: bool,

        /// Show all channels including closed ones
        #[arg(long)]
        all: bool,
    },

    /// Close a channel (hide from listings, preserves history)
    Close {
        /// Channel to close
        channel: String,
    },

    /// Reopen a closed channel
    Reopen {
        /// Channel to reopen
        channel: String,
    },

    /// Delete a channel permanently (admin only)
    Delete {
        /// Channel to delete
        channel: String,
    },

    /// Rename a channel (admin only)
    Rename {
        /// Current channel name
        old_name: String,
        /// New channel name
        new_name: String,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum HooksCommands {
    /// Add a new channel hook
    Add {
        /// Channel that triggers this hook (default: "*" for all non-DM channels)
        #[arg(long)]
        channel: Option<String>,

        /// Claim pattern — acquire this claim when the hook fires (atomic check-and-stake).
        /// For claim-only hooks, the hook fires when the pattern is available.
        /// Can be combined with --mention to acquire a claim when the mention fires.
        #[arg(long)]
        claim: Option<String>,

        /// Agent mention — fire when this agent is @mentioned.
        /// Can be combined with --claim to acquire a claim when the mention fires.
        #[arg(long)]
        mention: Option<String>,

        /// Working directory for the command
        #[arg(long)]
        cwd: PathBuf,

        /// Cooldown between firings (e.g., "30s", "5m", "1h"; default: 30s)
        #[arg(long)]
        cooldown: Option<String>,

        /// Claim TTL in seconds (acquire claim when hook fires, hold for this duration)
        #[arg(long, conflicts_with = "release_on_exit")]
        ttl: Option<u64>,

        /// Release the claim when the spawned command exits
        #[arg(long, conflicts_with = "ttl")]
        release_on_exit: bool,

        /// Agent that should own the claim (default: message sender)
        #[arg(long)]
        claim_owner: Option<String>,

        /// Priority for hook execution (lower runs first, Unix convention; default: 0)
        #[arg(long, default_value = "0")]
        priority: i32,

        /// Only fire this hook if the message contains the specified !flag (e.g., "dev" for !dev)
        #[arg(long)]
        require_flag: Option<String>,

        /// Optional description for identification/deduplication (e.g., "botbox:respond:general")
        #[arg(long)]
        description: Option<String>,

        /// Command to execute (place after --)
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },

    /// List all active hooks
    List,

    /// Remove (deactivate) a hook
    Remove {
        /// Hook ID to remove (e.g., "hk-abc")
        hook_id: String,
    },

    /// Dry-run test a hook (evaluate condition without executing)
    Test {
        /// Hook ID to test
        hook_id: String,
    },
}

#[derive(Subcommand)]
pub enum ClaimsCommands {
    /// Claim files for editing (advisory lock)
    Stake {
        /// Glob patterns to claim (relative paths expanded to absolute)
        patterns: Vec<String>,

        /// Time-to-live in seconds (default: 3600)
        #[arg(short, long, default_value = "3600")]
        ttl: u64,

        /// Optional message about the claim
        #[arg(short, long)]
        message: Option<String>,
    },

    /// Extend TTL on existing claims
    Refresh {
        /// Glob patterns to refresh (matches existing claims)
        patterns: Vec<String>,

        /// New time-to-live in seconds (default: 3600)
        #[arg(short, long, default_value = "3600")]
        ttl: u64,
    },

    /// Release file claims
    Release {
        /// Patterns to release (default: all your claims)
        patterns: Vec<String>,

        /// Release all your claims
        #[arg(long)]
        all: bool,
    },

    /// List active file claims
    List {
        /// Include expired claims
        #[arg(long)]
        all: bool,

        /// Only show my claims
        #[arg(long)]
        mine: bool,

        /// Limit output to N most recent claims
        #[arg(short = 'n', long)]
        limit: Option<usize>,

        /// Show claims created after this time (e.g., "2h ago", "2026-01-28")
        #[arg(long)]
        since: Option<String>,
    },

    /// Check if a file is claimed by another agent
    Check {
        /// File path or pattern to check
        path: String,
    },
}

#[derive(Subcommand)]
pub enum SubscriptionsCommands {
    /// Subscribe to a channel
    Add {
        /// Channel to subscribe to
        channel: String,
    },

    /// Unsubscribe from a channel
    Remove {
        /// Channel to unsubscribe from
        channel: String,
    },

    /// List subscribed channels
    List,
}

#[derive(Subcommand)]
pub enum StatusesCommands {
    /// Set your status message
    Set {
        /// Status message (max 32 characters)
        message: String,

        /// How long the status lasts (e.g., "1h", "30m", "8h"; default: 1h)
        #[arg(short, long, default_value = "1h")]
        ttl: String,
    },

    /// Clear your status
    Clear,

    /// List all agent statuses
    List,
}

#[derive(Subcommand)]
pub enum AgentsMdCommands {
    /// Generate or update AGENTS.md with Rite workflow instructions
    Init {
        /// Explicit file path (default: auto-detect AGENTS.md, CLAUDE.md, etc.)
        #[arg(long)]
        file: Option<PathBuf>,

        /// Remove Rite instructions instead of adding/updating
        #[arg(long)]
        remove: bool,
    },

    /// Print the Rite section that would be added to AGENTS.md
    Show,
}

#[derive(Subcommand)]
pub enum SyncCommands {
    /// Initialize git repository in data directory
    Init {
        /// Remote URL (e.g., git@github.com:user/rite-data.git)
        #[arg(long)]
        remote: Option<String>,
    },

    /// Push local commits to remote
    Push,

    /// Pull and merge changes from remote
    Pull,

    /// Show git status (uncommitted changes, ahead/behind)
    Status,

    /// Show recent git commits in the data directory
    Log {
        /// Number of commits to show (default: 10)
        #[arg(short = 'n', long, default_value = "10")]
        count: usize,
    },

    /// Check sync repository health
    Check,

    /// Commit all uncommitted changes in the data directory
    Commit {
        /// Commit message (default: "chore: manual commit")
        #[arg(short, long)]
        message: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum IndexCommands {
    /// Rebuild the search index from JSONL files
    Rebuild {
        /// Only rebuild if JSONL files are newer than index
        #[arg(long)]
        if_needed: bool,
    },

    /// Show index status (whether rebuild is needed)
    Status,
}
