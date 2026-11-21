//! Wall-clock helpers: Unix time and RFC 3339 / ISO 8601 formatting & parsing.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) → days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Formats Unix milliseconds as RFC 3339 UTC, e.g. `2026-07-28T12:00:00.000Z`.
pub fn iso8601_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        ms % 1000
    )
}

/// Formats Unix seconds as RFC 3339 UTC without fractional seconds.
pub fn iso8601_secs(secs: u64) -> String {
    let s = secs as i64;
    let (y, mo, d) = civil_from_days(s.div_euclid(86_400));
    let rem = s.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Formats Unix seconds as a calendar date `YYYY-MM-DD`.
pub fn date_from_secs(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parses `YYYY-MM-DD`, `YYYY/MM/DD`, or an RFC 3339 timestamp
/// (`YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)`) into Unix seconds.
pub fn parse_datetime(s: &str) -> Option<u64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let t = s.get(r)?;
        if t.bytes().all(|c| c.is_ascii_digit()) {
            t.parse().ok()
        } else {
            None
        }
    };
    let y = num(0..4)? as i64;
    if !(b[4] == b'-' || b[4] == b'/') || b[7] != b[4] {
        return None;
    }
    let mo = num(5..7)?;
    let d = num(8..10)?;
    if !(1..=12).contains(&mo) || d == 0 || d > days_in_month(y, mo) {
        return None;
    }
    let mut secs = days_from_civil(y, mo, d) * 86_400;
    if b.len() > 10 {
        if !(b[10] == b'T' || b[10] == b't' || b[10] == b' ') || b.len() < 19 {
            return None;
        }
        let h = num(11..13)?;
        let mi = num(14..16)?;
        let se = num(17..19)?;
        if b[13] != b':' || b[16] != b':' || h > 23 || mi > 59 || se > 60 {
            return None;
        }
        secs += (h * 3600 + mi * 60 + se) as i64;
        let mut i = 19;
        if b.get(i) == Some(&b'.') {
            i += 1;
            while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
        }
        match b.get(i) {
            None | Some(b'Z') | Some(b'z') => {}
            Some(&sign) if sign == b'+' || sign == b'-' => {
                let oh = num(i + 1..i + 3)? as i64;
                let om = num(i + 4..i + 6)? as i64;
                let off = oh * 3600 + om * 60;
                secs += if sign == b'+' { -off } else { off };
            }
            _ => return None,
        }
    }
    u64::try_from(secs).ok()
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse() {
        assert_eq!(iso8601_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_secs(1_785_196_800), "2026-07-28T00:00:00Z");
        assert_eq!(parse_datetime("2026-07-28"), Some(1_785_196_800));
        assert_eq!(parse_datetime("2026/07/28"), Some(1_785_196_800));
        assert_eq!(
            parse_datetime("2026-07-28T01:02:03Z"),
            Some(1_785_196_800 + 3723)
        );
        assert_eq!(
            parse_datetime("2026-07-28T01:02:03.456+01:00"),
            Some(1_785_196_800 + 123)
        );
        assert_eq!(parse_datetime("2024-02-30"), None);
        assert_eq!(parse_datetime("2024-02-29"), Some(1_709_164_800));
        assert_eq!(parse_datetime("nope"), None);
        assert_eq!(date_from_secs(1_785_196_800), "2026-07-28");
        for days in [-1000i64, 0, 1, 365, 10_000, 20_662] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
