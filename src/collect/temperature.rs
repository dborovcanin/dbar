//! Temperature, from `/sys/class/hwmon`.
//!
//! A machine has many sensors and no interest in most of them: the processor package is
//! what a bar means by "temperature", and the drive, the wireless card and the charger are
//! not. Without a chip named in the config, the processor's own sensor is looked for by the
//! names the kernel drivers use, so the ordinary case needs no configuration.
//!
//! Readings are in millidegrees, and a great many of them are zero: firmware lists sensors
//! for hardware that is not fitted, and a ThinkPad reports eight of which two are wired up.
//! Zero is treated as nothing to report rather than as a very cold processor.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::{Collector, Reading};
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value};

pub const FIELDS: &[FieldSpec] = &[
    // The hottest sensor on the chip, which is what a temperature module is about.
    FieldSpec {
        name: "temp",
        kind: Kind::Num(Unit::Celsius),
    },
    // The average across the chip's sensors, steadier than the peak.
    FieldSpec {
        name: "average",
        kind: Kind::Num(Unit::Celsius),
    },
    // What the kernel calls the hottest sensor: "Tctl", "Package id 0", "Composite".
    FieldSpec {
        name: "label",
        kind: Kind::Text,
    },
    // The driver the reading came from: "k10temp", "coretemp", "nvme".
    FieldSpec {
        name: "chip",
        kind: Kind::Text,
    },
];

const CLASS: &str = "/sys/class/hwmon";

/// Drivers that report the processor package, most specific first.
///
/// These are the sensors a person means by "how hot is this machine", and they are named
/// by driver rather than found by guessing, because every other chip in the list is also a
/// real temperature and none of them is the answer.
const CPU_CHIPS: [&str; 4] = ["k10temp", "coretemp", "zenpower", "cpu_thermal"];

/// Above this, `Warning`; the processor is working but not in trouble.
const WARM: f64 = 75.0;
/// Above this, `Critical`: most parts throttle somewhere near here.
const HOT: f64 = 90.0;

pub struct Temperature {
    class: PathBuf,
    /// The chip named in the config, or nothing to look for the processor.
    wanted: Option<String>,
}

impl Temperature {
    pub fn new(wanted: Option<String>) -> Temperature {
        Temperature {
            class: PathBuf::from(CLASS),
            wanted,
        }
    }
}

impl Collector for Temperature {
    fn read(&mut self) -> Result<Reading> {
        let Some(chip) = pick(&self.class, self.wanted.as_deref()) else {
            // Naming a chip that is not there is a mistake worth reporting; finding no
            // processor sensor at all is a machine dbar cannot read, and it says so.
            match &self.wanted {
                Some(name) => bail!("no hwmon chip is called {name:?}"),
                None => bail!("no processor temperature sensor was found"),
            }
        };

        let mut fields = Fields::default();
        let Some(hottest) = chip.hottest() else {
            for name in ["temp", "average", "label"] {
                fields.set(name, Value::Absent);
            }
            fields.set("chip", Value::Text(chip.name.clone()));
            fields.set_primary("temp");
            return Ok(Reading {
                fields,
                state: State::Idle,
            });
        };

        let celsius = |v: f64| Value::Num {
            v,
            unit: Unit::Celsius,
        };
        fields.set("temp", celsius(hottest.degrees));
        fields.set("average", celsius(chip.average()));
        fields.set(
            "label",
            match &hottest.label {
                Some(label) => Value::Text(label.clone()),
                None => Value::Absent,
            },
        );
        fields.set("chip", Value::Text(chip.name.clone()));
        fields.set_primary("temp");

        Ok(Reading {
            fields,
            state: match hottest.degrees {
                d if d >= HOT => State::Critical,
                d if d >= WARM => State::Warning,
                _ => State::Idle,
            },
        })
    }
}

#[derive(Clone, Debug)]
struct Sensor {
    degrees: f64,
    label: Option<String>,
}

#[derive(Clone, Debug)]
struct Chip {
    name: String,
    sensors: Vec<Sensor>,
}

impl Chip {
    fn read(path: &Path) -> Option<Chip> {
        let name = super::read_to_string(path.join("name"))
            .ok()?
            .trim()
            .to_string();

        let mut sensors = Vec::new();
        // The kernel numbers sensors from one and leaves gaps, so the count is not known
        // up front; stop after a run of misses rather than guessing a limit.
        for i in 1..=32 {
            let Ok(raw) = super::read_to_string(path.join(format!("temp{i}_input"))) else {
                continue;
            };
            let Ok(milli) = raw.trim().parse::<f64>() else {
                continue;
            };
            // Firmware lists sensors for hardware that is not fitted, and they read zero.
            if milli <= 0.0 {
                continue;
            }
            sensors.push(Sensor {
                degrees: milli / 1000.0,
                label: super::read_to_string(path.join(format!("temp{i}_label")))
                    .ok()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty()),
            });
        }
        Some(Chip { name, sensors })
    }

    fn hottest(&self) -> Option<&Sensor> {
        self.sensors
            .iter()
            .max_by(|a, b| a.degrees.total_cmp(&b.degrees))
    }

    fn average(&self) -> f64 {
        let sum: f64 = self.sensors.iter().map(|s| s.degrees).sum();
        sum / self.sensors.len().max(1) as f64
    }
}

/// The chip to read: the one named, or the processor's own.
fn pick(class: &Path, wanted: Option<&str>) -> Option<Chip> {
    let Ok(entries) = std::fs::read_dir(class) else {
        return None;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // hwmon numbering is not stable across boots, but sorting at least makes one run
    // repeatable.
    paths.sort();
    let chips: Vec<Chip> = paths.iter().filter_map(|p| Chip::read(p)).collect();

    if let Some(name) = wanted {
        return chips.into_iter().find(|c| c.name == name);
    }
    // Most specific driver first, so a machine with both k10temp and a generic acpitz
    // reads the processor rather than the case.
    CPU_CHIPS.iter().find_map(|driver| {
        chips
            .iter()
            .find(|c| c.name == *driver && !c.sensors.is_empty())
            .cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `/sys/class/hwmon`-shaped directory.
    fn class(name: &str, chips: &[(&str, &[(&str, &str)])]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dbar-hwmon-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for (i, (chip, files)) in chips.iter().enumerate() {
            let dir = root.join(format!("hwmon{i}"));
            std::fs::create_dir_all(&dir).expect("the fixture directory is writable");
            std::fs::write(dir.join("name"), format!("{chip}\n")).expect("writable");
            for (file, value) in *files {
                std::fs::write(dir.join(file), format!("{value}\n")).expect("writable");
            }
        }
        root
    }

    #[test]
    fn the_processor_is_preferred_over_every_other_sensor() {
        let root = class(
            "prefer-cpu",
            &[
                ("acpitz", &[("temp1_input", "40000")]),
                ("nvme", &[("temp1_input", "38850")]),
                (
                    "k10temp",
                    &[("temp1_input", "46625"), ("temp1_label", "Tctl")],
                ),
                ("amdgpu", &[("temp1_input", "44000")]),
            ],
        );
        let chip = pick(&root, None).expect("a processor sensor is present");
        assert_eq!(chip.name, "k10temp");
        assert_eq!(chip.hottest().unwrap().degrees, 46.625);
        assert_eq!(chip.hottest().unwrap().label.as_deref(), Some("Tctl"));
    }

    #[test]
    fn a_named_chip_is_read_instead() {
        let root = class(
            "named",
            &[
                ("k10temp", &[("temp1_input", "46625")]),
                ("amdgpu", &[("temp1_input", "44000")]),
            ],
        );
        assert_eq!(pick(&root, Some("amdgpu")).unwrap().name, "amdgpu");
        assert!(pick(&root, Some("nonesuch")).is_none());
    }

    #[test]
    fn sensors_for_hardware_that_is_not_fitted_are_ignored() {
        // A ThinkPad lists eight and wires up two.
        let root = class(
            "thinkpad",
            &[(
                "thinkpad",
                &[
                    ("temp1_input", "46000"),
                    ("temp1_label", "CPU"),
                    ("temp2_input", "0"),
                    ("temp2_label", "GPU"),
                    ("temp3_input", "0"),
                    ("temp4_input", ""),
                ],
            )],
        );
        let chip = pick(&root, Some("thinkpad")).expect("the chip is present");
        assert_eq!(chip.sensors.len(), 1, "only the wired-up sensor counts");
        assert_eq!(chip.hottest().unwrap().degrees, 46.0);
        // An unfitted sensor reading zero must not drag the average down.
        assert_eq!(chip.average(), 46.0);
    }

    #[test]
    fn the_hottest_sensor_on_the_chip_is_the_one_reported() {
        let root = class(
            "many",
            &[(
                "k10temp",
                &[
                    ("temp1_input", "40000"),
                    ("temp1_label", "Tctl"),
                    ("temp2_input", "60000"),
                    ("temp2_label", "Tccd1"),
                ],
            )],
        );
        let chip = pick(&root, Some("k10temp")).expect("the chip is present");
        assert_eq!(chip.hottest().unwrap().degrees, 60.0);
        assert_eq!(chip.hottest().unwrap().label.as_deref(), Some("Tccd1"));
        assert_eq!(chip.average(), 50.0);
    }

    #[test]
    fn a_chip_that_cannot_be_found_is_reported_rather_than_drawn_empty() {
        let root = class("none", &[("nvme", &[("temp1_input", "38850")])]);
        let mut collector = Temperature {
            class: root.clone(),
            wanted: None,
        };
        let e = collector
            .read()
            .expect_err("a machine with no processor sensor cannot be read");
        assert!(format!("{e:#}").contains("processor"), "{e:#}");

        let mut named = Temperature {
            class: root,
            wanted: Some("nonesuch".to_string()),
        };
        let e = named
            .read()
            .expect_err("a chip that is not there is a mistake");
        assert!(format!("{e:#}").contains("nonesuch"), "{e:#}");
    }

    #[test]
    fn how_hot_is_worth_alarming_about() {
        let root = class("hot", &[("k10temp", &[("temp1_input", "95000")])]);
        let mut collector = Temperature {
            class: root,
            wanted: None,
        };
        assert_eq!(collector.read().expect("reads").state, State::Critical);
    }
}
