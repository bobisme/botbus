# rite Claude Code Channel Plugin

Status: proposal only. I do not see an implementation in this repo yet.

A Bun-based Claude Code channel server that bridges local rite channels into a running Claude Code session via the Claude Code channels feature: https://code.claude.com/docs/en/channels

Instead of having agents poll `rite inbox` or block on `rite wait`, the channel server pushes new rite messages directly into the already-running Claude session.

## Problem

Agents currently pull: they run `rite wait` or `rite inbox` to check for messages. That means either:

- blocking on `wait`, which prevents other work, or
- polling periodically, which adds latency and boilerplate.

Claude Code channels solve the right problem here: an MCP server can push external events into the live local session.

## Current Claude Code Constraints

This design should assume the current channels model, not the early preview-only framing:

- Channels require a recent Claude Code version and a `claude.ai` login.
- Team and Enterprise orgs may need channels explicitly enabled by policy.
- Local development of a custom channel server still uses `--dangerously-load-development-channels`.
- Installed packaged plugins use `--channels plugin:<name>@<marketplace>`.

This note describes a channel server first. Packaging it as an installable plugin is a later step.

## Design Overview

Each agent runs the channel server as part of their Claude Code session. For local development/testing during the current preview:

```bash
claude --dangerously-load-development-channels server:rite-channel
```

The server:

1. Watches one or more rite channels.
2. Streams new messages as they arrive.
3. Filters out messages sent by the current agent.
4. Pushes each inbound message into Claude Code as a `notifications/claude/channel` event.
5. Exposes a `reply` tool so Claude can answer back through rite.

Multiple agents can watch the same rite channel simultaneously. Each Claude Code session gets its own event stream.

## Channel Selection

The server runs in one of two selection modes. Both always include the agent's
own DM channels, and both always drop self-authored messages.

### Explicit mode (default)

Watch a fixed set, merged and deduplicated:

1. Always: DM channels whose participants include the current agent — existing
   `_dm_*` files plus newly-created ones (see Watching Mechanism).
2. Explicit config: `RITE_CHANNELS` env var (comma-separated project channels).

Every message on a watched channel is forwarded — this is broadcast: you see all
traffic on the channels you subscribed to. Subscription auto-discovery
(`rite subscriptions list`) is deferred until rite grows `--format json` for it;
do not parse colored text (see Disposition #3).

Example: an agent working on the `rite` project sets `RITE_CHANNELS=rite` and
receives both project-channel traffic and direct messages.

### Mention mode (opt-in: `RITE_MENTION_ROUTING=1`)

Watch *every* channel, but forward only messages that concern the agent. This is
what makes `@<agent> hello` reach the agent from **any** channel, without having
to subscribe to each one. See [Mention Routing](#mention-routing).

Example: `rite-dev` sets `RITE_MENTION_ROUTING=1` and becomes reachable as
`@rite-dev` in any channel — plus its DMs — without enumerating channels in
`RITE_CHANNELS`.

## Identity

Use an explicit agent identity. Recommended order:

1. `RITE_AGENT`
2. `AGENT` fallback for compatibility
3. Otherwise fail with a clear error

Do not rely on shell `USER` fallback here. Channel routing should be deterministic.

## Watching Mechanism

Use one subprocess per watched channel:

```bash
rite history <channel> --format json -f --after-offset <offset>
```

This emits one compact `Message` JSON object per line as messages arrive. The server reads stdout line-by-line, parses JSONL, and emits Claude channel notifications.

In **explicit mode** the watched set is `RITE_CHANNELS` plus the agent's DM
channels. In **mention mode** the watched set is *every* channel file in
`channels_dir()` (project channels and `_dm_*` alike); the same one-subprocess-
per-channel mechanism is used, only the channel list is wider. Either way, watch
the channels directory for newly-created channel files and spawn a follower for
each as it appears (first DMs — and, in mention mode, brand-new channels — can
arrive after startup). Newly-created files are seeded from offset 0 so a
channel's very first message is not missed.

### Startup offset

Before spawning the follow process, ask rite for the current end offset and start from there so the channel only delivers messages that arrive after the Claude session opens:

```bash
rite history <channel> --format json -n 1 --show-offset
```

Parse `next_offset` from the JSON response and pass it to `--after-offset`.

### Important caveat — RESOLVED in current rite

This was the plan's hard blocker (Disposition #1). It is now fixed in the source
and the workaround it describes is obsolete:

- `follow_channel_json` (`src/cli/history.rs:426`) seeds its follow cursor from
  the bounded read's `next_offset`, not the file's EOF (`history.rs:446`), so
  messages that land between the initial read and the follow handoff are drained,
  not dropped.
- `--follow-count` is a flag **distinct** from `-n` (`src/cli/mod.rs:148`).
  The server uses `-n 0 --show-offset` only to compute the startup baseline, then
  follows with `--after-offset <n>` and **no** `--follow-count`, so there is no
  count cap on the live stream. The old "set a very high `-n`" advice is no
  longer needed.
- The directory watcher is registered *before* the backlog drain
  (`history.rs:439-441`), closing the startup race.

Still outstanding: the burst no-drop regression test from Disposition #7
(asserting that more messages than the count, landing between baseline and follow
handoff, are not skipped). The existing test at `history.rs:699` covers bounded
pagination only, not follow mode.

## Message Filtering

Forwarding is decided per message. Forward if **any** of:

- the message is on a channel in the explicit `RITE_CHANNELS` broadcast set, **or**
- the message is a DM channel and the current agent is a participant, **or**
- mention mode is on and `message.mentions` contains the current agent
  (see [Mention Routing](#mention-routing)).

Then, regardless of the above, always drop:

- messages where `message.agent == my_agent` (self-loop prevention), and
- anything excluded by the opt-in `RITE_ALLOWED_AGENTS` / `RITE_FORWARD_LABELS`
  filters (Disposition #4).

## Mention Routing

Goal: posting `@<agent> ...` in **any** rite channel delivers that message to the
agent's live Claude session, without the agent having to subscribe to the channel.

This works because rite already parses mentions at write time: every stored
`Message` carries `mentions: string[]`, populated by `extract_mentions`
(`src/core/message.rs:50`). The routing key already travels in the JSON the
follower reads — mention routing is a forwarding-rule change plus a wider watch
set, not new parsing.

### Discovery

In mention mode (`RITE_MENTION_ROUTING=1`) the server watches the whole channels
directory:

1. At startup, enumerate every channel file in `channels_dir()` — project
   channels (`<name>.jsonl`) and DM channels (`_dm_*.jsonl`).
2. Spawn a follower per channel, each seeded from its current end offset
   (`-n 0 --show-offset`, the baseline mechanism from Disposition #1).
3. Watch the directory for newly-created channel files and spawn a follower for
   each as it appears, seeded from offset 0 so a channel's very first message —
   which may itself be the mention — is not missed.

### Forwarding rule

For each inbound message, forward if **any** of:

- it is a DM channel and the current agent is a participant (always deliver my
  DMs), **or**
- `message.mentions` contains the current agent, **or**
- the channel is in the explicit `RITE_CHANNELS` broadcast set.

Always, regardless of the above: drop `message.agent == my_agent`, and apply the
opt-in `RITE_ALLOWED_AGENTS` / `RITE_FORWARD_LABELS` filters.

**Case:** match mentions case-insensitively. Agent names are canonically
lowercase (rite convention), but `extract_mentions` preserves whatever case was
typed (`@Rite-Dev` → `Rite-Dev`), so fold case before comparing.

**DM privacy:** a mention does **not** override DM participation. If a message in
a DM channel the agent is *not* part of happens to contain `@<agent>`, it is
**not** forwarded — DMs are private to their two participants. Mentions only pull
in *project*-channel messages.

### reply_target

Unchanged from the base design:

- mention in a project channel → `reply_target` is the bare channel name; the
  reply goes back to where the agent was mentioned.
- DM → `reply_target` is `@<other participant>`.

The notification also carries a `route` meta key (`broadcast` | `dm` | `mention`)
so Claude knows *why* it received a message — see Notification Format.

### Cost, and the scalable alternative

Watch-all is one subprocess and one file watcher per channel, and every message
in every channel is parsed and mention-checked even when discarded. This is fine
for tens of channels. It does not scale to hundreds: process count, file
descriptors, and wakeup volume grow with total channel count and traffic — not
with how many messages actually mention the agent.

The scalable replacement is a dedicated rite streaming primitive:

```bash
rite mentions follow --agent <name> --format json [--include-dms] [-L label]
```

It would do the cross-channel scan inside a single rite process — one watcher on
`channels_dir()`, reading each changed file incrementally, emitting only messages
whose `mentions` include the agent (plus the agent's DMs) as JSONL. The Bun
server then consumes one stream instead of N subprocesses. rite already has the
pieces: mentions are parsed at write time, and `rite wait --mention` does
single-shot mention matching; this generalizes that to a no-cap stream with
per-channel offset bookkeeping seeded at "now".

**Plan:** ship the watch-all + filter version in the v1 plugin; swap mention mode
to consume `rite mentions follow` once it lands in rite. Track the rite-side work
as its own bone.

## Notification Format

Use only identifier-safe meta keys. Claude's channel contract drops keys with hyphens or other punctuation.

Suggested payload:

```ts
await mcp.notification({
  method: 'notifications/claude/channel',
  params: {
    content: message.body,
    meta: {
      from_agent: message.agent,
      channel_name: message.channel,
      reply_target: isDm ? `@${message.agent}` : message.channel,
      // route explains WHY this message was forwarded:
      //   'dm'        — a direct message to this agent
      //   'mention'   — matched message.mentions in mention mode
      //   'broadcast' — full-channel subscription via RITE_CHANNELS
      route: isDm ? 'dm' : (mentionMatched ? 'mention' : 'broadcast'),
      msg_id: message.id.toString(), // meta values must be strings
      ...(message.labels?.length ? { labels: message.labels.join(',') } : {}),
    },
  },
})
```

Why `reply_target` matters:

- project channels can reply to the same bare channel name, for example `rite`
- DM messages are stored in canonical rite DM channels such as `_dm_alice_bob`
- Claude should not have to reconstruct DM routing from the stored channel name
- instead, the server should compute the correct reply target once and expose it directly

Claude sees something like:

```text
<channel source="rite-channel" from_agent="bob-agent" channel_name="rite" reply_target="rite" msg_id="01ARZ3...">
Starting work on claim validation - staking src/cli/claims.rs
</channel>
```

For a DM:

```text
<channel source="rite-channel" from_agent="bob-agent" channel_name="_dm_alice_bob-agent" reply_target="@bob-agent" msg_id="01ARZ3...">
Can you review the parser change?
</channel>
```

## Reply Tool

Two-way support should be a normal MCP tool. Claude passes the `reply_target` value from the inbound event unchanged.

```ts
tools: [{
  name: 'reply',
  description: 'Send a rite message in response to an inbound rite channel event',
  inputSchema: {
    type: 'object',
    properties: {
      target: {
        type: 'string',
        description: 'Reply target copied verbatim from the inbound event meta.reply_target',
      },
      text: { type: 'string', description: 'Message body to send' },
      labels: {
        type: 'array',
        items: { type: 'string' },
        description: 'Optional rite labels',
      },
    },
    required: ['target', 'text'],
  },
}]
```

Implementation shells out to rite:

```ts
const args = ['send', '--agent', myAgent, target, text]
for (const label of labels ?? []) args.push('-L', label)
Bun.spawnSync(['rite', ...args])
```

Validation is still useful, but the concern is correctness, not shell injection. `spawnSync(['rite', ...args])` is argv-based, not shell-parsed. Validate targets against rite's actual rules:

- project channels: lowercase alphanumeric plus hyphens
- DM targets: `@<agent>` where agent names follow rite agent-name rules

If needed, share validation logic with rite or mirror `src/core/channel.rs` and `src/core/names.rs`.

## Instructions String

Add something like this to the channel server instructions:

```text
Inbound rite messages arrive as <channel source="rite-channel" ...> events.
These are real messages from other local agents working in the same codebase.

The meta.route field says why you got each message: "dm" (sent directly to you),
"mention" (someone wrote @<you> in a channel), or "broadcast" (a channel you
subscribe to). For "mention", reply in that channel unless asked otherwise.

To reply, call the reply tool and pass meta.reply_target unchanged.
Do not reconstruct DM targets from channel_name.

Use rite inbox only for messages that arrived before this session started.
Messages that arrive while this session is open are pushed here automatically.
```

## Configuration

| Variable | Description |
|---|---|
| `RITE_AGENT` | Agent name. Recommended and effectively required for predictable routing. |
| `AGENT` | Optional compatibility fallback if `RITE_AGENT` is unset. |
| `RITE_CHANNELS` | Comma-separated list of project channels to watch (broadcast — every message forwarded). |
| `RITE_MENTION_ROUTING` | Opt-in. When set (`1`), watch *all* channels and forward only messages that mention the agent (plus its DMs). Makes `@<agent>` reachable from any channel. See [Mention Routing](#mention-routing). |
| `RITE_ALLOWED_AGENTS` | Optional routing allowlist (Disposition #4). When set, only forward messages from these senders. Unset = forward all (trust the local data dir). Not authentication — local sender names are spoofable. |
| `RITE_FORWARD_LABELS` | Optional. When set, only forward messages carrying one of these labels. |
| `RITE_DATA_DIR` | Optional rite data directory override, passed through to subprocesses. |

If neither `RITE_AGENT` nor `AGENT` is set, exit with a clear error.

## MCP Registration

For a local bare server during development/testing:

```json
{
  "mcpServers": {
    "rite-channel": {
      "command": "bun",
      "args": ["./scripts/rite-channel.ts"],
      "env": {
        "RITE_AGENT": "rite-dev",
        "RITE_CHANNELS": "rite"
      }
    }
  }
}
```

Run Claude Code with:

```bash
claude --dangerously-load-development-channels server:rite-channel
```

If we later package this as a real plugin, startup changes to the normal `--channels plugin:...` flow.

## Security Model

This is safe only under rite's current trusted-local-machine model.

- rite is local append-only storage, not a cryptographic identity system
- sender identity is whatever local process wrote the message using a chosen agent name
- if an untrusted process can write to the same rite data dir, it can inject channel events into Claude
- mention routing widens this surface slightly: in mention mode, *any* writer to
  *any* channel can reach the agent just by typing `@<agent>`. Under the trusted-
  local-machine model this is fine (all writers are the same user); it is not a
  reason to forward across a trust boundary. `RITE_ALLOWED_AGENTS` narrows it.

So the correct claim is not "rite already handles authorization". The correct claim is:

> This channel server is acceptable for a trusted local rite data directory owned by the same user. It is not an authenticated remote messaging bridge.

If this ever grows beyond same-user local workflows, it needs real sender authentication and allowlisting before forwarding anything to Claude.

## Optional Extension: Permission Relay

If we want remote approval/deny flows through the same channel, Claude Code also supports the optional `claude/channel/permission` capability. That is not required for the first version, but it is worth keeping in mind for later.

## Open Questions

1. Should this ship first as `scripts/rite-channel.ts`, or only after it is packaged as a plugin?
2. Is subscription auto-discovery worth enabling by default, or should channel watch lists stay explicit?
3. Should rite grow a dedicated streaming API for "follow from offset with no count cap" before this is implemented?

## What This Is Not

- Not a daemon; it only runs while Claude Code is open.
- Not a replacement for `rite inbox` catch-up across sessions.
- Not a replacement for `rite wait` in non-Claude-Code loops.
- Not a rite server or relay; each Claude session reads local rite state directly.
- Not a way to reach an offline agent. Mention routing only pushes to a *live*
  session; mentions that arrive while the agent is offline wait in
  `rite inbox --mentions` for next session.

---

## Disposition (Review 1)

Decisions on `claude-code-channel-plugin.review.1.md`. The two load-bearing claims
were verified against the current source before deciding:

- **Streaming gap is real.** `follow_channel_json` in `src/cli/history.rs` seeds its
  cursor from `std::fs::metadata(path).len()` (current EOF), *not* from the initial
  read's `next_offset`. So `rite history <ch> --format json -f --after-offset 0 -n 50`
  over a 100-message file emits the first 50, then resumes following from EOF, silently
  dropping messages 51–100. The review's Change #1 is a confirmed defect, not a caveat.
- **No JSON subscriptions output.** `list_subscriptions` in `src/cli/subscribe.rs` prints
  only colored text; there is no `--format json`. The review's Change #3 is accurate.

Overall: the review is sound and is adopted. Summary table, then notes per item.

| # | Recommendation | Disposition |
|---|----------------|-------------|
| 1 | Make streaming lossless before building the server | **Adopt — blocking** |
| 2 | Spell out the full MCP channel contract | **Adopt** |
| 3 | Fix channel-selection assumptions | **Adopt** |
| 4 | Treat sender filtering as a security boundary | **Adopt (opt-in)** |
| 5 | String-only meta values + preserve context | **Adopt** |
| 6 | Start from a plugin scaffold | **Adopt** |
| 7 | Add a concrete test & diagnostics plan | **Adopt** |

### #1 — Lossless streaming — Adopt (blocking prerequisite)

Confirmed bug (see above). This is promoted from "Important caveat" to a hard prerequisite:
the channel server must not be built on the current lossy follow path. Fix in rite by
**either** seeding JSON follow mode's cursor from the initial read's `next_offset` instead
of EOF, **or** adding a dedicated `rite follow <channel> --format json --from-offset <n>`
with no count cap. The server uses `-n 0 --show-offset` only to compute the startup
baseline. A burst test (more messages than the default count landing between baseline and
follow handoff, asserting none are skipped) is part of "done". Do not rely on a high `-n`
as a correctness strategy.

### #2 — Full MCP contract — Adopt

Cheap and removes real ambiguity. The doc should state the channel-capable server shape:
`capabilities.experimental['claude/channel']`, `capabilities.tools: {}` (two-way bridge),
`ListToolsRequestSchema` + `CallToolRequestSchema` handlers registered *before*
`mcp.connect(new StdioServerTransport())`, and a startup diagnostic distinguishing
"loaded as a channel" from "loaded as a plain MCP server".

### #3 — Channel selection — Adopt

"The agent's own DM target(s)" is too vague to implement. v1 rule:
1. Watch explicit project channels from `RITE_CHANNELS`.
2. Watch existing `_dm_*` channel files whose parsed participants include the current agent
   (reuse/mirror `dm_agents()` from `src/core/channel.rs`).
3. Watch the channels directory for newly-created `_dm_*` files involving the agent and
   start streaming them (first DMs can arrive after startup).
4. **Defer** subscription auto-discovery until `rite subscriptions list` grows
   `--format json` — do not parse colored text. (Adding that JSON output to rite is a
   reasonable companion task but is not required for v1.)

### #4 — Sender filtering — Adopt, opt-in

Always drop self-authored messages. Add **opt-in** `RITE_ALLOWED_AGENTS` and
`RITE_FORWARD_LABELS` filters — when unset, behavior is unchanged (trust the local data
dir), preserving rite's zero-config / convention-over-configuration principle. Name it a
"routing allowlist," not authentication (local sender names are spoofable). Keep the
`claude/channel/permission` capability out of v1; it can approve local tool use and must
wait for real sender authentication or an explicit same-user approval policy.

### #5 — String-only meta + context — Adopt

Claude Code channel meta is `Record<string, string>`; the proposal's `msg_id: message.id`
(a number/ULID object) is a latent type bug — use `message.id.toString()`. Include
`from_agent`, `channel_name`, `reply_target`, `msg_id`, `ts`, `is_dm` ('true'/'false'),
and `labels_json` / `attachment_count` only when present. Content carries the body plus a
compact attachment summary; never inline arbitrary file contents.

### #6 — Plugin scaffold — Adopt

Use plugin layout from the start (`.claude-plugin/plugin.json` with a `channels` entry)
while keeping the bare-server `.mcp.json` dev path for
`--dangerously-load-development-channels`. This refines the original "packaging is later"
framing: **layout** now, **marketplace distribution** later. Avoids rewriting the server
around plugin config/data-dir/subprocess-env constraints after the fact.

### #7 — Test & diagnostics plan — Adopt

Add a Validation Plan: unit tests for identity resolution, target validation, self-message
filtering, DM vs project reply-target computation, allowlist/label filtering, and the burst
no-drop test from #1. Startup stderr logs resolved agent, data dir, and watched channels;
subprocess exits are logged and restarted with bounded backoff. Reserve one manual Claude
Code smoke-test checklist (loading is gated by external org policy/login).

### Open questions — resolved

1. Ship plugin-shaped from the start; keep bare-server registration for development.
2. Keep watch lists explicit in v1; revisit auto-discovery after JSON subscriptions exist.
3. Yes — fix the lossy follow path (or add a stream-from-offset mode) in rite **before**
   the channel server is implemented.

### Not adopted into the doc body yet

This section records the decisions; the granular inline rewrites proposed as git-diffs in
the review (notification payload code, MCP-contract section, etc.) are the follow-up edit
once #1 lands in rite. Nothing in the review was rejected.

---

## Disposition (Review 2) — Mention Routing

Added after a fresh check against the current source. Two findings:

- **Mention data is already on the wire.** Every `Message` carries
  `mentions: string[]`, populated at send time by `extract_mentions`
  (`src/core/message.rs:50`), and the value is case-preserving. Cross-channel
  mention routing needs no new parsing — only a wider watch set and a different
  forwarding rule.
- **The Disposition #1 streaming blocker has landed.** JSON follow seeds its
  cursor from the bounded read's `next_offset`, not EOF (`src/cli/history.rs:446`);
  `--follow-count` is distinct from `-n` (`src/cli/mod.rs:148`); and the watcher
  registers before the backlog drain. The lossy follow path the plan was blocked
  on is fixed. The burst no-drop regression test (Disposition #7) is still owed.

Decisions:

| # | Decision |
|---|----------|
| R2-1 | Add opt-in `RITE_MENTION_ROUTING` mention mode to v1: watch all channels, forward only messages mentioning the agent (plus its DMs). |
| R2-2 | Mentions pull in *project*-channel messages only; DM privacy is preserved (a mention never overrides DM participation). |
| R2-3 | Match mentions case-insensitively; agent names are canonically lowercase. |
| R2-4 | Add a `route` meta key (`dm` / `mention` / `broadcast`) so Claude knows why a message was forwarded. |
| R2-5 | Ship watch-all + filter now; add a `rite mentions follow` streaming primitive as the scalable replacement later, tracked as a separate rite bone. |

This **amends Disposition #3**: v1 now supports *both* the explicit watch-list mode
(default) and the opt-in all-channel mention mode. "Keep watch lists explicit"
remains the default; mention mode is the opt-in for agents that want to be
reachable as `@<agent>` from anywhere.

### Open questions (Review 2)

1. Should mention mode be the recommended default for interactive agents (most
   people expect `@me` to reach them), with explicit-list mode reserved for
   high-traffic or scoped setups? Leaning yes, but kept opt-in for v1.
2. Build `rite mentions follow` before or after the v1 plugin ships? Current call:
   after — the watch-all version is correct, just not maximally efficient.
