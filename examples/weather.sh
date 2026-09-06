#!/bin/sh
# The weather, for a dbar `command` module.
#
# dbar has no weather collector and is not going to grow one: the weather is a web
# service rather than something /proc knows, and fetching it is a script's job. This is
# that script - it prints one line of tab-separated `key=value` pairs per place, which is
# the whole of what a command module's side of the protocol is. With `pages = true` the
# module holds one reading per line and the wheel scrolls between them, so three cities
# cost one fetch and one module.
#
# The knobs come from the module's `params`, in the order it wrote them, so moving city
# or swapping units is a line in the config rather than an edit here:
#
#   $1     units: metric or imperial
#   $2     an OpenWeatherMap API key, or the path to a file holding one
#   $3...  where: one or more "latitude,longitude", a page each
#
# The places come last because they are the part that grows. A key kept in a file rather
# than in the config is the point of accepting both: a dotfile that goes into a git
# repository should not be carrying a credential.
#
# Needs `curl` and `jq`. Both endpoints used here are on OpenWeatherMap's free tier.
set -eu

units=${1:-metric}
if [ "$#" -gt 0 ]; then shift; fi
key=${1:-}
if [ "$#" -gt 0 ]; then shift; fi
if [ -f "$key" ]; then
	key=$(cat "$key")
fi
# Somewhere to be, so running this by hand says something.
[ "$#" -gt 0 ] || set -- 45.2517,19.8369

api=https://api.openweathermap.org/data/2.5

# Every failure still prints a line. A place that drops out would shift every page after
# it along, and a module that draws nothing has nothing left to click to try again.
unreachable() {
	printf 'state=error\ticon=!\tweather=%s\tlocation=%s\n' "$1" "$2"
}

for where in "$@"; do
	lat=${where%%,*}
	lon=${where##*,}

	now=$(curl -fsS --max-time 10 \
		"$api/weather?lat=$lat&lon=$lon&units=$units&appid=$key") || {
		unreachable unreachable "$where"
		continue
	}
	# Three steps of three hours: the nine hours the alternate wording is about. Losing
	# this leaves the forecast fields empty, and the format says so rather than the page
	# going.
	soon=$(curl -fsS --max-time 10 \
		"$api/forecast?lat=$lat&lon=$lon&units=$units&cnt=3&appid=$key") || soon=null

	jq -rn --argjson now "$now" --argjson soon "$soon" '
		def icon:
			if . >= 200 and . < 300 then "⛈"
			elif . >= 300 and . < 600 then "🌧"
			elif . >= 600 and . < 700 then "❄"
			elif . >= 700 and . < 800 then "🌫"
			elif . == 800 then "☀"
			else "☁" end;
		def compass:
			["N","NE","E","SE","S","SW","W","NW"][(((. + 22.5) / 45) | floor) % 8];
		def pairs: to_entries | map("\(.key)=\(.value)") | join("\t");

		($soon.list // []) as $steps
		| ($steps | map(.main.temp)) as $ahead
		| {
			icon:           ($now.weather[0].id | icon),
			weather:        $now.weather[0].main,
			location:       $now.name,
			temp:           ($now.main.temp | round),
			wind:           $now.wind.speed,
			direction:      ($now.wind.deg | compass),
			forecast_icon:  (if $steps == [] then "" else ($steps[-1].weather[0].id | icon) end),
			forecast_avg:   (if $ahead == [] then "" else (($ahead | add) / ($ahead | length) | round) end),
			forecast_min:   (if $ahead == [] then "" else ($ahead | min | round) end),
			forecast_max:   (if $ahead == [] then "" else ($ahead | max | round) end),
		}
		| pairs
	' || unreachable unreadable "$where"
done
