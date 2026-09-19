#!/bin/sh
# examples/weather.sh publishes dbar's fields as tab-separated key=value pairs.
# Waybar wants a line of text. Run the same script with the same arguments and
# render the first place the way gruvbox-islands.toml's format does, so the only
# difference between the bars is the drawing, not the fetching.
set -eu
root=${1:?usage: weather-shim.sh <dbar-root>}
"$root/examples/weather.sh" metric ~/.config/dbar/owm.key \
	45.2517,19.8369 44.8147,20.4143 43.9365,18.7986 2>/dev/null |
	head -n 1 |
	awk -F'\t' '
		{
			for (i = 1; i <= NF; i++) {
				split($i, kv, "=")
				f[kv[1]] = kv[2]
			}
			if (f["weather"] == "") print "unavailable"
			else printf "%s %s (%s) %d °C, %.1f m/s %s\n",
				f["icon"], f["weather"], f["location"], f["temp"], f["wind"], f["direction"]
		}
	'
