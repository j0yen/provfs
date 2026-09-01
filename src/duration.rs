//! Parse the short duration forms `prov find --since` accepts: `24h`,
//! `7d`, `30m`, `10s` — a leading integer plus a single unit suffix.

use std::time::Duration;

/// Parse a duration like `24h`, `7d`, `30m`, or `10s` into a
/// [`Duration`]. Returns a human-readable error string on malformed
/// input (empty, non-numeric magnitude, or unknown unit).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }
    let split_at = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i);
    let Some(split_at) = split_at else {
        return Err(format!("duration {s:?} is missing a unit suffix (s/m/h/d)"));
    };
    let (magnitude, unit) = s.split_at(split_at);
    if magnitude.is_empty() {
        return Err(format!("duration {s:?} is missing a numeric magnitude"));
    }
    let n: u64 = magnitude
        .parse()
        .map_err(|_| format!("duration {s:?} has an invalid magnitude"))?;
    let secs = match unit {
        "s" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(3_600),
        "d" => n.saturating_mul(86_400),
        other => return Err(format!("duration {s:?} has unknown unit {other:?} (expected s/m/h/d)")),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hours() {
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(24 * 3_600));
    }

    #[test]
    fn parses_days() {
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(7 * 86_400));
    }

    #[test]
    fn parses_minutes() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(30 * 60));
    }

    #[test]
    fn parses_seconds() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn rejects_missing_unit() {
        assert!(parse_duration("24").is_err());
    }

    #[test]
    fn rejects_missing_magnitude() {
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(parse_duration("24w").is_err());
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(parse_duration("  24h  ").unwrap(), Duration::from_secs(24 * 3_600));
    }
}
