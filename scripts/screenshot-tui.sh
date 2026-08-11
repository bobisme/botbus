#!/usr/bin/env bash
# Capture a screenshot of the TUI for the README.
#
# Supports niri and Hyprland. This is ONLY for regenerating images/tui.webp.
# To *verify* a TUI change, use vessel instead — it needs no compositor.
# See .agents/tui-screenshot.md.
#
# Requires: kitty, jq, ImageMagick (magick), plus grim on Hyprland.

set -euo pipefail

WIDTH="${1:-1200}"
HEIGHT="${2:-800}"
OUTPUT="${3:-images/tui.webp}"
TITLE="rite-screenshot"

cd "$(dirname "$0")/.."

# Detect the compositor by its IPC handle, not by which binaries are installed.
# `hyprctl` is often present on machines not running Hyprland, and it exits 0
# even when it cannot reach a compositor — so testing for the binary, or for a
# zero exit, both report success on a machine where every dispatch will fail.
detect_compositor() {
	if [[ -n "${NIRI_SOCKET:-}" ]]; then echo niri; return; fi
	if [[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]]; then echo hyprland; return; fi
	if [[ -n "${SWAYSOCK:-}" ]]; then echo sway; return; fi
	case "${XDG_CURRENT_DESKTOP:-}" in
	niri) echo niri ;;
	Hyprland) echo hyprland ;;
	sway) echo sway ;;
	*) echo unknown ;;
	esac
}

COMPOSITOR=$(detect_compositor)

case "$COMPOSITOR" in
niri | hyprland) ;;
*)
	cat >&2 <<-EOF
		Error: no supported compositor IPC found (detected: $COMPOSITOR).

		This script drives a real compositor to size and capture a window, so it
		cannot run headless, over SSH, or in a sandbox. It supports niri and
		Hyprland.

		To look at the TUI instead of photographing it, use vessel — it needs no
		compositor:

		    vessel spawn --name tui-check --env RITE_DATA_DIR=\$D --cwd \$PWD -- ./target/release/rite ui
		    vessel snapshot tui-check
		    vessel kill tui-check

		See .agents/tui-screenshot.md.
	EOF
	exit 1
	;;
esac

# Read niri's configured screenshot-path. niri writes captures there rather than
# to a path we choose, so we locate the result afterwards and move it.
niri_shot_dir() {
	local config="${XDG_CONFIG_HOME:-$HOME/.config}/niri/config.kdl"
	local line path
	# Skip commented-out lines; take the last active setting.
	line=$(grep -E '^[[:space:]]*screenshot-path[[:space:]]' "$config" 2>/dev/null | tail -1 || true)

	if [[ "$line" =~ null ]]; then
		echo "Error: niri screenshot-path is null, so captures never reach disk." >&2
		echo "Set a screenshot-path in $config to use this script." >&2
		exit 1
	fi

	if [[ -z "$line" ]]; then
		# niri's built-in default when the setting is absent.
		path="$HOME/Pictures/Screenshots/x.png"
	else
		path=$(sed -E 's/.*"(.*)".*/\1/' <<<"$line")
		path="${path/#\~/$HOME}"
	fi

	dirname "$path"
}

cleanup() {
	[[ -n "${KITTY_PID:-}" ]] && kill "$KITTY_PID" 2>/dev/null || true
}
trap cleanup EXIT

kitty --title "$TITLE" -e bash -c "./target/release/rite ui; read" &
KITTY_PID=$!
sleep 0.8

RAW=$(mktemp --suffix=.png)

if [[ "$COMPOSITOR" == niri ]]; then
	SHOT_DIR=$(niri_shot_dir)
	mkdir -p "$SHOT_DIR"

	WIN_ID=$(niri msg --json windows |
		jq -r --arg t "$TITLE" 'map(select(.title == $t)) | .[0].id // empty')

	if [[ -z "$WIN_ID" ]]; then
		echo "Error: could not find the '$TITLE' window" >&2
		exit 1
	fi

	# Float first — a tiled window ignores the size we ask for.
	niri msg action move-window-to-floating --id "$WIN_ID" >/dev/null
	niri msg action set-window-width --id "$WIN_ID" "$WIDTH" >/dev/null
	niri msg action set-window-height --id "$WIN_ID" "$HEIGHT" >/dev/null
	sleep 0.5

	# Note the newest existing capture so we can tell ours apart. The filename
	# is strftime-formatted, so we cannot predict it. niri's default pattern
	# contains spaces, so sort on mtime rather than parsing `ls`.
	# `sed -n 1p` rather than `head -1`: head closes the pipe early, which under
	# `set -o pipefail` turns SIGPIPE in find/sort into a script failure.
	newest_png() {
		find "$SHOT_DIR" -maxdepth 1 -name '*.png' -printf '%T@\t%p\n' 2>/dev/null |
			sort -rn | cut -f2- | sed -n 1p
	}

	BEFORE=$(newest_png)

	echo "Capturing ${WIDTH}x${HEIGHT} window (niri)..."
	niri msg action screenshot-window --id "$WIN_ID" -d true >/dev/null
	sleep 0.7

	CAPTURED=$(newest_png)
	if [[ -z "$CAPTURED" || "$CAPTURED" == "$BEFORE" ]]; then
		echo "Error: niri produced no new capture in $SHOT_DIR" >&2
		exit 1
	fi

	mv "$CAPTURED" "$RAW"
else
	hyprctl dispatch focuswindow "title:$TITLE" >/dev/null
	hyprctl dispatch togglefloating >/dev/null
	hyprctl dispatch resizeactive exact "$WIDTH" "$HEIGHT" >/dev/null
	hyprctl dispatch centerwindow >/dev/null
	sleep 0.3

	GEOMETRY=$(hyprctl clients -j |
		jq -r --arg t "$TITLE" '.[] | select(.title == $t) | "\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')

	if [[ -z "$GEOMETRY" ]]; then
		echo "Error: could not find the '$TITLE' window" >&2
		exit 1
	fi

	echo "Capturing ${WIDTH}x${HEIGHT} window (Hyprland)..."
	grim -g "$GEOMETRY" "$RAW"
fi

mkdir -p "$(dirname "$OUTPUT")"
magick "$RAW" -quality 90 "$OUTPUT"
rm -f "$RAW"

SIZE=$(du -h "$OUTPUT" | cut -f1)
echo "Saved to $OUTPUT ($SIZE)"
