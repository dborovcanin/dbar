# dbar

A small, event-driven Wayland status bar for Sway/SwayFX. It reads what it shows from
`/proc` and `/sys`, renders with `tiny-skia` on a `wlr-layer-shell` surface, and can also
take data from any i3bar-protocol provider.

`dbar-native.md` is the standing plan: architecture, phases, and what is deliberately out
of scope. Read it before starting anything structural.

## What this project is trying to be

A bar that is cheap enough to forget about and configurable enough to be worth keeping.
Every decision below follows from those two, in that order.

## 1. Performance is a feature, not a tuning pass

Memory and CPU footprint are the reason this exists rather than Waybar. Treat a regression
in either as a bug.

- Idle should cost nothing. The bar is event-driven: it redraws when something changed, and
  sleeps otherwise. Never add a permanent loop, an animation tick, or a poll that runs when
  nothing is on screen to change.
- Prefer being told over asking. Where the kernel or a service can notify - inotify, udev,
  `poll()` on a sysfs file, a D-Bus signal - use that instead of an interval. Polling is for
  things that are genuinely sampled, like cpu utilisation and throughput.
- Collectors share one timer. It wakes at the earliest deadline, reads everything due, and
  redraws once. Adding a source must not add a wake-up.
- Watch allocation on the per-frame path. Layout and rendering run on every redraw; anything
  that allocates there is paid for repeatedly, forever.
- Measure before and after anything that could matter. `/proc/PID/status` for `RssAnon`,
  `/proc/PID/stat` fields 14 and 15 for CPU. Compare release builds - the debug binary is
  ~19 MB RSS against ~9.5 MB for release, so mixing the two invents regressions that are not
  there.
- Dependencies are weight. Each one needs a reason that could not be met in a hundred lines.
  Current deps and why: `tiny-skia` (rasteriser), `cosmic-text` (shaping and fallback),
  `smithay-client-toolkit`/`wayland-client`/`calloop` (Wayland and the event loop), `serde`
  and `toml` (config), `serde_json` (the i3bar protocol), `jiff` (local time, which needs a
  tz database), `signal-hook` and `libc` (realtime signals), `anyhow`, `log`, `env_logger`.

Known and accepted: a redraw repaints the whole surface and re-shapes all text, so an update
costs the same whichever module changed. Damage tracking and a measurement cache are the fix
when it becomes worth it.

## 2. The architecture is layered, and the layers do not leak

```text
config -> sources -> typed fields -> formatter -> StatusItem -> layout -> Frame -> backend
```

Two rules hold the whole design together:

- **Nothing below `StatusItem` knows where data came from.** The i3bar protocol, sway IPC and
  the native collectors all converge on it. `I3BarBlock` lives in `status/i3bar.rs` and gets
  no further.
- **Nothing below `Frame` knows about config, formats or protocols.** `Frame` is positioned
  geometry and colour, so the renderer can be replaced without touching anything above it.

Values stay values. A source publishes typed fields with units; formatting reads them without
consuming them. Never parse a number back out of text that was written to be looked at - that
was the original sin this design removed, and it is the thing most likely to creep back in.
Text matching (`contains`, `strip`) is allowed only for i3bar modules, where rendered text
genuinely is all the protocol carries.

Keep the renderer swappable. A GPU backend must be possible without rewriting layout, which
means `Frame`, icons and separator geometry stay free of `tiny-skia` types.

No async runtime. The event loop is `calloop`; cheap reads happen on the main thread, and
anything that genuinely blocks gets a worker thread feeding a `calloop` channel, the way
`sway.rs`, `status/i3bar.rs` and `signal.rs` already do.

## 3. Both ways of getting data are first-class

Native collectors are the default and the point. The i3bar backend is permanent, not a
transitional thing: it is how dbar shows what it cannot yet read itself, and how someone with
a working i3status-rs or py3status setup moves over gradually.

Neither may compromise the other. A native module must not be shaped by protocol limitations,
and the i3bar path must keep working as collectors are added.

## 4. Not Waybar

Deliberately absent, and staying absent:

- CSS, or any styling language. Styles are a small cascade of named tables.
- An embedded interpreter - no Lua, no JavaScript, no expression language in the config.
- A widget tree, or arbitrary layout.
- Every module Waybar has. dbar covers what a bar is actually for.

Scripting does belong here, but as a source rather than a language: a module can run a
command and take what it prints, which is how anything dbar has no collector for gets onto
the bar. The command is the user's own program, and dbar's side of it stays a source like
any other - it publishes fields, a format decides the wording, and the same state rules
apply. That is the whole extension mechanism, and it is enough precisely because it does not
need dbar to grow a language.

The renderer's vocabulary is text, vector icons, rectangles, rounded rectangles, separators,
clipping and alpha. Adding a primitive needs a real case, not a hypothetical one.

Compatibility with i3status-rust's *configuration file* is not a goal either. Its block model
was good inspiration; its file is not something dbar parses. Never accept a worse design for
the sake of matching it.

## 5. The config should read like prose

TOML, small, and legible without the documentation open.

- Names say what they mean. `interval = "2s"` carries a unit, because `interval = 2` is two
  of something and which something is exactly the point.
- Sensible defaults everywhere. A module that wants the ordinary thing should say only where
  its content comes from.
- Reject mistakes when the config is read, not at three in the morning. An unknown field name
  in a format, a threshold on a field the source does not publish, a signal this system does
  not have - all of these are startup errors that name the module and say what was expected.
  `deny_unknown_fields` is on for a reason.
- Expressive beats terse. Rich, orthogonal keys are worth more than a short file, but every
  key has to earn its place.

`examples/config.toml` is the compiled-in default and must stay runnable with nothing else
installed. `examples/showcase.toml` documents the full surface. Both are parsed by the test
suite, so a config change that breaks them fails the build.

## Working on this

- `make` (debug), `make prod` (release), `make test`, `make clippy`, `make fmt`.
- Tests must not depend on the machine running them. Collectors parse fixtures; layout uses a
  stub text measurer that makes every character one unit wide.
- Clippy is clean and stays clean. So is `cargo fmt --check`.
- Comments explain why, not what. If a line needs a comment to say what it does, rewrite the
  line.
