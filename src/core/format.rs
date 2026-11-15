//! Display formatting shared by every view (byte counts, dates, durations).
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};

fn scaled(bytes: i64, base: f64) -> String {
    let neg = bytes < 0;
    let b = bytes.unsigned_abs() as f64;
    let s = if bytes == 0 {
        "Zero KB".to_string()
    } else if b < base {
        format!("{} {}", b as u64, if b as u64 == 1 { "byte" } else { "bytes" })
    } else if b < base * base {
        format!("{:.0} KB", b / base)
    } else if b < base * base * base {
        format!("{:.1} MB", b / (base * base))
    } else if b < base * base * base * base {
        format!("{:.2} GB", b / (base * base * base))
    } else {
        format!("{:.2} TB", b / (base * base * base * base))
    };
    if neg { format!("-{s}") } else { s }
}

/// 1024-based with KB/MB/GB labels (ByteCountFormatter `.binary`).
pub fn bytes_binary(bytes: i64) -> String {
    scaled(bytes, 1024.0)
}

/// 1000-based (ByteCountFormatter `.file` / `.decimal`).
pub fn bytes_file(bytes: i64) -> String {
    scaled(bytes, 1000.0)
}

/// "17 Oct 2026 at 00:20" (DateFormatter medium date + short time).
pub fn medium_date_short_time<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    dt.with_timezone(&Local).format("%-d %b %Y at %H:%M").to_string()
}

/// "17/10/2026, 00:17" (numeric date + short time).
pub fn short_date_time<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    dt.with_timezone(&Local).format("%d/%m/%Y, %H:%M").to_string()
}

pub fn short_time<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    dt.with_timezone(&Local).format("%H:%M").to_string()
}

/// Parses the naive local timestamps borg prints (`2026-10-16T22:20:05.000000`
/// or without fractional seconds).
pub fn parse_borg_date(raw: &str) -> Option<DateTime<Local>> {
    let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    Local.from_local_datetime(&naive).earliest()
}

/// Borg archive `start` → "17 Oct 2026 at 00:20"; returns the raw string when
/// it can't be parsed.
pub fn borg_date(raw: &str) -> String {
    parse_borg_date(raw).map(|d| medium_date_short_time(&d)).unwrap_or_else(|| raw.to_string())
}

/// Parses RFC 3339 / ISO 8601 timestamps with or without fractional seconds.
pub fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

/// Relative phrasing like RelativeDateTimeFormatter: "in 3 hours",
/// "2 minutes ago", "now".
pub fn relative<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    let secs = dt.with_timezone(&Utc).signed_duration_since(Utc::now()).num_seconds();
    let future = secs > 0;
    let s = secs.abs();
    let (n, unit) = if s < 5 {
        return "now".to_string();
    } else if s < 60 {
        (s, "second")
    } else if s < 3600 {
        (s / 60, "minute")
    } else if s < 86_400 {
        (s / 3600, "hour")
    } else if s < 86_400 * 7 {
        (s / 86_400, "day")
    } else if s < 86_400 * 30 {
        (s / (86_400 * 7), "week")
    } else if s < 86_400 * 365 {
        (s / (86_400 * 30), "month")
    } else {
        (s / (86_400 * 365), "year")
    };
    let plural = if n == 1 { "" } else { "s" };
    if future { format!("in {n} {unit}{plural}") } else { format!("{n} {unit}{plural} ago") }
}

/// Abbreviated relative phrasing: "in 3 hr", "2 min ago".
pub fn relative_short<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    let secs = dt.with_timezone(&Utc).signed_duration_since(Utc::now()).num_seconds();
    let future = secs > 0;
    let s = secs.abs();
    let text = if s < 5 {
        return "now".to_string();
    } else if s < 60 {
        format!("{s} sec")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86_400 {
        format!("{} hr", s / 3600)
    } else if s < 86_400 * 7 {
        let d = s / 86_400;
        format!("{d} day{}", if d == 1 { "" } else { "s" })
    } else if s < 86_400 * 30 {
        format!("{} wk", s / (86_400 * 7))
    } else if s < 86_400 * 365 {
        format!("{} mo", s / (86_400 * 30))
    } else {
        format!("{} yr", s / (86_400 * 365))
    };
    if future { format!("in {text}") } else { format!("{text} ago") }
}

/// "1h 05m", "4m 10s", "12s".
pub fn duration(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Lowercase slug keeping letters, digits, `-` and `_`; spaces become `sep`.
pub fn slug(s: &str, sep: char) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c == ' ' { sep } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts() {
        assert_eq!(bytes_binary(0), "Zero KB");
        assert_eq!(bytes_binary(1536), "2 KB");
        assert_eq!(bytes_binary(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(bytes_binary((701.29 * 1073741824.0) as i64), "701.29 GB");
    }

    #[test]
    fn borg_dates() {
        assert!(parse_borg_date("2026-10-16T22:20:05.000000").is_some());
        assert!(parse_borg_date("2026-10-16T22:20:05").is_some());
        assert_eq!(borg_date("garbage"), "garbage");
    }
}
