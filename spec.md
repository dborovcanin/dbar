# Lightweight Wayland Status Bar Specification

## 1. Goal

Build a lightweight, native Wayland status bar for Sway/SwayFX with richer rendering than `swaybar`, while avoiding the dependency and styling complexity of Waybar.

The bar should:

- be written in Rust;
- use native Wayland/layer-shell integration;
- use `i3status-rust` as the primary status-data provider;
- support attractive vector-rendered UI;
- support Powerline-style layouts without relying on Powerline/Nerd Font separator glyphs;
- support transparency and compositor-provided blur;
- remain event-driven and lightweight;
- expose a deliberately small declarative configuration format;
- avoid becoming a general-purpose GUI toolkit or CSS engine.

---

## 2. Design Principles

### 2.1 Small renderer, rich output

The renderer should provide only the primitives needed by a status bar:

- text;
- vector icons;
- rectangles;
- rounded rectangles;
- borders;
- alpha/transparency;
- gradients where useful;
- separator/transition shapes;
- clipping;
- hover/pressed states.

It should not expose arbitrary widget trees, CSS, JavaScript, Lua, or a browser-style layout engine.

### 2.2 Event-driven

The bar should not run a permanent animation/render loop.

Redraw only when:

- `i3status-rust` emits an update;
- Sway emits an IPC event;
- pointer input changes state;
- output geometry changes;
- a short animation is active.

Idle CPU usage should be effectively zero.

### 2.3 Geometry, not font hacks

Decorative elements should be rendered directly.

Do **not** use font glyphs for:

- Powerline separators;
- chevrons;
- slanted edges;
- rounded transitions;
- borders;
- structural UI shapes.

This avoids font alignment, fallback, baseline, and HiDPI problems.

---

## 3. High-Level Architecture

```text
                 ┌──────────────────────┐
                 │    i3status-rust     │
                 │                      │
                 │ CPU / RAM / NET /    │
                 │ BAT / CLOCK / etc.   │
                 └──────────┬───────────┘
                            │
                     i3bar JSON / API
                            │
                            ▼
┌────────────────────────────────────────────────────┐
│                    custom-bar                      │
│                                                    │
│  status provider                                   │
│       │                                            │
│       ▼                                            │
│  normalized block state                            │
│       │                                            │
│       ├───────────────┐                            │
│       ▼               ▼                            │
│  layout engine    interaction engine               │
│       │               │                            │
│       └───────┬───────┘                            │
│               ▼                                    │
│        vector renderer                             │
│               │                                    │
│               ▼                                    │
│       Wayland layer-shell                          │
└────────────────────────────────────────────────────┘
```

Optional data sources may later include:

```text
StatusProvider
├── I3BarProvider
│   └── external i3status-rs
├── SwayProvider
│   └── workspaces / focused window
└── NativeI3StatusProvider
    └── optional direct i3status-rust integration
```

The first implementation should prefer the standard i3bar process boundary because it:

- keeps the bar decoupled from i3status-rust internals;
- allows alternative i3bar-compatible providers;
- avoids GPL linkage concerns if the bar uses a more permissive license;
- keeps the renderer independently testable.

---

## 4. Suggested Rust Stack

Preferred direction:

```text
Rust
├── smithay-client-toolkit
├── wayland-client
├── wayland-protocols-wlr
├── calloop
├── serde
├── toml
├── tiny-skia or equivalent 2D renderer
└── text shaping/rasterization library
```

Avoid GTK, Qt, Electron, and general-purpose GUI frameworks unless a future requirement clearly justifies them.

---

## 5. Rendering

### 5.1 Surface

The bar should use a Wayland layer-shell surface.

Requirements:

- top or bottom placement;
- configurable height;
- configurable margins;
- support transparent pixels;
- proper multi-monitor output handling;
- logical-coordinate rendering for HiDPI;
- compositor blur left to SwayFX rather than implemented in the bar.

Example:

```text
wallpaper
    ↓
SwayFX blur
    ↓
translucent custom-bar surface
```

### 5.2 Transparency

Colors should support alpha directly.

Example:

```toml
[bar.background]
color = "#1e1e2ecc"
radius = 10
```

Where `cc` is the alpha channel.

### 5.3 Icons

Preferred order:

1. built-in vector icon set;
2. SVG icons;
3. font icons as an optional fallback.

Structural UI must never require Nerd Font or Powerline glyphs.

---

## 6. Layout Model

The bar should expose three logical positions:

```text
left
center
right
```

Each position contains one or more groups.

Example:

```toml
[left]
groups = ["desktop"]

[center]
groups = ["center"]

[right]
groups = ["system"]
```

Groups own:

- module ordering;
- shared background;
- radius;
- padding;
- separator style;
- direction;
- spacing.

This enables floating-island layouts without absolute positioning.

Example:

```text
╭──────────────╲───────────────╮
│ 1  2  [3]     ╲ Terminal      │
╰──────────────╲───────────────╯

                     ╭────────╲────────╲──────────╲───────────╮
                     │ CPU 12% ╲ RAM 31% ╲ WiFi    ╲ BAT 84%   │
                     ╰────────╲────────╲──────────╲───────────╯
```

---

## 7. Configuration Format

Use TOML.

Configuration should remain intentionally constrained.

### 7.1 Bar

```toml
[bar]
height = 34
position = "top"
margin = 6
gap = 6
font = "Inter 10"

[bar.background]
color = "#1e1e2ecc"
radius = 10
```

### 7.2 Colors

```toml
[colors]
background = "#1e1e2e"
surface = "#313244"
text = "#cdd6f4"
accent = "#89b4fa"
warning = "#f9e2af"
critical = "#f38ba8"
```

Named colors may be referenced using `$name`.

Example:

```toml
background = "$surface"
foreground = "$text"
```

### 7.3 Reusable styles

```toml
[style.default]
background = "$surface"
foreground = "$text"
radius = 8
padding = 8

[style.accent]
background = "$accent"
foreground = "$background"

[style.warning]
background = "$warning"
foreground = "$background"

[style.critical]
background = "$critical"
foreground = "$background"
```

### 7.4 Groups

```toml
[group.desktop]
modules = ["workspaces", "window"]
background = "$surface"
radius = 10
padding = 4

[group.system]
modules = ["cpu", "memory", "network", "audio", "battery", "clock"]
background = "$surface"
radius = 10
padding = 4
```

### 7.5 Modules

```toml
[module.cpu]
style = "default"
icon = "cpu"

[module.memory]
style = "default"
icon = "memory"

[module.network]
style = "default"
icon = "wifi"

[module.battery]
style = "default"
```

### 7.6 State overrides

```toml
[module.battery.states.warning]
style = "warning"

[module.battery.states.critical]
style = "critical"
```

Configuration precedence:

```text
built-in defaults
        ↓
named style
        ↓
group style
        ↓
module style
        ↓
module state override
```

---

## 8. Supported Style Properties

Keep the initial property set small.

Recommended properties:

```text
background
foreground
opacity
padding
margin
radius
border-width
border-color
separator
separator-width
font
font-size
icon-size
min-width
max-width
```

A useful rule:

> If a configuration property cannot map almost directly to a layout or rendering field, do not add it.

---

## 9. Separators

Separators are a first-class rendering primitive.

They must be drawn as vector geometry, not rendered as font characters.

### 9.1 Group-level configuration

```toml
[group.system.separator]
shape = "chevron"
width = 10
direction = "right"
color = "previous"
overlap = 1
```

### 9.2 Initial separator shapes

Support a deliberately small set:

```text
none
line
slant
chevron
notch
round
curve
```

Conceptual examples:

```text
slant

CPU 12% ╲ RAM 31% ╲ WIFI


chevron

CPU 12% ▶ RAM 31% ▶ WIFI


round

CPU 12% ) RAM 31% ) WIFI
```

These examples are textual approximations only. The real implementation draws paths/polygons.

### 9.3 Separator as transition

Internally, a separator should be modeled as a transition between neighboring modules:

```text
Module A
    ↓
Transition(A.style, B.style)
    ↓
Module B
```

This is preferable to treating the separator as an independent text-like object.

Possible representation:

```rust
struct Separator {
    shape: SeparatorShape,
    width: f32,
    direction: Direction,
    mode: SeparatorMode,
    overlap: f32,
}
```

### 9.4 Color modes

Support:

```text
previous
next
foreground
background
fixed
```

Example:

```toml
[group.system.separator]
shape = "chevron"
color = "previous"
```

Classic Powerline rendering uses the previous module's background for the wedge while the next module's background appears behind it.

### 9.5 Direction

```toml
direction = "right"
```

or:

```toml
direction = "left"
```

This allows layouts such as:

```text
Workspaces ▶ Window

                         Clock ◀ Battery ◀ WiFi
```

### 9.6 Overlap

Support a small overlap to prevent antialiased seams:

```toml
overlap = 1
```

Conceptually:

```text
module A ends at x = 100
separator starts at x = 99
```

This avoids one-pixel wallpaper gaps between independently antialiased edges.

### 9.7 Outer edges

Internal separators and group edges should be separate concepts.

Example:

```toml
[group.system.edges]
left = "round"
right = "round"
radius = 10
```

Result:

```text
╭────────▶────────▶────────╮
│ CPU 12%   RAM 31%   WIFI │
╰────────▶────────▶────────╯
```

---

## 10. Interaction

Support:

- hover;
- left click;
- middle click;
- right click;
- scroll up;
- scroll down.

Click events should be forwarded to `i3status-rust` where appropriate.

Optional custom actions:

```toml
[module.audio.actions]
left = "pavucontrol"
middle = "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"
scroll-up = "wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+"
scroll-down = "wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-"
```

Pointer handling requires mapping coordinates to rendered module rectangles.

---

## 11. Custom Modules

Support simple command-driven modules.

Example:

```toml
[module.kernel]
type = "command"
command = "uname -r"
interval = "60s"
icon = "linux"
format = "{output}"
```

Keep formatting deliberately limited.

Possible placeholders:

```text
{value}
{icon}
{name}
{output}
```

Do not add:

- loops;
- arbitrary expressions;
- embedded scripting;
- JavaScript;
- Lua.

If complex behavior is needed, users can supply an external script.

---

## 12. Animation

Animation should be optional and short-lived.

Good use cases:

- hover fade;
- workspace indicator movement;
- subtle state transition;
- opacity interpolation.

Example:

```text
alpha: 0.70 → 0.90
```

The renderer should wake only while an animation is active.

Do not run a permanent 60 FPS loop.

---

## 13. Non-Goals

The project should intentionally avoid becoming:

- a full desktop shell;
- a replacement for Quickshell;
- a general GUI framework;
- a CSS engine;
- an HTML-like layout system;
- a JavaScript runtime;
- a plugin host;
- a notification daemon;
- a launcher;
- a control center.

Tray and popups may be added later, but they should not shape the initial architecture.

---

## 14. Initial Feature Scope

### V0

- Wayland layer-shell surface;
- top/bottom placement;
- transparency;
- basic text rendering;
- i3status-rust/i3bar input;
- left/center/right layout;
- basic groups;
- rounded backgrounds;
- click forwarding.

### V1

- reusable styles;
- named colors;
- vector icons;
- separator primitives;
- Powerline-style transitions;
- hover states;
- module state styling;
- multi-monitor support;
- HiDPI support.

### V1.x

- short animations;
- command modules;
- SVG icon loading;
- richer outer-edge shapes;
- optional Sway-specific workspace/window integration.

### Later

- tray;
- popups;
- direct i3status-rust library integration if worthwhile;
- additional status-provider backends.

---

## 15. Example Complete Configuration

```toml
[bar]
height = 34
position = "top"
margin = 6
gap = 6
font = "Inter 10"

[colors]
background = "#1e1e2e"
surface = "#313244cc"
text = "#cdd6f4"
accent = "#89b4fa"
warning = "#f9e2af"
critical = "#f38ba8"

[left]
groups = ["desktop"]

[right]
groups = ["system"]

[style.default]
foreground = "$text"
padding = 8

[style.focused]
background = "$accent"
foreground = "$background"

[style.warning]
background = "$warning"
foreground = "$background"

[style.critical]
background = "$critical"
foreground = "$background"

[group.desktop]
modules = ["workspaces", "window"]
background = "$surface"
radius = 10
padding = 4

[group.desktop.separator]
shape = "slant"
width = 10
direction = "right"
color = "previous"
overlap = 1

[group.system]
modules = ["cpu", "memory", "network", "audio", "battery", "clock"]
background = "$surface"
radius = 10
padding = 4

[group.system.separator]
shape = "chevron"
width = 10
direction = "left"
color = "previous"
overlap = 1

[group.system.edges]
left = "round"
right = "round"
radius = 10

[module.workspaces]
style = "default"

[module.window]
style = "default"

[module.cpu]
style = "default"
icon = "cpu"

[module.memory]
style = "default"
icon = "memory"

[module.network]
style = "default"
icon = "wifi"

[module.audio]
style = "default"
icon = "volume"

[module.battery]
style = "default"
icon = "battery"

[module.battery.states.warning]
style = "warning"

[module.battery.states.critical]
style = "critical"

[module.clock]
style = "default"
```

Expected visual direction:

```text
╭──────────────╲───────────────╮
│ 1  2  [3]     ╲ Terminal      │
╰──────────────╲───────────────╯

                     ╭────────╲────────╲──────────╲───────────╮
                     │ CPU 12% ╲ RAM 31% ╲ WiFi    ╲ BAT 84%   │
                     ╰────────╲────────╲──────────╲───────────╯
```

Again, all separators and structural edges are vector-rendered geometry, not font glyphs.

---

## 16. Core Product Definition

The project can be summarized as:

> A tiny, event-driven, native Wayland status-bar renderer with i3bar-compatible status input and a constrained declarative styling model.

The defining visual feature is:

> Powerline-style composition implemented as real vector geometry rather than font glyphs.

The defining architectural constraint is:

> Rich enough to look polished, but deliberately too small to become CSS, GTK, or a desktop shell.

---

## 17. Implementation Status

### 17.1 Done

**V0** — wlr-layer-shell surface with top/bottom placement, margins and an
exclusive zone; per-pixel transparency; text shaping and font fallback;
i3bar-protocol input; left/center/right positions holding groups; rounded
backgrounds; click and scroll forwarding; integer HiDPI buffer scaling.

**V1, in part** — separators as real vector geometry (`none`, `line`, `slant`,
`chevron`, `notch`, `round`, `curve`), each mirrorable, with the Powerline
colour modes and an overlap that hides antialiasing seams; per-side group edges
with contents clipped to the group outline; reusable styles and named colours;
a built-in vector icon set, including icons graded over five steps by a
percentage read from the module's own text; module state styling keyed on a
value or on the provider's urgent flag.

Two things were added that the specification did not anticipate:

- **`[status] blocks`** names the provider's blocks positionally. The i3bar
  protocol gives a provider no way to name its blocks usefully — i3status-rs
  numbers them and rejects a `name` key — so without this a group selects on
  `"0"` and `"1"` and silently follows a different block whenever the provider
  is reordered.
- **Failure diagnostics.** A group list matching nothing logs a warning naming
  the blocks that are actually arriving, and a provider failure bypasses the
  group configuration entirely so it cannot be hidden by a module list.

### 17.2 What's left

Roughly in the order the work depends on itself.

**Multi-monitor.** One surface, bound to no particular output. Wants a bar per
output, plus hotplug handling. The renderer already takes a size and a scale,
so this is a matter of holding several of them.

**Hover states.** Pointer enter, motion and leave already arrive; nothing keeps
per-module pointer state or redraws on it. Fits the module state mechanism as a
condition alongside `below`, `above` and `urgent`.

**Expand on click.** A module showing a longer form while active. Some
providers cover this themselves — i3status-rs `format_alt` toggles on click for
several blocks, and dbar already forwards the click — but a general version
needs per-module toggle state and somewhere to put the second form.

**Configuration unification.** Generating the provider's configuration from
dbar's, so blocks are declared once. Passing unknown keys through opaquely
avoids mirroring the provider's schema. This does not fix positional drift:
when a block fails to emit, every block after it shifts, and no protocol field
identifies them.

**Colour work.** Gradients, which the specification asks for and the renderer
does not yet do; theme files; derived colours such as lighten and darken.

**On-click actions.** Running a command on click or scroll, per §10, instead of
or alongside forwarding to the provider.

**Workspaces and focused window.** A Sway IPC subscription, workspace buttons
that switch on click, and a window title module. Also the source of application
identity, and so a prerequisite for application icons.

**System tray.** StatusNotifierItem over D-Bus. Tray icons arrive as pixmaps or
as icon-theme names, so this needs the raster arm of the icon artwork and, for
menus, popup surfaces.

**Animations.** Short and opt-in, per §12: hover fades and workspace indicator
movement, driven by frame callbacks, with the bar still idle when nothing is
moving.

### 17.3 Smaller known gaps

- Fractional scaling. Only integer buffer scale is honoured today.
- SVG icon loading, and application icons behind it.
- Command modules, per §11.
- Positional block names are unstable while the provider is still starting up:
  until every block has emitted once, the aliases in `[status] blocks` can
  point one slot off. It settles on its own.
- Text is not clipped to the group outline. Backgrounds and separators are;
  text sits inside the padding, where it has not mattered.
