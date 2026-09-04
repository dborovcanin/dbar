//! Screen brightness, from `/sys/class/backlight`.
//!
//! The kernel exposes one directory per controller, each with a raw `brightness` and the
//! `max_brightness` it scales against. The raw numbers are meaningless on their own - one
//! panel counts to 255, another to 96000 - so what is published is the share.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::{Collector, Reading};
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value};

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "brightness",
        kind: Kind::Num(Unit::Percent),
    },
    FieldSpec {
        name: "device",
        kind: Kind::Text,
    },
];

const CLASS: &str = "/sys/class/backlight";

pub struct Backlight {
    /// Where controllers are listed. A field rather than a constant so tests can point it
    /// at a fixture instead of at whatever hardware happens to be in the machine.
    class: PathBuf,
    /// The controller being read. Resolved on the first tick rather than at start-up, so
    /// a display connected later is still found.
    device: Option<PathBuf>,
}

impl Backlight {
    pub fn new() -> Backlight {
        Backlight {
            class: PathBuf::from(CLASS),
            device: None,
        }
    }

    /// The controller to read, choosing one again if the last has gone away.
    fn resolve(&mut self) -> Option<&Path> {
        let usable = self
            .device
            .as_ref()
            .is_some_and(|d| d.join("brightness").exists());
        if !usable {
            self.device = pick(&self.class);
        }
        self.device.as_deref()
    }
}

impl Collector for Backlight {
    fn read(&mut self) -> Result<Reading> {
        let mut fields = Fields::default();

        // A machine with no backlight is ordinary, not broken. The fields have nothing to
        // report, so a module built on them draws nothing.
        let (brightness, device) = match self.resolve() {
            Some(device) => (share(device)?, name_of(device)),
            None => (Value::Absent, Value::Absent),
        };
        fields.set("brightness", brightness);
        fields.set("device", device);
        fields.set_primary("brightness");

        Ok(Reading {
            fields,
            state: State::Idle,
        })
    }
}

/// The controller's brightness as a share of what it can do.
///
/// The raw numbers mean nothing on their own: one panel counts to 255, another to 96000.
fn share(device: &Path) -> Result<Value> {
    let current = number(&device.join("brightness"))?;
    let max = number(&device.join("max_brightness"))?;
    if max == 0 {
        bail!("{} reports a maximum brightness of zero", device.display());
    }
    Ok(Value::Num {
        v: current.min(max) as f64 * 100.0 / max as f64,
        unit: Unit::Percent,
    })
}

fn name_of(device: &Path) -> Value {
    match device.file_name() {
        Some(name) => Value::Text(name.to_string_lossy().into_owned()),
        None => Value::Absent,
    }
}

/// Move the brightness by a share of the controller's whole range.
///
/// A share of the range rather than of the current value: a step that shrinks as the panel
/// darkens takes ever smaller bites and never arrives at either end.
///
/// Writing the file is the whole operation. The kernel then notifies the watcher, which is
/// what puts the new number on the bar - so what is drawn is what the hardware accepted,
/// never what dbar hoped it would.
pub fn adjust(step: f64) -> Result<()> {
    adjust_in(Path::new(CLASS), step)
}

fn adjust_in(class: &Path, step: f64) -> Result<()> {
    let device = pick(class).context("no backlight controller to change")?;
    let max = number(&device.join("max_brightness"))?;
    let current = number(&device.join("brightness"))?;
    if max == 0 {
        bail!("{} reports a maximum brightness of zero", device.display());
    }

    let moved = current as f64 + step * max as f64 / 100.0;
    let wanted = moved.round().clamp(0.0, max as f64) as u64;
    // A step too small to move an integer still has to move: on a panel counting to 10, a
    // 5% notch would otherwise do nothing at all, for ever.
    let wanted = match (wanted == current, step > 0.0) {
        (true, true) => (current + 1).min(max),
        (true, false) => current.saturating_sub(1),
        (false, _) => wanted,
    };
    if wanted == current {
        return Ok(());
    }

    let path = device.join("brightness");
    std::fs::write(&path, wanted.to_string()).with_context(|| {
        format!(
            "writing {}; dbar changes the brightness itself, which needs write access -              the usual way is membership of the `video` group",
            path.display()
        )
    })
}

/// The file the kernel notifies on when the brightness changes.
///
/// `brightness` is what was last asked for; `actual_brightness` is what the panel is
/// doing, and it is the one the backlight class calls `sysfs_notify` on. They move
/// together, so watching one and reading the other is not a race - it is reading the
/// request once the hardware has acknowledged it. A controller too old to have the file
/// is read on an interval instead.
pub fn watch_path() -> Option<PathBuf> {
    let actual = pick(Path::new(CLASS))?.join("actual_brightness");
    actual.exists().then_some(actual)
}

/// Choose a controller from a `/sys/class/backlight`-shaped directory.
///
/// A machine with no backlight has no such directory, which is an answer rather than a
/// failure.
fn pick(class: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(class)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("brightness").exists() && p.join("max_brightness").exists())
        .collect();

    // A keyboard backlight is a backlight, but it is not what the bar is about. Everything
    // else sorts by name so the choice does not depend on directory order.
    candidates.sort_by_key(|p| {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (name.contains("kbd"), name)
    });
    candidates.into_iter().next()
}

fn number(path: &Path) -> Result<u64> {
    let text = super::read_to_string(path)?;
    text.trim()
        .parse()
        .with_context(|| format!("{} does not hold a number", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `/sys/class/backlight`-shaped directory under the scratch space.
    fn class(name: &str, devices: &[(&str, u64, u64)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dbar-backlight-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for (device, current, max) in devices {
            let dir = root.join(device);
            std::fs::create_dir_all(&dir).expect("the fixture directory is writable");
            std::fs::write(dir.join("brightness"), format!("{current}\n")).expect("writable");
            std::fs::write(dir.join("max_brightness"), format!("{max}\n")).expect("writable");
        }
        root
    }

    fn brightness_of(root: &Path, device: &str) -> u64 {
        number(&root.join(device).join("brightness")).expect("the fixture holds a number")
    }

    #[test]
    fn a_scroll_moves_a_share_of_the_whole_range() {
        let root = class("adjust", &[("intel_backlight", 48, 96)]);
        adjust_in(&root, 25.0).expect("the fixture is writable");
        assert_eq!(brightness_of(&root, "intel_backlight"), 72);
        adjust_in(&root, -25.0).expect("the fixture is writable");
        assert_eq!(brightness_of(&root, "intel_backlight"), 48);
    }

    #[test]
    fn a_step_never_runs_past_either_end() {
        let root = class("clamp", &[("intel_backlight", 90, 96)]);
        adjust_in(&root, 50.0).expect("writable");
        assert_eq!(brightness_of(&root, "intel_backlight"), 96);
        adjust_in(&root, -500.0).expect("writable");
        assert_eq!(brightness_of(&root, "intel_backlight"), 0);
    }

    #[test]
    fn a_step_too_small_to_land_on_a_new_number_still_moves() {
        // A panel counting to ten has no 5% to give, and a scroll that does nothing is a
        // broken scroll rather than a subtle one.
        let root = class("coarse", &[("intel_backlight", 5, 10)]);
        adjust_in(&root, 5.0).expect("writable");
        assert_eq!(brightness_of(&root, "intel_backlight"), 6);
        adjust_in(&root, -5.0).expect("writable");
        assert_eq!(brightness_of(&root, "intel_backlight"), 5);
    }

    #[test]
    fn a_machine_with_no_backlight_says_so_rather_than_writing_somewhere_else() {
        assert!(adjust_in(Path::new("/nonexistent/backlight"), 5.0).is_err());
    }

    #[test]
    fn a_panel_is_preferred_over_a_keyboard() {
        let root = class(
            "panel-first",
            &[("kbd_backlight", 1, 3), ("intel_backlight", 48, 96)],
        );
        let picked = pick(&root).expect("a controller is present");
        assert_eq!(picked.file_name().unwrap(), "intel_backlight");
    }

    #[test]
    fn a_directory_without_the_files_is_not_a_controller() {
        let root = class("empty", &[]);
        std::fs::create_dir_all(root.join("bogus")).expect("the fixture directory is writable");
        assert_eq!(pick(&root), None);
    }

    #[test]
    fn a_machine_with_no_backlight_class_is_not_an_error() {
        assert_eq!(pick(Path::new("/nonexistent/backlight")), None);
    }

    #[test]
    fn brightness_is_a_share_of_the_maximum() {
        let root = class("share", &[("acpi_video0", 48, 96)]);
        let mut collector = Backlight {
            class: root.clone(),
            device: None,
        };
        let reading = collector.read().expect("the fixture reads");
        assert!(matches!(
            reading.fields.primary(),
            Some(Value::Num { v, .. }) if (v - 50.0).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn a_missing_controller_reports_nothing_rather_than_failing() {
        let mut collector = Backlight {
            class: PathBuf::from("/nonexistent/backlight"),
            device: None,
        };
        let reading = collector
            .read()
            .expect("an absent backlight is not an error");
        assert!(matches!(reading.fields.primary(), Some(Value::Absent)));
    }
}
