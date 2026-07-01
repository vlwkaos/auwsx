//! Pure schedule/countdown formatting shared by the project tree and the
//! project-detail panel. No clock access here — callers pass `now_ms` so these
//! stay deterministic and unit-testable.

use chrono::{Local, TimeZone};

/// Compact human duration for a NON-negative millisecond span.
/// `<60s` → "{s}s"; `<1h` → "{m}m {s}s" (drop seconds if 0 → "{m}m");
/// `>=1h` → "{h}h {m}m" (drop minutes if 0 → "{h}h").
pub fn fmt_duration_ms(ms: i64) -> String {
    if ms < 60_000 {
        format!("{}s", ms / 1_000)
    } else if ms < 3_600_000 {
        let m = ms / 60_000;
        let s = (ms % 60_000) / 1_000;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    } else {
        let h = ms / 3_600_000;
        let m = (ms % 3_600_000) / 60_000;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    }
}

/// Full "next auto" label for the detail panel.
pub fn next_due_label(
    schedule_cron: Option<&str>,
    last_auto_fired_at: Option<i64>,
    created_at_ms: i64,
    now_ms: i64,
    tick_interval_secs: i64,
) -> String {
    match auwsx_core::schedule::next_due_ms(
        schedule_cron,
        last_auto_fired_at,
        created_at_ms,
        now_ms,
        tick_interval_secs,
    ) {
        Ok(None) => "manual only".to_string(),
        Ok(Some(next_ms)) => match countdown_remaining(next_ms.saturating_sub(now_ms)) {
            CountdownRemaining::Future(dur) => format!("in {dur}"),
            CountdownRemaining::DueNow => "due now".to_string(),
            CountdownRemaining::Overdue(dur) => format!("overdue {dur}"),
        },
        Err(_) => "invalid schedule".to_string(),
    }
}

pub fn schedule_due_for_tree(
    schedule_cron: Option<&str>,
    last_auto_fired_at: Option<i64>,
    created_at_ms: i64,
    now_ms: i64,
    tick_interval_secs: i64,
) -> Option<String> {
    match auwsx_core::schedule::next_due_ms(
        schedule_cron,
        last_auto_fired_at,
        created_at_ms,
        now_ms,
        tick_interval_secs,
    ) {
        Ok(None) => None,
        Ok(Some(next_ms)) => Some(match countdown_remaining(next_ms.saturating_sub(now_ms)) {
            CountdownRemaining::Future(dur) => dur,
            CountdownRemaining::DueNow => "due".to_string(),
            CountdownRemaining::Overdue(dur) => format!("overdue {dur}"),
        }),
        Err(_) => Some("invalid".to_string()),
    }
}

pub fn tree_schedule_label(
    schedule_cron: Option<&str>,
    last_auto_fired_at: Option<i64>,
    created_at_ms: i64,
    now_ms: i64,
    tick_interval_secs: i64,
) -> String {
    let cadence = interval_label(schedule_cron, tick_interval_secs);
    match schedule_due_for_tree(
        schedule_cron,
        last_auto_fired_at,
        created_at_ms,
        now_ms,
        tick_interval_secs,
    ) {
        Some(remaining) => format!("\u{23f1} {remaining}/{cadence}"),
        None => "manual".to_string(),
    }
}

pub fn interval_label(schedule_cron: Option<&str>, tick_interval_secs: i64) -> String {
    let label = auwsx_core::schedule::cadence_label(schedule_cron);
    compact_cadence_label(&label, tick_interval_secs)
}

fn compact_cadence_label(label: &str, tick_interval_secs: i64) -> String {
    if label == "tick" {
        return format!("{}s", tick_interval_secs.max(1));
    }
    if let Some(every) = label.strip_prefix("@every ") {
        return every.to_string();
    }
    if let Some(min) = label
        .strip_prefix("*/")
        .and_then(|rest| rest.strip_suffix(" * * * *"))
    {
        return format!("{min}m");
    }
    if let Some(hour) = label
        .strip_prefix("0 */")
        .and_then(|rest| rest.strip_suffix(" * * *"))
    {
        return format!("{hour}h");
    }
    if let Some(day) = label
        .strip_prefix("0 0 */")
        .and_then(|rest| rest.strip_suffix(" * *"))
    {
        return format!("{day}d");
    }
    match label {
        "0 * * * *" => "1h".to_string(),
        "0 0 * * *" => "1d".to_string(),
        "0 0 * * 0" => "7d".to_string(),
        _ => label.to_string(),
    }
}

enum CountdownRemaining {
    Future(String),
    DueNow,
    Overdue(String),
}

/// Classify a signed millisecond remainder into one of three display states.
/// The -1000 ms window absorbs minor scheduler jitter so a tick that fired
/// slightly late still shows "due" rather than "overdue".
fn countdown_remaining(remaining_ms: i64) -> CountdownRemaining {
    if remaining_ms > 0 {
        CountdownRemaining::Future(fmt_duration_ms(remaining_ms))
    } else if remaining_ms > -1000 {
        CountdownRemaining::DueNow
    } else {
        CountdownRemaining::Overdue(fmt_duration_ms(-remaining_ms))
    }
}

/// Human-readable local timestamp for daemon-observability rows stored as epoch ms.
pub fn format_epoch_ms_local(epoch_ms: i64) -> String {
    match Local.timestamp_millis_opt(epoch_ms).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
        None => format!("{epoch_ms} ms"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_valid_epoch_ms_when_format_local_then_readable_date() {
        let formatted = format_epoch_ms_local(0);

        assert!(formatted.contains("1970"));
        assert_ne!(formatted, "0 ms");
    }

    #[test]
    fn given_auto_schedule_when_tree_label_then_countdown_and_cadence_are_joined() {
        let label = tree_schedule_label(Some("@every 30m"), Some(0), 0, 15 * 60_000, 10);

        assert_eq!(label, "\u{23f1} 15m/30m");
    }

    #[test]
    fn given_manual_schedule_when_tree_label_then_manual() {
        let label = tree_schedule_label(Some("@manual"), None, 0, 0, 10);

        assert_eq!(label, "manual");
    }
}
