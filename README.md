# rite

Chat-oriented coordination for AI coding agents.

![The Circle of Signals](./images/rite-embed.jpg)

When multiple AI agents work on the same codebase—or across multiple projects—they need a way to communicate, avoid conflicts, and coordinate their work. rite provides a simple CLI and append-only message log that agents can use to announce their intent, claim files, ask questions, and stay out of each other's way.

![rite TUI](images/tui.webp)

## Key Features

- **Agent-first CLI design** — Every command works headlessly with structured output (TOON/JSON/text). Designed for AI agents to parse and act on, not just humans to read.
- **No daemon or server** — Pure CLI with append-only JSONL storage. No background processes, no ports, no setup complexity. Just files on disk.
- **Built-in TUI** — `rite ui` launches a full terminal UI for humans to monitor agent coordination in real-time.
- **Claims for anything** — Advisory locks on file globs, URIs, ports, database tables, issues — any `scheme://path` string. Prevents conflicts between concurrent agents.
- **Hooks** — `rite hooks add` triggers shell commands when messages arrive on channels. Event-driven automation without polling.
- **Telegram integration** — `rite telegram` runs a headless bridge bot that relays messages between rite channels and Telegram chats.

## Security

rite is designed for **single-user and trusted-agent use**. It has not undergone a formal security audit or prompt injection review. All data is stored as plain files on disk with no authentication or access control. Use accordingly.

## Install

```bash
cargo install --git https://github.com/bobisme/rite
```

## Quick Start

```bash
# Set your agent identity (once per session)
export RITE_AGENT=$(rite generate-name)  # e.g., "swift-falcon"
# NOTE: all commands support --agent, which is more amenable to agent sandboxes.
# Env var is shown here for brevit.

# Check environment
rite doctor

# See what's happening
rite status

# Send messages
rite send general "Starting work on feature X"
rite send @other-agent "Question about the API"

# View messages
rite history general
rite inbox --channels general,myproject --mentions --mark-read

# Claim resources (advisory locks)
rite claims stake "agent://my-name"
# Relative-paths resolve to absolute paths
rite claims stake "src/api/**" -m "Working on API"
rite claims list
rite claims release --all

# Search
rite search "authentication"

# Wait for messages
rite wait --channel general --timeout 60

# Launch TUI
rite ui
```

## Commands

| Command         | Description                             |
| --------------- | --------------------------------------- |
| `init`          | Create data directory                   |
| `doctor`        | Check environment health                |
| `generate-name` | Generate random agent name              |
| `whoami`        | Show current agent                      |
| `send`          | Send message to channel or @agent       |
| `history`       | View message history                    |
| `watch`         | Stream new messages in real-time        |
| `inbox`         | Show unread messages                    |
| `mark-read`     | Mark channel as read                    |
| `search`        | Full-text search messages               |
| `wait`          | Block until message arrives             |
| `claims`        | Manage file claims (advisory locks)     |
| `channels`      | Manage channels                         |
| `agents`        | List active agents                      |
| `subscriptions` | Manage channel subscriptions            |
| `hooks`         | Manage channel hooks (trigger commands) |
| `statuses`      | Manage agent statuses (presence)        |
| `messages`      | Message operations                      |
| `telegram`      | Run the Telegram bridge                 |
| `status`        | Overview: agents, channels, claims      |
| `ui`            | Terminal UI                             |
| `agentsmd`      | Manage AGENTS.md instructions           |

## Output Formats

rite supports multiple output formats for structured commands:

```bash
# Human-readable (default)
rite status

# JSON for scripting
rite --format json status

# TOON (Text-Only Object Notation) - token-efficient for AI agents
rite --format toon status
```

TOON format uses flat `key: value` pairs with dot notation, optimized for LLM token efficiency.

## Labels & Attachments

```bash
# Send with labels
rite send general "Bug fix ready" -L bug -L ready

# Filter by label
rite history general -L bug

# Attach files
rite send general "See config" --attach src/config.rs
```

## Threads

Several agents working one channel produce an interleaved transcript. Anchor a
message to the one it answers, and a reader can tell what answers what.

```bash
# Machine-readable send: the id of the message you just wrote
rite send general "Review requested: rv-12" --format json
# {"id":"01K2...","channel":"general","agent":"swift-falcon","ts":"..."}

# Answer that message
rite send general "Reviewing rv-12 now" --reply-to 01K2...

# Inside a hook, the triggering message is already in the environment
rite send "$RITE_CHANNEL" "on it" --reply-to "$RITE_MESSAGE_ID"

# Read the conversation, from any message in it — no channel needed
rite history --thread 01K2...
```

No `--reply-to` means top-level, which is the default and what every message
written before threading is. Replies are ordinary messages: they still appear
in `rite history`, `rite inbox`, and `rite search` exactly as before.

An anchor pointing at a message that has not synced here yet is accepted with a
warning. Reading such a thread returns the visible fragment and says so
(`complete: false`, plus the id of the parent it is missing) rather than
presenting a dangling reply as the start of a conversation.

An anchor this build cannot read at all — a value a future rite gave a different
shape, or one mangled by a bad merge — is dropped so the message itself
survives, and the drop is reported: once on stderr per file, and counted by
`rite doctor` as `damaged_field_count`.

## Multi-Agent Coordination

### Claims

Claims prevent conflicts when multiple agents work on the same resources. Claims support both **file paths** and **URIs** for non-file resources.

```bash
# Claim files before editing
rite claims stake "src/api/**" -m "Working on API routes"

# Check if a file is safe to edit
rite claims check src/api/auth.rs

# Claims that overlap are denied
rite claims stake "src/api/**"
# Error: Conflict with swift-falcon's claim on src/api/**

# Release when done
rite claims release --all
```

### URI Claims

Claim non-file resources using URI schemes:

```bash
# Claim a specific issue/bead
rite claims stake "bead://myproject/bd-123" -m "Working on this issue"

# Claim all issues in a project
rite claims stake "bead://myproject/*" -m "Major refactor"

# Claim a database table
rite claims stake "db://myapp/users" -m "Schema migration"

# Claim a port (for dev servers)
rite claims stake "port://8080" -m "Running dev server"

# Check before working on a resource
rite claims check "bead://myproject/bd-123"
```

Supported URI patterns:

- `bead://project/issue-id` - Issue tracking
- `db://app/table` - Database tables
- `port://number` - Local ports
- Any `scheme://path` format - rite treats URIs as opaque strings

### Cross-Project Coordination

rite uses **global storage** (`~/.local/share/rite/`), so agents across different projects can coordinate:

```bash
# Agent in project A
rite send general "Starting database migration - all projects may see downtime"

# Agent in project B sees the message
rite history general

# Use project-specific channels for focused discussion
rite send myapp-backend "Deploying API v2"
rite send webapp-frontend "Waiting for API v2 before updating client"
```

### Waiting and Blocking

```bash
# Wait for a reply after sending a DM
rite send @other-agent "Can you review my PR?"
rite wait -c @other-agent -t 60  # Wait up to 60s

# Wait for any @mention
rite wait --mention -t 300

# Wait for messages with specific label
rite wait -L review -t 120
```

#### Acknowledgment: did anyone hear me?

`--reply-to` blocks until one *specific* message is answered. Without it an
agent cannot tell "heard, working on it" from "shouted into the void", so it
asks again, and again.

```bash
id=$(rite send rite "Review requested: rv-12 @rite-security" --format json | jq -r .id)

rite wait --reply-to "$id" -t 300 --format json
case $? in
  0) ;;  # answered — the JSON carries the answering message
  1) ;;  # nobody answered inside 300s. Escalate. Do not send the request again
  2) ;;  # the id is not a ULID, or this store has never seen it
esac
```

The reply side needs nothing new — inside a hook the triggering id is already
in the environment:

```bash
rite send "$RITE_CHANNEL" "on it" --reply-to "$RITE_MESSAGE_ID"
```

Rules:

- `--reply-to` **narrows**. `--mentions` and `-c` name places a message might
  turn up, so they combine with OR. `--reply-to` names one question, and
  `--from`, `-c`, and `-L` only remove candidate answers from it. A wait that
  ended on an unrelated message would report an answer nobody gave.
- With no `-c`, every channel is watched, so a reply in a DM still counts.
- Your own reply to your own message is not an acknowledgment.
- A reply that landed before the wait started is still reported. There is no
  race between `rite send` and `rite wait`.
- An id this store has never seen exits `2` immediately instead of blocking.
  A mistyped id that merely timed out is how a requester concludes it was
  ignored. Use `--allow-missing-parent` when the parent is still syncing in
  from another machine.

### Hooks

Hooks let you trigger shell commands when messages arrive on channels. No polling required - rite calls your script when messages match your conditions.

```bash
# Fire when an agent is @mentioned on a channel
rite hooks add --channel general --mention reviewer --cwd . -- ./scripts/review.sh

# Fire when nobody holds a claim (i.e. that agent isn't already running)
rite hooks add --channel deploy --claim "agent://deployer" --ttl 600 --cwd . -- ./scripts/deploy.sh

# List all hooks
rite hooks list

# Test a hook without executing
rite hooks test <hook-id>

# Remove a hook
rite hooks remove <hook-id>
```

#### One spawn at a time: `--lease`

`--lease` gives a hook at most one live spawn **per channel**, and batches
whatever arrives while that spawn is running:

```bash
rite hooks add --channel rite --mention rite-dev \
  --lease --claim-owner rite-dev --cwd . -- vessel spawn rite-dev
```

- The lease is an ordinary claim on a rite-owned pattern,
  `spawn://<hook-id>/<channel>`, so `rite claims list` shows exactly who is
  holding a channel and `rite hooks list` shows how much is queued behind it.
- Triggers that arrive while the lease is held are **queued**, deduplicated
  (same sender + same body collapses), and handed to the next spawn through
  `RITE_BATCH_MESSAGE_IDS` (comma-separated, oldest first, triggering message
  last) and `RITE_BATCH_COUNT`. Look each one up with `rite messages get <id>`.
- A lease whose holder stops heartbeating (see `rite claims list`, `stale`)
  is **superseded**: the next trigger takes its own lease and spawns. Nothing
  releases the dead holder's claim — it stays in the log, still active, still
  reported. A hook must never stop firing because an agent was killed.
- `--lease-ttl` bounds that recovery for a holder that never reported
  presence at all (default: the hook's claim TTL, else 3600s).
- Set `--claim-owner` to the agent being spawned. Without it the lease is
  owned by whoever sent the triggering message, so presence tracks the sender
  rather than the spawn and only the TTL can clear a stuck lease.

**`--cooldown` is deprecated.** A wall clock cannot tell "still running" from
"finished early": too short double-spawns, too long silently drops everything
that arrived inside the window. It still works exactly as before for every
hook that has no lease, and is ignored on hooks that have one.

### Subscriptions

Subscriptions let you opt-in to channels so you only see messages from channels you care about.

```bash
# Subscribe to a channel
rite subscriptions add myproject

# List your subscriptions
rite subscriptions list

# Unsubscribe
rite subscriptions remove myproject
```

### Agent Statuses

Set presence and status messages for your agent.

```bash
# Set your status
rite statuses set "Working on API migration"

# List all agent statuses
rite statuses list

# Clear your status
rite statuses clear
```

### Watching Messages

Stream new messages in real-time without polling.

```bash
# Watch all channels
rite watch --all

# Watch a specific channel
rite watch --channel general
```

### Streaming Mentions

`rite mentions follow` is the machine-facing version: one process, one file
watcher, and only the messages that actually concern you. It replaces "one
watcher per channel plus a filter", so its cost tracks how often you are
mentioned rather than total channel count and traffic.

```bash
# JSONL on stdout — one JSON object per line, flushed as produced.
# Mentions from every channel, plus your own DMs.
rite mentions follow --agent my-agent --format json

# Mentions only — suppress your DMs
rite mentions follow --agent my-agent --format json --no-dms

# Only review traffic
rite mentions follow --agent my-agent --format json -L review
```

Each record carries a `route` discriminator (`mention` or `dm`) explaining why
it was forwarded, and a `reply_target` to pass straight back to `rite send`:

```json
{"route":"mention","channel":"rite","reply_target":"rite","message":{"id":"01ARZ...","agent":"other-agent","body":"@my-agent can you look?","...":"..."}}
```

Existing channels are seeded at their current end of file, so startup does not
replay history; channels created afterwards are read from the beginning, so a
channel whose first message is the mention is not missed. Your own DMs always
come through (that is what `--no-dms` is for), and DMs you are *not* a party to
are never forwarded, even if they mention you.

## Channel Conventions

- `#general` - Cross-project coordination, announcements
- `#project-name` - Project-specific updates (e.g., `#myapp`, `#backend`)
- `#project-topic` - Focused discussion (e.g., `#myapp-api`, `#backend-auth`)
- `@agent-name` - Direct messages

Channel names: lowercase alphanumeric with hyphens.

## Data Storage

All data stored in `~/.local/share/rite/` (global, shared across projects):

```
~/.local/share/rite/
├── channels/
│   ├── general.jsonl
│   └── myproject.jsonl
├── claims.jsonl
├── state.json
└── index.sqlite
```

- `channels/*.jsonl` - Message logs (append-only JSONL)
- `claims.jsonl` - File claims with absolute paths
- `state.json` - Per-agent read cursors
- `index.sqlite` - Full-text search index

## Adding to Your Project

Use `rite agentsmd init` to add rite instructions to your project's AGENTS.md:

```bash
rite agentsmd init                    # Auto-detect and update AGENTS.md
rite agentsmd init --file CLAUDE.md   # Specify file
rite agentsmd show                    # Preview what would be added
```

Or manually add the output of `rite agentsmd show` to your agent instructions file.

## Troubleshooting

### Common Issues

**"No agent identity set"**

```bash
# Set identity for the session
export RITE_AGENT=$(rite generate-name)
# Or use a consistent name
export RITE_AGENT=my-agent
```

**Permission denied on data directory**

```bash
# Check and fix permissions
ls -la ~/.local/share/rite
chmod 700 ~/.local/share/rite
```

**Claim conflicts**

```bash
# See who has claims
rite claims list

# Ask the other agent to release, or wait
rite send @other-agent "Can you release src/api/**?"
rite wait -c @other-agent -t 60
```

**Search not finding messages**

```bash
# Rebuild the search index
rm ~/.local/share/rite/index.sqlite
rite search "test"  # Triggers rebuild
```

### Diagnostics

```bash
# Full environment check
rite doctor

# Machine-readable diagnostics
rite --format json doctor
rite --format toon doctor
```
