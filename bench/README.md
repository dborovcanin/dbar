# The benchmark harness

Everything needed to reproduce the table in the README's [What it costs to leave
running](../README.md#what-it-costs-to-leave-running). It lives here because the
first run did not keep its configs, and rebuilding a Waybar config module for
module is most of the work of repeating the comparison.

The point of the harness is that all three bars are asked to show **the same
fifteen things, on the same intervals**, at the same time, on the same screen,
and to draw them as nearly the same way as each bar's own vocabulary allows. A
bar measured alone on a quiet machine is not comparable to one measured
alongside two others, a bar showing eleven modules is not comparable to one
showing fifteen, and a bar drawing bare numbers on a flat strip is not
comparable to one drawing icons, rounded islands and shaded runs.

## The three configurations

| bar | config | what draws what |
| --- | --- | --- |
| dbar | [`dbar/config.toml.in`](dbar/config.toml.in) | all fifteen modules |
| Waybar | [`waybar/config.jsonc.in`](waybar/config.jsonc.in) + [`waybar/style.css`](waybar/style.css) | all fifteen modules |
| swaybar + i3status-rs | [`i3status/status.toml`](i3status/status.toml) | i3status-rs draws eleven blocks; swaybar itself draws the workspaces, the binding mode and the tray |

All three live here, so what was measured is one directory rather than a config
in `examples/` that the project is free to change for its own reasons. dbar's is
a copy of [`../examples/gruvbox-islands.toml`](../examples/gruvbox-islands.toml)
and everything below its `[bar]` line should stay identical to it:

```sh
diff <(sed -n '/^\[bar\]/,$p' examples/gruvbox-islands.toml) \
     <(sed -n '/^\[bar\]/,$p' bench/dbar/config.toml.in | sed 's|@DBAR_ROOT@/||')
```

The two `.in` files are templates: `run.sh` fills in the machine's CPU sensor and
this checkout's path and writes `bench-config.toml` and `bench-config.jsonc` into
`$XDG_RUNTIME_DIR/dbar-bench`, which is what the bars are actually started with.
The one path dbar needs spelled out is the weather script's - it runs argv
directly, with no shell to expand anything.

The modules, and the interval each is given:

| | dbar | Waybar | i3status-rs |
| --- | --- | --- | --- |
| workspaces | `workspaces` | `sway/workspaces` | swaybar |
| binding mode | `mode` | `sway/mode` | swaybar |
| window title | `window` | `sway/window` | — |
| tray | `tray` | `tray` | swaybar |
| media | `media` | `mpris` | `music` |
| weather | `command`, once | `custom/weather`, once | `weather`, 600 s |
| cpu | 2 s | 2 s | 5 s |
| memory | 5 s | 5 s | event |
| temperature | 5 s | 5 s | 5 s |
| keyboard layout | `language` | `sway/language` | `keyboard_layout` |
| network | 2 s | 2 s | event |
| volume | `audio`, PipeWire | `wireplumber` | `sound` |
| backlight | `backlight` | `backlight` | `backlight` |
| battery | 30 s | 30 s | event |
| clock | 1 m | 1 m | 5 s |

## What each bar is asked to draw

Waybar is given `gruvbox-islands.toml`'s appearance as closely as a stylesheet
can say it: the same gruvbox colours, the same 16px rounded islands with 3px
between them, the same runs of modules sharing one island in two alternating
shades, the same `Noto Sans` at the same 18px with the same fallback list, the
same tray icon size, the same fixed module widths, and an icon wherever dbar
draws one. Without that, the comparison measures a bar drawing flat text against
a bar drawing a themed one.

Three differences in the drawing are left standing, because Waybar has no way to
say them and all three leave dbar doing the extra work:

- **Icons.** dbar rasterises its own vector artwork at `icon_size = 20`. Waybar
  has no vector icons, so each module is given the Nerd Font glyph for the
  picture dbar draws there - a shaped and cached character rather than a path,
  which is if anything cheaper.
- **Workspace icons.** dbar names an icon for workspaces 0 to 3 and falls back to
  the workspace's own name. Waybar's `format-icons` takes literal strings and a
  `"default"` of `{name}` is drawn as those six characters rather than
  substituted, so Waybar draws names throughout and dbar rasterises artwork
  Waybar does not. Both bars are set to draw every workspace, not only this
  output's.
- **Separators.** dbar turns one shade into the next with a `curve` - a sigmoid
  with horizontal tangents. Waybar's vocabulary is a box with a border-radius, so
  a run's modules alternate between the two greys and only the run's outer
  corners are rounded.

Two smaller ones are cosmetic and cost nothing either way: Waybar's
`sway/language` prints xkb's own short name (`us`) where dbar's `layouts` table
renames it (`EN`), and the weather glyph the shared script prints lands on a
monochrome font in Waybar and a colour one in dbar.

Three asymmetries in *what is read* are deliberate, and all are against dbar's
favour or neutral:

- **swaybar has no window title**, so it is drawing fourteen things rather than
  fifteen - slightly *less* work than the other two, not more.
- **i3status-rs reads a different temperature sensor.** Its config names
  `chip = "thinkpad-isa-0000"`, which on the review machine costs about 2.6 ms a
  read; dbar's temperature module names no chip, so it takes the default CPU
  sensor at about 5 µs, and Waybar is pointed at that same CPU sensor. Both of
  those were true of the original run, so they are kept rather than corrected:
  changing them would make the new numbers incomparable with the old ones.
- **The units are dbar's.** Its fields carry them, so its formatter writes `%`
  and `°C` without being asked; Waybar's formats are told to write the same
  characters, so both bars shape the same strings.

`waybar/weather-shim.sh` runs `examples/weather.sh` - the same script, with the
same arguments - and reshapes its `key=value` output into the line Waybar wants,
including the script's own weather glyph. The fetch is identical, and it happens
once at startup in both, so no measurement window is charged for somebody else's
web service. The OpenWeatherMap key is the one dbar's own module reads,
`~/.config/dbar/owm.key`, so neither bar is holding a key the other is not; with
no key there, all three bars draw "unavailable" and the comparison is
unaffected.

## Running it

```sh
make prod                 # release only - a debug build invents regressions
./bench/run.sh up         # stops your bar, starts all three
./bench/measure.sh idle   # three two-minute windows, hands off
./bench/measure.sh use    # one window, while you use the bars
./bench/measure.sh static # binary size and shared library count
./bench/run.sh down       # restores the sway config
```

`run.sh up` appends a `bar {}` block to `~/.config/sway/config` and reloads,
because swaybar cannot be started on its own - sway spawns it, and only for a bar
the session's config declares. `down` takes that block back out again by its own
marker rather than copying a whole file over your config, so anything you edited
while the run was up survives; the copy taken at `up` stays in
`bench/.sway-config.backup` until `down` succeeds, as something to fall back on
by hand. A sway reload does not re-run `exec` lines, so nothing else in the
session restarts. `down` does not restart the bar you normally run; start it
again the way your compositor config does.

Both halves find the sway socket themselves. A shell carried over from an earlier
session has a stale `SWAYSOCK`, and that used to fail quietly in the worst way:
`up` would edit the config and never reload, `down` would put it back and leave
the bench bar on screen.

**Stop whatever locks or blanks your screen before an idle run.** Three windows
is more than six minutes, and a typical `swayidle` locks at five and blanks at
ten. A lock screen over the bars, or an output that has been switched off,
changes what every bar is doing - `pkill -STOP swayidle` before and
`pkill -CONT swayidle` after is enough, and costs no pointer movement of its own.

Override the window length with `WINDOW=300 ./bench/measure.sh idle`.

## What is measured

CPU is `utime + stime` from `/proc/PID/stat`, differenced across the window and
divided by `CLK_TCK`. Resident memory is `VmRSS` from `/proc/PID/status`, which
counts the font files and shared libraries a bar has mapped; heap is `RssAnon`,
which is what it allocated for itself. A bar that runs a helper is counted whole,
so swaybar is measured together with the `i3status-rs` it starts.

The idle windows are only idle if nothing crosses a bar. A pointer over a module
is a redraw, and a media player running turns the media module in all three bars
into a source of updates - which is a fair comparison, but not the same
measurement as the idle one, so do not mix them.

## The last run

20 September 2026, on SwayFX 0.6 (Sway 1.12.0), a Ryzen 7 PRO 5850U, against
Waybar 0.15.0 and i3status-rs 0.36.1. Two outputs were connected and every bar
drew on both: DP-1 at 2560x1440 and eDP-1 at 1920x1200, each at scale 1. A player
was running throughout, so the media module was live in all three.

Three two-minute idle windows, the spread across them in brackets:

| idle | resident | heap | CPU | processes | threads |
| --- | --- | --- | --- | --- | --- |
| **dbar** | **15.4 MB** | **3.2 MB** | **0.16 %** (0.15-0.17) | **1** | 12 |
| swaybar + i3status-rs | 55.0 MB | 12.6 MB | 0.60 % (0.58-0.62) | 2 | 8 |
| Waybar | 75.2 MB | 20.4 MB | 1.40 % (1.38-1.42) | 1 | 39 |

One window with the pointer crossing all three bars, driven from a script over
one sway IPC connection rather than by hand, so that the same workload can be
repeated: 6,593 pointer moves in two minutes, about thirty-five a second, across
the top of both top bars and swaybar at the bottom. No buttons, so nothing was
clicked, folded or switched:

| pointer crossing them | resident | heap | CPU |
| --- | --- | --- | --- |
| **dbar** | **15.4 MB** | **3.2 MB** | **0.29 %** |
| swaybar + i3status-rs | 55.0 MB | 12.6 MB | 0.67 % |
| Waybar | 75.3 MB | 20.4 MB | 1.82 % |

At a tenth of that rate - two moves a second, which is closer to a hand reaching
for something - dbar's cost is not distinguishable from its idle one.

| | binary | shared libraries |
| --- | --- | --- |
| **dbar** | **7.1 MB** | **4** |
| swaybar + i3status-rs | 0.1 + 17.4 MB | 45 / 28 |
| Waybar | 2.1 MB | 115 |

Three things moved since the numbers in the top-level README, and only the first
of them is dbar getting cheaper:

- dbar's own footprint fell - fixed-size native icons, a layout pass that no
  longer allocates per frame, and bounded caches.
- Waybar's configuration here now draws what dbar draws: an icon per module,
  rounded islands in two alternating shades, the same font at the same size, the
  same fixed widths, the tray at the same icon size. It was a plainer bar before,
  and a plainer bar is cheaper.
- Both bars drew on two outputs rather than one, which is roughly twice the
  drawing for everything on screen.

The first is dbar's to claim. The second and third are conditions of the run, and
they apply to every bar in it.
