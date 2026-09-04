/// Current time as epoch seconds (0 if the system clock predates the epoch).
pub fn now_epoch() -> i64 {
    crate::runtime::now_epoch()
}

/// PURE: epoch seconds as the UTC RFC 3339 shape written by the reference shell reader.
pub(super) fn format_rfc3339_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// PURE: the inverse of `days_from_civil`, returning a Gregorian UTC calendar date.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

/// PURE: an RFC 3339 timestamp into epoch seconds. Accepts the two shapes the usage payloads
/// carry, a `Z` suffix and a numeric `+HH:MM` offset, plus fractional seconds.
pub fn parse_rfc3339_epoch(timestamp: &str) -> Option<i64> {
    if timestamp.len() < 19 {
        return None;
    }
    let bytes = timestamp.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = timestamp.get(0..4)?.parse().ok()?;
    let month: i64 = timestamp.get(5..7)?.parse().ok()?;
    let day: i64 = timestamp.get(8..10)?.parse().ok()?;
    let hour: i64 = timestamp.get(11..13)?.parse().ok()?;
    let minute: i64 = timestamp.get(14..16)?.parse().ok()?;
    let second: i64 = timestamp.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = timestamp.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraction.len());
        rest = fraction.get(digits..)?;
    }
    let offset_seconds = match rest {
        "" | "Z" | "z" => 0,
        offset => {
            let (sign, body) = offset.split_at(1);
            let sign = match sign {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let (hours, minutes) = body.split_once(':')?;
            sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds)
}

/// PURE: days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm,
/// the civil date conversion used by the provider timestamp parsers).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_accepts_z_and_numeric_offsets() {
        assert_eq!(
            parse_rfc3339_epoch("2026-07-30T01:40:00Z"),
            Some(1_785_375_600)
        );
        assert_eq!(
            parse_rfc3339_epoch("2026-07-30T01:40:00.492061+00:00"),
            Some(1_785_375_600)
        );
        // A non-UTC offset is applied, not ignored.
        assert_eq!(
            parse_rfc3339_epoch("2026-07-30T03:40:00+02:00"),
            Some(1_785_375_600)
        );
        assert_eq!(parse_rfc3339_epoch("nope"), None);
        assert_eq!(parse_rfc3339_epoch("2026-07-30 01:40:00Z"), None);
    }
}
