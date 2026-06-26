//! Project cadence parsing and evaluation.
//!
//! User-facing project schedules are cron strings. For simple repeats, auwsx
//! also accepts duration shorthand and normalizes it to cron-compatible text.

use anyhow::{anyhow, bail};
use chrono::{Datelike, Local, TimeZone, Timelike};

const MINUTE_MS: i64 = 60_000;
const MAX_SCAN_MINUTES: i64 = 366 * 24 * 60 * 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cadence {
    Manual,
    Tick,
    Cron(String),
}

impl Cadence {
    pub fn as_cron(&self) -> Option<&str> {
        match self {
            Cadence::Manual => None,
            Cadence::Tick => Some("@tick"),
            Cadence::Cron(expr) => Some(expr.as_str()),
        }
    }
}

pub fn normalize_cadence_input(input: &str) -> crate::Result<Option<String>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let lowered = trimmed.to_ascii_lowercase();
    match lowered.as_str() {
        "manual" | "@manual" | "none" | "off" | "disabled" => return Ok(None),
        "@tick" => return Ok(Some("@tick".to_string())),
        "@hourly" => return Ok(Some("0 * * * *".to_string())),
        "@daily" => return Ok(Some("0 0 * * *".to_string())),
        "@weekly" => return Ok(Some("0 0 * * 0".to_string())),
        _ => {}
    }
    if let Some(cadence) = duration_shorthand_to_cadence(&lowered)? {
        return Ok(Some(cadence));
    }
    if lowered.starts_with("@every ") {
        parse_every_ms(trimmed)?;
        return Ok(Some(lowered));
    }
    CronSpec::parse(trimmed)?;
    Ok(Some(trimmed.to_string()))
}

pub fn legacy_interval_to_cron(interval_min: Option<i64>) -> Option<String> {
    match interval_min {
        None => None,
        Some(min) if min <= 0 => Some("@tick".to_string()),
        Some(min) if min <= 59 => Some(format!("*/{min} * * * *")),
        Some(min) if min % (24 * 60) == 0 => Some(format!("0 0 */{} * *", min / (24 * 60))),
        Some(min) if min % 60 == 0 => Some(format!("0 */{} * * *", min / 60)),
        Some(min) => Some(format!("@every {min}m")),
    }
}

pub fn legacy_deepsleep_to_cron(interval_days: i64) -> Option<String> {
    match interval_days {
        days if days <= 0 => None,
        1 => Some("0 0 * * *".to_string()),
        7 => Some("0 0 * * 0".to_string()),
        days => Some(format!("0 0 */{days} * *")),
    }
}

pub fn cadence_label(schedule_cron: Option<&str>, legacy_interval_min: Option<i64>) -> String {
    let expr = schedule_cron
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| legacy_interval_to_cron(legacy_interval_min));
    match expr.as_deref() {
        None => "manual".to_string(),
        Some("@tick") => "tick".to_string(),
        Some(cron) => cron.to_string(),
    }
}

pub fn is_due(
    schedule_cron: Option<&str>,
    legacy_interval_min: Option<i64>,
    since_ms: Option<i64>,
    created_at_ms: i64,
    now_ms: i64,
    tick_secs: i64,
) -> crate::Result<bool> {
    let expr = schedule_cron
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| legacy_interval_to_cron(legacy_interval_min));
    let Some(expr) = expr else {
        return Ok(false);
    };
    let Some(expr) = normalize_cadence_input(&expr)? else {
        return Ok(false);
    };
    if expr == "@tick" {
        let _ = (since_ms, now_ms, tick_secs);
        return Ok(true);
    }
    if expr.starts_with("@every ") {
        let Some(since_ms) = since_ms else {
            return Ok(true);
        };
        return Ok(now_ms.saturating_sub(since_ms) >= parse_every_ms(&expr)?);
    }
    let spec = CronSpec::parse(&expr)?;
    let since = since_ms.unwrap_or(created_at_ms.saturating_sub(MINUTE_MS));
    Ok(spec
        .next_after(since)?
        .map(|next| next <= now_ms)
        .unwrap_or(false))
}

pub fn next_due_ms(
    schedule_cron: Option<&str>,
    legacy_interval_min: Option<i64>,
    since_ms: Option<i64>,
    created_at_ms: i64,
    now_ms: i64,
    tick_secs: i64,
) -> crate::Result<Option<i64>> {
    let expr = schedule_cron
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| legacy_interval_to_cron(legacy_interval_min));
    let Some(expr) = expr else {
        return Ok(None);
    };
    let Some(expr) = normalize_cadence_input(&expr)? else {
        return Ok(None);
    };
    if expr == "@tick" {
        return Ok(Some(match since_ms {
            Some(since) => since + tick_secs.max(1) * 1_000,
            None => now_ms,
        }));
    }
    if expr.starts_with("@every ") {
        let gap = parse_every_ms(&expr)?;
        return Ok(Some(match since_ms {
            Some(since) => since.saturating_add(gap),
            None => now_ms,
        }));
    }
    let spec = CronSpec::parse(&expr)?;
    let since = since_ms.unwrap_or(created_at_ms.saturating_sub(MINUTE_MS));
    spec.next_after(since)
}

fn duration_shorthand_to_cadence(input: &str) -> crate::Result<Option<String>> {
    let Some(unit) = input.chars().last() else {
        return Ok(None);
    };
    if !matches!(unit, 'm' | 'h' | 'd') {
        return Ok(None);
    }
    let amount = &input[..input.len().saturating_sub(1)];
    if amount.is_empty() || !amount.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(None);
    }
    let value: i64 = amount.parse()?;
    if value <= 0 {
        bail!("cadence shorthand must be positive");
    }
    let cron = match unit {
        'm' if value <= 59 => format!("*/{value} * * * *"),
        'm' if value % 60 == 0 => format!("0 */{} * * *", value / 60),
        'm' => format!("@every {value}m"),
        'h' if value <= 23 => format!("0 */{value} * * *"),
        'h' if value % 24 == 0 => format!("0 0 */{} * *", value / 24),
        'h' => format!("@every {value}h"),
        'd' => legacy_deepsleep_to_cron(value).ok_or_else(|| anyhow!("invalid day shorthand"))?,
        _ => unreachable!(),
    };
    Ok(Some(cron))
}

fn parse_every_ms(expr: &str) -> crate::Result<i64> {
    let raw = expr
        .trim()
        .strip_prefix("@every ")
        .ok_or_else(|| anyhow!("expected @every duration"))?;
    let Some(unit) = raw.chars().last() else {
        bail!("@every requires a duration like 90m, 2h, or 3d");
    };
    let amount = &raw[..raw.len().saturating_sub(1)];
    if amount.is_empty() || !amount.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("@every requires a duration like 90m, 2h, or 3d");
    }
    let value: i64 = amount.parse()?;
    if value <= 0 {
        bail!("@every duration must be positive");
    }
    let unit_ms = match unit {
        'm' => MINUTE_MS,
        'h' => 60 * MINUTE_MS,
        'd' => 24 * 60 * MINUTE_MS,
        _ => bail!("@every unit must be m, h, or d"),
    };
    Ok(value.saturating_mul(unit_ms))
}

#[derive(Debug, Clone)]
struct CronSpec {
    minutes: Field,
    hours: Field,
    dom: Field,
    months: Field,
    dow: Field,
}

impl CronSpec {
    fn parse(expr: &str) -> crate::Result<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            bail!("cron must have 5 fields, or use shorthand like 30m, 1h, 1d");
        }
        Ok(Self {
            minutes: Field::parse(parts[0], 0, 59, false)?,
            hours: Field::parse(parts[1], 0, 23, false)?,
            dom: Field::parse(parts[2], 1, 31, false)?,
            months: Field::parse(parts[3], 1, 12, false)?,
            dow: Field::parse(parts[4], 0, 7, true)?,
        })
    }

    fn next_after(&self, since_ms: i64) -> crate::Result<Option<i64>> {
        let mut candidate = floor_minute(since_ms).saturating_add(MINUTE_MS);
        for _ in 0..MAX_SCAN_MINUTES {
            let Some(dt) = Local.timestamp_millis_opt(candidate).single() else {
                candidate = candidate.saturating_add(MINUTE_MS);
                continue;
            };
            if self.matches(dt.minute(), dt.hour(), dt.day(), dt.month(), dt.weekday()) {
                return Ok(Some(candidate));
            }
            candidate = candidate.saturating_add(MINUTE_MS);
        }
        bail!("could not find next cron occurrence within scan window")
    }

    fn matches(
        &self,
        minute: u32,
        hour: u32,
        day: u32,
        month: u32,
        weekday: chrono::Weekday,
    ) -> bool {
        let dom_match = self.dom.matches(day);
        let dow_value = weekday.num_days_from_sunday();
        let dow_match = self.dow.matches(dow_value) || (dow_value == 0 && self.dow.matches(7));
        let day_match = if self.dom.any && self.dow.any {
            true
        } else if self.dom.any {
            dow_match
        } else if self.dow.any {
            dom_match
        } else {
            dom_match || dow_match
        };
        self.minutes.matches(minute)
            && self.hours.matches(hour)
            && self.months.matches(month)
            && day_match
    }
}

#[derive(Debug, Clone)]
struct Field {
    any: bool,
    values: Vec<u32>,
}

impl Field {
    fn parse(raw: &str, min: u32, max: u32, sunday_alias: bool) -> crate::Result<Self> {
        if raw == "*" {
            return Ok(Self {
                any: true,
                values: Vec::new(),
            });
        }
        let mut values = Vec::new();
        for part in raw.split(',') {
            if part.is_empty() {
                bail!("empty cron field component");
            }
            if let Some(step_raw) = part.strip_prefix("*/") {
                let step: u32 = step_raw.parse()?;
                if step == 0 {
                    bail!("cron step must be positive");
                }
                if step > max.saturating_sub(min).saturating_add(1) {
                    bail!("cron step exceeds field range");
                }
                let mut value = min;
                while value <= max {
                    values.push(value);
                    value = value.saturating_add(step);
                    if value == u32::MAX {
                        break;
                    }
                }
            } else if let Some((start_raw, end_raw)) = part.split_once('-') {
                let start = parse_field_value(start_raw, min, max, sunday_alias)?;
                let end = parse_field_value(end_raw, min, max, sunday_alias)?;
                if start > end {
                    bail!("cron range start must be <= end");
                }
                values.extend(start..=end);
            } else {
                values.push(parse_field_value(part, min, max, sunday_alias)?);
            }
        }
        values.sort_unstable();
        values.dedup();
        Ok(Self { any: false, values })
    }

    fn matches(&self, value: u32) -> bool {
        self.any || self.values.binary_search(&value).is_ok()
    }
}

fn parse_field_value(raw: &str, min: u32, max: u32, sunday_alias: bool) -> crate::Result<u32> {
    let value: u32 = raw.parse()?;
    if sunday_alias && value == 7 {
        return Ok(7);
    }
    if value < min || value > max {
        bail!("cron value {value} outside allowed range {min}-{max}");
    }
    Ok(value)
}

fn floor_minute(ms: i64) -> i64 {
    ms - ms.rem_euclid(MINUTE_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_duration_shorthand_when_normalized_then_cron() {
        assert_eq!(
            normalize_cadence_input("30m").unwrap(),
            Some("*/30 * * * *".to_string())
        );
        assert_eq!(
            normalize_cadence_input("1h").unwrap(),
            Some("0 */1 * * *".to_string())
        );
        assert_eq!(
            normalize_cadence_input("7d").unwrap(),
            Some("0 0 * * 0".to_string())
        );
        assert_eq!(
            normalize_cadence_input("90m").unwrap(),
            Some("@every 90m".to_string())
        );
    }

    #[test]
    fn given_manual_words_when_normalized_then_none() {
        assert_eq!(normalize_cadence_input("").unwrap(), None);
        assert_eq!(normalize_cadence_input("manual").unwrap(), None);
        assert_eq!(normalize_cadence_input("@manual").unwrap(), None);
    }

    #[test]
    fn given_cron_when_due_checked_then_uses_next_matching_minute() {
        let since = Local
            .with_ymd_and_hms(2026, 6, 25, 10, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let now = Local
            .with_ymd_and_hms(2026, 6, 25, 10, 30, 0)
            .single()
            .unwrap()
            .timestamp_millis();

        assert!(is_due(Some("*/30 * * * *"), None, Some(since), since, now, 10).unwrap());
    }
}
