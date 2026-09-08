#!/usr/bin/env bash
set -euo pipefail

# A calendar you glance at and dismiss.
#
# Run with no arguments it opens a floating foot window running itself again, so
# it can be bound to a key or called from anywhere without a wrapper. The window
# lasts only as long as you are looking at it: n and p walk the months, t comes
# back to today, and anything else closes it.
#
#   for_window [app_id="calendar"] floating enable, move position center

APP_ID=calendar
# Three months side by side is 64 columns; the rest is the header and the hint.
SIZE=68x13

if [[ ${1:-} != --view ]]; then
    self=$(realpath "${BASH_SOURCE[0]}")
    # The server holds the fonts and the config, so a client window costs almost
    # nothing to open. Without one running there is nothing to talk to.
    term=footclient
    pgrep -x foot >/dev/null 2>&1 || term=foot
    exec "$term" --app-id "$APP_ID" -W "$SIZE" -- "$self" --view
fi

month=$((10#$(date +%m)))
year=$((10#$(date +%Y)))

while true; do
    clear
    printf '  %s\n\n' "$(date '+%A, %-d %B %Y')"
    # -m starts the week on Monday, which is what the calendar on the wall does
    # here; the locale is en_US and would otherwise start it on Sunday.
    cal -3 -m "$month" "$year"
    printf '\n  n next    p prev    t today    q quit\n'

    IFS= read -rsn1 key || break
    # An arrow arrives as an escape sequence. Nothing following the escape means
    # the key itself was Escape, which is one of the ways out.
    if [[ $key == $'\e' ]]; then
        read -rsn2 -t 0.05 rest || rest=
        key+=$rest
    fi

    case $key in
        n | l | $'\e[C')
            ((month++)) || true
            if ((month > 12)); then
                month=1
                ((year++)) || true
            fi
            ;;
        p | h | $'\e[D')
            ((month--)) || true
            if ((month < 1)); then
                month=12
                ((year--)) || true
            fi
            ;;
        t)
            month=$((10#$(date +%m)))
            year=$((10#$(date +%Y)))
            ;;
        *)
            break
            ;;
    esac
done
