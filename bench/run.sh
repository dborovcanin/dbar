#!/bin/bash
# Bring up all three bars for a measurement run, and take them down again.
#
#   ./bench/run.sh up        start dbar, Waybar, and swaybar via a temporary
#                            sway bar block
#   ./bench/run.sh down      stop them and put the sway config back
#
# swaybar cannot be started on its own: sway spawns it, and only for a bar this
# session's config declares. So `up` appends a bar block to the sway config,
# reloads, and `down` restores the file it saved. A reload does not re-run `exec`
# lines, so nothing else in the session is restarted.
#
# Whatever bar you normally run is stopped for the duration and is NOT restarted
# by `down` - start it again the way your compositor config does.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BENCH=$ROOT/bench
SWAY_CONFIG=${SWAY_CONFIG:-$HOME/.config/sway/config}
BACKUP=$BENCH/.sway-config.backup
RUNTIME=${XDG_RUNTIME_DIR:-/tmp}/dbar-bench
MARK="# >>> dbar bench bar - remove me"

# Talk to the sway that is running, not to the one this shell was told about.
#
# A shell started in an earlier session carries that session's SWAYSOCK, and a stale one
# does not fail loudly here: `up` would append its bar block and never reload, and `down`
# would take the block back out and leave the bar on screen until something else reloaded.
find_swaysock() {
	if [ -n "${SWAYSOCK:-}" ] && swaymsg -t get_version >/dev/null 2>&1; then
		return
	fi
	local socket
	socket=$(ls -t "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"/sway-ipc.*.sock 2>/dev/null | head -1)
	[ -n "$socket" ] || { echo "no sway socket to talk to; is sway running?" >&2; exit 1; }
	export SWAYSOCK=$socket
	swaymsg -t get_version >/dev/null 2>&1 ||
		{ echo "the sway socket at $socket does not answer" >&2; exit 1; }
	echo "using the sway socket at $SWAYSOCK"
}

render() {
	mkdir -p "$RUNTIME"
	local hwmon temp_input=""
	# k10temp, coretemp and the rest are numbered in boot order, so the chip is
	# found by name rather than assumed to be hwmon4.
	for hwmon in /sys/class/hwmon/hwmon*; do
		case $(cat "$hwmon/name" 2>/dev/null) in
		k10temp | coretemp | zenpower | cpu_thermal)
			[ -r "$hwmon/temp1_input" ] && temp_input=$hwmon/temp1_input && break
			;;
		esac
	done
	[ -n "$temp_input" ] || { echo "no CPU temperature sensor found" >&2; exit 1; }

	local config
	for config in waybar/config.jsonc dbar/config.toml; do
		sed -e "s|@TEMP_INPUT@|$temp_input|" \
			-e "s|@DBAR_ROOT@|$ROOT|" \
			-e "s|@BENCH@|$BENCH|" \
			"$BENCH/$config.in" > "$RUNTIME/bench-$(basename "$config")"
	done
}

up() {
	find_swaysock
	[ -x "$ROOT/target/release/dbar" ] || { echo "build it first: make prod" >&2; exit 1; }
	grep -qF "$MARK" "$SWAY_CONFIG" &&
		{ echo "a run is already up; ./bench/run.sh down first" >&2; exit 1; }

	echo "stopping whatever bar is running"
	pkill -x dbar 2>/dev/null || true
	pkill -x waybar 2>/dev/null || true
	sleep 1

	render

	echo "starting dbar"
	(cd "$ROOT" && setsid "$ROOT/target/release/dbar" -c "$RUNTIME/bench-config.toml" \
		>"$RUNTIME/dbar.log" 2>&1 &)

	echo "starting Waybar"
	setsid waybar -c "$RUNTIME/bench-config.jsonc" -s "$BENCH/waybar/style.css" \
		>"$RUNTIME/waybar.log" 2>&1 &

	echo "adding a temporary bar block to $SWAY_CONFIG"
	cp "$SWAY_CONFIG" "$BACKUP"
	{
		echo ""
		echo "$MARK"
		echo "bar {"
		echo "    id dbar-bench"
		echo "    position bottom"
		echo "    font pango:JetBrainsMono NL, FontAwesome 14"
		echo "    status_command i3status-rs $BENCH/i3status/status.toml"
		echo "}"
	} >> "$SWAY_CONFIG"
	swaymsg reload >/dev/null

	sleep 5
	echo
	echo "up. Let them settle, then: ./bench/measure.sh idle"
}

# Take the block back out of the sway config, by its own marker.
#
# The block `up` appended is the only thing this run added, so removing exactly that is
# what putting the file back means. Copying the backup over the whole file would be a
# blunter instrument than the job needs: it would also undo anything edited while the run
# was up, and a stray backup from another machine - one that got committed, or copied in
# with the repo - would overwrite a config it was never taken from.
strip_block() {
	awk -v mark="$MARK" '
		$0 == mark { skipping = 1; if (blank) { blank = 0 } ; next }
		skipping && $0 == "}" { skipping = 0; next }
		skipping { next }
		{ if (blank) print ""; blank = 0 }
		/^$/ { blank = 1; next }
		{ print }
		END { if (blank) print "" }
	' "$SWAY_CONFIG" > "$SWAY_CONFIG.bench-tmp" && mv "$SWAY_CONFIG.bench-tmp" "$SWAY_CONFIG"
}

down() {
	find_swaysock
	echo "stopping dbar and Waybar"
	pkill -f "release/dbar -c $RUNTIME/bench-config.toml" 2>/dev/null || true
	pkill -f "waybar -c $RUNTIME/bench-config.jsonc" 2>/dev/null || true

	if grep -qF "$MARK" "$SWAY_CONFIG"; then
		echo "taking the bench bar block back out of $SWAY_CONFIG"
		strip_block
		rm -f "$BACKUP"
		# Last, and allowed to fail: the file is already what it was, and a reload that
		# did not happen is a swaybar still on screen rather than a config left edited.
		swaymsg reload >/dev/null || echo "could not reload sway; do it by hand" >&2
	elif [ -e "$BACKUP" ]; then
		# The block is gone but the run never ended: the file was edited by hand, or by
		# another copy of this script. The backup is left where it is rather than copied
		# over a file it may no longer match.
		echo "no '$MARK' block in $SWAY_CONFIG; the copy taken at 'up' is still at $BACKUP" >&2
	else
		echo "nothing to undo: no '$MARK' block and no saved copy" >&2
	fi
	echo "done. Start your own bar again the way your compositor config does."
}

case ${1:-} in
up) up ;;
down) down ;;
*) echo "usage: $0 [up|down]" >&2; exit 1 ;;
esac
