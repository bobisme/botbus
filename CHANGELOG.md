# Changelog

All notable changes to this project are documented here. This project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.32.0] - 2026-08-09

### Added

- `rite mentions follow` — a single-process JSONL stream of every message that
  mentions you, across all channels, plus your DMs. Replaces the one-watcher-
  per-channel approach: one inotify watch, incremental reads with per-channel
  offsets, and constant memory as channels accumulate. Your own DMs stream by
  default; pass `--no-dms` for a mentions-only stream. A mention never routes a
  message out of a DM you are not part of.
- Message threading. `Message` carries an optional `reply_to`; `rite send
  --reply-to <id>` anchors a reply and `rite history --thread <id>` retrieves a
  thread. Missing, unsynced, and tombstoned parents degrade to a labelled
  fragment instead of a silent reparent. Self-references and cycles terminate.
- `rite claims list` and `rite claims check` report a `stale` flag when the
  claim holder's presence has lapsed, so a claim held by a dead agent is
  visible as such. Staleness is a report: nothing auto-releases another agent's
  claim. Presence is derived from activity, with a TTL of three heartbeat
  intervals so one missed beat does not flap an agent offline.
- Hook spawn leases (`rite hooks add --lease`). One live spawn per hook and
  channel, with triggers arriving during a turn batched and deduplicated for
  the next spawn instead of dropped or spawned per message. Opt-in; existing
  cooldown hooks are unchanged. A lease whose holder has provably gone away
  does not block forever.
- `rite send` gained `--format`. JSON and text output now report the new
  message id.

### Fixed

- JSONL readers no longer abort a whole file over one unreadable record. A
  record whose type this build does not recognize is read and preserved; a
  record whose type is known but whose body does not fit is reported as
  damaged. `rite doctor` reports both counts separately, so a future format and
  real corruption are never confused.
- Hook records preserve fields this build does not understand across a rewrite,
  so an older rite firing a hook can no longer silently erase newer
  configuration such as a spawn lease.
- `AGENTS.md` documented a `rite wait --mention` flag that does not exist. The
  flag is `--mentions`.

### Changed

- **`rite send` text output changed.** It previously printed
  `Sent: Message sent to #channel`; it now leads with `id: <ulid>`. Scripts
  parsing the old string need updating. Interactive (TTY) output keeps the
  confirmation line and adds the id.

## [0.31.3] - 2026-06-16

### Fixed

- `history -f` (follow mode) no longer drops messages. The follow cursor is now
  seeded from the initial bounded read's `next_offset` instead of the file's
  current EOF, and any startup backlog is drained before the event loop. With
  `--after-offset` plus a count limit, messages between the bounded read and EOF
  were previously skipped.
- `history --after-id <id> -n <count>` now reports a correct `next_offset`.
  Previously the result was truncated to `count` but `next_offset` pointed at
  EOF, so paginating with `--after-offset` skipped every message between the
  count-th returned message and EOF. `--after-id` now resolves to a byte offset
  and shares the lossless offset-based read path, and the pagination advice hint
  works for both `--after-offset` and `--after-id`.

## [0.31.2] - 2026-05-01

### Added

- `wait --from` conversation filter.

### Fixed

- Fresh-eyes bug sweep.
