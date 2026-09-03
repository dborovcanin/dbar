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
- separators as real vector geometry: `line`, `slant`, `chevron`, `notch`,
  `round` and `curve`, each mirrorable, with Powerline colour modes and an
  overlap that hides antialiasing seams
- per-side group edges, with group contents clipped to the group outline

Not yet implemented: vector icons, hover states, module state styling,
animations, command modules, multi-monitor. Those are V1 and V1.x in the spec.

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

[examples/config.toml](examples/config.toml) is the annotated default, and is
also what dbar compiles in and uses when no config file exists.
[examples/showcase.toml](examples/showcase.toml) exercises every key and every
separator shape across all three positions:

```sh
dbar -c examples/showcase.toml
```

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

### Icons

dbar draws its own icons as vector geometry, so they scale with `icon_size`
rather than riding on a font:

```toml
[module.battery]
icon = "battery"
icon_size = 16          # optional; defaults to 1.4x the bar font size
```

Left unset, `icon_size` follows the font, so changing `[bar] font` scales icons
along with the text. Setting it pins the icon independently.

Fixed: `cpu`, `memory`, `disk`, `clock`, `ethernet`, `headphones`, `wifi-off`,
`volume-muted`.

Graded: `battery`, `battery-charging`, `wifi`, `volume`, `brightness`. These have five steps and
pick one by reading a percentage out of the text beside them - a battery at
`58%` draws a little over half full. Only an `NN%` pattern counts, so text such
as `92GB`, `23:59` or `3h 5m` leaves a graded icon at its lowest step rather
than grading on a number that means something else.

`battery-charging` grades like `battery` and cuts its bolt out of the charge
bar, so it reads at any level. Nothing selects the off states automatically
yet; `wifi-off`, `volume-muted` and `headphones` are named outright. Value-driven selection belongs with module state styling.

#### Provider glyph icons

Alternatively the status provider can emit icons as text. i3status-rs ships
several icon sets, and every block in
[examples/i3status-rs.toml](examples/i3status-rs.toml) formats as
`$icon $value`, so one line decides what you get:

```toml
[icons]
icons = "none"          # the default: not "no icon", but short text labels
# icons = "material-nf" # glyph icons - needs a font that carries them
```

Glyph sets live in the Unicode private use area, so the font has to carry them.
Pair one with a proportional Nerd Font such as `DejaVuSansM Nerd Font Propo`, so
body text still reads like a UI font while the icons resolve from the same face:

```toml
[bar]
font = "DejaVuSansM Nerd Font Propo 10"
```

A font without those glyphs still works - dbar falls back per glyph - but on a
system with many Nerd Fonts installed the fallback can source each icon from a
different face, leaving them mismatched in weight and size.

This route leaves the provider choosing which glyph each block gets, and ties
icon size to the font size. dbar's own icons are independent of both.

### Separators

A separator is a transition between two neighbouring modules, drawn as vector
geometry rather than as a font glyph. It is configured per group:

```toml
[group.system.separator]
shape = "chevron"     # none | line | slant | chevron | notch | round | curve
width = 12            # horizontal space the transition occupies
direction = "left"    # "right" | "left"; mirrors the shape
color = "previous"    # previous | next | foreground | background, or a color
overlap = 1           # bleed past each side, hiding antialiasing seams
```

`color = "previous"` takes the preceding module's background for the leading
region, which is what gives the classic Powerline wedge. A group without a
separator falls back to its `spacing` for the gap between modules.

Outer corners are a separate concept:

```toml
[group.system.edges]
left = "round"        # "round" | "none"
right = "round"
radius = 12           # defaults to the group's own radius
```

Group contents are clipped to the group outline, so square module corners and
separator overlap never spill past a rounded edge.

Instead of `"*"`, a group may list block names to select and order them
explicitly:

```toml
[group.system]
modules = ["cpu", "memory", "time"]

[module.cpu]
style = "default"
```

### Naming blocks

The i3bar protocol gives a provider no way to name its blocks usefully -
`i3status-rs` numbers them `"0"`, `"1"`, ... and rejects a `name` key on a
block - so a group selecting on those numbers silently follows a different
block whenever the provider's config is reordered.

Name them once, in the order the provider emits them:

```toml
[status]
command = "i3status-rs"
blocks = ["cpu", "memory", "disk", "load", "net", "volume", "uptime", "clock"]
```

Groups and modules then select on those names:

```toml
[group.desktop]
modules = ["cpu", "memory"]

[module.cpu]
style = "accent"
```

The names are dbar's own. Click events still carry the name the provider gave
the block, so it can route them back. Without `blocks`, groups select on
whatever the provider sends, which for `i3status-rs` means `"0"`, `"1"`, ...

If a group's module list matches nothing, dbar logs a warning naming the block
names the provider is actually sending, rather than leaving a blank bar with no
explanation. Failures of the provider itself bypass the group configuration and
are always drawn, so they cannot be hidden by a module list that filters them
out.
