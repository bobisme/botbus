# Rite CLI Output Guide for AI Agents

Rite outputs are designed to be readable by both humans and AI agents. This guide explains how to interpret command outputs effectively.

## Output Formats

Rite supports three output formats (controlled by `--format` flag):

1. **TOON (default)**: Text-Only Object Notation - optimized for AI agents
   - Structured key-value format
   - Minimal tokens, maximum information density
   - Easy to parse line-by-line
   - Example: `total_unread: 5`

2. **JSON**: Machine-readable structured data
   - Use when piping to other tools
   - Provides complete nested structures
   - Example: `{"total_unread": 5, "channels": [...]}`

3. **Text**: Human-friendly formatted output
   - Colored, formatted for terminal display
   - Use for interactive sessions or final user output
   - Example: `✓ 5 unread messages across 2 channels`

## Command Aliases

Rite provides hidden command aliases for common guesses based on CLI conventions. These aliases work seamlessly but don't appear in `--help` output to keep documentation clean.

| Alias | Actual Command | Reason |
|-------|----------------|--------|
| `post` | `send` | GitHub API and REST conventions |
| `read` | `history` | Natural language pattern |
| `show` | `history` | Git-style command (`git show`) |
| `list-channels` | `channels` | Explicit list prefix |
| `list-agents` | `agents` | Explicit list prefix |
| `list-claims` | `claims` | Explicit list prefix |
| `ls` | `channels` | Unix convention |

**Examples**:
```bash
rite post general "Hello"        # Same as: rite send general "Hello"
rite read general                # Same as: rite history general
rite ls                          # Same as: rite channels
rite list-agents                 # Same as: rite agents
```

These aliases are invisible to help output but work identically to their canonical commands.

## Reading TOON Output

TOON format is the default because it's token-efficient and easy for agents to parse:

```
# Scalar values
field_name: value

# Lists (indented with dash)
items:
  - id: item-1
    name: foo
  - id: item-2
    name: bar

# Nested objects (indented)
channel: general
  is_dm: false
  unread_count: 3
  messages:
    - id: 01HMEX...
      agent: swift-falcon
      body: Hello
```

**Parsing strategy**:
- Split on `:` to get key-value pairs
- Indentation indicates nesting level
- Lists are prefixed with `-`
- Empty lines separate top-level sections

## Common Field Meanings

Understanding what fields represent helps agents make decisions:

| Field | Meaning | Action Hint |
|-------|---------|-------------|
| `total_unread` | Count of unread messages | If > 0, read inbox to see messages |
| `unread_count` | Unread in specific channel | Check this channel for new work |
| `is_dm` | Channel is a direct message | Different handling for DMs vs public channels |
| `active` | Claim/agent is currently active | Indicates ongoing work |
| `expires_at` | Claim expiration timestamp | Check before editing files |
| `agent` | Message sender or claim owner | Compare with your identity to filter |
| `mentions` | List of @mentioned agents | Check if you're mentioned for actionable work |
| `labels` | Message classification tags | Filter messages by topic/priority |
| `ts` | Timestamp (RFC3339) | Sort by time, check recency |
| `reply_to` | Message this one answers (ULID) | Absent means top-level. Use `rite history --thread <id>` to read the conversation |
| `thread.root` | Topmost message of a thread | Present only for `--thread` reads |
| `thread.complete` | Whether the whole chain is here | `false` means a parent is missing or deleted — treat the result as a fragment |
| `thread.missing_parent` | Anchor that is not in the channel | Try `rite sync pull`, then read again |
| `thread.depths` | Reply depth per message, same order as `messages` | Indent by this to render the shape |
| `damaged_field_count` | (`doctor`) Field values this build could not read | If > 0, some data was dropped from records that were otherwise kept |
| `damaged_fields` | (`doctor`) File, line, field, and value for each drop | A dropped `reply_to` means a reply now reads as top-level |
| `next_offset` | Byte offset for next read | Use with `--after-offset` for incremental reading |
| `marked_read` | Whether message was marked read | Tracks what you've processed |

## Error Messages and Context

Rite errors are designed to be self-explanatory:

```bash
# Example: No identity set
$ rite status
Error: No agent identity detected.

→ Here is a random identity you could use:

  swift-falcon

To use it with --agent flag (recommended for agents/scripts):
  rite --agent swift-falcon <command>

Or set in environment (for interactive shells):
  export RITE_AGENT=swift-falcon

Or generate a different name:
  rite generate-name

Note: Environment variables don't persist in sandboxed environments.
  Use --agent flag for reliable identity across commands.
```

**Key patterns in errors**:
- Clear problem statement (first line)
- Suggested solution (concrete example)
- Alternative approaches (multiple options)
- Context about environment (e.g., sandboxing)
- Relevant commands to fix the issue

## Status Indicators

In text format, Rite uses visual indicators:

- `✓` (green) - Success, no action needed
- `→` (cyan) - Informational, status update
- `!` (yellow) - Warning, check this
- `Error:` (red) - Failed, must fix

In TOON/JSON formats, check:
- `status` field for command result
- `error` field for error details
- Counts like `unread_count` for decision points

## Actionable Output

Many commands provide next-step suggestions:

```
# inbox suggests how to mark as read
Tip: Run 'rite inbox --mark-read' to mark all as read

# check-claim tells you who to contact
Error: File claimed by other-agent until 2026-01-30T15:30:00Z
Tip: Send message with 'rite send @other-agent "..."'

# claims shows expiration context
Claim expires in 45m - consider extending with 'rite claim --extend <pattern>'
```

**For agents**: Look for these suggestions when you need to know what to do next.

## Empty Results

Commands handle empty results gracefully:

```bash
$ rite inbox
✓ No unread messages

$ rite claims --mine
# (no output - no claims found)
```

**For agents**:
- Zero unread messages = no work to do
- Empty claims list = you're not claiming anything
- Empty channel list = no activity yet

Use exit codes to check success: `0` = success, `1` = error/failure.

## Time Specifications

Commands that accept time use flexible formats:

**Relative times** (most common for agents):
- `2h` or `2h ago` = 2 hours ago
- `30m` = 30 minutes ago
- `1d` = 1 day ago
- `3600s` = 3600 seconds ago

**Absolute times**:
- `2026-01-30` = start of day
- `2026-01-30T15:30:00Z` = exact timestamp (RFC3339)

**Examples**:
```bash
rite claims --since 2h          # Claims from last 2 hours
rite history --since "1d ago"   # Messages from last day
rite history --before 2026-01-30 # Before specific date
```

## Interpreting Counts and Limits

When using `--limit` or `-n` flags:
- Output is truncated to N items
- Most recent items are shown first (by default)
- Check if result count equals limit - there may be more

```bash
$ rite claims --limit 5
# Shows 5 claims

# If you see exactly 5, there might be more:
$ rite claims --limit 100  # Get more results
```

## Channel Names in Output

- Public channels: `general`, `rite`, `webapp` (no prefix in storage)
- DM channels: `dm-agent1-agent2` (alphabetically sorted names)
- Display format: `#general` (public), `@agent` (DM)

**For agents**: Strip `#` prefix when passing to commands, though Rite now accepts it.

## Understanding Claims Output

Claims show file/resource coordination:

```
pattern: /home/user/project/src/**
agent: swift-falcon
expires_at: 2026-01-30T16:00:00Z
message: Working on API routes
active: true
```

**Before editing files**:
1. Run `rite check-claim <path>`
2. If claimed by another agent, coordinate via DM
3. If unclaimed, claim it before editing
4. Release when done
