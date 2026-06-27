# Review: rite Claude Code Channel Plugin

Reviewed proposal: `ws/default/notes/claude-code-channel-plugin.md`

Research sources:

- Claude Code Channels reference: https://code.claude.com/docs/en/channels-reference
- Claude Code Plugins reference: https://code.claude.com/docs/en/plugins-reference
- Claude Code changelog: https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md

Local code checked:

- `ws/default/src/cli/history.rs`
- `ws/default/src/core/channel.rs`
- `ws/default/src/core/names.rs`
- `ws/default/src/cli/subscribe.rs`
- `ws/default/src/core/project.rs`

## Executive Summary

The proposal is directionally strong. It chooses the right primitive: Claude Code channels are explicitly built for MCP servers that push external events into an already-running session, and the proposed `reply_target` idea is the right way to avoid making Claude reconstruct rite DM routing.

The main issue is that the current plan treats a few unstable or underspecified parts as implementation details. The lossless watch path is not ready: current `rite history --format json -f --after-offset <offset>` can skip messages because the initial bounded read and the follow cursor are not the same cursor. The proposal also assumes JSON subscription output that does not exist today, and it does not spell out the full MCP tool discovery contract required for two-way channels.

My recommendation is to approve the idea, but gate implementation on a small rite streaming API fix and a stricter first-version contract: explicit watched channels, no subscription auto-discovery by default, no permission relay, string-only channel meta, and sender filtering treated as a prompt-injection boundary even in the trusted-local model.

## Proposed Changes

### [High Impact, High Effort] Change #1: Make Streaming Lossless Before Building The Channel Server

**Current State:**

The proposal recommends:

```bash
rite history <channel> --format json -f --after-offset <offset>
```

It correctly notes that `rite history` applies the count limit before follow mode and can drop messages if many arrive between the initial read and follow handoff.

Local code confirms this is worse than a minor caveat. In `history.rs`, JSON follow mode emits `output.messages` first, then calls `follow_channel_json()`. That function initializes its cursor with the channel file's current EOF, not with `output.next_offset`. With `--after-offset` and the default count of 50, any unread records after those first 50 and before EOF can be skipped.

**Proposed Change:**

Promote this from caveat to blocking prerequisite. Add a dedicated lossless streaming path before implementing the channel server:

- Preferred: add `rite history --format json -f --after-offset <offset> --no-limit` or `rite follow <channel> --format json --from-offset <offset>`.
- The follow cursor must start at the offset returned after the initial read, not current EOF.
- The channel server should use `-n 0 --show-offset --format json` only to compute the startup EOF baseline.
- Do not rely on "very high `-n`" as a correctness strategy.

**Rationale:**

Claude Code channel delivery itself is not acknowledged: the docs say `mcp.notification()` resolves once the message is written to transport, not once Claude processes it. If rite also has a lossy handoff, operators have no reliable way to know which messages were missed.

**Benefits:**

- Prevents silent message loss during bursts.
- Gives the channel server a simple contract: every JSONL record after offset is delivered exactly once unless the process exits.
- Makes tests deterministic.

**Trade-offs:**

- Requires a rite CLI/API change before the Bun server can be considered production-ready.
- Slightly delays the visible channel integration, but avoids building on a known lossy primitive.

**Implementation Notes:**

Add a test that writes more messages than the default history count between the baseline offset and follow startup, then verifies the stream emits all of them. Also test `-n 0 --show-offset --format json` as the session-open baseline.

**Git-Diff:**

```diff
--- a/ws/default/notes/claude-code-channel-plugin.md
+++ b/ws/default/notes/claude-code-channel-plugin.md
@@
-Use one subprocess per watched channel:
+Use one subprocess per watched channel, but only after rite has a lossless
+"stream from offset" mode:
 
 ```bash
-rite history <channel> --format json -f --after-offset <offset>
+rite follow <channel> --format json --from-offset <offset>
 ```
 
-This emits one compact `Message` JSON object per line as messages arrive. The server reads stdout line-by-line, parses JSONL, and emits Claude channel notifications.
+This emits one compact `Message` JSON object per line for every message after
+the supplied byte offset, then continues streaming new messages. The server
+reads stdout line-by-line, parses JSONL, and emits Claude channel notifications.
@@
 ```bash
-rite history <channel> --format json -n 1 --show-offset
+rite history <channel> --format json -n 0 --show-offset
 ```
 
 Parse `next_offset` from the JSON response and pass it to `--after-offset`.
 
-### Important caveat
+### Blocking prerequisite
 
-`rite history` currently applies the count limit before switching to follow mode. If many new messages land between the initial read and the follow handoff, a low count can drop some of them. If we keep this design, the server should set a very high `-n` value or rite should grow a dedicated no-limit `follow from offset` mode.
+`rite history` currently applies the count limit before switching to follow
+mode, then its follow loop starts from the channel file's current end. That can
+drop messages between the bounded initial read and the follow handoff. Do not
+ship the channel server on top of that behavior. First add a dedicated no-limit
+follow-from-offset mode, or change history follow mode so its streaming cursor
+continues from the returned `next_offset`.
```

### [High Impact, Low Effort] Change #2: Spell Out The Full MCP Channel Contract

**Current State:**

The proposal includes the notification call and a `tools: [{ ... }]` shape, but it does not explicitly require the MCP server constructor capabilities and request handlers needed for Claude Code to discover and invoke the reply tool.

The current Claude Code docs require a channel server to declare `capabilities.experimental['claude/channel']`, connect over stdio, and, for two-way channels, set `capabilities.tools: {}` plus standard MCP tool handlers. The docs also say the `instructions` string is how Claude learns which tag attributes to pass back.

**Proposed Change:**

Add an "MCP Contract" section with:

- `experimental: { 'claude/channel': {} }`.
- `tools: {}` because this is a two-way chat bridge.
- `ListToolsRequestSchema` and `CallToolRequestSchema` handlers for `reply`.
- A clear startup diagnostic if the server is loaded as a normal MCP server but not as a channel.

**Rationale:**

Without `capabilities.tools: {}` and the request handlers, the server can inject inbound events but Claude cannot send replies. Without the channel capability, notifications may be written to stdio but not routed into the session.

**Benefits:**

- Reduces ambiguity for the implementer.
- Makes local testing align with the official channel examples.
- Gives reviewers a concrete contract to validate.

**Trade-offs:**

- Adds some MCP boilerplate to the proposal.

**Implementation Notes:**

Use the official SDK pattern from the docs: create the `Server`, register tool request handlers before `mcp.connect(new StdioServerTransport())`, then start any local watchers.

**Git-Diff:**

```diff
--- a/ws/default/notes/claude-code-channel-plugin.md
+++ b/ws/default/notes/claude-code-channel-plugin.md
@@
 ## Design Overview
@@
 Multiple agents can watch the same rite channel simultaneously. Each Claude Code session gets its own event stream.
+
+## MCP Contract
+
+The server must be a channel-capable MCP server, not only a normal MCP tool
+server:
+
+```ts
+const mcp = new Server(
+  { name: 'rite-channel', version: '0.0.1' },
+  {
+    capabilities: {
+      experimental: { 'claude/channel': {} },
+      tools: {},
+    },
+    instructions: RITE_CHANNEL_INSTRUCTIONS,
+  },
+)
+```
+
+Two-way support requires standard MCP tool discovery and call handlers:
+
+- `ListToolsRequestSchema` returns the `reply` tool schema.
+- `CallToolRequestSchema` validates arguments and shells out to `rite send`.
+- The handlers are registered before `mcp.connect(new StdioServerTransport())`.
+
+The server should log a clear startup line to stderr with the resolved agent,
+watched channels, and whether it registered the channel capability. If events do
+not appear in Claude, users should check `/mcp` and the Claude debug log.
```

### [High Impact, Low Effort] Change #3: Fix Channel Selection Assumptions

**Current State:**

The proposal says watched channels come from:

1. the agent's own DM target(s)
2. `RITE_CHANNELS`
3. optional `rite subscriptions list --format json`

There are two problems:

- `rite subscriptions list` currently prints text and does not accept/output JSON in `src/cli/subscribe.rs`.
- "The agent's own DM target(s)" is not a concrete discovery rule. Existing DMs are stored as `_dm_<agent1>_<agent2>` files, and first-time DMs may create new channel files after the channel server has already started.

**Proposed Change:**

For v1:

- Watch explicit project channels from `RITE_CHANNELS`.
- Watch existing DM channels whose parsed participants include `myAgent`.
- Watch the channels directory for newly-created `_dm_*` files involving `myAgent`, then start a stream for that DM from offset 0 or from the creation-time EOF depending on the desired catch-up semantics.
- Defer subscription auto-discovery until `rite subscriptions list --format json` exists.

**Rationale:**

Auto-discovery is useful, but it should not be based on parsing human text output. DM delivery is core to this feature; it needs a precise rule for both existing and newly-created DM channels.

**Benefits:**

- Avoids brittle parsing.
- Handles first-time direct messages.
- Keeps v1 behavior understandable and auditable.

**Trade-offs:**

- Users must set `RITE_CHANNELS` for project channels until JSON subscription output exists.
- Dynamic DM discovery adds a little watcher complexity.

**Implementation Notes:**

Reuse or mirror `dm_agents()` from `src/core/channel.rs`. If subscription discovery is added later, add JSON output to the Rust command rather than parsing colored text.

**Git-Diff:**

```diff
--- a/ws/default/notes/claude-code-channel-plugin.md
+++ b/ws/default/notes/claude-code-channel-plugin.md
@@
 Watch channels from three sources, merged and deduplicated:
 
-1. Always: the agent's own DM target(s)
+1. Always: existing DM channels whose participants include the current agent
 2. Explicit config: `RITE_CHANNELS` env var (comma-separated)
-3. Optional auto-discovery: `rite subscriptions list --format json`
+3. Later: optional auto-discovery from `rite subscriptions list --format json`
+   after that command has real machine-readable output
 
 Example: an agent working on the `rite` project sets `RITE_CHANNELS=rite` and receives both project-channel traffic and direct messages.
+
+The server must also notice newly-created DM channel files while it is running.
+A first DM from another agent may create `_dm_<a>_<b>.jsonl` after startup; if
+either participant is `myAgent`, the server should start watching that file.
```

### [High Impact, Low Effort] Change #4: Treat Sender Filtering As A Security Boundary

**Current State:**

The proposal filters only `message.agent == my_agent` before forwarding to Claude. It does have a good security model section that says rite is trusted local storage, not authenticated remote messaging.

The current Claude Code docs are stricter for channels generally: they call ungated channels a prompt-injection vector and recommend sender allowlisting before emitting channel notifications. For rite, a local sender can choose any agent name, so an allowlist is not cryptographic authentication, but it is still useful fail-closed routing.

**Proposed Change:**

Add explicit v1 filtering:

- Always drop self-authored messages.
- Optionally allow only `RITE_ALLOWED_AGENTS` when set.
- Optionally allow only labels matching `RITE_FORWARD_LABELS` when set.
- Never enable `claude/channel/permission` until there is real sender authentication or a deliberate same-user approval model.
- At startup, warn if `RITE_DATA_DIR` exists with unsafe ownership or permissions where the platform can check it.

**Rationale:**

The channel injects text into an active coding session with local tool access. Even under a same-user model, forwarding every message from every subscribed channel gives too much ambient authority.

**Benefits:**

- Reduces accidental prompt injection from noisy/shared channels.
- Makes the trusted-local security claim operational, not just documentary.
- Keeps permission relay safely out of v1.

**Trade-offs:**

- Agent names remain spoofable without deeper rite identity changes.
- Allowlist configuration adds one more setup knob.

**Implementation Notes:**

Do not overstate this as authentication. Name it "routing allowlist" or "sender allowlist" and keep the proposal's caveat that same-user write access to the rite data directory is trusted.

**Git-Diff:**

```diff
--- a/ws/default/notes/claude-code-channel-plugin.md
+++ b/ws/default/notes/claude-code-channel-plugin.md
@@
 ## Message Filtering
 
-Before emitting a notification, drop:
+Before emitting a notification, apply filters in this order:
 
 - messages where `message.agent == my_agent` to prevent self-loops
+- messages whose sender is not in `RITE_ALLOWED_AGENTS`, when that variable is set
+- messages whose labels do not intersect `RITE_FORWARD_LABELS`, when that variable is set
+
+This is not authentication; rite sender names are local assertions. It is a
+routing allowlist that reduces accidental prompt injection from channels the
+agent did not intend to delegate to Claude.
@@
 | `RITE_CHANNELS` | Comma-separated list of project channels to watch. |
+| `RITE_ALLOWED_AGENTS` | Optional comma-separated sender allowlist. If unset, trust the local rite data dir. |
+| `RITE_FORWARD_LABELS` | Optional comma-separated labels that must be present before forwarding. |
 | `RITE_DATA_DIR` | Optional rite data directory override, passed through to subprocesses. |
@@
-If we want remote approval/deny flows through the same channel, Claude Code also supports the optional `claude/channel/permission` capability. That is not required for the first version, but it is worth keeping in mind for later.
+If we want remote approval/deny flows through the same channel, Claude Code also
+supports the optional `claude/channel/permission` capability. Do not declare
+that capability in v1. It should only be enabled after the bridge has real
+sender authentication or an explicit same-user approval policy, because a
+permission relay can approve local tool use.
```

### [Medium Impact, Low Effort] Change #5: Preserve Message Context And Enforce String Meta Values

**Current State:**

The proposed notification content is only `message.body`, and meta contains `message.id` and labels joined with commas.

Claude Code channel meta is `Record<string, string>`, and identifier-safe keys are required. The proposal has safe keys, but it should also make every value explicitly string typed and preserve enough context for Claude to understand what arrived.

**Proposed Change:**

Use string-only meta values:

- `msg_id: message.id.toString()`
- `ts: message.ts`
- `from_agent`
- `channel_name`
- `reply_target`
- `is_dm: 'true' | 'false'`
- `labels_json` if labels exist
- `attachments_json` or `attachment_count` if attachments exist

For content, include the body plus a compact attachment summary when attachments exist. Do not inline arbitrary file contents unless the user explicitly asks later.

**Rationale:**

The body alone can be ambiguous. Timestamps, labels, and attachments are part of the rite message contract, and Claude needs enough structured context to decide whether to reply, inspect a referenced file, or ignore an operational event.

**Benefits:**

- Avoids accidental type/schema issues in MCP notification params.
- Preserves routing and audit context.
- Makes future attachment support easier.

**Trade-offs:**

- More tokens per channel event.
- Attachment summaries may need careful truncation.

**Implementation Notes:**

Keep meta compact. If attachments become large or sensitive, send only names/types/URLs and rely on a later tool call for retrieval.

**Git-Diff:**

```diff
--- a/ws/default/notes/claude-code-channel-plugin.md
+++ b/ws/default/notes/claude-code-channel-plugin.md
@@
 Suggested payload:
 
 ```ts
+const meta: Record<string, string> = {
+  from_agent: message.agent,
+  channel_name: message.channel,
+  reply_target: isDm ? `@${message.agent}` : message.channel,
+  msg_id: message.id.toString(),
+  ts: message.ts,
+  is_dm: isDm ? 'true' : 'false',
+}
+if (message.labels?.length) meta.labels_json = JSON.stringify(message.labels)
+if (message.attachments?.length) {
+  meta.attachment_count = String(message.attachments.length)
+}
+
 await mcp.notification({
   method: 'notifications/claude/channel',
   params: {
-    content: message.body,
-    meta: {
-      from_agent: message.agent,
-      channel_name: message.channel,
-      reply_target: isDm ? `@${message.agent}` : message.channel,
-      msg_id: message.id,
-      ...(message.labels?.length ? { labels: message.labels.join(',') } : {}),
-    },
+    content: formatChannelContent(message),
+    meta,
   },
 })
 ```
```

### [Medium Impact, Low Effort] Change #6: Start From A Plugin Scaffold, Even For Local Development

**Current State:**

The proposal says it describes a channel server first and packaging as a plugin is a later step.

Current Claude Code plugin docs now have a first-class `channels` manifest field, and `claude plugin init --with channel` scaffolds a channel server, `.mcp.json`, and `package.json`.

**Proposed Change:**

Build the v1 in plugin shape from the beginning, but still support bare-server loading during development:

- Put the server under a plugin directory structure.
- Include `.claude-plugin/plugin.json` with a `channels` entry bound to the MCP server.
- Keep a documented `.mcp.json` path for `--dangerously-load-development-channels server:rite-channel`.
- Defer marketplace distribution, not plugin layout.

**Rationale:**

Packaging affects configuration, data directories, subprocess environment, and eventual install flow. Starting from plugin layout avoids rewriting the server around those constraints later.

**Benefits:**

- Aligns with current Claude Code tooling.
- Makes project/user/local install decisions explicit.
- Reduces packaging churn after the server works.

**Trade-offs:**

- Slightly more initial files than a single script.
- The plugin install path may still require development flags until allowlisted during the research preview.

**Implementation Notes:**

Use `claude plugin init rite-channel --with channel` as a reference shape, then replace the generated server with the rite bridge. Keep environment-based config for local testing; later, map user-configured plugin options into environment variables.

**Git-Diff:**

```diff
--- a/ws/default/notes/claude-code-channel-plugin.md
+++ b/ws/default/notes/claude-code-channel-plugin.md
@@
-This note describes a channel server first. Packaging it as an installable plugin is a later step.
+This note should use plugin layout from the start, while still allowing bare
+server loading during local development. Marketplace distribution can remain a
+later step, but the file layout should match Claude Code's channel plugin
+contract early.
@@
 ## MCP Registration
 
-For a local bare server during development/testing:
+For a local bare server during development/testing, the same server can still be
+registered through `.mcp.json`:
@@
-If we later package this as a real plugin, startup changes to the normal `--channels plugin:...` flow.
+The plugin layout should also include `.claude-plugin/plugin.json` with a
+`channels` entry whose `server` matches the plugin MCP server. Once distributed
+through a marketplace, startup changes to the normal `--channels plugin:...`
+flow.
```

### [Medium Impact, Medium Effort] Change #7: Add A Concrete Test And Diagnostics Plan

**Current State:**

The proposal describes behavior but does not define success tests, diagnostics, or process supervision.

**Proposed Change:**

Add a "Validation Plan" section covering:

- startup fails clearly without `RITE_AGENT` or `AGENT`
- invalid channel names and invalid DM targets are rejected
- self-authored messages are dropped
- DM reply target is `@sender`, not `_dm_*`
- project reply target is the bare project channel
- burst test proves no messages are skipped after startup offset
- `rite` subprocess exits are logged and restarted with bounded backoff
- `stderr` includes resolved identity, watched channels, and spawned command lines without secrets
- manual Claude Code smoke test confirms a channel event appears and reply calls `rite send`

**Rationale:**

This feature sits between a local file stream, subprocess management, MCP stdio, and Claude Code policy. Failures will otherwise look like "Claude ignored my message."

**Benefits:**

- Gives implementers a done definition.
- Makes lossy delivery and policy failures visible.
- Reduces debugging time for preview-channel quirks.

**Trade-offs:**

- Requires a small fake MCP/stdio harness or manual smoke-test script.

**Implementation Notes:**

Keep the server tests mostly outside Claude Code by unit-testing message-to-notification conversion and subprocess parsing. Reserve one manual or integration checklist for actual Claude Code channel loading because org policy and login state are external.

**Git-Diff:**

```diff
--- a/ws/default/notes/claude-code-channel-plugin.md
+++ b/ws/default/notes/claude-code-channel-plugin.md
@@
 ## Open Questions
 
 1. Should this ship first as `scripts/rite-channel.ts`, or only after it is packaged as a plugin?
 2. Is subscription auto-discovery worth enabling by default, or should channel watch lists stay explicit?
 3. Should rite grow a dedicated streaming API for "follow from offset with no count cap" before this is implemented?
+
+## Validation Plan
+
+Automated tests should cover identity resolution, target validation, self-message
+filtering, DM reply target computation, project-channel reply target
+computation, label/agent allowlist filtering, and burst delivery from an offset
+larger than the default history count.
+
+Manual Claude Code smoke test:
+
+1. Register the MCP server.
+2. Start Claude Code with the development channel flag.
+3. Send a rite message from another agent.
+4. Confirm the `<channel source="rite-channel" ...>` event appears.
+5. Ask Claude to call the reply tool.
+6. Confirm `rite history` shows the reply with `--agent myAgent`.
+
+Diagnostics should include startup stderr showing resolved agent, data dir,
+watched channels, and each spawned rite subprocess. If a subprocess exits, log
+the exit status and restart with bounded backoff.
```

## Priority Summary

Do immediately:

- Make the MCP contract explicit.
- Fix channel selection assumptions.
- Add sender/label filtering and permission-relay guardrails.
- Make meta values string-only.

Must include before implementation is considered ready:

- Add a lossless rite stream-from-offset mode or fix `history -f --after-offset`.
- Add burst/no-drop tests.

Should include:

- Start from plugin layout, even if marketplace distribution waits.
- Add diagnostics and subprocess restart behavior.

Defer:

- Subscription auto-discovery until JSON output exists.
- Permission relay until sender authentication or an explicit same-user approval policy exists.

## Open Questions Answered

1. Ship first as plugin-shaped code, not only as `scripts/rite-channel.ts`. Bare server registration can still be supported for development.
2. Keep channel watch lists explicit in v1. Revisit subscription auto-discovery after `rite subscriptions list --format json` exists.
3. Yes, rite should grow a dedicated streaming API or fix history follow mode before the channel server is implemented.
