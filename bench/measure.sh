#!/bin/bash
# Measure what dbar, swaybar + i3status-rs and Waybar cost to leave running.
#
# All three bars are up at the same time, on the same screen, showing the same
# things, so that no run gets a quieter machine than another. CPU is utime+stime
# from /proc/PID/stat differenced across the window; memory is read from
# /proc/PID/status at the end of it. A bar that runs a helper is counted whole.
#
#   ./bench/measure.sh idle          three two-minute windows, no interaction
#   ./bench/measure.sh use           one window, while you use the bars
#   ./bench/measure.sh static        binary size and shared library count
#
# Start the bars with ./bench/run.sh first, and read bench/README.md before
# trusting a number that came out of here.
set -euo pipefail

WINDOW=${WINDOW:-120}
ROOT=$(cd "$(dirname "$0")/.." && pwd)

# A bar and everything it starts, since swaybar's status_command is swaybar's
# cost and not somebody else's.
RUNTIME=${XDG_RUNTIME_DIR:-/tmp}/dbar-bench

pids_of() {
	case $1 in
	dbar) pgrep -f "release/dbar -c $RUNTIME/bench-config.toml" || true ;;
	swaybar) { pgrep -x swaybar; pgrep -f "i3status-rs.*bench/i3status/status.toml"; } 2>/dev/null || true ;;
	waybar) pgrep -f "waybar -c $RUNTIME/bench-config.jsonc" || true ;;
	esac
}

jiffies() {
	local total=0 pid
	for pid in $(pids_of "$1"); do
		[ -r "/proc/$pid/stat" ] || continue
		# Fields 14 and 15, counted after the comm field, which may itself hold
		# spaces and brackets - so the prefix up to the last ')' goes first.
		local rest u s
		rest=$(sed 's/.*) //' "/proc/$pid/stat")
		u=$(echo "$rest" | cut -d' ' -f12)
		s=$(echo "$rest" | cut -d' ' -f13)
		total=$((total + u + s))
	done
	echo "$total"
}

mem() {
	local key=$1 bar=$2 total=0 pid v
	for pid in $(pids_of "$bar"); do
		[ -r "/proc/$pid/status" ] || continue
		v=$(awk -v k="$key:" '$1 == k { print $2 }' "/proc/$pid/status")
		total=$((total + ${v:-0}))
	done
	echo "$total"
}

threads() {
	local total=0 pid v
	for pid in $(pids_of "$1"); do
		[ -r "/proc/$pid/status" ] || continue
		v=$(awk '$1 == "Threads:" { print $2 }' "/proc/$pid/status")
		total=$((total + ${v:-0}))
	done
	echo "$total"
}

procs() { pids_of "$1" | grep -c . || true; }

check() {
	local bar missing=0
	for bar in dbar swaybar waybar; do
		if [ "$(procs "$bar")" -eq 0 ]; then
			echo "$bar is not running" >&2
			missing=1
		fi
	done
	[ "$missing" -eq 0 ] || { echo "start them with ./bench/run.sh" >&2; exit 1; }
}

window() {
	local label=$1 bar u0 u1 hz
	hz=$(getconf CLK_TCK)
	declare -A start
	for bar in dbar swaybar waybar; do start[$bar]=$(jiffies "$bar"); done
	sleep "$WINDOW"
	printf '%-24s %10s %10s %8s %10s %8s\n' "$label" resident heap CPU processes threads
	for bar in dbar swaybar waybar; do
		u0=${start[$bar]}
		u1=$(jiffies "$bar")
		printf '%-24s %7s MB %7s MB %6s %% %10s %8s\n' \
			"$bar" \
			"$(awk -v k="$(mem VmRSS "$bar")" 'BEGIN { printf "%.1f", k / 1024 }')" \
			"$(awk -v k="$(mem RssAnon "$bar")" 'BEGIN { printf "%.1f", k / 1024 }')" \
			"$(awk -v d="$((u1 - u0))" -v hz="$hz" -v w="$WINDOW" 'BEGIN { printf "%.2f", 100 * d / hz / w }')" \
			"$(procs "$bar")" "$(threads "$bar")"
	done
	echo
}

# The dynamic loader and the vDSO are not dependencies anybody chose, so they are
# not counted: what is being compared is how much of each bar lives outside its
# own binary.
libs() {
	ldd "$1" 2>/dev/null |
		grep '=>' |
		grep -vE 'ld-linux|linux-vdso' |
		grep -c . || echo 0
}

static() {
	local bar bin
	printf '%-24s %12s %18s\n' "" binary "shared libraries"
	for bar in "$ROOT/target/release/dbar" /usr/bin/swaybar /usr/bin/i3status-rs /usr/bin/waybar; do
		[ -x "$bar" ] || continue
		bin=$(awk -v b="$(stat -c%s "$bar")" 'BEGIN { printf "%.1f", b / 1048576 }')
		printf '%-24s %9s MB %18s\n' "$(basename "$bar")" "$bin" \
			"$(libs "$bar")"
	done
}

case ${1:-idle} in
idle)
	check
	echo "three ${WINDOW}s windows, no interaction - keep your hands off the bars"
	echo
	for i in 1 2 3; do window "idle window $i"; done
	;;
use)
	check
	echo "one ${WINDOW}s window - use the bars: cross them with the pointer,"
	echo "hover and click modules, switch workspaces. Starting now."
	echo
	window "in use"
	;;
static) static ;;
*)
	echo "usage: $0 [idle|use|static]" >&2
	exit 1
	;;
esac
