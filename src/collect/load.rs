//! Run-queue length, from `/proc/loadavg`.
//!
//! The three numbers are how many processes were runnable on average over the last one,
//! five and fifteen minutes. They are counts rather than percentages, so what "busy" means
//! depends on how many cores the machine has: 4.0 is saturation on a quad-core and idling
//! on a sixteen. Both the raw figures and a share of the cores are published, so a config
//! can key on whichever it means.

use anyhow::{Context as _, Result};

use super::{Collector, Reading};
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value};

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "one",
        kind: Kind::Num(Unit::None),
    },
    FieldSpec {
        name: "five",
        kind: Kind::Num(Unit::None),
    },
    FieldSpec {
        name: "fifteen",
        kind: Kind::Num(Unit::None),
    },
    // The one-minute figure against the number of cores, so a threshold means the same
    // thing on every machine.
    FieldSpec {
        name: "percent",
        kind: Kind::Num(Unit::Percent),
    },
];

const LOADAVG: &str = "/proc/loadavg";

pub struct Load;

impl Collector for Load {
    fn read(&mut self) -> Result<Reading> {
        let [one, five, fifteen] = parse(&super::read_to_string(LOADAVG)?)?;

        let mut fields = Fields::default();
        for (name, v) in [("one", one), ("five", five), ("fifteen", fifteen)] {
            fields.set(
                name,
                Value::Num {
                    v,
                    unit: Unit::None,
                },
            );
        }
        let cores = cores();
        fields.set(
            "percent",
            Value::Num {
                v: one / cores as f64 * 100.0,
                unit: Unit::Percent,
            },
        );
        // The raw one-minute figure is what a load module is about; the share is there for
        // a threshold that wants to mean the same thing on every machine.
        fields.set_primary("one");

        Ok(Reading {
            fields,
            state: State::Idle,
        })
    }
}

/// How many processors the machine has, and never zero.
fn cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// The three averages. The rest of the line is the running/total process count and the
/// last pid, neither of which a bar has any use for.
fn parse(text: &str) -> Result<[f64; 3]> {
    let mut out = [0.0; 3];
    let mut fields = text.split_whitespace();
    for (i, slot) in out.iter_mut().enumerate() {
        let field = fields
            .next()
            .with_context(|| format!("/proc/loadavg has no field {i}"))?;
        *slot = field
            .parse()
            .with_context(|| format!("load average {field:?} is not a number"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_averages_are_read_and_the_rest_ignored() {
        let load = parse("0.63 1.17 1.41 1/4464 160973\n").expect("the sample parses");
        assert_eq!(load, [0.63, 1.17, 1.41]);
    }

    #[test]
    fn a_line_that_is_not_three_numbers_is_an_error() {
        assert!(parse("0.63 1.17\n").is_err());
        assert!(parse("busy quiet idle\n").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn the_machine_always_has_at_least_one_core_to_divide_by() {
        assert!(cores() >= 1);
    }
}
