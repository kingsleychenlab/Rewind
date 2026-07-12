//! Small shared helpers: timestamps and human-readable formatting.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Current time as Unix epoch milliseconds.
pub fn now_millis() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as i64
}

/// Format epoch milliseconds as an RFC 3339 (UTC) timestamp. Falls back to the
/// raw number if the value is out of range.
pub fn format_timestamp(millis: i64) -> String {
    let nanos = (millis as i128) * 1_000_000;
    match OffsetDateTime::from_unix_timestamp_nanos(nanos) {
        Ok(dt) => dt.format(&Rfc3339).unwrap_or_else(|_| millis.to_string()),
        Err(_) => millis.to_string(),
    }
}

/// Format a short local-ish time (HH:MM:SS in UTC) for compact timeline rows.
pub fn format_clock(millis: i64) -> String {
    let nanos = (millis as i128) * 1_000_000;
    match OffsetDateTime::from_unix_timestamp_nanos(nanos) {
        Ok(dt) => format!("{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second()),
        Err(_) => "--:--:--".to_string(),
    }
}

/// Format a duration given in milliseconds compactly (e.g. `1.20s`, `340ms`).
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m{s:02}s")
    }
}

/// Truncate a string to `max` display columns, appending an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return s.chars().take(max).collect();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(1500), "1.50s");
        assert_eq!(format_duration_ms(65_000), "1m05s");
    }

    #[test]
    fn timestamp_is_rfc3339() {
        // 2021-01-01T00:00:00Z == 1609459200000 ms
        let s = format_timestamp(1_609_459_200_000);
        assert!(s.starts_with("2021-01-01T00:00:00"));
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
    }
}
