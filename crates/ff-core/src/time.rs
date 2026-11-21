//! Local time without a C library: reads the system's time zone file
//! (TZif, as in /etc/localtime) and converts between Unix seconds and
//! local calendar dates.

use std::sync::OnceLock;

/// Unix seconds -> (year, month, day, hour, minute) in UTC.
pub fn utc_civil(secs: i64) -> (i64, i64, i64, i64, i64) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y, m, d, rem / 3600, rem % 3600 / 60)
}

/// Days since 1970-01-01 of a calendar date.
pub(crate) fn utc_civil_days(y: i64, m: i64, d: i64) -> i64 {
    days_from_civil(y, m, d)
}

/// Days since 1970-01-01 of a calendar date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// When a daylight-saving rule switches, as written in a POSIX TZ string.
#[derive(Debug, Clone, Copy, PartialEq)]
enum RuleDay {
    /// `Mm.w.d`: weekday `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    MonthWeekDay(i64, i64, i64),
    /// `Jn`: day 1-365 of the year, never counting February 29.
    Julian(i64),
    /// `n`: day 0-365 of the year, counting February 29.
    ZeroBased(i64),
}

/// The rule for times after the last listed transition, e.g.
/// `CET-1CEST,M3.5.0,M10.5.0/3`.
#[derive(Debug, Clone, PartialEq)]
struct PosixTz {
    std_offset: i32,
    /// Daylight-saving offset and its (start, end) as (day, seconds into the day).
    dst: Option<(i32, (RuleDay, i64), (RuleDay, i64))>,
}

impl PosixTz {
    fn parse(s: &str) -> Option<PosixTz> {
        let mut rest = s.trim();
        // A name is letters, or anything inside <...>.
        fn name(s: &str) -> Option<&str> {
            if let Some(quoted) = s.strip_prefix('<') {
                return Some(&quoted[quoted.find('>')? + 1..]);
            }
            let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
            (end >= 3).then(|| &s[end..])
        }
        // [+-]hh[:mm[:ss]] -> seconds, and the rest of the string.
        fn time(s: &str) -> Option<(i64, &str)> {
            let (sign, s) = match s.as_bytes().first()? {
                b'-' => (-1, &s[1..]),
                b'+' => (1, &s[1..]),
                _ => (1, s),
            };
            let end = s.find(|c: char| !c.is_ascii_digit() && c != ':').unwrap_or(s.len());
            let mut parts = s[..end].split(':').map(|p| p.parse::<i64>());
            let h = parts.next()?.ok()?;
            let m = parts.next().transpose().ok()?.unwrap_or(0);
            let sec = parts.next().transpose().ok()?.unwrap_or(0);
            Some((sign * (h * 3600 + m * 60 + sec), &s[end..]))
        }
        fn rule(s: &str) -> Option<((RuleDay, i64), &str)> {
            let end = s.find([',', '/']).unwrap_or(s.len());
            let spec = &s[..end];
            let day = if let Some(m) = spec.strip_prefix('M') {
                let mut p = m.split('.').map(|v| v.parse::<i64>().ok());
                RuleDay::MonthWeekDay(p.next()??, p.next()??, p.next()??)
            } else if let Some(j) = spec.strip_prefix('J') {
                RuleDay::Julian(j.parse().ok()?)
            } else {
                RuleDay::ZeroBased(spec.parse().ok()?)
            };
            let rest = &s[end..];
            match rest.strip_prefix('/') {
                Some(t) => {
                    let (secs, rest) = time(t)?;
                    Some(((day, secs), rest))
                }
                None => Some(((day, 7200), rest)), // 02:00 by default
            }
        }

        rest = name(rest)?;
        let (std, after) = time(rest)?;
        // POSIX writes offsets west-positive; we keep seconds east of UTC.
        let std_offset = -std as i32;
        if after.is_empty() {
            return Some(PosixTz { std_offset, dst: None });
        }
        rest = name(after)?;
        let mut dst_offset = std_offset + 3600;
        if !rest.starts_with(',') {
            let (dst, after) = time(rest)?;
            dst_offset = -dst as i32;
            rest = after;
        }
        let (start, after) = rule(rest.strip_prefix(',')?)?;
        let (end, _) = rule(after.strip_prefix(',')?)?;
        Some(PosixTz { std_offset, dst: Some((dst_offset, start, end)) })
    }

    /// Seconds from the start of `year` (in local standard days) to a rule day.
    fn day_of_year(day: RuleDay, year: i64) -> i64 {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        match day {
            RuleDay::ZeroBased(n) => n,
            RuleDay::Julian(n) => n - 1 + (leap && n > 59) as i64,
            RuleDay::MonthWeekDay(m, w, d) => {
                let first = days_from_civil(year, m, 1);
                let weekday = (first + 4).rem_euclid(7); // 1970-01-01 was a Thursday
                let mut day = first + (d - weekday).rem_euclid(7) + (w - 1) * 7;
                let next_month = if m == 12 { days_from_civil(year + 1, 1, 1) } else { days_from_civil(year, m + 1, 1) };
                while day >= next_month {
                    day -= 7; // "week 5" means the last one
                }
                day - days_from_civil(year, 1, 1)
            }
        }
    }

    fn offset_at(&self, utc: i64) -> i32 {
        let Some((dst_offset, start, end)) = self.dst else { return self.std_offset };
        let year = utc_civil(utc + self.std_offset as i64).0;
        let year_start = days_from_civil(year, 1, 1) * 86400;
        // Rule times are local wall-clock: the start is in standard time,
        // the end in daylight time.
        let start_utc = year_start + Self::day_of_year(start.0, year) * 86400 + start.1 - self.std_offset as i64;
        let end_utc = year_start + Self::day_of_year(end.0, year) * 86400 + end.1 - dst_offset as i64;
        let in_dst = if start_utc < end_utc {
            utc >= start_utc && utc < end_utc
        } else {
            utc >= start_utc || utc < end_utc // southern hemisphere
        };
        if in_dst { dst_offset } else { self.std_offset }
    }
}

/// A time zone: its recorded offset changes and the rule after them.
#[derive(Debug, Default)]
pub struct TimeZone {
    /// (instant in Unix seconds, offset in seconds east of UTC from then on).
    transitions: Vec<(i64, i32)>,
    /// Offset before the first transition.
    first_offset: i32,
    after: Option<PosixTz>,
}

impl TimeZone {
    /// Parses the contents of a TZif file (version 2 or later).
    pub fn parse(data: &[u8]) -> Option<TimeZone> {
        let be32 = |b: &[u8], i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize);
        // Header: "TZif", version, 15 unused bytes, then six counts.
        let header = |b: &[u8]| -> Option<[usize; 6]> {
            if b.get(..4)? != b"TZif" {
                return None;
            }
            Some([be32(b, 20)?, be32(b, 24)?, be32(b, 28)?, be32(b, 32)?, be32(b, 36)?, be32(b, 40)?])
        };
        if *data.get(4)? < b'2' {
            return None; // version 1 only has 32-bit times; nothing modern ships it
        }
        // Skip the version-1 block to reach the 64-bit one.
        let [isut, isstd, leap, times, types, chars] = header(data)?;
        let v1_len = times * 4 + times + types * 6 + chars + leap * 8 + isstd + isut;
        let block = data.get(44 + v1_len..)?;
        let [isut, isstd, leap, times, types, chars] = header(block)?;
        let body = block.get(44..)?;
        let offset_of = |index: usize| -> Option<i32> {
            let at = times * 8 + times + index * 6;
            Some(i32::from_be_bytes(body.get(at..at + 4)?.try_into().ok()?))
        };
        let mut transitions = Vec::with_capacity(times);
        for i in 0..times {
            let at = i64::from_be_bytes(body.get(i * 8..i * 8 + 8)?.try_into().ok()?);
            let kind = *body.get(times * 8 + i)? as usize;
            transitions.push((at, offset_of(kind)?));
        }
        let footer_at = times * 8 + times + types * 6 + chars + leap * 12 + isstd + isut;
        let footer = body.get(footer_at..).unwrap_or(&[]);
        let after = std::str::from_utf8(footer).ok().and_then(|f| PosixTz::parse(f.trim_matches('\n')));
        Some(TimeZone { transitions, first_offset: offset_of(0).unwrap_or(0), after })
    }

    /// The zone this machine is set to, as Windows reports it (with its
    /// daylight-saving rule); UTC if it cannot be read.
    #[cfg(windows)]
    pub fn system() -> TimeZone {
        // Windows describes the zone as a bias and two yearly rules, the
        // same shape as a POSIX TZ string.
        let Some(tz) = crate::win::time_zone() else { return TimeZone::default() };
        let std_offset = -(tz.Bias + tz.StandardBias) * 60;
        let rule = |d: &crate::win::SYSTEMTIME| {
            let day = RuleDay::MonthWeekDay(d.wMonth as i64, d.wDay as i64, d.wDayOfWeek as i64);
            (day, d.wHour as i64 * 3600 + d.wMinute as i64 * 60 + d.wSecond as i64)
        };
        let dst = (tz.DaylightDate.wMonth != 0 && tz.StandardDate.wMonth != 0 && tz.DynamicDaylightTimeDisabled == 0)
            .then(|| (-(tz.Bias + tz.DaylightBias) * 60, rule(&tz.DaylightDate), rule(&tz.StandardDate)));
        TimeZone { transitions: Vec::new(), first_offset: std_offset, after: Some(PosixTz { std_offset, dst }) }
    }

    /// The zone this machine is set to: `$TZ` if it names one, else
    /// /etc/localtime; UTC if neither can be read.
    #[cfg(not(windows))]
    pub fn system() -> TimeZone {
        let named = std::env::var("TZ").ok().filter(|tz| !tz.is_empty()).map(|tz| {
            let name = tz.trim_start_matches(':');
            if name.starts_with('/') { name.to_string() } else { format!("/usr/share/zoneinfo/{name}") }
        });
        let path = named.unwrap_or_else(|| "/etc/localtime".into());
        std::fs::read(path).ok().and_then(|data| TimeZone::parse(&data)).unwrap_or_default()
    }

    /// Seconds east of UTC at a given instant.
    pub fn offset_at(&self, utc: i64) -> i32 {
        match self.transitions.last() {
            Some(&(last, offset)) if utc >= last => self.after.as_ref().map_or(offset, |rule| rule.offset_at(utc)),
            Some(_) => {
                let after = self.transitions.partition_point(|&(at, _)| at <= utc);
                if after == 0 { self.first_offset } else { self.transitions[after - 1].1 }
            }
            None => self.after.as_ref().map_or(self.first_offset, |rule| rule.offset_at(utc)),
        }
    }

    /// Unix seconds -> local (year, month, day, hour, minute).
    pub fn civil(&self, utc: i64) -> (i64, i64, i64, i64, i64) {
        utc_civil(utc + self.offset_at(utc) as i64)
    }

    /// Unix seconds of local midnight starting the given calendar day.
    pub fn midnight(&self, year: i64, month: i64, day: i64) -> i64 {
        let wall = days_from_civil(year, month, day) * 86400;
        // The offset depends on the instant we are looking for: guess with
        // the offset at the wall time, then correct once.
        let guess = wall - self.offset_at(wall) as i64;
        wall - self.offset_at(guess) as i64
    }
}

/// The system time zone, read once.
pub fn local() -> &'static TimeZone {
    static ZONE: OnceLock<TimeZone> = OnceLock::new();
    ZONE.get_or_init(TimeZone::system)
}

/// Unix seconds -> (year, month, day, hour, minute) in local time.
pub fn local_civil(secs: i64) -> (i64, i64, i64, i64, i64) {
    local().civil(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(name: &str) -> Option<TimeZone> {
        TimeZone::parse(&std::fs::read(format!("/usr/share/zoneinfo/{name}")).ok()?)
    }

    /// Unix seconds of a UTC date and time.
    fn utc(y: i64, m: i64, d: i64, h: i64) -> i64 {
        days_from_civil(y, m, d) * 86400 + h * 3600
    }

    #[test]
    fn calendar_round_trips() {
        for &(y, m, d) in &[(1970, 1, 1), (2000, 2, 29), (2024, 12, 31), (2026, 11, 14), (1969, 12, 31)] {
            let (yy, mm, dd, h, mi) = utc_civil(days_from_civil(y, m, d) * 86400 + 3 * 3600 + 7 * 60);
            assert_eq!((yy, mm, dd, h, mi), (y, m, d, 3, 7));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn posix_rules() {
        let paris = PosixTz::parse("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        assert_eq!(paris.std_offset, 3600);
        assert_eq!(paris.offset_at(utc(2040, 1, 15, 12)), 3600);
        assert_eq!(paris.offset_at(utc(2040, 7, 15, 12)), 7200);
        // Last Sunday of March 2040 is the 25th; the switch is at 01:00 UTC.
        assert_eq!(paris.offset_at(utc(2040, 3, 25, 0)), 3600);
        assert_eq!(paris.offset_at(utc(2040, 3, 25, 1)), 7200);
        // Last Sunday of October 2040 is the 28th; back at 01:00 UTC.
        assert_eq!(paris.offset_at(utc(2040, 10, 28, 0)), 7200);
        assert_eq!(paris.offset_at(utc(2040, 10, 28, 1)), 3600);

        let fixed = PosixTz::parse("<+03>-3").unwrap();
        assert_eq!((fixed.std_offset, fixed.dst.is_none()), (3 * 3600, true));
        let new_york = PosixTz::parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!(new_york.offset_at(utc(2040, 1, 1, 12)), -5 * 3600);
        assert_eq!(new_york.offset_at(utc(2040, 7, 1, 12)), -4 * 3600);
        // Southern hemisphere: daylight time spans the new year.
        let sydney = PosixTz::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(sydney.offset_at(utc(2040, 1, 15, 0)), 11 * 3600);
        assert_eq!(sydney.offset_at(utc(2040, 7, 15, 0)), 10 * 3600);
        assert!(PosixTz::parse("nonsense").is_none());
    }

    #[test]
    fn real_zone_files() {
        // Skipped quietly on a system without the zone database.
        if let Some(paris) = zone("Europe/Paris") {
            assert_eq!(paris.offset_at(utc(2026, 1, 15, 12)), 3600);
            assert_eq!(paris.offset_at(utc(2026, 7, 15, 12)), 7200);
            assert_eq!(paris.offset_at(utc(1995, 7, 15, 12)), 7200); // from the recorded transitions
            assert_eq!(paris.offset_at(utc(2090, 7, 15, 12)), 7200); // from the rule after them
            assert_eq!(paris.civil(utc(2026, 7, 15, 22) + 30 * 60), (2026, 7, 16, 0, 30));
            // Local midnight of 2026-07-16 is 22:00 UTC the day before.
            assert_eq!(paris.midnight(2026, 7, 16), utc(2026, 7, 15, 22));
            assert_eq!(paris.midnight(2026, 1, 16), utc(2026, 1, 15, 23));
        }
        if let Some(tunis) = zone("Africa/Tunis") {
            assert_eq!(tunis.offset_at(utc(2026, 7, 15, 12)), 3600);
            assert_eq!(tunis.offset_at(utc(2026, 1, 15, 12)), 3600);
        }
        if let Some(riyadh) = zone("Asia/Riyadh") {
            assert_eq!(riyadh.civil(utc(2026, 11, 14, 22)), (2026, 11, 15, 1, 0));
        }
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(TimeZone::parse(b"").is_none());
        assert!(TimeZone::parse(b"not a zone file at all, just some text").is_none());
        assert_eq!(TimeZone::default().offset_at(0), 0);
    }
}
