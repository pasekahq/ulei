//! Local time of a configured IANA zone, read from a compiled zone file (`TZif`, RFC 8536).
//!
//! Std only: the transition table covers the past, the POSIX `TZ` footer the time after the
//! last transition.

use std::fmt;
use std::path::Path;

/// Directories searched for a named zone, in order: the default search path of the
/// reference's `zoneinfo`, as the engine reads `engine.timezone`.
pub const ZONEINFO_DIRS: [&str; 4] = [
    "/usr/share/zoneinfo",
    "/usr/lib/zoneinfo",
    "/usr/share/lib/zoneinfo",
    "/etc/zoneinfo",
];

/// Broken-down local time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTime {
    /// Year.
    pub year: i64,
    /// Month, 1-12.
    pub month: u32,
    /// Day of month, 1-31.
    pub day: u32,
    /// Hour, 0-23.
    pub hour: u32,
    /// Minute, 0-59.
    pub minute: u32,
    /// Second, 0-59.
    pub second: u32,
    /// Offset from UTC in seconds.
    pub offset: i64,
}

impl LocalTime {
    /// `dd.mm.yyyy HH:MM`.
    pub fn dmy_hm(&self) -> String {
        format!(
            "{:02}.{:02}.{:04} {:02}:{:02}",
            self.day, self.month, self.year, self.hour, self.minute
        )
    }

    /// `yyyy-mm-dd HH:MM`.
    pub fn ymd_hm(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }

    /// `yyyy-mm-ddTHH:MM:SS±hhmm`.
    pub fn iso(&self) -> String {
        let sign = if self.offset < 0 { '-' } else { '+' };
        let off = self.offset.abs();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{sign}{:02}{:02}",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            off / 3600,
            off % 3600 / 60
        )
    }
}

/// Why a zone could not be loaded.
#[derive(Debug)]
pub enum ZoneError {
    /// The zone name is not a relative IANA name.
    BadName,
    /// None of the searched directories holds the zone.
    NotFound,
    /// The zone file could not be read.
    Io(std::io::Error),
    /// The zone file is not valid `TZif`.
    Malformed,
}

impl fmt::Display for ZoneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadName => f.write_str("time zone name is not a relative IANA name"),
            Self::NotFound => f.write_str("time zone is not in the zone database"),
            Self::Io(e) => write!(f, "time zone file: {e}"),
            Self::Malformed => f.write_str("time zone file is not valid TZif"),
        }
    }
}

impl std::error::Error for ZoneError {}

/// A time zone: transition table plus the rule for later times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    transitions: Vec<(i64, i64)>,
    initial: i64,
    rule: Option<Rule>,
}

impl Zone {
    /// Coordinated Universal Time.
    pub fn utc() -> Self {
        Self {
            transitions: Vec::new(),
            initial: 0,
            rule: None,
        }
    }

    /// Loads the IANA zone `name` (for example `Europe/Berlin`) from the first of `dirs`
    /// holding its file.
    ///
    /// # Errors
    /// The name is not a plain relative name, or its file is missing or malformed.
    pub fn named(name: &str, dirs: &[&Path]) -> Result<Self, ZoneError> {
        let ok = !name.is_empty()
            && !name.starts_with('/')
            && name
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != "..")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/_-+".contains(c));
        if !ok {
            return Err(ZoneError::BadName);
        }
        let path = dirs
            .iter()
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
            .ok_or(ZoneError::NotFound)?;
        Self::from_file(&path)
    }

    /// Loads a compiled zone file.
    ///
    /// # Errors
    /// The file is unreadable or not valid `TZif`.
    pub fn from_file(path: &Path) -> Result<Self, ZoneError> {
        let bytes = std::fs::read(path).map_err(ZoneError::Io)?;
        Self::from_tzif(&bytes).ok_or(ZoneError::Malformed)
    }

    /// Parses `TZif` bytes (versions 1-4).
    pub fn from_tzif(bytes: &[u8]) -> Option<Self> {
        let header = Header::parse(bytes)?;
        if header.version == 0 {
            return Self::from_block(&header, bytes.get(44..)?, 4, None);
        }
        let v2 = bytes.get(44 + header.block_len(4)..)?;
        let header2 = Header::parse(v2)?;
        let data = v2.get(44..)?;
        let footer = data.get(header2.block_len(8)..)?;
        let rule = std::str::from_utf8(footer).ok().and_then(|f| {
            let tz = f.strip_prefix('\n')?.split('\n').next()?;
            Rule::parse(tz)
        });
        Self::from_block(&header2, data, 8, rule)
    }

    fn from_block(h: &Header, data: &[u8], width: usize, rule: Option<Rule>) -> Option<Self> {
        let times = data.get(..h.timecnt * width)?;
        let idx = data.get(h.timecnt * width..h.timecnt * (width + 1))?;
        let types = data.get(h.timecnt * (width + 1)..h.timecnt * (width + 1) + h.typecnt * 6)?;
        let offsets: Vec<i64> = types
            .as_chunks::<6>()
            .0
            .iter()
            .map(|t| i64::from(i32::from_be_bytes([t[0], t[1], t[2], t[3]])))
            .collect();
        let mut transitions = Vec::with_capacity(h.timecnt);
        for (i, &ty) in idx.iter().enumerate() {
            let raw = &times[i * width..(i + 1) * width];
            let at = if width == 8 {
                i64::from_be_bytes(raw.try_into().ok()?)
            } else {
                i64::from(i32::from_be_bytes(raw.try_into().ok()?))
            };
            transitions.push((at, *offsets.get(usize::from(ty))?));
        }
        Some(Self {
            transitions,
            initial: *offsets.first()?,
            rule,
        })
    }

    /// UTC offset in seconds at Unix time `t`.
    pub fn offset_at(&self, t: i64) -> i64 {
        match self.transitions.last() {
            Some(&(last, _)) if t >= last && self.rule.is_some() => {
                self.rule.as_ref().map_or(0, |r| r.offset_at(t))
            }
            None => self.rule.as_ref().map_or(self.initial, |r| r.offset_at(t)),
            _ => match self.transitions.partition_point(|&(at, _)| at <= t) {
                0 => self.initial,
                n => self.transitions[n - 1].1,
            },
        }
    }

    /// Local time at Unix time `t`, for any `t`.
    pub fn local(&self, t: i64) -> LocalTime {
        let offset = self.offset_at(t);
        let (year, month, day) = civil_from_days(local_days(t, offset));
        let tod = (t.rem_euclid(86_400) + offset).rem_euclid(86_400);
        LocalTime {
            year,
            month,
            day,
            hour: u32::try_from(tod / 3600).unwrap_or(0),
            minute: u32::try_from(tod % 3600 / 60).unwrap_or(0),
            second: u32::try_from(tod % 60).unwrap_or(0),
            offset,
        }
    }
}

struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl Header {
    fn parse(b: &[u8]) -> Option<Self> {
        if b.get(..4)? != b"TZif" {
            return None;
        }
        let count = |i: usize| -> Option<usize> {
            let raw: [u8; 4] = b.get(20 + i * 4..24 + i * 4)?.try_into().ok()?;
            usize::try_from(u32::from_be_bytes(raw)).ok()
        };
        let version = match *b.get(4)? {
            0 => 0,
            v @ b'2'..=b'9' => v - b'0',
            _ => return None,
        };
        Some(Self {
            version,
            isutcnt: count(0)?,
            isstdcnt: count(1)?,
            leapcnt: count(2)?,
            timecnt: count(3)?,
            typecnt: count(4)?,
            charcnt: count(5)?,
        })
    }

    fn block_len(&self, width: usize) -> usize {
        self.timecnt * (width + 1)
            + self.typecnt * 6
            + self.charcnt
            + self.leapcnt * (width + 4)
            + self.isstdcnt
            + self.isutcnt
    }
}

/// A POSIX `TZ` rule: standard offset and an optional daylight period.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    std_offset: i64,
    dst: Option<Dst>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Dst {
    offset: i64,
    start: (DateRule, i64),
    end: (DateRule, i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateRule {
    /// `Jn`: day 1-365, February 29 never counted.
    Julian(i64),
    /// `n`: zero-based day 0-365.
    Zero(i64),
    /// `Mm.w.d`: day `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    Month(i64, i64, i64),
}

impl Rule {
    fn parse(tz: &str) -> Option<Self> {
        let mut s = Cursor(tz.as_bytes());
        s.name()?;
        let std_offset = -s.hms()?;
        if s.0.is_empty() {
            return Some(Self {
                std_offset,
                dst: None,
            });
        }
        s.name()?;
        let offset = if s.0.first().is_some_and(|c| *c != b',') {
            -s.hms()?
        } else {
            std_offset + 3600
        };
        let start = s.transition()?;
        let end = s.transition()?;
        s.0.is_empty().then_some(Self {
            std_offset,
            dst: Some(Dst { offset, start, end }),
        })
    }

    fn offset_at(&self, t: i64) -> i64 {
        let Some(dst) = &self.dst else {
            return self.std_offset;
        };
        let (year, _, _) = civil_from_days(local_days(t, self.std_offset));
        // Start is given in standard local time, end in daylight local time. In `i128` the
        // instants of the years at both ends of `i64` stay exact.
        let instant = |(date, time): (DateRule, i64), offset: i64| {
            i128::from(days_from_civil(year, 1, 1) + date.day_of_year(year)) * 86_400
                + i128::from(time - offset)
        };
        let start = instant(dst.start, self.std_offset);
        let end = instant(dst.end, dst.offset);
        let t = i128::from(t);
        let in_dst = if start <= end {
            t >= start && t < end
        } else {
            t >= start || t < end
        };
        if in_dst { dst.offset } else { self.std_offset }
    }
}

impl DateRule {
    fn day_of_year(self, year: i64) -> i64 {
        match self {
            Self::Julian(n) => n - 1 + i64::from(is_leap(year) && n >= 60),
            Self::Zero(n) => n,
            Self::Month(m, w, d) => {
                let first = days_from_civil(year, m, 1);
                let weekday = (first + 4).rem_euclid(7);
                let mut day = 1 + (d - weekday).rem_euclid(7) + (w - 1) * 7;
                let len = month_len(year, m);
                while day > len {
                    day -= 7;
                }
                first + day - 1 - days_from_civil(year, 1, 1)
            }
        }
    }
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn name(&mut self) -> Option<()> {
        if self.0.first() == Some(&b'<') {
            let end = self.0.iter().position(|c| *c == b'>')?;
            self.0 = &self.0[end + 1..];
            return Some(());
        }
        let len = self
            .0
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        (len >= 3).then(|| self.0 = &self.0[len..])
    }

    fn number(&mut self) -> Option<i64> {
        let len = self.0.iter().take_while(|c| c.is_ascii_digit()).count();
        let n = std::str::from_utf8(self.0.get(..len)?).ok()?.parse().ok()?;
        self.0 = &self.0[len..];
        Some(n)
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.0.first() == Some(&c);
        if hit {
            self.0 = &self.0[1..];
        }
        hit
    }

    /// `[+-]hh[:mm[:ss]]` with the ranges RFC 8536 allows (hours up to 167).
    fn hms(&mut self) -> Option<i64> {
        let sign = if self.eat(b'-') {
            -1
        } else {
            self.eat(b'+');
            1
        };
        let mut secs = self.number().filter(|h| *h <= 167)? * 3600;
        if self.eat(b':') {
            secs += self.number().filter(|m| *m <= 59)? * 60;
            if self.eat(b':') {
                secs += self.number().filter(|s| *s <= 59)?;
            }
        }
        Some(sign * secs)
    }

    fn transition(&mut self) -> Option<(DateRule, i64)> {
        if !self.eat(b',') {
            return None;
        }
        let date = if self.eat(b'M') {
            let m = self.number().filter(|m| (1..=12).contains(m))?;
            let w = if self.eat(b'.') {
                self.number().filter(|w| (1..=5).contains(w))?
            } else {
                return None;
            };
            let d = if self.eat(b'.') {
                self.number().filter(|d| (0..=6).contains(d))?
            } else {
                return None;
            };
            DateRule::Month(m, w, d)
        } else if self.eat(b'J') {
            DateRule::Julian(self.number().filter(|n| (1..=365).contains(n))?)
        } else {
            DateRule::Zero(self.number().filter(|n| (0..=365).contains(n))?)
        };
        let time = if self.eat(b'/') { self.hms()? } else { 7200 };
        Some((date, time))
    }
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn month_len(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Day count since 1970-01-01 of the local date at Unix time `t` and UTC offset `offset`;
/// it cannot overflow for any `t` and a zone file's offsets.
fn local_days(t: i64, offset: i64) -> i64 {
    t.div_euclid(86_400) + (t.rem_euclid(86_400) + offset).div_euclid(86_400)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Proleptic Gregorian date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for days in [-1, 0, 59, 365, 10_957, 20_000, 24_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, i64::from(m), i64::from(d)), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn posix_rule_switches_on_last_sundays() {
        let rule = Rule::parse("EET-2EEST,M3.5.0/3,M10.5.0/4").expect("rule");
        // 2026-03-29 00:59:59 UTC is the last second of standard time.
        let start = days_from_civil(2026, 3, 29) * 86_400 + 3600;
        assert_eq!(rule.offset_at(start - 1), 7200);
        assert_eq!(rule.offset_at(start), 10_800);
        // 2026-10-25 01:00:00 UTC ends daylight time.
        let end = days_from_civil(2026, 10, 25) * 86_400 + 3600;
        assert_eq!(rule.offset_at(end - 1), 10_800);
        assert_eq!(rule.offset_at(end), 7200);
    }

    #[test]
    fn posix_rule_without_dst_and_negative_offset() {
        assert_eq!(Rule::parse("<-03>3").expect("rule").offset_at(0), -10_800);
        assert_eq!(Rule::parse("UTC0").expect("rule").offset_at(0), 0);
    }
}
