//! Native calendar calculations; no background timers, database or provider calls.
use crate::{Error, Result};
use chrono::{Datelike, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
pub const MAX_TIME: u64 = 9_007_199_254_740_991;
fn invalid(message: &str) -> Error {
    Error::Input(message.into())
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schedule {
    At {
        unix_ms: u64,
    },
    Interval {
        every_ms: u64,
        #[serde(default)]
        anchor_unix_ms: Option<u64>,
    },
    Cron {
        expression: String,
        timezone: String,
        #[serde(default)]
        dom_and_dow: bool,
    },
}
fn integer(text: &str) -> Result<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("invalid cron number"));
    }
    text.parse().map_err(|_| invalid("invalid cron number"))
}
struct Field {
    values: Vec<u32>,
    restricted: bool,
}
fn field(text: &str, min: u32, max: u32, names: &[&str]) -> Result<Field> {
    let text = if text == "?" { "*" } else { text };
    let number = |text: &str| -> Result<u32> {
        if let Some(index) = names.iter().position(|n| n.eq_ignore_ascii_case(text)) {
            return Ok(min + index as u32);
        }
        integer(text)
    };
    let mut values = BTreeSet::new();
    let mut restricted = false;
    for item in text.split(',') {
        let parts = item.split('/').collect::<Vec<_>>();
        if parts.len() > 2 {
            return Err(invalid("invalid cron step"));
        }
        let step = parts.get(1).map(|v| integer(v)).transpose()?.unwrap_or(1);
        if step == 0 {
            return Err(invalid("cron step must be positive"));
        }
        let base = parts[0];
        let (low, high) = if base == "*" {
            restricted |= step > 1;
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            restricted = true;
            (number(a)?, number(b)?)
        } else {
            restricted = true;
            let n = number(base)?;
            (n, if parts.len() == 2 { max } else { n })
        };
        if low < min || high > max || low > high {
            return Err(invalid("cron field out of range"));
        }
        for value in low..=high {
            if (value - low).is_multiple_of(step) {
                values.insert(value);
            }
        }
    }
    Ok(Field {
        values: values.into_iter().collect(),
        restricted,
    })
}
struct Cron {
    second: Field,
    minute: Field,
    hour: Field,
    dom: Field,
    month: Field,
    dow: Field,
    tz: chrono_tz::Tz,
    and: bool,
}
impl Cron {
    fn parse(expression: &str, timezone: &str, and: bool) -> Result<Self> {
        if expression.len() > 256 {
            return Err(invalid("cron expression exceeds 256 bytes"));
        }
        let mut fields = expression.split_whitespace().collect::<Vec<_>>();
        if fields.len() == 5 {
            fields.insert(0, "0");
        }
        if fields.len() != 6 {
            return Err(invalid("cron requires five or six fields"));
        }
        let tz = timezone
            .parse()
            .map_err(|_| invalid("invalid IANA timezone"))?;
        let mut dow = field(
            fields[5],
            0,
            7,
            &["sun", "mon", "tue", "wed", "thu", "fri", "sat"],
        )?;
        dow.values = dow
            .values
            .into_iter()
            .map(|v| v % 7)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(Self {
            second: field(fields[0], 0, 59, &[])?,
            minute: field(fields[1], 0, 59, &[])?,
            hour: field(fields[2], 0, 23, &[])?,
            dom: field(fields[3], 1, 31, &[])?,
            month: field(
                fields[4],
                1,
                12,
                &[
                    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov",
                    "dec",
                ],
            )?,
            dow,
            tz,
            and,
        })
    }
    fn next(&self, after: i64) -> Result<Option<u64>> {
        let from = self
            .tz
            .timestamp_millis_opt(after)
            .single()
            .ok_or_else(|| invalid("timestamp exceeds calendar range"))?;
        let mut date = from.date_naive();
        for _ in 0..366 * 8 {
            let dom = self.dom.values.contains(&date.day());
            let dow = self
                .dow
                .values
                .contains(&date.weekday().num_days_from_sunday());
            let days = match (self.dom.restricted, self.dow.restricted) {
                (true, true) => {
                    if self.and {
                        dom && dow
                    } else {
                        dom || dow
                    }
                }
                (true, false) => dom,
                (false, true) => dow,
                _ => true,
            };
            if days && self.month.values.contains(&date.month()) {
                for hour in &self.hour.values {
                    for minute in &self.minute.values {
                        for second in &self.second.values {
                            // An ambiguous wall time is represented by its first instant only.
                            let local = date
                                .and_hms_opt(*hour, *minute, *second)
                                .expect("validated calendar fields");
                            if let Some(at) = self.tz.from_local_datetime(&local).earliest() {
                                let ms = at.timestamp_millis();
                                if ms >= 0 && ms > after && ms as u64 <= MAX_TIME {
                                    return Ok(Some(ms as u64));
                                }
                            }
                        }
                    }
                }
            }
            date = match date.succ_opt() {
                Some(next) => next,
                None => return Ok(None),
            };
        }
        Ok(None)
    }
}
impl Schedule {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::At { unix_ms } if *unix_ms <= MAX_TIME => Ok(()),
            Self::Interval {
                every_ms,
                anchor_unix_ms,
            } if (1..=31_536_000_000).contains(every_ms)
                && anchor_unix_ms.is_none_or(|v| v <= MAX_TIME) =>
            {
                Ok(())
            }
            Self::Cron {
                expression,
                timezone,
                dom_and_dow,
            } => {
                Cron::parse(expression, timezone, *dom_and_dow)?;
                Ok(())
            }
            _ => Err(invalid("invalid schedule or portable timestamp")),
        }
    }
    /// Inclusive calendar boundary, including Unix epoch zero.
    pub fn next_at_or_after(&self, at: u64) -> Result<Option<u64>> {
        self.validate()?;
        if at > MAX_TIME {
            return Err(invalid("observation exceeds portable timestamp range"));
        }
        match self {
            Self::At { unix_ms } => Ok((*unix_ms >= at).then_some(*unix_ms)),
            Self::Interval {
                every_ms,
                anchor_unix_ms,
            } => {
                let anchor = anchor_unix_ms.unwrap_or(at);
                let next = if anchor >= at {
                    Some(anchor)
                } else {
                    let steps = (at - anchor - 1) / every_ms + 1;
                    steps
                        .checked_mul(*every_ms)
                        .and_then(|delta| anchor.checked_add(delta))
                };
                Ok(next.filter(|v| *v <= MAX_TIME))
            }
            Self::Cron {
                expression,
                timezone,
                dom_and_dow,
            } => Cron::parse(expression, timezone, *dom_and_dow)?.next(at as i64 - 1),
        }
    }
    /// Strictly after `after`; interval anchors denote the first due instant.
    pub fn next_after(&self, after: u64) -> Result<Option<u64>> {
        self.validate()?;
        if after > MAX_TIME {
            return Err(invalid("observation exceeds portable timestamp range"));
        }
        match self {
            Self::At { unix_ms } => Ok((*unix_ms > after).then_some(*unix_ms)),
            Self::Interval {
                every_ms,
                anchor_unix_ms,
            } => {
                let anchor = anchor_unix_ms.unwrap_or(after);
                let next = if anchor > after {
                    Some(anchor)
                } else {
                    ((after - anchor) / every_ms)
                        .checked_add(1)
                        .and_then(|steps| steps.checked_mul(*every_ms))
                        .and_then(|delta| anchor.checked_add(delta))
                };
                Ok(next.filter(|v| *v <= MAX_TIME))
            }
            Self::Cron {
                expression,
                timezone,
                dom_and_dow,
            } => Cron::parse(expression, timezone, *dom_and_dow)?.next(after as i64),
        }
    }
}
