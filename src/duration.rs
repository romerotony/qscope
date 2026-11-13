//! Normalize scheduler durations without confusing elapsed time with timestamps.

pub fn seconds(text: &str) -> Option<u64> {
  let text = text.trim();
  if let Some(seconds) = text.strip_suffix('s') {
    let value: f64 = seconds.parse().ok()?;
    return (value.is_finite() && value >= 0.0 && value < u64::MAX as f64).then_some(value as u64);
  }
  let (days, clock) = match text.split_once('-') {
    Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
    None => (0, text),
  };
  let parts = clock
    .split(':')
    .map(str::parse::<u64>)
    .collect::<Result<Vec<_>, _>>()
    .ok()?;
  let elapsed = match parts.as_slice() {
    [seconds] if days == 0 => *seconds,
    [minutes, seconds] if *seconds < 60 => minutes.checked_mul(60)?.checked_add(*seconds)?,
    [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => hours
      .checked_mul(3600)?
      .checked_add(minutes.checked_mul(60)?)?
      .checked_add(*seconds)?,
    _ => return None,
  };
  days.checked_mul(86400)?.checked_add(elapsed)
}

pub fn clock(seconds: u64) -> String {
  let days = seconds / 86400;
  let hours = seconds / 3600 % 24;
  let minutes = seconds / 60 % 60;
  let secs = seconds % 60;
  if days > 0 {
    format!("{days}-{hours:02}:{minutes:02}:{secs:02}")
  } else if hours > 0 {
    format!("{hours:02}:{minutes:02}:{secs:02}")
  } else {
    format!("{minutes:02}:{secs:02}")
  }
}

pub fn elapsed(text: &str, state: &str) -> String {
  if matches!(
    state,
    "PENDING" | "DEPEND" | "PRIORITY" | "SCHED" | "CONFIGURING" | "NEW"
  ) {
    return "—".into();
  }
  seconds(text).map(clock).unwrap_or_else(|| text.into())
}

/// Interpolate running clocks locally between scheduler snapshots. Pending,
/// suspended, and terminal states must not accumulate additional runtime.
pub fn elapsed_since(text: &str, state: &str, additional_seconds: u64) -> String {
  if matches!(state, "RUN" | "RUNNING")
    && let Some(seconds) = seconds(text)
  {
    clock(seconds.saturating_add(additional_seconds))
  } else {
    elapsed(text, state)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn local_elapsed_ticks_only_for_running_jobs() {
    assert_eq!(elapsed_since("120s", "RUNNING", 10), "02:10");
    assert_eq!(elapsed_since("00:02:00", "RUN", 10), "02:10");
    assert_eq!(elapsed_since("120s", "COMPLETED", 10), "02:00");
    assert_eq!(elapsed_since("120s", "SUSPENDED", 10), "02:00");
    assert_eq!(elapsed_since("0:00", "PENDING", 10), "—");
    assert_eq!(elapsed_since("—", "RUN", 10), "—");
  }
  #[test]
  fn scheduler_times_become_clocks() {
    for (input, expected) in [
      ("45s", "00:45"),
      ("750s", "12:30"),
      ("8100s", "02:15:00"),
      ("1-04:05:06", "1-04:05:06"),
      ("0:05", "00:05"),
    ] {
      assert_eq!(elapsed(input, "RUNNING"), expected);
    }
    assert_eq!(elapsed("0:00", "PENDING"), "—");
    assert_eq!(elapsed("—", "RUN"), "—");
  }
  #[test]
  fn rejects_invalid_durations() {
    for value in ["Unlimited", "UNLIMITED", "N/A", "—", "NaNs", "-1s"] {
      assert_eq!(seconds(value), None);
    }
    assert!(seconds("18446744073709551615-00:00:00").is_none());
    assert!(seconds("10:99").is_none());
  }
}
