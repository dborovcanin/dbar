# dbar — Product and Architecture Specification

This is the standing specification for dbar. It describes the product we are
building, the architecture that exists now, and the constraints future work must
preserve. It is not a catalogue of every configuration key; the README and the
annotated examples serve that purpose.

Unless a section is explicitly marked as future work, it describes current
behaviour.

## 1. Product definition

dbar is a minimal, standalone Wayland status bar for Sway and SwayFX. It should
be cheap enough to forget about, configurable enough to keep, and understandable
enough that a user can own the whole setup in one TOML file.

Standalone means that dbar can collect and render an ordinary desktop status by
itself. A native-only configuration starts no status daemon and no helper
process. It is not intended to mean a static binary or a bar that reimplements
the operating system services it talks to.

The core desktop surface is built in:

- processor use;
- memory and swap use;
- temperature;
- sound volume, mute state, device and port;
- keyboard layout;
- battery state and remaining time;
- workspaces, focused window and binding mode;
- local or zoned time; and
- user commands, both streaming and scheduled.

dbar also has built-in support for backlight, load, disks, network state and
throughput, media players, and StatusNotifierItem trays.

An i3bar-protocol provider remains supported for migration and for data dbar
does not read itself. Native sources are preferred: they avoid a second daemon,
preserve typed values, support direct controls, and let dbar coordinate polling
and redraws. Provider compatibility is a permanent escape hatch, not the centre
of the design.

## 2. Priorities

When goals conflict, use this order:

1. Correctness and predictable behaviour.
2. Low idle CPU, memory and process count.
3. A clear standalone configuration.
4. Native integration for common status and controls.
5. Scriptability and provider interoperability.
6. Visual flexibility, including light animation.

The order matters. A visual option must not introduce a permanent animation
tick. A new native collector must not add a process merely to wrap a command. A
compatibility feature must not force native data through the limitations of the
i3bar protocol.

Two short rules summarize the product:

- **Native by default, open at the edges.** Common desktop status belongs in
  dbar; personal and uncommon status belongs in a command; existing provider
  ecosystems remain usable through i3bar.
- **Work only when there is work.** Prefer events to polling, share unavoidable
  timers, coalesce updates, suppress unchanged frames, and animate only while a
  bounded transition is active.

## 3. Supported environment

dbar targets Linux compositors that implement `wlr-layer-shell`, specifically
Sway and SwayFX today. The compositor owns output discovery, workspace and
window state, keyboard layout, positioning, and frame pacing.

The current presentation stack is:

- `smithay-client-toolkit` and `wayland-client` for Wayland and layer shell;
- `calloop` for the event loop;
- shared-memory buffers for presentation;
- `tiny-skia` for CPU rasterization; and
- `cosmic-text` for shaping and font fallback.

Integer output scaling is supported. The bar can appear on every output or a
configured subset, and follows outputs as they are added and removed.

## 4. Current product surface

### 4.1 Sources built into dbar

| Source | Main mechanism | Delivery model |
| --- | --- | --- |
| `cpu` | `/proc/stat` | sampled |
| `memory` | `/proc/meminfo` | sampled |
| `load` | `/proc/loadavg` | sampled |
| `temperature` | `/sys/class/hwmon` | sampled |
| `disk` | filesystem statistics | sampled |
| `network` | sysfs counters and nl80211 queries | sampled |
| `backlight` | `/sys/class/backlight` | sysfs notification with polling fallback |
| `battery` | `/sys/class/power_supply` | sampled plus kernel uevents |
| `audio` | PipeWire | event-driven |
| `media` | MPRIS on the session bus | event-driven |
| `time` | system clock and time-zone database | aligned timer |
| `command` | an argv supplied by the user | streaming, periodic, or once/on demand |

Sway integrations are built into the same process and read compositor IPC:

- `sway:workspaces`;
- `sway:window`;
- `sway:language`; and
- `sway:mode`.

The `tray` source implements StatusNotifierItem discovery, icons, activation,
menus, passive-item filtering, and configured ordering.

### 4.2 External providers

The optional `[i3bar]` backend starts one i3bar-compatible provider and consumes
its JSON stream. Modules may select named provider blocks, and a wildcard group
may retain provider order. Pointer actions are sent back over the provider's
click-event stream. A module with no explicit `source` is a provider module;
native and other built-in sources are always named deliberately.

The protocol exposes presentation-oriented blocks rather than dbar's typed
source fields. Provider-specific text matching and colours are therefore kept
on this path and do not shape the native source model.

### 4.3 Presentation and interaction

The current bar supports:

- top or bottom placement, layer selection, margins and exclusive zones;
- per-output bars and output-scoped compositor data;
- left, centre and right runs of named groups;
- named style inheritance plus per-module and state overrides;
- rounded bar, group and module backgrounds with transparency;
- vector line, slant, chevron, notch, round and curve separators;
- per-side group ends, joined groups, clipping and overlap control;
- built-in vector icons, written glyphs, graded icons and themed tray artwork;
- hover styles and state rules over typed fields;
- direct brightness, volume and media controls;
- user-supplied click commands;
- provider click forwarding;
- alternate wordings, module collapse and group collapse;
- optional bounded animations for group collapse, module collapse and wording
  changes; and
- command progress spinners that exist only while a slow command is running.

## 5. Standalone configuration

Configuration is TOML. With no command-line path, dbar reads
`$XDG_CONFIG_HOME/dbar/config.toml`, conventionally
`~/.config/dbar/config.toml`, and falls back to an annotated built-in default.

A complete standalone configuration can remain small because built-in sources
have default formats and sensible scheduling:

```toml
[bar]
height = 32
position = "top"

[left]
groups = ["desktop"]

[right]
groups = ["system", "clock"]

[group.desktop]
modules = ["workspaces", "language"]

[group.system]
modules = ["cpu", "memory", "temperature", "audio", "battery", "custom"]
spacing = 6

[group.clock]
modules = ["time"]

[module.workspaces]
source = "sway:workspaces"

[module.language]
source = "sway:language"

[module.cpu]
source = "cpu"

[module.memory]
source = "memory"

[module.temperature]
source = "temperature"

[module.audio]
source = "audio"
scroll = "5%"

[module.battery]
source = "battery"

[module.custom]
source = "command"
command = ["my-status-script"]

[module.time]
source = "time"
```

The hierarchy is intentionally fixed and shallow:

```text
bar
├── left
│   └── groups
├── center
│   └── groups
└── right
    └── groups
        └── modules
```

There is no widget tree, selector engine, embedded interpreter, or arbitrary
layout language. The fixed model keeps both configuration and layout costs
predictable.

The main configuration tables are:

- `[bar]` for the surface, outputs, font and global geometry;
- `[menu]` for tray menu presentation;
- `[colors]` for reusable colour names;
- `[style.*]` for reusable visual styles;
- `[left]`, `[center]`, and `[right]` for group order;
- `[group.*]` for modules, spacing, separators, ends and collapse;
- `[module.*]` for a source, format, state rules and interaction; and
- `[i3bar]` only when an external provider is used.

An icon is written the same way in every slot that takes one, a workspace's and
a fold's included: `$name` is one of the built-in vector icons, sized by
`icon_size`; `none` removes the icon; and any other string is text shaped with
the font. A misspelled `$name` is a configuration error; a glyph makes no claim
that can be checked.

Unknown keys, invalid field references, incompatible options, conflicting
button assignments, and impossible geometry are configuration errors. A key
that cannot do anything should be rejected rather than accepted and ignored.

## 6. Source strategy

### 6.1 Preferred order

For a new status need, prefer:

1. an existing built-in source;
2. a user command for personal or uncommon data;
3. an i3bar provider for an existing provider-based setup; and
4. a new built-in source when the use case is common enough to justify it.

A feature is a good native-source candidate when at least one of these is true:

- it is expected on an ordinary desktop bar;
- an event API can make it substantially cheaper or more responsive than a
  command;
- dbar can control it directly as well as display it;
- typed fields materially improve formatting and state rules; or
- the provider/helper alternative carries disproportionate process or
  dependency cost.

Native does not mean implementing every protocol from scratch. Stable system
libraries are appropriate for difficult subsystems such as PipeWire, and D-Bus
is appropriate for services whose public interface lives there. Each dependency
still has to justify its footprint and maintenance cost.

### 6.2 Commands are the extension mechanism

A command module executes an argv directly; dbar does not insert a shell. It can
operate in three modes:

| Configuration | Behaviour |
| --- | --- |
| no `interval` | start once and consume a stream of lines |
| duration such as `"30s"` | run to completion on the shared schedule |
| `"once"` | run at startup and thereafter only when refreshed |

Streaming is preferred when the underlying thing can announce changes. Silence
then costs no wake-up or process restart. Periodic commands remain supported
because they are the form most existing scripts already have.

Plain commands publish `$text`. A module can declare additional typed fields,
and a command can publish values plus a source state. The ordinary format,
state-rule and icon machinery then applies without a command-specific rendering
path.

Commands may expose multiple output lines as pages, take configured parameters,
be refreshed by a pointer button or realtime signal, and show a delayed spinner
while a run is outstanding. Blocking command work is isolated from the event
loop.

### 6.3 The i3bar boundary

i3bar support is permanent but opt-in. A native-only configuration does not
start a provider. When enabled, there is one provider process and one reader,
not one provider per module.

The provider boundary must preserve:

- i3bar block order and identity as far as the protocol permits;
- provider foreground, background and urgency;
- Pango-markup stripping for text dbar draws itself;
- click names, instances, coordinates and button numbers; and
- coexistence with native, Sway, command and tray modules in the same groups.

Provider compatibility does not include parsing an i3status-rust configuration
file or generating one on the user's behalf.

## 7. Architecture

```text
                           TOML configuration
                                   │
                                   ▼
                         parsed and resolved config
                                   │
          ┌────────────────────────┼────────────────────────┐
          ▼                        ▼                        ▼
  native collectors          pushed services          external inputs
 /proc /sys / netlink     PipeWire / MPRIS / Sway    command / i3bar / tray
          │                        │                        │
          └────────────────────────┴────────────────────────┘
                                   │
                                   ▼
                       current typed source state
                                   │
                        ┌──────────┴──────────┐
                        ▼                     ▼
                 layout per output      interaction routing
                        │                     │
                        ▼                     │
           Frame: positioned geometry ◄──────┘
                        │
                        ▼
             tiny-skia + shaped text
                        │
                        ▼
               Wayland shm surface
```

The implementation remains one Rust crate. Splitting small, tightly coupled
layers into services or workspace crates would add build and interface overhead
without reducing runtime work.

### 7.1 Runtime ownership

`App` owns session-wide state: configuration, source readings, provider state,
Sway state, tray state, interaction state and active transitions. Collection
happens once for the session.

Each `Bar` owns output-specific state: its Wayland surface, dimensions, scale,
shared-memory pool, clipping storage, pointer position and last presented
`Frame`. Layout and presentation happen per output because width, scale,
workspace scope and pointer state can differ.

A second output adds a surface, buffer and layout pass. It does not add another
collector schedule or provider process.

### 7.2 Event loop and workers

There is no async runtime. `calloop` multiplexes Wayland, timers and channels.

- Cheap sampled reads happen on the main thread when the shared collector
  scheduler says they are due.
- Blocking or connection-owning integrations run on focused worker threads and
  feed bounded state changes into the loop.
- Wayland frame callbacks prevent presentation from outrunning the compositor.
- Source updates are collected and invalidated together before drawing.

A thread is acceptable when it isolates genuinely blocking work or owns an
external connection. A thread per displayed module is not.

### 7.3 Layer boundaries

The practical boundaries are:

```text
config -> source state -> typed fields -> formatting -> layout -> Frame -> render
```

They impose these rules:

- I/O and protocol parsing end before geometry is built.
- Native values remain typed; a number is never scraped back out of text.
- Formatting reads fields without consuming them, so state rules and icons can
  use the same original values.
- Layout is deterministic for its configuration, current inputs, output size
  and pointer position.
- `Frame` contains positioned geometry, colours, text, icons and actions. The
  renderer does not read configuration or source protocols.
- Hit testing uses the same `Frame` that was drawn.
- Renderer-neutral icon and separator descriptions remain outside tiny-skia.

The current renderer is a direct CPU/shm implementation, not a generic backend
trait. `Frame` is the seam that keeps another renderer possible if measurement
ever justifies one; a speculative GPU abstraction is not a current requirement.

## 8. Typed status and formatting

Native readings publish named `Fields`. Values have one of these shapes:

```rust
enum Value {
    Num { v: f64, unit: Unit },
    Text(String),
    Time(SystemTime),
    Dur(Duration),
    Flag(bool),
    Absent,
}
```

The implemented numeric units are unitless numbers, percentages, bytes, bytes
per second, Celsius and watts. A source nominates a primary value for state
thresholds and graded icons.

`Absent` means that a declared field has nothing to report right now. It is not
the same as an unknown field: unknown fields are rejected when the config is
loaded.

Formats deliberately provide a small expression vocabulary rather than a
general language:

```text
format      ::= item*
item        ::= literal | placeholder | group
placeholder ::= ('$' name | '${' name '}') ('.' function)? ('|' alternative)*
group       ::= '{' item* '}'
function    ::= ident '(' (arg (',' arg)*)? ')'
arg         ::= ident ':' (number | ident | quoted)
```

A function is written with its parentheses whether or not it takes arguments, so
`.up()` and not `.up`. One spelling, and one function to a placeholder: what
follows a finished call is ordinary text.

- A brace group disappears when a directly contained value is absent.
- A fallback chain selects the first available field or quoted literal.
- `.n()` formats numbers and units.
- `.str()` pads or truncates text.
- `.time()` formats a time in the local or a configured time zone.
- `.dur()` formats durations.
- `.up()` and `.low()` change text case.

Formats and function/value compatibility are checked at startup against the
fields declared by the selected source.

State rules may match the source state, a numeric threshold, an exact field
value, several exact fields, urgency, workspace focus or visibility, and hover.
They select a style; they do not mutate the source value. Text matching and
stripping exist only for provider modules, where presentation text is the only
semantic material the protocol supplies.

## 9. Layout and rendering

The layout vocabulary is intentionally limited to runs, groups, modules,
separators and edges. A frame carries enough information for both painting and
pointer routing.

Groups are the important visual and damage unit. Rounded outlines, opacity,
outer caps, joins and internal separators cross module boundaries and must be
resolved together. Empty modules and groups disappear without leaving their
spacing behind.

Text is shaped and measured before placement. A module that cannot fit within
its configured or available width truncates text rather than pushing unrelated
runs out of the bar. Alternate wording and fold transitions reserve stable
budgets so neighbouring text does not repeatedly reflow during an animation.

The renderer paints a complete CPU buffer for a changed frame. Frame comparison
still computes logical damage so the compositor only has to reconsider changed
regions. Because a newly acquired shm buffer has no reliable retained contents,
partial CPU repaint is not correct without explicit buffer history and stale
region handling. That complexity should be introduced only after measurement
shows a material need.

Group opacity is implemented by drawing the group opaque into reusable scratch
storage and compositing it once. This prevents overlapping translucent
separators from becoming darker than the modules they connect.

## 10. Interaction

Pointer actions are resolved in a fixed order, and configuration rejects two
features that claim the same button on one module. Supported actions include:

- user command execution;
- provider click forwarding;
- brightness and volume scrolling;
- volume mute;
- media play/pause and track movement;
- workspace switching;
- source refresh;
- paging command results;
- alternate wording selection;
- module collapse, with or without travel; and
- group collapse.

Hover is paint-only. It may change foreground, background and radius, but not
the layout metrics under the pointer; otherwise a module can resize itself out
from under the pointer and oscillate.

Custom programs are argv arrays and are executed without an implicit shell.
Users can request a shell explicitly when shell syntax is genuinely wanted.

## 11. Light animation

Animation is an accent, not a runtime mode.

Three user-triggered changes currently support optional smooth travel:

- `collapse_animation` on a group moves it between its expanded and collapsed
  widths;
- `collapse_animation` on a module moves it between its wording and its icon;
- `alt_animation` moves a module between wording widths.

All three are immediate by default. When enabled, they use a smoothstep progression,
are limited to ten seconds, and share one 16 ms timer. The timer is created only
while at least one travel is active and is dropped on the frame the last travel
arrives. Wayland frame callbacks cap actual presentation at the compositor's
pace.

A fold, at either level, can reverse while in flight without jumping back to an
endpoint. Module folds and group folds are tracked separately, because a module
and the group holding it can be travelling at the same time and their two ends
are not the same two ends.

A wording choice settles logically when clicked even while its visual width is
still travelling, and so does a fold. Every transition reserves the wider
affected width for its run, preventing neighbours from being truncated
differently on every frame.

A command spinner is a separate, deliberately slower animation. It appears only
after a command has been outstanding for 400 ms, steps every 60 ms while needed,
and disappears with the last outstanding command.

Any new animation must satisfy all of these:

- immediate behaviour remains available and remains the default;
- no timer or interpolation work exists at rest;
- finite animations have a configured, bounded duration;
- concurrent animations share scheduling rather than adding timers;
- logical state changes immediately and is never lost if an animation reverses;
- the first and final animated frames match the corresponding settled frames;
- clips, joins, caps, text and icons remain correct throughout the transition;
- damage covers both the old and new visible bounds; and
- performance is measured during animation separately from idle performance.

Continuous decorative animation, an always-running marquee, and an application-
wide animation loop are out of scope. Additional effects such as easing choices,
crossfades or slides may be added incrementally when they preserve these rules.
A generic scene or physics animation engine is not a goal.

## 12. Efficiency contract

Efficiency is a product feature and a regression dimension, not a later tuning
pass.

### 12.1 Idle

- A native-only bar runs as one process.
- A new reading that produces the same frame causes no buffer paint or surface
  commit.
- No animation timer exists when nothing is moving.
- Event-driven sources add no polling wake-up.
- Sampled sources share one scheduler that wakes at the earliest deadline and
  reads everything then due.
- The time source aligns itself to the boundary its format needs.
- Multiple outputs share collection and external connections.

### 12.2 Active work

- Several source changes observed together produce one invalidation cycle.
- Frame callbacks prevent rendering more buffers than the compositor accepts.
- Layout, rendering and allocation on the per-frame path remain explicit and
  measurable.
- Animation cost is permitted only for its short active interval.
- A slow command delays itself, not the compositor or another collector.

The README records reproducible measurements for the current reference setup.
The V0 reference workload measured about 21 MB resident, 4.6 MB of its own heap,
0.21% of one core at idle, and one process for a native configuration. These are
regression baselines, not machine-independent ceilings.

Absolute RSS varies with loaded fonts, icon artwork, libraries and hardware, so
the durable target is comparative: on an equivalent workload dbar should remain
materially lighter than a bar plus external status daemon and than a general
widget-toolkit bar. Heap, total resident memory, CPU, process count and thread
count should be reported separately.

Dependencies are part of the footprint. A new dependency needs a concrete
capability or a substantial correctness benefit that is unreasonable to provide
locally.

## 13. Failure behaviour

Configuration errors fail before the Wayland session starts and name the table
or module responsible. `dbar --check-config` provides the same validation
without needing a compositor.

A sampled collector failure retains its last good reading, exposes an error
state for styling, logs transitions rather than every failed tick, and backs off
until it recovers. Pushed sources reconnect or fall back to polling where the
source contract provides that path.

Command sources have an explicit timeout. A failed streaming command is
reported and restarted with bounded backoff. Provider shutdown and malformed
provider input must not corrupt native source state.

Wayland presentation only replaces the remembered on-screen `Frame` after a
buffer was successfully painted and committed. Failed draws remain dirty and
are retried by the next relevant event.

## 14. Inspection and documentation

The binary provides:

- `--check-config` to parse and validate configuration without Wayland;
- `--fields` to list sources and their declared fields;
- `--fields SOURCE` to inspect one source; and
- `--print-config` to print the annotated built-in configuration.

`examples/config.toml` is the compiled-in default and must remain runnable
without an external provider or personal script. `examples/showcase.toml`
exercises and explains the broad configuration surface. All shipped examples
must parse in the test suite.

The README is the user guide and measurement record. This file owns product
scope, architectural constraints, source policy and the rules for future work.

## 15. Evolution

The current implementation is the V0 baseline. The original native-status plan
is complete: dbar can operate without an external status process, has typed
sources and formats, supports the common desktop modules, and keeps i3bar
compatibility at the boundary.

Future work should be demand-led and incremental:

1. Add Bluetooth only when its user-visible contract and dependency choice are
   clear enough to preserve the footprint goal.
2. Add further light animation effects one concrete interaction at a time,
   reusing the existing bounded scheduler.
3. Add native sources where they beat commands or providers by a measurable
   amount in process cost, responsiveness, control or semantics.
4. Optimize layout or painting only after a representative profile identifies
   it as material.
5. Introduce a different rendering backend only against a working second
   implementation and measured benefit, not as speculative abstraction.

Breaking configuration changes are still possible before a stable release, but
they are not free merely because the project is young. A change must make the
standalone configuration clearer or remove a real architectural limitation, and
must update the default, showcase and README together.

## 16. Testing requirements

- Collectors parse deterministic fixtures and do not require the developer's
  hardware.
- Rate collectors receive explicit prior/current samples and elapsed time.
- Formats have exact-output tests, including absent groups, fallbacks, units,
  time zones and type errors.
- Config tests cover all shipped examples and assert useful messages for invalid
  combinations.
- Layout tests use a deterministic text measurer and cover width budgets,
  state selection, hit testing, joins and damage.
- Rendering tests use off-screen buffers for clipping, alpha, separators, caps,
  rounded edges and transition endpoints.
- Animated geometry is checked at its endpoints and intermediate positions,
  including narrow visible slices and fractional device coordinates.
- Changes that can affect footprint are measured in release mode, with idle and
  active behaviour reported separately.
- Wayland, PipeWire and compositor behaviour that fixtures cannot prove is
  validated in an appropriate live environment before being claimed.

## 17. Non-goals

- Running an unmodified i3status-rust configuration file.
- Requiring an external provider for the ordinary desktop status set.
- Matching every module or widget exposed by a general-purpose bar.
- CSS or another styling language.
- Lua, JavaScript or another embedded interpreter.
- A DOM, widget tree, plugin runtime or arbitrary layout engine.
- An async runtime.
- A helper process for functionality available cheaply through a stable native
  interface.
- Permanent decorative animation or a global frame tick.
- A GPU renderer without evidence that the CPU renderer is a real constraint.
- Partial CPU repaint without reliable retained-buffer history.

The intended result is not the most extensible bar in the abstract. It is a
small, fast, native-first bar whose limits are easy to understand and whose
escape hatches—commands and i3bar providers—cover the rest without taking over
the design.
