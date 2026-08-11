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
./scripts/screenshot-tui.sh           # 1200x800 to images/tui.png
./scripts/screenshot-tui.sh 1600 900  # custom dimensions
```

Requires kitty, grim, jq, pngquant, and **Hyprland**. The script spawns a
floating window, captures it, and compresses the result.

This is only for the README hero image. Do not reach for it to check your work —
use `vessel` above.

> **Broken on niri.** The script drives window placement through `hyprctl`, so it
> fails wherever Hyprland is not running. Tracked in `bn-b4ai`.
