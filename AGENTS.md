# rite

Project type: Rust CLI (`cargo`)
Tools: `beads`, `maw`, `crit`, `rite`, `botty`
Reviewer roles: security

## What This Is

Chat-oriented coordination CLI for AI coding agents. When multiple agents work on the same codebase — or across projects — they need a way to communicate, claim resources, and stay out of each other's way. rite provides that with zero infrastructure.

**Design principles:**
- **Zero infrastructure** — append-only JSONL on disk. No daemon, no server, no ports, no database.
- **Agent-first, human-friendly** — every command works headlessly with structured output (TOON/JSON/text). Humans get `rite ui`.
- **Claims for anything** — file globs, URIs (`bead://`, `db://`), ports — any string. Advisory locks, not enforced.
- **Append-only** — JSONL files are the source of truth. SQLite indexes are derived and rebuildable (`rite index rebuild`).
- **Convention over configuration** — sensible defaults, minimal setup. `rite send general "hello"` just works.

**Architecture:** Single binary (`rite`). Storage at `~/.local/share/rite/` — channels are `channels/<name>.jsonl`, claims in `claims.jsonl`, agent state in SQLite (derived). Telegram bridge (`rite telegram`) runs as a long-lived process. TUI (`rite ui`) is a separate mode.

**Scope boundaries — rite is a coordination primitive.** It is NOT a task runner, CI system, build tool, or general-purpose message queue. Push back on scope creep into: job scheduling, build automation, git operations beyond sync, file editing/patching, or process management.

---

## Pre-commit Checks

```bash
cargo fmt && cargo clippy -- -D warnings && just test
```

CI enforces these. Skipping them causes build failures and error emails.

---

## CLI Reference

All commands support `--agent <name>` (or `RITE_AGENT` env var), `--format toon|json|text`, `-q` (quiet), `-v` (verbose).

### Core

| Command | Usage |
|---------|-------|
| `send` | `rite send <target> <message> [-L label] [--attach file] [--no-hooks]` |
| `history` | `rite history [channel] [-n count] [-f] [--since/--before] [--from] [-L label]` |
| `inbox` | `rite inbox [-c channels] [--all] [--mentions] [-n count] [--mark-read] [--count-only]` |
| `mark-read` | `rite mark-read <channel>` |
| `search` | `rite search <query> [-c channel] [-n count] [--from]` |
| `wait` | `rite wait [-c channel] [--mention] [-L label] [-t timeout]` |
| `watch` | `rite watch [channel]` — stream messages in real-time |
| `status` | `rite status` — overview of agents, channels, claims |

### Claims (advisory locks)

| Command | Usage |
|---------|-------|
| `claims stake` | `rite claims stake <patterns...> [-t ttl] [-m message]` |
| `claims check` | `rite claims check <path>` |
| `claims release` | `rite claims release [patterns...] [--all]` |
| `claims list` | `rite claims list [--all] [--mine] [-n limit]` |
| `claims refresh` | `rite claims refresh` — extend TTL |

### Management

| Command | Usage |
|---------|-------|
| `agents` | `rite agents [--active]` |
| `channels` | `rite channels list\|close\|reopen\|delete\|rename` |
| `hooks` | `rite hooks add\|list\|remove\|test` |
| `subscriptions` | `rite subscriptions add\|remove\|list` |
| `statuses` | `rite statuses set\|clear\|list` |
| `messages` | `rite messages get <id>` |

### Sync & Infra

| Command | Usage |
|---------|-------|
| `sync` | `rite sync init\|push\|pull\|status\|log\|check` |
| `index` | `rite index rebuild\|status` |
| `telegram` | `rite telegram` — run Telegram bridge |
| `ui` | `rite ui [-c channel]` — terminal UI |
| `init` | `rite init` — create data directory |
| `doctor` | `rite doctor` — check environment health |
| `generate-name` | `rite generate-name` — random kebab-case name |
| `whoami` | `rite whoami` |

### Attachments

```bash
rite send general "see attached" --attach ./screenshot.png
rite send general "link" --attach https://example.com/file.tar.gz
rite send general "named" --attach "label:./path/to/file"
```

Files are stored in a content-addressed cache (SHA256). The Telegram bridge relays attachments bidirectionally.

### Message Flags

Inline flags in message body suppress hook execution:
- `!nohooks` — suppress all hooks
- `!nochanhooks` — suppress channel hooks only
- `!noathooks` — suppress @-mention hooks only

Example: `rite send general "deploy done !nohooks"`

Alternatively, use `--no-hooks` on the CLI.

---

## Agent Communication

### Identity

```bash
# Recommended: --agent flag (works in sandboxed environments)
rite --agent my-agent send general "hello"

# Alternative: env var (doesn't persist across sandboxed commands)
export RITE_AGENT=$(rite generate-name)
```

### Quick Start

```bash
rite status                                    # What's happening?
rite send general "Starting work on X"         # Announce
rite send @other-agent "Question about Y"      # DM
rite claims stake "src/api/**" -m "Working on API"  # Claim files
rite claims check src/api/routes.rs            # Check before editing
rite claims release --all                      # Release when done
rite wait -c @other-agent -t 60               # Wait for reply
```

### Channel Conventions

- `#general` — cross-project coordination
- `#project-name` — project-specific (e.g., `#rite`)
- `@agent-name` — direct messages

Names: lowercase alphanumeric with hyphens.

### Message Style

Keep messages concise and actionable:
- "Starting work on bd-xyz: Add foo feature"
- "Blocked: need database credentials to proceed"
- "Done: implemented bar, tests passing"

---

## Development Notes

- Storage: `~/.local/share/rite/` (override with `RITE_DATA_DIR`)
- Identity: `RITE_AGENT` env var or `--agent` flag
- Claims stored with absolute paths, displayed relative when in same directory tree
- Git sync disables GPG signing in data repos automatically
- JSONL is append-only; indexes derived via `rite index rebuild`

### Output Formats

Commands default to TOON (token-efficient for agents). Use `--format json` for structured parsing or `--format text` for human reading. See [.agents/cli-output.md](.agents/cli-output.md) for detailed format guidance.

### Further Reading

- [Testing strategy and test harness](.agents/testing.md)
- [TUI screenshot workflow](.agents/tui-screenshot.md)
- [CLI output format details](.agents/cli-output.md)

---

## Tools

### Beads (Issue Tracking)

Uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust). Issues in `.beads/`, tracked in git. `br` never runs git commands — after `br sync --flush-only`, manually commit and push.

```bash
br ready                          # Actionable work
br show <id>                      # Full details
br create --title="..." --type=task --priority=2
br close <id>
```

### bv (Beads Viewer)

Fast TUI for `.beads/issues.jsonl` with precomputed dependency metrics. For agents, use the robot flags instead of parsing JSONL:

- `bv --robot-help` — all AI-facing commands
- `bv --robot-plan` — execution plan with parallel tracks
- `bv --robot-priority` — priority recommendations
- `bv --robot-insights` — graph metrics (PageRank, critical path, cycles)

---

<!-- edict:managed-start -->
## Edict Workflow

### How to Make Changes

1. **Create a bone** to track your work: `bn create --title "..." --description "..."`
2. **Create a workspace** for your changes: `maw ws create <bone-id> --from main --description "<bone-title>"` — use the bone ID as workspace name; this gives you `.maw/workspaces/<bone-id>/`
3. **Edit files in your workspace** (`.maw/workspaces/<name>/`), never in the trunk at the repo root
4. **Merge when done**: `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` (use conventional commit prefix: `feat:`, `fix:`, `chore:`, etc.; swap `default` for a change id when merging back into a tracked change)
5. **Close the bone**: `bn done <id>`

Do not create git branches manually — `maw ws create` handles branching for you. See [worker-loop.md](.agents/edict/worker-loop.md) for the full triage → start → work → finish cycle.

**All tools have `--help`** with usage examples. When unsure, run `<tool> --help` or `<tool> <command> --help`.

### Conflicts Are Data, Not Errors

`maw ws sync` rebases committed-ahead workspaces onto the latest epoch by default. On conflict it does not abort — it commits labeled conflict markers and leaves the workspace `lifecycle:conflicted` (visible in `maw ws list`). Treat a conflicted workspace as a normal state, not a failure.

- `maw ws resolve <ws> --list` shows conflicts; `--keep epoch|<ws>|both|union` (or `--keep PATH=NAME`) resolves them.
- `maw ws merge` auto-syncs stale sources and accepts `--resolve cf-id=<ws>` / `--resolve-all=<ws>` to resolve inline.
- `maw ws conflicts <ws>` inspects conflict details.
- The one hard gate: merge refuses a source whose HEAD still has unresolved conflict markers (bypass with `--force` only for legitimate marker-like content).

### Directory Structure

This project uses the **root** layout. The project root is the trunk working copy — source files, `.bones/`, config, and `AGENTS.md` live there. Extra agent workspaces live under `.maw/workspaces/`.

```
project-root/              ← trunk working copy (AGENTS.md, .bones/, src/, etc.)
├── src/, AGENTS.md, …     ← your project files, edited here directly
├── .maw/
│   ├── workspaces/
│   │   ├── bn-1abc/       ← agent workspace (named after bone ID)
│   │   └── bn-2def/       ← another agent workspace
│   └── manifold/          ← maw metadata/artifacts
└── .git/                  ← git data
```

**Key rules:**
- The project root is the trunk — bones, config, and project files live here, and you edit them directly
- **Never merge or destroy the `default` workspace.** `default` names the trunk (the repo root); other workspaces merge INTO it, not the other way around.
- Agent workspaces (`.maw/workspaces/<name>/`) are isolated Git worktrees managed by maw
- Use `maw exec <ws> -- <command>` to run commands in a non-default workspace context
- Run `bn ...` directly at the repo root for bones commands (no `maw exec` prefix needed — they always target the trunk)
- Use `maw exec <ws> -- seal ...` for review commands (always in the review's workspace)

### Bones Quick Reference

| Operation | Command |
|-----------|---------|
| Triage (scores) | `bn triage` |
| Next bone | `bn next` |
| Next N bones | `bn next N` (e.g., `bn next 4` for dispatch) |
| Show bone | `bn show <id>` |
| Create | `bn create --title "..." --description "..."` |
| Start work | `bn do <id>` |
| Add comment | `bn bone comment add <id> "message"` |
| Close | `bn done <id>` |
| Add dependency | `bn triage dep add <blocker> --blocks <blocked>` |
| Search | `bn search <query>` |

Identity resolved from `$AGENT` env. No flags needed in agent loops.

### Workspace Quick Reference

| Operation | Command |
|-----------|---------|
| Create workspace | `maw ws create <bone-id> --from main --description "<title>"` |
| List workspaces | `maw ws list` |
| Check merge readiness | `maw ws merge <name> --into default --check` |
| Merge to main | `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` |
| Destroy (no merge) | `maw ws destroy <name>` |
| Run command in workspace | `maw exec <name> -- <command>` |
| Diff workspace vs epoch | `maw ws diff <name>` |
| Check workspace overlap | `maw ws overlap <name1> <name2>` |
| View workspace history | `maw ws history <name>` |
| Sync stale workspace | `maw ws sync <name>` |
| Inspect merge conflicts | `maw ws conflicts <name>` |
| Undo local workspace changes | `maw ws undo <name>` |
| List recovery snapshots | `maw ws recover` |
| Recover destroyed workspace | `maw ws recover <name> --to <new-name>` |
| Search recovery snapshots | `maw ws recover --search <pattern>` |
| Show file from snapshot | `maw ws recover <name> --show <path>` |

**Inspecting a workspace:**
```bash
maw exec <name> -- git status             # what changed (unstaged)
maw exec <name> -- git log --oneline -5   # recent commits
maw ws diff <name>                        # diff vs epoch (maw-native)
```

**Lead agent merge workflow** — after a worker finishes a bone:
1. `maw ws list` — look for `active (+N to merge)` entries
2. `maw ws merge <name> --into default --check` — verify no conflicts
3. `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` — merge and clean up (use conventional commit prefix)

**Workspace safety:**
- Never merge or destroy `default`.
- Always `maw ws merge <name> --into default --check` before `--destroy`.
- Commit workspace changes with `maw exec <name> -- git add -A && maw exec <name> -- git commit -m "..."`.
- **No work is ever lost in maw.** Recovery snapshots are created automatically on every destroy. If a workspace was destroyed and you suspect code is missing, ALWAYS run `maw ws recover` before concluding work was lost. Never reopen a bone or start over without checking recovery first.

### Protocol Quick Reference

Use these commands at protocol transitions to check state and get exact guidance. Each command outputs instructions for the next steps.

| Step | Command | Who | Purpose |
|------|---------|-----|---------|
| Resume | `edict protocol resume --agent $AGENT` | Worker | Detect in-progress work from previous session |
| Start | `edict protocol start <bone-id> --agent $AGENT` | Worker | Verify bone is ready, get start commands |
| Review | `edict protocol review <bone-id> --agent $AGENT` | Worker | Verify work is complete, get review commands |
| Finish | `edict protocol finish <bone-id> --agent $AGENT` | Worker | Verify review approved, get close/cleanup commands |
| Merge | `edict protocol merge <workspace> --agent $AGENT` | Lead | Check preconditions, detect conflicts, get merge steps |
| Cleanup | `edict protocol cleanup --agent $AGENT` | Worker | Check for held resources to release |

All commands support JSON output with `--format json` for parsing. If a command is unavailable or fails (exit code 1), fall back to manual steps documented in [start](.agents/edict/start.md), [review-request](.agents/edict/review-request.md), and [finish](.agents/edict/finish.md).

### Bones Conventions

- Create a bone before starting work. Update state: `open` → `doing` → `done`.
- Post progress comments during work for crash recovery.
- **Run checks before committing**: `just check` (or your project's build/test command). Fix any failures before proceeding.
- After finishing a bone, follow [finish.md](.agents/edict/finish.md). **Workers: do NOT push** — the lead handles merges and pushes.

### Release Instructions

- Bump the version of all crates
- Regenerate the Cargo.lock
- Add notes to CHANGELOG.md
- If the README.md references the version, update it.
- Commit
- Tag and push: `maw release vX.Y.Z`
- use `gh release create vX.Y.Z --notes "..."`
- Install locally: `maw exec default -- just install`

### Identity

Your agent name is set by the hook or script that launched you. Use `$AGENT` in commands.
For manual sessions, use `<project>-dev` (e.g., `myapp-dev`).

### Claims

When working on a bone, stake claims to prevent conflicts:

```bash
rite claims stake --agent $AGENT "bone://<project>/<id>" -m "<id>"
rite claims stake --agent $AGENT "workspace://<project>/<ws>" -m "<id>"
rite claims release --agent $AGENT --all  # when done
```

### Reviews

Use `@<project>-<role>` mentions to request reviews:

```bash
maw exec $WS -- seal reviews request <review-id> --reviewers $PROJECT-security --agent $AGENT
rite send --agent $AGENT $PROJECT "Review requested: <review-id> @$PROJECT-security" -L review-request
```

The @mention triggers the auto-spawn hook for the reviewer.

### Bus Communication

Agents communicate via rite channels. You don't need to be expert on everything — ask the right project.

| Operation | Command |
|-----------|---------|
| Send message | `rite send --agent $AGENT <channel> "message" [-L label]` |
| Check inbox | `rite inbox --agent $AGENT --channels <ch> [--mark-read]` |
| Wait for reply | `rite wait -c <channel> --mention -t 120` |
| Browse history | `rite history <channel> -n 20` |
| Search messages | `rite search "query" -c <channel>` |

**Conversations**: After sending a question, use `rite wait -c <channel> --mention -t <seconds>` to block until the other agent replies. This enables back-and-forth conversations across channels.

**Project experts**: Each `<project>-dev` is the expert on their project. When stuck on a companion tool (rite, maw, seal, vessel, bn), post a question to its project channel instead of guessing.

### Cross-Project Communication

**Don't suffer in silence.** If a tool confuses you or behaves unexpectedly, post to its project channel.

1. Find the project: `rite history projects -n 50` (the #projects channel has project registry entries)
2. Post question or feedback: `rite send --agent $AGENT <project> "..." -L feedback`
3. For bugs, create bones in their repo first
4. **Always create a local tracking bone** so you check back later:
   ```bash
   bn create --title "[tracking] <summary>" --tag tracking --kind task
   ```

See [cross-channel.md](.agents/edict/cross-channel.md) for the full workflow.

### Session Search (optional)

Use `cass search "error or problem"` to find how similar issues were solved in past sessions.


### Design Guidelines


- [CLI tool design for humans, agents, and machines](.agents/edict/design/cli-conventions.md)



### Workflow Docs


- [Find work from inbox and bones](.agents/edict/triage.md)

- [Claim bone, create workspace, announce](.agents/edict/start.md)

- [Change bone state (open/doing/done)](.agents/edict/update.md)

- [Close bone, merge workspace, release claims](.agents/edict/finish.md)

- [Full triage-work-finish lifecycle](.agents/edict/worker-loop.md)

- [Turn specs/PRDs into actionable bones](.agents/edict/planning.md)

- [Explore unfamiliar code before planning](.agents/edict/scout.md)

- [Create and validate proposals before implementation](.agents/edict/proposal.md)

- [Request a review](.agents/edict/review-request.md)

- [Handle reviewer feedback (fix/address/defer)](.agents/edict/review-response.md)

- [Reviewer agent loop](.agents/edict/review-loop.md)

- [Merge a worker workspace (protocol merge + conflict recovery)](.agents/edict/merge-check.md)

- [Validate toolchain health](.agents/edict/preflight.md)

- [Ask questions, report bugs, and track responses across projects](.agents/edict/cross-channel.md)

- [Report bugs/features to other projects](.agents/edict/report-issue.md)

- [groom](.agents/edict/groom.md)

<!-- edict:managed-end -->
