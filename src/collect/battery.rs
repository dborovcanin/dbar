//! Battery charge, from `/sys/class/power_supply`.
//!
//! The kernel describes a battery in one of two families, depending on what the hardware
//! reports: energy in µWh with power in µW, or charge in µAh with current in µA. They carry
//! the same information, and the second becomes the first by multiplying through the
//! voltage, so everything below works in watt-hours and watts once the files are read.
//!
//! A machine can have several batteries, or none. Several are summed, because what a person
//! wants to know is how much is left in the laptop rather than in each cell; none is
//! ordinary rather than an error, and reports nothing so a module built on it draws nothing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use super::{Collector, Reading};
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value};

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "percent",
        kind: Kind::Num(Unit::Percent),
    },
    // `charging`, `discharging`, `full`, `not charging` or `unknown`, lowercased so a
    // state rule can match it without knowing how the kernel capitalises.
    FieldSpec {
        name: "status",
        kind: Kind::Text,
    },
    // What the battery is drawing or taking on. Absent when the hardware cannot say.
    FieldSpec {
        name: "power",
        kind: Kind::Num(Unit::Watts),
    },
    // Until empty when discharging, until full when charging. Absent when nothing is
    // moving, because there is no answer rather than an infinite one.
    FieldSpec {
        name: "time",
        kind: Kind::Dur,
    },
    // What the battery still holds at full, against what it held when new.
    FieldSpec {
        name: "health",
        kind: Kind::Num(Unit::Percent),
    },
    // The level charging stops at, when something has capped it - TLP and the ThinkPad
    // firmware both do. Absent when nothing is holding it back, so a format only mentions
    // a cap on a machine that has one.
    FieldSpec {
        name: "threshold",
        kind: Kind::Num(Unit::Percent),
    },
];

const CLASS: &str = "/sys/class/power_supply";

/// Below this, the reading is `Warning`.
const LOW: f64 = 30.0;
/// Below this, `Critical`.
const URGENT: f64 = 15.0;

pub struct Battery {
    /// Where power supplies are listed. A field rather than a constant so tests can point
    /// it at a fixture instead of at whatever hardware happens to be in the machine.
    class: PathBuf,
}

impl Battery {
    pub fn new() -> Battery {
        Battery {
            class: PathBuf::from(CLASS),
        }
    }
}

impl Collector for Battery {
    fn read(&mut self) -> Result<Reading> {
        // Batteries are enumerated on every tick rather than once, so one that is removed
        // or hot-plugged is noticed without dbar being restarted.
        let cells: Vec<Cell> = batteries(&self.class)
            .iter()
            .filter_map(|path| Cell::read(path))
            .collect();

        let mut fields = Fields::default();
        fields.set_primary("percent");

        let Some(total) = Total::of(&cells) else {
            for name in ["percent", "status", "power", "time", "health", "threshold"] {
                fields.set(name, Value::Absent);
            }
            fields.set_primary("percent");
            return Ok(Reading {
                fields,
                state: State::Idle,
            });
        };

        let percent = total.percent();
        fields.set(
            "percent",
            Value::Num {
                v: percent,
                unit: Unit::Percent,
            },
        );
        fields.set("status", Value::Text(total.status.name().to_string()));
        fields.set(
            "power",
            match total.watts {
                Some(w) => Value::Num {
                    v: w,
                    unit: Unit::Watts,
                },
                None => Value::Absent,
            },
        );
        fields.set(
            "time",
            match total.remaining() {
                Some(d) => Value::Dur(d),
                None => Value::Absent,
            },
        );
        fields.set(
            "health",
            match total.health() {
                Some(h) => Value::Num {
                    v: h,
                    unit: Unit::Percent,
                },
                None => Value::Absent,
            },
        );
        fields.set(
            "threshold",
            match total.threshold {
                Some(t) => Value::Num {
                    v: t,
                    unit: Unit::Percent,
                },
                None => Value::Absent,
            },
        );
        fields.set_primary("percent");

        Ok(Reading {
            fields,
            state: total.state(percent),
        })
    }
}

/// Every battery the kernel is currently listing, in a stable order.
fn batteries(class: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(class) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            // A power supply is also the mains adapter, a UPS or a wireless mouse; only a
            // battery has a charge to report.
            super::read_to_string(p.join("type"))
                .map(|t| t.trim() == "Battery")
                .unwrap_or(false)
        })
        .collect();
    found.sort();
    found
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Charging,
    Discharging,
    Full,
    /// Plugged in, but deliberately not charging: a charge threshold is holding it.
    NotCharging,
    Unknown,
}

impl Status {
    fn parse(text: &str) -> Status {
        match text.trim() {
            "Charging" => Status::Charging,
            "Discharging" => Status::Discharging,
            "Full" => Status::Full,
            "Not charging" => Status::NotCharging,
            _ => Status::Unknown,
        }
    }

    /// How much this status has to say about what the machine is doing, for deciding
    /// which of several batteries speaks for all of them.
    fn urgency(self) -> u8 {
        match self {
            Status::Unknown => 0,
            Status::Full => 1,
            Status::NotCharging => 2,
            Status::Charging => 3,
            Status::Discharging => 4,
        }
    }

    /// Lowercased, so a config matching on it does not have to know how the kernel
    /// capitalises.
    fn name(self) -> &'static str {
        match self {
            Status::Charging => "charging",
            Status::Discharging => "discharging",
            Status::Full => "full",
            Status::NotCharging => "not charging",
            Status::Unknown => "unknown",
        }
    }
}

/// One battery, in watt-hours and watts whichever family the kernel reported it in.
#[derive(Clone, Copy, Debug)]
struct Cell {
    now: f64,
    full: f64,
    design: Option<f64>,
    watts: Option<f64>,
    status: Status,
    /// Where charging stops, when something has capped it below full.
    threshold: Option<f64>,
}

impl Cell {
    fn read(path: &Path) -> Option<Cell> {
        // A battery bay with nothing in it still has a directory.
        if number(path, "present").is_some_and(|p| p == 0.0) {
            return None;
        }

        let status = super::read_to_string(path.join("status"))
            .map(|s| Status::parse(&s))
            .unwrap_or(Status::Unknown);

        // The kernel writes microunits. The energy family is µWh and µW, so a millionth of
        // each is watt-hours and watts; the charge family is µAh and µA, which become the
        // same thing multiplied by the voltage in µV, a millionth of a millionth.
        const MICRO: f64 = 1e6;
        let (now, full, design, watts) = match number(path, "energy_now") {
            Some(now) => (
                now / MICRO,
                number(path, "energy_full")? / MICRO,
                number(path, "energy_full_design").map(|d| d / MICRO),
                number(path, "power_now").map(|p| p / MICRO),
            ),
            None => {
                let volts = number(path, "voltage_now")?;
                let watt_hours = |v: f64| v * volts / (MICRO * MICRO);
                (
                    watt_hours(number(path, "charge_now")?),
                    watt_hours(number(path, "charge_full")?),
                    number(path, "charge_full_design").map(watt_hours),
                    number(path, "current_now").map(watt_hours),
                )
            }
        };

        // A battery reporting a capacity of zero cannot be a share of anything.
        if full <= 0.0 {
            return None;
        }

        Some(Cell {
            now,
            full,
            design,
            // The sign is not consistent across drivers, and the direction is what `status`
            // is for, so only the magnitude is kept.
            watts: watts.map(f64::abs),
            status,
            threshold: threshold(path),
        })
    }
}

/// Every battery in the machine, added together.
#[derive(Clone, Copy, Debug)]
struct Total {
    now: f64,
    full: f64,
    design: Option<f64>,
    watts: Option<f64>,
    status: Status,
    threshold: Option<f64>,
}

impl Total {
    fn of(cells: &[Cell]) -> Option<Total> {
        let first = cells.first()?;
        let mut total = Total {
            now: 0.0,
            full: 0.0,
            design: Some(0.0),
            watts: None,
            status: first.status,
            threshold: first.threshold,
        };
        for cell in cells {
            total.now += cell.now;
            total.full += cell.full;
            // One battery that cannot say what it held when new makes the whole figure
            // unanswerable, rather than quietly reporting the health of the others.
            total.design = match (total.design, cell.design) {
                (Some(sum), Some(d)) => Some(sum + d),
                _ => None,
            };
            if let Some(w) = cell.watts {
                total.watts = Some(total.watts.unwrap_or(0.0) + w);
            }
            // The busiest battery decides what the machine is doing: one draining while
            // another sits full means the machine is draining.
            if cell.status.urgency() > total.status.urgency() {
                total.status = cell.status;
            }
        }
        (total.full > 0.0).then_some(total)
    }

    fn percent(&self) -> f64 {
        (self.now / self.full * 100.0).clamp(0.0, 100.0)
    }

    /// What is left at full against what the battery held when new.
    fn health(&self) -> Option<f64> {
        let design = self.design?;
        (design > 0.0).then(|| (self.full / design * 100.0).clamp(0.0, 100.0))
    }

    /// Until empty, or until full, at the rate it is going right now.
    ///
    /// Nothing to report when nothing is moving: a battery sitting at a charge threshold
    /// has no time until anything, and dividing by a rate of zero would say forever.
    fn remaining(&self) -> Option<Duration> {
        let watts = self.watts.filter(|w| *w > 0.0)?;
        let hours = match self.status {
            Status::Discharging => self.now / watts,
            Status::Charging => (self.full - self.now).max(0.0) / watts,
            _ => return None,
        };
        // A rate that has only just been sampled can be small enough to imply centuries.
        (hours.is_finite() && hours < 240.0).then(|| Duration::from_secs_f64(hours * 3600.0))
    }

    /// How the battery rates its own situation.
    ///
    /// Only a battery that is actually draining is worth alarming about: the same 10% with
    /// the charger in is fine, and a bar that shouts about it is one people learn to ignore.
    fn state(&self, percent: f64) -> State {
        match self.status {
            Status::Discharging if percent < URGENT => State::Critical,
            Status::Discharging if percent < LOW => State::Warning,
            Status::Discharging => State::Idle,
            Status::Charging => State::Info,
            Status::Full => State::Good,
            Status::NotCharging | Status::Unknown => State::Idle,
        }
    }
}

/// The percentage charging stops at, if something is holding it below full.
///
/// TLP and the ThinkPad firmware cap charging to spare the cells, which leaves a battery
/// sitting at 80% reporting "Not charging" for days. That is working as intended, so what
/// is worth reporting is the cap itself; a battery allowed to fill has nothing to say here,
/// and a format mentioning the threshold simply disappears on such a machine.
///
/// The two file names are the same setting under different kernel versions.
fn threshold(path: &Path) -> Option<f64> {
    let stop = number(path, "charge_control_end_threshold")
        .or_else(|| number(path, "charge_stop_threshold"))?;
    (stop > 0.0 && stop < 100.0).then_some(stop)
}

/// One of the kernel's integer files, or nothing if it is missing or unreadable.
fn number(path: &Path, name: &str) -> Option<f64> {
    super::read_to_string(path.join(name))
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `/sys/class/power_supply`-shaped directory.
    fn class(name: &str, devices: &[(&str, &[(&str, &str)])]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dbar-battery-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for (device, files) in devices {
            let dir = root.join(device);
            std::fs::create_dir_all(&dir).expect("the fixture directory is writable");
            for (file, value) in *files {
                std::fs::write(dir.join(file), format!("{value}\n")).expect("writable");
            }
        }
        root
    }

    /// A battery of the energy family, as this laptop reports one.
    fn energy(status: &'static str, now: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![
            ("type", "Battery"),
            ("present", "1"),
            ("status", status),
            ("energy_now", now),
            ("energy_full", "50000000"),
            ("energy_full_design", "54000000"),
            ("power_now", "10000000"),
        ]
    }

    fn read(root: &Path) -> Fields {
        Battery {
            class: root.to_path_buf(),
        }
        .read()
        .expect("a battery reading is never an error")
        .fields
    }

    fn num(fields: &Fields, name: &str) -> Option<f64> {
        fields.get(name)?.num()
    }

    #[test]
    fn charge_is_a_share_of_what_the_battery_holds_at_full() {
        let root = class("share", &[("BAT0", &energy("Discharging", "25000000"))]);
        let fields = read(&root);
        assert_eq!(num(&fields, "percent"), Some(50.0));
        // 50 Wh capacity against 54 Wh when new.
        assert!(matches!(num(&fields, "health"), Some(h) if (h - 92.59).abs() < 0.01));
    }

    #[test]
    fn the_charge_family_becomes_the_energy_family_through_the_voltage() {
        let root = class(
            "charge-family",
            &[(
                "BAT0",
                &[
                    ("type", "Battery"),
                    ("present", "1"),
                    ("status", "Discharging"),
                    ("charge_now", "2000000"),
                    ("charge_full", "4000000"),
                    ("current_now", "1000000"),
                    ("voltage_now", "12000000"),
                ],
            )],
        );
        let fields = read(&root);
        assert_eq!(num(&fields, "percent"), Some(50.0));
        // Two hours of charge left at the rate it is going.
        assert!(matches!(
            fields.get("time"),
            Some(Value::Dur(d)) if *d == Duration::from_secs(7200)
        ));
    }

    #[test]
    fn time_left_counts_down_when_discharging_and_up_when_charging() {
        let discharging = class("dis", &[("BAT0", &energy("Discharging", "25000000"))]);
        assert!(matches!(
            read(&discharging).get("time"),
            // 25 Wh left at 10 W.
            Some(Value::Dur(d)) if *d == Duration::from_secs(9000)
        ));

        let charging = class("chg", &[("BAT0", &energy("Charging", "25000000"))]);
        assert!(matches!(
            read(&charging).get("time"),
            // 25 Wh still to take on at 10 W.
            Some(Value::Dur(d)) if *d == Duration::from_secs(9000)
        ));
    }

    #[test]
    fn a_battery_that_is_not_moving_has_no_time_until_anything() {
        let mut files = energy("Not charging", "25000000");
        files.retain(|(f, _)| *f != "power_now");
        files.push(("power_now", "0"));
        let root = class("held", &[("BAT0", &files)]);
        assert!(matches!(read(&root).get("time"), Some(Value::Absent)));
    }

    #[test]
    fn several_batteries_are_one_number() {
        let root = class(
            "two",
            &[
                ("BAT0", &energy("Discharging", "25000000")),
                ("BAT1", &energy("Full", "50000000")),
            ],
        );
        let fields = read(&root);
        // 75 Wh of a 100 Wh machine.
        assert_eq!(num(&fields, "percent"), Some(75.0));
        // One cell draining is what the machine is doing, whatever the other says.
        assert!(matches!(
            fields.get("status"),
            Some(Value::Text(s)) if s == "discharging"
        ));
        // Both cells' draw, added.
        assert_eq!(num(&fields, "power"), Some(20.0));
    }

    #[test]
    fn the_busiest_battery_speaks_for_the_machine() {
        // Draining beats charging beats holding beats full beats unknown.
        let order = [
            Status::Unknown,
            Status::Full,
            Status::NotCharging,
            Status::Charging,
            Status::Discharging,
        ];
        for (i, quieter) in order.iter().enumerate() {
            for busier in &order[i + 1..] {
                assert!(
                    busier.urgency() > quieter.urgency(),
                    "{busier:?} should outrank {quieter:?}"
                );
            }
        }
    }

    #[test]
    fn a_machine_with_no_battery_reports_nothing_rather_than_failing() {
        let root = class("none", &[("AC", &[("type", "Mains"), ("online", "1")])]);
        let fields = read(&root);
        for name in ["percent", "status", "power", "time", "health"] {
            assert!(
                matches!(fields.get(name), Some(Value::Absent)),
                "${name} should have nothing to report"
            );
        }
    }

    #[test]
    fn an_empty_bay_is_not_a_flat_battery() {
        let root = class(
            "empty-bay",
            &[(
                "BAT1",
                &[("type", "Battery"), ("present", "0"), ("status", "Unknown")],
            )],
        );
        assert!(matches!(read(&root).get("percent"), Some(Value::Absent)));
    }

    #[test]
    fn only_a_draining_battery_is_worth_alarming_about() {
        let cases = [
            ("Discharging", 10.0, State::Critical),
            ("Discharging", 20.0, State::Warning),
            ("Discharging", 80.0, State::Idle),
            // The same charge with the charger in is not a problem.
            ("Charging", 10.0, State::Info),
            ("Full", 100.0, State::Good),
            ("Not charging", 60.0, State::Idle),
        ];
        for (status, percent, expected) in cases {
            let total = Total {
                now: percent,
                full: 100.0,
                design: None,
                watts: Some(10.0),
                status: Status::parse(status),
                threshold: None,
            };
            assert_eq!(total.state(percent), expected, "{status} at {percent}%");
        }
    }

    #[test]
    fn a_charge_cap_is_reported_only_when_something_is_capping() {
        let mut capped = energy("Not charging", "40000000");
        capped.push(("charge_control_end_threshold", "80"));
        let root = class("capped", &[("BAT0", &capped)]);
        assert_eq!(num(&read(&root), "threshold"), Some(80.0));

        // A battery allowed to fill has no cap worth mentioning.
        let mut open = energy("Charging", "40000000");
        open.push(("charge_control_end_threshold", "100"));
        let root = class("uncapped", &[("BAT0", &open)]);
        assert!(matches!(read(&root).get("threshold"), Some(Value::Absent)));
    }

    #[test]
    fn a_battery_held_at_its_threshold_is_not_a_problem() {
        let mut held = energy("Not charging", "40000000");
        held.push(("charge_control_end_threshold", "80"));
        let root = class("held-ok", &[("BAT0", &held)]);
        let fields = read(&root);
        assert!(matches!(
            fields.get("status"),
            Some(Value::Text(s)) if s == "not charging"
        ));
        // Sitting at a cap for days is working as intended, so there is nothing to alarm
        // about and nothing counting down.
        assert!(matches!(fields.get("time"), Some(Value::Absent)));
    }

    #[test]
    fn a_capacity_of_zero_is_not_a_battery() {
        let root = class(
            "zero",
            &[(
                "BAT0",
                &[
                    ("type", "Battery"),
                    ("present", "1"),
                    ("status", "Unknown"),
                    ("energy_now", "0"),
                    ("energy_full", "0"),
                ],
            )],
        );
        assert!(matches!(read(&root).get("percent"), Some(Value::Absent)));
    }
}
