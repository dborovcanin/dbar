# dbar

A small, event-driven Wayland status bar for Sway/SwayFX. It renders with
`tiny-skia` on a `wlr-layer-shell` surface and takes its data from
`i3status-rs` over the standard i3bar protocol.

This is the **V0** milestone from [spec.md](spec.md).

## What works

- `wlr-layer-shell` surface, top or bottom placement, configurable margin and
  exclusive zone
- per-pixel transparency, so SwayFX blur shows through
- text rendering with shaping and font fallback (`cosmic-text`)
- i3bar input: any i3bar-compatible provider, `i3status-rs` by default
- `left` / `center` / `right` positions holding groups of modules
- rounded group and module backgrounds
- click forwarding back to the provider (left, middle, right, scroll)
- integer HiDPI buffer scaling

Not yet implemented: separators and Powerline transitions, vector icons, hover
states, module state styling, animations, command modules, multi-monitor.
Those are V1 and V1.x in the spec.

## Build

```sh
make          # debug build, fast
make prod     # optimized build
make install  # installs to ~/.local/bin (override with PREFIX=)
```

## Run

```sh
dbar                    # uses ~/.config/dbar/config.toml, or built-in defaults
dbar -c path/to.toml    # explicit config
dbar --print-config     # writes the built-in default config to stdout
```

`dbar` needs a status provider. To get started:

```sh
mkdir -p ~/.config/i3status-rust ~/.config/dbar
cp examples/i3status-rs.toml ~/.config/i3status-rust/config.toml
dbar --print-config > ~/.config/dbar/config.toml
```

Then in your Sway config, replace the `bar { ... }` block with:

```
exec_always pkill -x dbar; dbar
```

## Configuration

See [examples/config.toml](examples/config.toml) for the annotated default.

```toml
[bar]
height = 34
position = "top"      # or "bottom"
margin = 6            # floats the bar off the screen edge
gap = 6               # space between groups
font = "Inter 10"

[bar.background]
color = "#00000000"   # last two hex digits are alpha
radius = 0

[status]
command = "i3status-rs"
args = []

[colors]
surface = "#313244cc"
text = "#cdd6f4"

[right]
groups = ["system"]

[style.default]
foreground = "$text"  # "$name" refers to a [colors] entry
padding = 8

[group.system]
modules = ["*"]       # "*" takes every block the provider emits
background = "$surface"
radius = 10
padding = 4
spacing = 0
```

Style resolution runs built-in defaults, then the named `[style.*]` a module
picks, then that module's own keys.

Instead of `"*"`, a group may list block names to select and order them
explicitly:

```toml
[group.system]
modules = ["cpu", "memory", "time"]

[module.cpu]
style = "default"
```

Block names come from the provider. `i3status-rs` numbers its blocks by
default; set `name` on a block in its config to address it here.
