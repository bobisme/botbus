# Looking at the TUI

Two different jobs, two different tools. Pick by what you are trying to do.

## Verify a TUI change (the common case)

Use `vessel`. It is a PTY runtime over Unix sockets, so it needs no compositor
and works over SSH and in sandboxes.

```bash
# 1. Build. A stale binary silently shows pre-change output.
cargo build --release

# 2. Seed an isolated data dir. Never point the TUI at the live one —
#    it holds real hooks that spawn real agents.
D=$(mktemp -d)
export RITE_DATA_DIR=$D
PARENT=$(./target/release/rite send --agent alice demo "a question" --format json | jq -r .id)
./target/release/rite send --agent bob demo "an answer" --reply-to "$PARENT" -q

# 3. Spawn, look, kill.
vessel spawn --name tui-check --env RITE_DATA_DIR=$D --cwd $PWD -- ./target/release/rite ui -c demo
sleep 2
vessel snapshot tui-check
vessel kill tui-check
```

`vessel snapshot` prints the screen as text, which is enough to check layout,
indentation, badges, and truncation.

Beyond a static look, `vessel` also drives the TUI:

| Need | Command |
|------|---------|
| Press keys | `vessel send-keys tui-check Down Down Enter` |
| Type text | `vessel send tui-check "hello" --enter` |
| Wait for output | `vessel wait tui-check --contains "reply"` |
| Assert in a script | `vessel assert tui-check --contains "↩ reply"` |
| Test narrow terminals | `vessel resize tui-check --cols 60 --rows 20` |
| Full transcript | `vessel dump tui-check` |

Use `resize` for anything width-sensitive. Truncation and wrapping bugs only
appear in narrow terminals, and the default size hides them.

**Always `--env RITE_DATA_DIR=`.** The live data directory drives the real hook
fleet. A stray message there spawns real agents in real repositories.

**Always rebuild first.** `target/release/rite` does not rebuild itself, and a
stale binary shows the old rendering — which reads exactly like a change that
failed to land.

## Regenerate the README image

```bash
./scripts/screenshot-tui.sh           # 1200x800 to images/tui.webp
./scripts/screenshot-tui.sh 1600 900  # custom dimensions
```

Requires kitty, jq, ImageMagick, and a running **niri** or **Hyprland** session
(plus grim on Hyprland). The script floats a window at the requested size,
captures it, and converts the result to webp.

It picks the compositor from its IPC handle — `NIRI_SOCKET` or
`HYPRLAND_INSTANCE_SIGNATURE` — rather than from which binaries are installed.
`hyprctl` is frequently present on machines not running Hyprland, and it exits 0
even when it cannot reach a compositor, so neither the binary nor its exit
status tells you anything.

With no compositor — headless, SSH, a sandbox — the script fails immediately and
says so rather than erroring partway through. That is the normal case for an
agent, and it is not a problem to work around: use `vessel` above.

This is only for the README hero image. Do not reach for it to check your work.

On niri, captures land in the directory from your `screenshot-path` setting in
`~/.config/niri/config.kdl`, because niri chooses the filename rather than
accepting one. The script moves the newest capture out of there, so that
directory is left as it was found. If `screenshot-path` is `null`, nothing
reaches disk and the script tells you to set one.
