//! The current time.
//!
//! There is nothing to read and nothing that can fail: the value is the instant the tick
//! happened. What makes this a collector rather than a special case is that a module built
//! on it takes its wording from a format like any other, and the scheduler lands its
//! readings on the wall clock so a clock showing minutes changes when the minute does.

use anyhow::Result;

use super::{Collector, Reading};
use crate::status::{FieldSpec, Fields, Kind, State, Value};

pub const FIELDS: &[FieldSpec] = &[FieldSpec {
    name: "now",
    kind: Kind::Time,
}];

pub struct Time;

impl Collector for Time {
    fn read(&mut self) -> Result<Reading> {
        let mut fields = Fields::default();
        fields.set("now", Value::Time(std::time::SystemTime::now()));
        Ok(Reading {
            fields,
            state: State::Idle,
        })
    }
}
