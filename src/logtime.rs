//! Log timestamps in the zone the config asks for.
//!
//! tracing prints UTC by default, which makes a log hard to line up with the operator's
//! clock. The `time_zone` config key picks local time, UTC or a fixed offset, and every
//! binary hands [`Stamp`] to its subscriber as the timer.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, FixedOffset, Local, SecondsFormat, Utc};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

use crate::config::NetworkConfig;
use crate::error::Error;

/// Config value that selects the OS local zone.
const LOCAL: &str = "local";
/// Config value that selects UTC.
const UTC: &str = "utc";
/// Seconds in an hour, for turning an offset into seconds.
const HOUR: i32 = 3600;
/// Seconds in a minute, for turning an offset into seconds.
const MINUTE: i32 = 60;
/// Length of a fixed offset such as `+06:00`.
const OFFSET_LEN: usize = 6;

/// The zone log timestamps are written in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimeZone {
  /// The OS local zone, read at each line so a daylight change is followed.
  #[default]
  Local,
  /// Coordinated universal time.
  Utc,
  /// A fixed offset from UTC.
  Fixed(FixedOffset),
}

/// Parses `+HH:MM` or `-HH:MM` into an offset, or `None` when it is malformed or out of
/// range.
fn offset(text: &str) -> Option<FixedOffset> {
  let bytes = text.as_bytes();
  if bytes.len() != OFFSET_LEN || bytes[3] != b':' {
    return None;
  }
  let sign = match bytes[0] {
    b'+' => 1,
    b'-' => -1,
    _ => return None,
  };
  let digits = |range: std::ops::Range<usize>| -> Option<i32> {
    let part = &text[range];
    if part.bytes().all(|b| b.is_ascii_digit()) {
      part.parse().ok()
    } else {
      None
    }
  };
  let hours = digits(1..3)?;
  let minutes = digits(4..6)?;
  if minutes >= 60 {
    return None;
  }
  FixedOffset::east_opt(sign * (hours * HOUR + minutes * MINUTE))
}

impl FromStr for TimeZone {
  type Err = Error;

  /// Accepts `local`, `utc` or a `+HH:MM` / `-HH:MM` offset; the error names the key and
  /// the rejected value and nothing else.
  fn from_str(text: &str) -> Result<Self, Error> {
    match text {
      LOCAL => Ok(Self::Local),
      UTC => Ok(Self::Utc),
      _ => offset(text)
        .map(Self::Fixed)
        .ok_or_else(|| Error::Config(format!("time_zone: {text:?}"))),
    }
  }
}

/// Formats `instant` in `zone` as RFC 3339 with microseconds, the shape tracing prints.
pub fn format_instant(zone: TimeZone, instant: DateTime<Utc>) -> String {
  match zone {
    TimeZone::Local => instant
      .with_timezone(&Local)
      .to_rfc3339_opts(SecondsFormat::Micros, false),
    TimeZone::Utc => instant.to_rfc3339_opts(SecondsFormat::Micros, true),
    TimeZone::Fixed(fixed) => instant
      .with_timezone(&fixed)
      .to_rfc3339_opts(SecondsFormat::Micros, false),
  }
}

/// The zone of `network`'s config, or local time when the config cannot be read, so a bad
/// config still logs and the failure is reported by the load that follows.
pub fn zone_of(network: &str) -> TimeZone {
  NetworkConfig::load(network)
    .map(|cfg| cfg.time_zone)
    .unwrap_or_default()
}

/// A tracing timer that stamps each line in one [`TimeZone`].
#[derive(Clone, Copy, Debug)]
pub struct Stamp(pub TimeZone);

impl FormatTime for Stamp {
  /// Writes the current instant in the configured zone.
  fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
    w.write_str(&format_instant(self.0, Utc::now()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::TimeZone as _;

  /// All three forms parse.
  #[test]
  fn parses_every_form() {
    assert_eq!("local".parse::<TimeZone>().unwrap(), TimeZone::Local);
    assert_eq!("utc".parse::<TimeZone>().unwrap(), TimeZone::Utc);
    let plus: TimeZone = "+06:00".parse().unwrap();
    assert_eq!(plus, TimeZone::Fixed(FixedOffset::east_opt(6 * HOUR).unwrap()));
    let minus: TimeZone = "-03:30".parse().unwrap();
    assert_eq!(
      minus,
      TimeZone::Fixed(FixedOffset::west_opt(3 * HOUR + 30 * MINUTE).unwrap())
    );
  }

  /// Bad values are rejected with only the key and the value in the message.
  #[test]
  fn rejects_bad_values() {
    for bad in ["bogus", "+6:00", "0600", "+06:60", "+25:00", "+0a:00", ""] {
      let err = bad.parse::<TimeZone>().unwrap_err();
      assert_eq!(err.to_string(), format!("config: time_zone: {bad:?}"));
    }
  }

  /// The timer writes a fixed instant in the requested offset.
  #[test]
  fn formats_in_fixed_offset() {
    let instant = Utc.with_ymd_and_hms(2026, 11, 7, 18, 30, 5).unwrap();
    let zone: TimeZone = "+06:00".parse().unwrap();
    assert_eq!(
      format_instant(zone, instant),
      "2026-11-08T00:30:05.000000+06:00"
    );
    assert_eq!(
      format_instant(TimeZone::Utc, instant),
      "2026-11-07T18:30:05.000000Z"
    );
  }
}
