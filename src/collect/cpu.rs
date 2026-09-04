//! Processor utilisation, from `/proc/stat`.
//!
//! The kernel counts time spent in each state since boot, so utilisation is a difference
//! between two readings rather than something that can be sampled once. The first reading
//! is taken when the collector opens, which means the first tick already has an interval
//! to report on.

use anyhow::{Context as _, Result, bail};

use super::{Collector, Reading};
use crate::status::{FieldSpec, Kind, State, Unit, Value};

pub const FIELDS: &[FieldSpec] = &[FieldSpec {
    name: "utilization",
    kind: Kind::Num(Unit::Percent),
}];

const STAT: &str = "/proc/stat";

pub struct Cpu {
    previous: Option<Times>,
}

/// The two totals utilisation is worked out from, in kernel ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Times {
    /// Every state the kernel counts, idle included.
    total: u64,
    /// Time the processor had nothing to do, which is idle plus waiting on io.
    idle: u64,
}

impl Cpu {
    pub fn new() -> Cpu {
        // Failing here is not worth reporting: the first tick will try again and say so.
        let previous = super::read_to_string(STAT)
            .ok()
            .and_then(|s| parse(&s).ok());
        Cpu { previous }
    }
}

impl Collector for Cpu {
    fn read(&mut self) -> Result<Reading> {
        let now = parse(&super::read_to_string(STAT)?)?;
        let previous = self.previous.replace(now);

        let mut fields = crate::status::Fields::default();
        // Without a previous sample there is no interval to average over, so the field has
        // nothing to report rather than a number that would be wrong.
        let value = previous.and_then(|previous| utilization(previous, now));
        fields.set(
            "utilization",
            match value {
                Some(v) => Value::Num {
                    v,
                    unit: Unit::Percent,
                },
                None => Value::Absent,
            },
        );
        fields.set_primary("utilization");
        Ok(Reading {
            fields,
            state: State::Idle,
        })
    }
}

/// The share of the interval that was not idle, as a percentage.
///
/// Returns nothing when the counters did not move, which happens if two readings land in
/// the same tick, and when they went backwards, which means the counters were reset.
fn utilization(previous: Times, now: Times) -> Option<f64> {
    let total = now.total.checked_sub(previous.total)?;
    let idle = now.idle.checked_sub(previous.idle)?;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle);
    Some(busy as f64 * 100.0 / total as f64)
}

/// Read the aggregate `cpu` line, which sums every core.
///
/// The fields are user, nice, system, idle, iowait, irq, softirq, steal, and two more the
/// kernel counts for guests. Only the total and the idle part matter here.
fn parse(text: &str) -> Result<Times> {
    let line = text
        .lines()
        .find(|l| l.starts_with("cpu "))
        .context("/proc/stat has no aggregate cpu line")?;

    let mut total = 0u64;
    let mut idle = 0u64;
    let mut seen = 0;
    for (i, field) in line.split_whitespace().skip(1).enumerate() {
        let value: u64 = field
            .parse()
            .with_context(|| format!("field {i} of the cpu line is not a number: {field:?}"))?;
        total += value;
        // Fields 3 and 4 are idle and iowait: both are time with nothing to do.
        if i == 3 || i == 4 {
            idle += value;
        }
        seen += 1;
    }
    if seen < 4 {
        bail!("the cpu line in /proc/stat has only {seen} field(s)");
    }
    Ok(Times { total, idle })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
cpu  10 20 30 40 50 60 70 80 0 0
cpu0 5 10 15 20 25 30 35 40 0 0
intr 12345
";

    #[test]
    fn the_aggregate_line_is_the_one_read() {
        let times = parse(SAMPLE).expect("the sample parses");
        assert_eq!(times.total, 10 + 20 + 30 + 40 + 50 + 60 + 70 + 80);
        // Idle plus iowait.
        assert_eq!(times.idle, 40 + 50);
    }

    #[test]
    fn utilisation_is_the_share_of_the_interval_that_was_busy() {
        let previous = Times {
            total: 100,
            idle: 100,
        };
        let now = Times {
            total: 200,
            idle: 175,
        };
        // 100 ticks passed, 75 of them idle.
        assert_eq!(utilization(previous, now), Some(25.0));
    }

    #[test]
    fn nothing_is_reported_when_the_counters_did_not_move() {
        let same = Times {
            total: 100,
            idle: 50,
        };
        assert_eq!(utilization(same, same), None);
    }

    #[test]
    fn counters_going_backwards_report_nothing_rather_than_nonsense() {
        let previous = Times {
            total: 200,
            idle: 100,
        };
        let now = Times {
            total: 100,
            idle: 50,
        };
        assert_eq!(utilization(previous, now), None);
    }

    #[test]
    fn a_stat_file_without_a_cpu_line_is_an_error() {
        assert!(parse("intr 1\nctxt 2\n").is_err());
        assert!(parse("cpu  10 20\n").is_err());
        assert!(parse("cpu  a b c d\n").is_err());
    }
}
