//! Memory in use, from `/proc/meminfo`.
//!
//! "Used" is total minus available, not total minus free. Free memory on a healthy Linux
//! machine is close to zero because the kernel keeps caches in whatever is spare; available
//! is the kernel's own estimate of what a new process could actually get, which is the
//! number a person means when they ask how much memory is left.

use anyhow::{Context as _, Result};

use super::{Collector, Reading};
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value};

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "percent",
        kind: Kind::Num(Unit::Percent),
    },
    FieldSpec {
        name: "used",
        kind: Kind::Num(Unit::Bytes),
    },
    FieldSpec {
        name: "total",
        kind: Kind::Num(Unit::Bytes),
    },
    FieldSpec {
        name: "available",
        kind: Kind::Num(Unit::Bytes),
    },
    FieldSpec {
        name: "swap_percent",
        kind: Kind::Num(Unit::Percent),
    },
    FieldSpec {
        name: "swap_used",
        kind: Kind::Num(Unit::Bytes),
    },
    FieldSpec {
        name: "swap_total",
        kind: Kind::Num(Unit::Bytes),
    },
];

const MEMINFO: &str = "/proc/meminfo";

pub struct Memory;

impl Collector for Memory {
    fn read(&mut self) -> Result<Reading> {
        let info = parse(&super::read_to_string(MEMINFO)?)?;

        let mut fields = Fields::default();
        fields.set("percent", percent(info.used(), Some(info.total)));
        fields.set("used", bytes(Some(info.used())));
        fields.set("total", bytes(Some(info.total)));
        fields.set("available", bytes(Some(info.available)));
        fields.set("swap_percent", percent(info.swap_used(), info.swap_total));
        fields.set(
            "swap_used",
            bytes(info.swap_total.map(|_| info.swap_used())),
        );
        fields.set("swap_total", bytes(info.swap_total));
        fields.set_primary("percent");

        Ok(Reading {
            fields,
            state: State::Idle,
        })
    }
}

fn bytes(v: Option<u64>) -> Value {
    match v {
        Some(v) => Value::Num {
            v: v as f64,
            unit: Unit::Bytes,
        },
        None => Value::Absent,
    }
}

/// A share of a total, or nothing when there is no total to be a share of.
///
/// A machine with no swap has no swap percentage - not zero percent - so a format that
/// mentions it can disappear rather than claim the swap is empty.
fn percent(used: u64, total: Option<u64>) -> Value {
    match total {
        Some(total) if total > 0 => Value::Num {
            v: used as f64 * 100.0 / total as f64,
            unit: Unit::Percent,
        },
        _ => Value::Absent,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Info {
    total: u64,
    available: u64,
    /// Absent on a machine built without swap, which is not the same as swap being empty.
    swap_total: Option<u64>,
    swap_free: u64,
}

impl Info {
    fn used(self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    fn swap_used(self) -> u64 {
        self.swap_total.unwrap_or(0).saturating_sub(self.swap_free)
    }
}

/// Read the handful of lines that matter, each `Name:  <value> kB`.
fn parse(text: &str) -> Result<Info> {
    let mut total = None;
    let mut available = None;
    let mut free = None;
    let mut swap_total = None;
    let mut swap_free = None;

    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let slot = match name {
            "MemTotal" => &mut total,
            "MemAvailable" => &mut available,
            "MemFree" => &mut free,
            "SwapTotal" => &mut swap_total,
            "SwapFree" => &mut swap_free,
            _ => continue,
        };
        *slot = Some(kilobytes(rest).with_context(|| format!("in the {name} line"))?);
    }

    let total = total.context("/proc/meminfo has no MemTotal line")?;
    Ok(Info {
        total,
        // MemAvailable arrived in Linux 3.14. On anything older, free is the closest
        // honest answer rather than a refusal to report at all.
        available: available.or(free).unwrap_or(0),
        swap_total: swap_total.filter(|&t| t > 0),
        swap_free: swap_free.unwrap_or(0),
    })
}

/// The value of a meminfo line, in bytes.
///
/// Every size the kernel writes there is in kibibytes and says so; a line without the unit
/// is a count rather than a size, and none of those are read here.
fn kilobytes(rest: &str) -> Result<u64> {
    let mut parts = rest.split_whitespace();
    let value: u64 = parts
        .next()
        .context("no value")?
        .parse()
        .context("the value is not a number")?;
    match parts.next() {
        Some("kB") | Some("KB") => Ok(value * 1024),
        Some(other) => anyhow::bail!("unexpected unit {other:?}"),
        None => Ok(value * 1024),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
MemTotal:       16000000 kB
MemFree:          500000 kB
MemAvailable:    8000000 kB
Buffers:          200000 kB
Cached:          6000000 kB
SwapTotal:       4000000 kB
SwapFree:        3000000 kB
";

    #[test]
    fn used_is_total_less_available_not_total_less_free() {
        let info = parse(SAMPLE).expect("the sample parses");
        assert_eq!(info.total, 16_000_000 * 1024);
        assert_eq!(info.available, 8_000_000 * 1024);
        assert_eq!(info.used(), 8_000_000 * 1024);
        assert_eq!(info.swap_used(), 1_000_000 * 1024);
    }

    #[test]
    fn a_machine_without_swap_reports_no_swap_rather_than_an_empty_one() {
        let text = "MemTotal: 100 kB\nMemAvailable: 50 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
        let info = parse(text).expect("the sample parses");
        assert_eq!(info.swap_total, None);
        assert!(matches!(
            percent(info.swap_used(), info.swap_total),
            Value::Absent
        ));
    }

    #[test]
    fn free_stands_in_when_the_kernel_is_too_old_for_available() {
        let text = "MemTotal: 100 kB\nMemFree: 40 kB\n";
        let info = parse(text).expect("the sample parses");
        assert_eq!(info.available, 40 * 1024);
    }

    #[test]
    fn a_percentage_is_a_share_of_the_total() {
        let info = parse(SAMPLE).expect("the sample parses");
        assert!(matches!(
            percent(info.used(), Some(info.total)),
            Value::Num { v, .. } if (v - 50.0).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn a_meminfo_without_a_total_is_an_error() {
        assert!(parse("MemFree: 40 kB\n").is_err());
        assert!(parse("MemTotal: lots kB\n").is_err());
        assert!(parse("MemTotal: 10 MB\n").is_err());
    }
}
