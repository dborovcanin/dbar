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
