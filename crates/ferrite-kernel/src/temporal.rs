//! Product-neutral S-100 time interval declarations. Selector evaluation is separate.
use crate::IntervalClosure;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalBounds {
    pub begin: Option<String>,
    pub end: Option<String>,
}
impl TemporalBounds {
    /// Bounds are already DEF-decoded by the portrayal adapter.
    pub fn new(begin: Option<String>, end: Option<String>) -> Result<Self> {
        if begin.as_ref().is_some_and(String::is_empty)
            || end.as_ref().is_some_and(String::is_empty)
        {
            bail!("empty temporal bound must be represented as None");
        }
        if begin.is_none() && end.is_none() {
            bail!("temporal interval requires a bound");
        }
        Ok(Self { begin, end })
    }
}

/// Date, Time, DateTime state consumed by one TimeValid command.
/// Raw ISO lexical values remain intact, including truncated dates and UTC offsets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalInterval {
    pub date: Option<TemporalBounds>,
    pub time: Option<TemporalBounds>,
    pub date_time: Option<TemporalBounds>,
    pub closure: IntervalClosure,
}
impl TemporalInterval {
    pub fn new(
        date: Option<TemporalBounds>,
        time: Option<TemporalBounds>,
        date_time: Option<TemporalBounds>,
        closure: IntervalClosure,
    ) -> Result<Self> {
        if date.is_none() && time.is_none() && date_time.is_none() {
            bail!("TimeValid requires preceding Date, Time or DateTime");
        }
        Ok(Self {
            date,
            time,
            date_time,
            closure,
        })
    }
}
