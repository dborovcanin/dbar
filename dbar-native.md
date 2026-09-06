# dbar Native Status Engine — Plan

## 0. Decisions

Locked, and the rest of this document follows from them:

1. **dbar owns its configuration dialect.** It is i3status-rust *inspired* — same
   mental model of blocks, formats, intervals, thresholds and states — but it is
   dbar's format and it is richer. Compatibility is never a reason to accept a
   worse design.
2. **Breaking changes are free.** The project is days old. Prefer the right shape
   over migration comfort, every time.
3. **The i3bar protocol stays**, as an input backend, roughly as implemented now.
   It is how dbar consumes `i3status-rs`, `py3status` and anything else legacy.
4. **dbar collects its own data**, so it runs with no external status process.
   First targets are the cheap ones: cpu, memory, backlight.
5. **Interfaces stay renderer-neutral**, so a GPU backend can replace tiny-skia
   without touching layout, formatting or collection.
6. **Config and format grammar stay small and legible**, but never at the cost of
   expressiveness.

Non-goal: running an unmodified `i3status-rust` config file. Reusing its *ideas*
is worth it; reusing its *file* buys a bar with no visual configuration.

---

## 1. Target architecture

```text
                 config  (dbar dialect)
                    │
        ┌───────────┼────────────┬──────────────┐
        ▼           ▼            ▼              ▼
   collectors   i3bar child   sway IPC     command modules
   (native)     (protocol)    (protocol)      (later)
        │           │            │              │
        └───────────┴─────┬──────┴──────────────┘
                          ▼
                       Reading            typed fields + state
                          │
                          ▼
                      formatter           format string → text
                          │
                          ▼
                     StatusItem           id, text, fields, state, icon, action
                          │
                ┌─────────┴─────────┐
                ▼                   ▼
             layout             interaction
                └─────────┬─────────┘
                          ▼
                        Frame             pure geometry + colour
                          │
                          ▼
                   Backend (trait)        tiny-skia now, GPU later
                          │
                          ▼
                       Wayland
```

Two rules keep this honest:

- nothing below `StatusItem` knows where data came from;
- nothing below `Frame` knows about config, format or protocol.

Both already half-hold in the current code (`layout.rs` is documented as
"purely geometric"); the plan is to finish the job.

A third rule follows from those two once there is more than one screen. Everything
above `StatusItem` happens once for the session: one round of collecting, one set of
readings, one i3bar child. Everything from layout down happens once per screen, because
that is where width, scale and the pointer come in. `App` owns the first half and a
`Bar` owns the second - its surface, its shm pool, its clip mask, its `Frame` - so a
second monitor costs one more buffer and no more wake-ups. What a screen is showing
reaches layout as `Inputs::output`, and is the only thing there that knows a screen
exists at all.

---

## 2. What exists today

Worth stating, because the plan is an edit to this and not a greenfield design.

| Area | State |
|---|---|
| `config.rs` | 945 LOC. `[bar] [status] [colors] [left|center|right] [style.*] [group.*] [module.*]`, two-stage raw→resolved with a style cascade, `deny_unknown_fields` |
| `layout.rs` | groups, modules, separators, edges, clipping, hit testing, `Measure` trait, `Frame` of placed geometry |
| `render.rs` | tiny-skia; vector separators (line/slant/chevron/notch/round/curve), edged rects, icons |
| `icon.rs` | built-in vector icons in a unit square, graded levels |
| `status.rs` | i3bar child process, reader thread → calloop channel, click stream, pango stripping |
| `sway.rs` | workspaces and focused window over IPC |
| `app.rs` | layer shell, event-driven redraw, frame-callback throttling |

The config model is already more expressive than i3status-rust's: named modules
referenced by name from named groups, a style cascade, ordered state rules with
specificity, per-side group edges, vector separators with Powerline colour modes.
That is the asset. The plan grows data collection under it, it does not replace it.

Known warts to remove along the way:

- `status::percent(text)` — scrapes a number back out of rendered text
  (`layout.rs:309`). The entire reason the semantic model exists.
- `[status] blocks` positional aliasing — exists only because the i3bar protocol
  gives blocks no usable names. Native modules must not inherit this.
- `StateRule::contains` / `strip` — text matching used as a substitute for data.
  Stays for the i3bar source, where text is all there is; unavailable to native.
- `[status.block]` generated-provider mode — dbar writing an `i3status-rs` config
  and shelling out. Superseded by native collection; see §9.

---

## 3. Configuration

### 3.1 Shape

Native data sources hook into the **existing** `[module.*]` table via `source`,
which already distinguishes provider from compositor modules. No `[[block]]`
array is introduced: dbar names its modules and groups order them, which is
strictly better than positional arrays.

```toml
[bar]
height = 34
position = "top"
margin = 6
font = "sans-serif 10"

[colors]
surface = "#313244cc"
text    = "#cdd6f4"
warn    = "#f9e2af"
crit    = "#f38ba8"

[module.cpu]
source   = "cpu"
interval = "2s"
format   = " $icon $usage.n(d:0,w:3) "
icon     = "cpu"

[module.mem]
source = "memory"
format = " $icon $used.n(d:1,unit:bin) {of $total.n(d:0,unit:bin)} "
icon   = "memory"

[module.light]
source = "backlight"
format = " $icon $brightness "
icon   = "brightness"

[module.time]
source = "time"
format = " $icon $now.time(f:'%a %d %b  %R') "
icon   = "clock"

[group.system]
modules    = ["cpu", "mem", "light"]
background = "$surface"
padding    = 4
radius     = 10

[group.system.separator]
shape = "chevron"
width = 10

[right]
groups = ["system", "clock"]
```

Legacy input keeps working, and is the only thing `[i3bar]` is for:

```toml
[i3bar]
command = "i3status-rs"
args    = ["~/.config/i3status-rust/config.toml"]
names   = ["net", "battery"]   # positional naming, i3bar's own limitation

[module.battery]
source = "i3bar:battery"
format = " $icon $text "
```

`[status]` is renamed to `[i3bar]` (breaking, fine) because it now describes one
backend among several rather than "the" status source.

### 3.2 Per-module keys

New on `[module.*]`, alongside the existing style cascade and `states`:

```text
source     required; "cpu" | "memory" | "backlight" | ... | "i3bar:<name>"
            | "sway:window" | "sway:workspaces" | "sway:language"
            | "command:<...>"
format     format string; defaults to a sensible per-source default
format_alt optional; click toggles between the two
interval   duration string ("2s", "500ms", "1m"); ignored by event-driven sources
signal     refresh on SIGRTMIN+N
[module.x.click]  button → action (see §7)
```

Durations are strings with units, not bare integers. `interval = 2` is
ambiguous; `interval = "2s"` is not.

### 3.3 Thresholds

State rules stop keying on scraped text and key on **fields**:

```toml
[module.cpu.states.warn]
above = 70            # the source's primary field
style = "warning"

[module.cpu.states.hot]
field = "temp"        # or any other field it publishes
above = 80
style = "critical"

[module.battery.states.low]
state = "critical"    # or on the state the collector itself declared
style = "critical"
```

`contains`/`strip` remain valid only for `source = "i3bar:*"`, where the block's
text really is the only signal. Config validation rejects them elsewhere rather
than silently never matching.

---

## 4. Semantic model

```rust
/// What a collector produces for one tick.
pub struct Reading {
    pub fields: Fields,          // ordered name -> Value map
    pub state: State,
}

pub enum Value {
    Num { v: f64, unit: Unit },
    Text(String),
    Time(std::time::SystemTime),
    Dur(std::time::Duration),
    Flag(bool),
    /// A field the source knows about but cannot supply right now: no battery,
    /// link down, sensor missing. Drives `{...}` elision in formats.
    Absent,
}

pub enum Unit {
    None, Percent, Bytes, BytesPerSec, Hertz, Celsius, Watts, Volts, Seconds,
}

pub enum State { Idle, Info, Good, Warning, Critical, Error }
```

Two deliberate choices:

- **`Value::Num` carries a unit rather than having a variant per shape.**
  A `Percent`/`Bytes`/`Rate` enum cannot answer "render this with one decimal and
  a binary prefix" — the formatter needs magnitude *and* unit *and* prefix
  family, and the unit decides the family (bytes → KiB/MiB, everything else → SI).
- **`Absent` is a value, not `Option`.** It is the input to conditional format
  groups, and it is a distinct render from "field does not exist" (a config error).

`State::Error` is included; the earlier draft's `Urgent` is dropped. Urgency is an
input flag (i3bar's `urgent`, sway's workspace urgency), not a point on the state
scale, and it already lives in `StateFlags`.

```rust
pub struct StatusItem {
    pub id: ModuleId,
    pub text: String,            // the formatted result
    pub fields: Fields,          // kept, so thresholds and icons read data not text
    pub state: State,
    pub flags: StateFlags,       // urgent / focused / visible
    pub icon: Option<Icon>,
    pub action: Option<ActionTarget>,
}
```

Icon grading reads `fields`, not `percent(text)`.

---

## 5. Format grammar

Small, total, and expressive enough that no `if` statement is ever needed.

```text
format      ::= item*
item        ::= literal | placeholder | group
placeholder ::= '$' name ( '.' func )? | '${' name ( '.' func )? '}'
                ( '|' fallback )*
group       ::= '{' item* '}'
func        ::= ident '(' arg ( ',' arg )* ')'
arg         ::= ident ':' ( number | ident | '\'' text '\'' )
escape      ::= '$$' | '{{' | '}}'
```

Semantics:

- **A group is dropped whole** if any placeholder inside it resolves to `Absent`.
  `{ of $total }` disappears when total is unknown. This replaces conditionals.
- **`|` is a fallback chain**: `$ssid|$device|'offline'`. First non-`Absent` wins.
- **Unknown field name is a config error**, reported at load, not at render.

Functions, kept deliberately few:

```text
.n(d:, w:, unit:, prefix:, sign:)   numbers
      d       decimals                       (default: unit-dependent)
      w       minimum width, space padded
      unit    si | bin | none | auto         (default auto: bytes→bin, else si)
      prefix  force a prefix: K, M, G, Ki, Mi, Gi
      sign    always | auto
.str(w:, max:, ell:)                text: pad, truncate, ellipsis
.time(f:'%R')                       SystemTime, strftime
.dur(style: hms | short | long)     Duration → "1:23:45" / "1h23m"
.up(), .low()                       case
```

`$cpu` with no function formats by unit default — percent as `42%`, bytes as
`7.4 GiB`, temperature as `61°C`. The common case needs no function call at all.

Truncation to a pixel budget stays where it already is: `max_width` in the style
cascade, applied by layout with real text measurement. `.str(max:)` is character
count, a different tool, and the two do not overlap.

Formatting never mutates fields. `StatusItem` carries both.

---

## 6. Collection

### 6.1 Runtime — no async

**Decision: no async runtime, no tokio.** The current design is a calloop event
loop plus worker threads feeding calloop channels (`status.rs`, `sway.rs` both do
this). Collection follows the same shape:

- **Polled collectors** (procfs/sysfs) read on the main thread from a calloop
  `Timer`. A `/proc/stat` read is microseconds; a thread would cost more than it
  saves.
- **Event-driven sources** (D-Bus, PipeWire, udev, inotify) get a worker thread
  that owns its connection and pushes `Reading`s over a calloop channel — one
  more source in the loop, same as the sway thread.

This keeps zero async dependencies in a bar whose whole selling point is being
small, and it keeps every source uniform from the event loop's point of view.

### 6.2 Scheduler

- Modules are bucketed by interval; one timer per distinct interval, not per
  module. Ten 1-second modules cause one wakeup, not ten.
- The `time` source aligns to the next boundary implied by its format — a format
  with no `%S` ticks on the minute, not every second.
- A tick collects every due source, then invalidates **once**. N updates, one
  redraw.
- A collector that errors yields `State::Error`, renders with the error style, and
  backs off (double up to a cap, reset on success). It is logged once per
  transition, not per tick.
- Where the kernel offers notification, use it instead of polling: `poll()` on
  the backlight's `actual_brightness`, which is the attribute the backlight class
  notifies on, and udev for battery. A watched source is taken off the timer
  entirely, and goes back on it only if its file disappears. Polling stays for
  genuinely sampled metrics (cpu, throughput), which is an accepted cost against
  spec.md's zero-idle-wakeup goal and should be documented as such.
- The bar animates in exactly one place: a command module whose program is still
  running. Its timer is separate from the collector timer, exists only while a run
  is out, drops itself when the last answer lands, and steps at 60ms. Nothing is
  drawn for the first 400ms of a run, so the scripts that answer straight away
  never animate at all and the idle case is untouched. This is the only sanctioned exception to
  "no animation tick", and it stays the only one: it is bounded by work that is
  genuinely in flight, which is the test any future exception has to pass.

### 6.3 Layout

`src/collect/` in the same crate — not a `dbar-sys` crate. `-sys` conventionally
means FFI bindings to a C library, and a workspace split at under 4k LOC is
overhead with no payoff. Split later when the collector set earns it.

```text
src/collect/
├── mod.rs        Source trait, registry, scheduler
├── cpu.rs        /proc/stat
├── memory.rs     /proc/meminfo
├── backlight.rs  /sys/class/backlight
├── battery.rs    /sys/class/power_supply
├── load.rs       /proc/loadavg
├── disk.rs       statvfs
├── temp.rs       /sys/class/hwmon
├── net.rs        /sys/class/net counters
└── time.rs
```

```rust
pub trait Source {
    /// Field names this source can publish; validated against formats at load.
    fn fields(&self) -> &'static [FieldSpec];
    fn read(&mut self) -> Result<Reading>;
}
```

External crates are welcome *inside* a collector for the hard subsystems
(PipeWire, BlueZ, NetworkManager, UPower, MPRIS). dbar owns the `Source` trait and
the field names; it does not own the D-Bus plumbing.

### 6.4 Licensing

Collectors are written from documented kernel interfaces (`/proc`, `/sys`,
`statvfs`) and public crate APIs. No line-by-line translation of GPL-3.0-only
i3status-rust collectors. Test fixtures are captured from a live machine, not
copied from another project's repository.

---

## 7. Actions

```rust
pub enum ActionTarget {
    /// Route back over the i3bar click protocol.
    I3Bar { name: Option<String>, instance: Option<String> },
    /// Handled inside dbar: toggle format_alt, adjust backlight, mute.
    Native { module: ModuleId, action: NativeAction },
    /// Run a shell command.
    Command(String),
    /// Send a compositor command over IPC.
    Sway(String),
}
```

Config surface:

```toml
[module.light.click]
scroll_up   = "brightness +5%"
scroll_down = "brightness -5%"

[module.time.click]
left = "format_alt"
```

Hit testing is unchanged — `Frame` already carries per-module rectangles; only
what a rectangle points at changes, from a block index to an `ActionTarget`.

`signal = N` on a module refreshes it on `SIGRTMIN+N`, matching the muscle memory
of every i3 user's `pkill -SIGRTMIN+8 i3status` scripts.

---

## 8. Renderer seam

`Frame` is already pure geometry and colour. Two things stand between it and a
GPU backend:

1. **Presentation is hard-wired.** `render::render_to_buffer` writes to an shm
   pixmap. Introduce:

   ```rust
   pub trait Backend {
       fn present(&mut self, frame: &Frame, size: Size, scale: i32) -> Result<()>;
       fn measure(&mut self) -> &mut dyn Measure;
   }
   ```

   `App` holds `Box<dyn Backend>`; the shm/tiny-skia backend is the first impl.

2. ~~**Icons hand out `tiny_skia::Path`.**~~ Done: `icon.rs` emits `PathCmd`
   commands in the unit square and the backend converts. Separator and edge
   geometry in `render.rs` is still built as `tiny_skia` paths, but it is built
   *inside* the backend rather than handed to it, so it moves with the backend
   rather than blocking one.

Text is isolated for both halves now: `Measure` for layout, and `DrawText` for
drawing, which hands back a rasterised run - where it sits relative to the
origin, and either coverage to tint or premultiplied pixels for a glyph that
carries its own colour. The backend places and colours it. A GPU backend uploads
those same bytes to an atlas.

With that and the icons, `tiny_skia` appears in `render.rs` and nowhere else.
What remains of item 1 is the presentation call itself, which is two functions
wide: `render::render_to_buffer` and `App::draw`. A `Backend` trait is worth
shaping against a second backend rather than inventing against one.

A group with an `opacity` is drawn opaque onto a spare pixmap and composited
once, which is the only way a filled separator and a translucent island can
coexist: fills that are already translucent composite where they overlap, and a
filled separator lays its ground across the whole gap and its shape over the
top. A GPU backend does the same thing with a render target and one textured
quad. The layer is sized to the widest island that asks for one and grown only,
so no redraw allocates.

---

## 9. i3bar backend

Kept, permanently, unchanged in behaviour:

```text
src/status/
├── mod.rs      StatusItem, Value, State, the source registry
├── i3bar.rs    child process, reader thread, click stream, pango stripping
└── native.rs   collectors → Reading → StatusItem
```

`Block` is renamed `I3BarBlock` and confined to `i3bar.rs`. It converts to
`StatusItem` with `text` from `full_text`, `state` from `urgent`, and fields
limited to `$text` — because text is genuinely all the protocol offers.

**`[status.block]` generated-provider mode is removed.** Having dbar write an
`i3status-rs` config file and shell out made sense as a bridge; once dbar collects
its own data it is a second, weaker config dialect living inside the first. Users
who want an external provider point `[i3bar] args` at their own file.

---

## 10. Phases

Each phase ends with the bar working. No phase leaves a broken tree.

### P0 — Decouple

- `Block` → `I3BarBlock`, moved into `src/status/i3bar.rs`.
- Introduce `StatusItem`, `Value`, `Unit`, `State`, `Fields`, `ActionTarget`.
- `layout.rs` consumes `StatusItem`; hit testing carries `ActionTarget`.
- Delete `status::percent`; icon grading and thresholds read fields. For the
  i3bar source, a single parsed field keeps current behaviour alive.

*Done when:* rendering is byte-identical, and nothing under `layout.rs` mentions
i3bar.

### P1 — Format engine

- Grammar from §5: parser, `Absent` group elision, `|` chains, `.n .str .time
  .dur`.
- Formats validated against source field specs at config load.
- Table-driven tests: (fields, format) → exact string.

*Done when:* `format` works end to end on the i3bar source's `$text`.

### P2 — First collectors

- `src/collect/` with the `Source` trait and the interval-bucketed scheduler.
- `cpu`, `memory`, `backlight`, `time`.
- `[module.*] source = "cpu"` wired through config.

*Done when:* a config with no `[i3bar]` table renders a working bar and no child
process is spawned.

### P3 — Config rework

- `[status]` → `[i3bar]`; generated-provider mode removed.
- `interval`, `format`, `format_alt`, `signal` on modules.
- Field-keyed thresholds; `contains`/`strip` restricted to the i3bar source and
  rejected elsewhere.
- Error state styling.
- `examples/` and `README.md` rewritten to the native-first config.

### P4 — Collector breadth

`battery` (sysfs), `disk`, `load`, `temperature`, `net` throughput. Sysfs
notification where available instead of polling.

### P5 — Backend seam

`Backend` trait, neutral path description for icons and separators, tiny-skia
backend behind it. No GPU code — only the seam that makes it possible.

### P6 — Event-driven sources

Audio (PipeWire/WirePlumber), UPower, NetworkManager, Bluetooth, MPRIS, each on a
worker thread feeding a calloop channel.

### P7 — Tooling and docs

- `dbar --check-config` — validates, resolves and reports unknown fields, unknown
  format placeholders, unreachable state rules.
- `docs/configuration.md`, `docs/formatting.md`, `docs/sources.md`.

---

## 11. Testing

- **Collectors:** parse captured fixtures (`/proc/stat`, `/proc/meminfo`, a sysfs
  battery tree), so tests do not depend on the developer's hardware. Rate
  collectors get two fixtures and an explicit elapsed time.
- **Formatter:** exact-output table tests, including `Absent` elision, fallback
  chains, prefix families and width padding.
- **Layout:** already testable with a stub `Measure`; extend to threshold and
  state-rule selection.
- **Config:** load every `examples/*.toml` in a test, plus a set of deliberately
  broken configs asserting the *error messages*, not just the failure.

---

## 12. Non-goals

- Running an unmodified i3status-rust config file.
- Depending on i3status-rust internals, or copying its collector implementations.
- CSS, or an embedded interpreter. Running a user's own command as a source is in scope
  (`source = "command:..."`); growing a language of dbar's own is not.
- Async runtime.
- A widget tree. The renderer draws text, icons, rects and separators; that is
  the whole vocabulary.
