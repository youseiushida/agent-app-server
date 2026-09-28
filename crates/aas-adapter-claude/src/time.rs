//! Minimal RFC 3339 timestamp parsing (Claude Code writes `2026-09-27T03:51:33.793Z`).

use aas_harness::Millis;

/// Parses an RFC 3339 timestamp (`YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`) into Unix
/// milliseconds. Returns `None` for anything that does not match the format exactly.
pub fn parse_rfc3339_millis(s: &str) -> Option<Millis> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ')
    {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = s.get(from..to)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut idx = 19;
    let mut millis = 0i64;
    if b.get(idx) == Some(&b'.') {
        idx += 1;
        let start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == start {
            return None;
        }
        let frac = &s[start..idx];
        let mut digits: String = frac.chars().take(3).collect();
        while digits.len() < 3 {
            digits.push('0');
        }
        millis = digits.parse().ok()?;
    }
    let offset_minutes = match b.get(idx) {
        Some(b'Z') | Some(b'z') if idx + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if idx + 6 == b.len() && b[idx + 3] == b':' => {
            let h = num(idx + 1, idx + 3)?;
            let m = num(idx + 4, idx + 6)?;
            let total = h * 60 + m;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(secs * 1_000 + millis)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_with_millis() {
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:01.5Z"), Some(1_500));
        // 2026-09-27T03:51:33.793Z
        assert_eq!(
            parse_rfc3339_millis("2026-09-27T03:51:33.793Z"),
            Some(1_790_481_093_793)
        );
        assert_eq!(
            parse_rfc3339_millis("2000-02-29T12:00:00.123456Z"),
            Some(951_825_600_123)
        );
    }

    #[test]
    fn parses_offsets() {
        assert_eq!(parse_rfc3339_millis("1970-01-01T09:00:00+09:00"), Some(0));
        assert_eq!(parse_rfc3339_millis("1969-12-31T23:00:00-01:00"), Some(0));
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "",
            "2026-09-27",
            "2026-13-01T00:00:00Z",
            "2026-09-27T03:51:33",
            "2026-09-27T03:51:33.Z",
            "x026-09-27T03:51:33Z",
        ] {
            assert_eq!(parse_rfc3339_millis(s), None, "{s}");
        }
    }
}
