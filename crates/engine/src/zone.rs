//! Local time of a time zone, read from a compiled zone file (`TZif`, RFC 8536): the UTC offset
//! and abbreviation in force at an instant. The transition table covers the past, the POSIX
//! `TZ` footer the time after the last transition. Also UTC dates and times as a tool's
//! arguments give them.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Directories searched for a named zone, in order.
pub const ZONEINFO_DIRS: [&str; 4] = [
    "/usr/share/zoneinfo",
    "/usr/lib/zoneinfo",
    "/usr/share/lib/zoneinfo",
    "/etc/zoneinfo",
];

/// The zone file of the host's local time.
pub const LOCALTIME: &str = "/etc/localtime";

/// Why a zone could not be loaded.
#[derive(Debug)]
pub enum Error {
    /// The name is not a relative IANA name.
    BadName(String),
    /// No directory holds a file of this name.
    NotFound(String),
    /// The zone file could not be read.
    Io(PathBuf, io::Error),
    /// The zone file is not valid `TZif`.
    Malformed(PathBuf),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadName(name) => write!(f, "time zone {name:?} is not a relative IANA name"),
            Self::NotFound(name) => write!(f, "time zone {name:?} is not in the zone database"),
            Self::Io(path, e) => write!(f, "time zone file {}: {e}", path.display()),
            Self::Malformed(path) => {
                write!(f, "time zone file {} is not valid TZif", path.display())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(_, e) => Some(e),
            _ => None,
        }
    }
}

/// The local time type in force at an instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    /// Offset from UTC in seconds, east positive.
    pub offset: i64,
    /// Abbreviation, such as `CET` or `+04`.
    pub abbrev: String,
}

/// A time zone: its IANA name when it was loaded by name, the transition table and the rule
/// for later times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    name: Option<String>,
    transitions: Vec<(i64, Local)>,
    initial: Local,
    rule: Option<Rule>,
}

impl Zone {
    /// Coordinated Universal Time, without a name.
    pub fn utc() -> Self {
        Self {
            name: None,
            transitions: Vec::new(),
            initial: Local {
                offset: 0,
                abbrev: "UTC".to_owned(),
            },
            rule: None,
        }
    }

    /// The IANA zone `name` from the first of `dirs` holding its file.
    ///
    /// # Errors
    /// [`Error::BadName`], [`Error::NotFound`], [`Error::Io`] or [`Error::Malformed`].
    pub fn named(name: &str, dirs: &[&Path]) -> Result<Self, Error> {
        let relative = !name.is_empty()
            && !name.starts_with('/')
            && name
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != "..");
        if !relative {
            return Err(Error::BadName(name.to_owned()));
        }
        let path = dirs
            .iter()
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
            .ok_or_else(|| Error::NotFound(name.to_owned()))?;
        let mut zone = Self::file(&path)?;
        zone.name = Some(name.to_owned());
        Ok(zone)
    }

    /// The zone of the host's local time: [`LOCALTIME`], or UTC when that file is missing.
    ///
    /// # Errors
    /// [`Error::Io`] or [`Error::Malformed`].
    pub fn system() -> Result<Self, Error> {
        let path = Path::new(LOCALTIME);
        match path.try_exists() {
            Ok(true) => Self::file(path),
            Ok(false) => Ok(Self::utc()),
            Err(e) => Err(Error::Io(path.to_owned(), e)),
        }
    }

    fn file(path: &Path) -> Result<Self, Error> {
        let bytes = std::fs::read(path).map_err(|e| Error::Io(path.to_owned(), e))?;
        Self::from_tzif(&bytes).ok_or_else(|| Error::Malformed(path.to_owned()))
    }

    /// The zone of `TZif` bytes (versions 1 to 4), without a name.
    pub fn from_tzif(bytes: &[u8]) -> Option<Self> {
        let header = Header::parse(bytes)?;
        if header.version == 0 {
            return Self::from_block(&header, bytes.get(44..)?, 4, None);
        }
        let v2 = bytes.get(44 + header.block_len(4)..)?;
        let header2 = Header::parse(v2)?;
        let data = v2.get(44..)?;
        let footer = data.get(header2.block_len(8)..)?;
        let rule = match std::str::from_utf8(footer)
            .ok()?
            .strip_prefix('\n')
            .and_then(|f| f.split('\n').next())
        {
            Some("") | None => None,
            Some(tz) => Some(Rule::parse(tz)?),
        };
        Self::from_block(&header2, data, 8, rule)
    }

    fn from_block(h: &Header, data: &[u8], width: usize, rule: Option<Rule>) -> Option<Self> {
        let times = data.get(..h.timecnt * width)?;
        let idx = data.get(h.timecnt * width..h.timecnt * (width + 1))?;
        let types_at = h.timecnt * (width + 1);
        let types = data.get(types_at..types_at + h.typecnt * 6)?;
        let chars = data.get(types_at + h.typecnt * 6..types_at + h.typecnt * 6 + h.charcnt)?;
        let locals = types
            .as_chunks::<6>()
            .0
            .iter()
            .map(|t| {
                let tail = chars.get(usize::from(t[5])..)?;
                let end = tail.iter().position(|c| *c == 0)?;
                Some(Local {
                    offset: i64::from(i32::from_be_bytes([t[0], t[1], t[2], t[3]])),
                    abbrev: String::from_utf8(tail[..end].to_vec()).ok()?,
                })
            })
            .collect::<Option<Vec<Local>>>()?;
        let mut transitions = Vec::with_capacity(h.timecnt);
        for (raw, &ty) in times.chunks_exact(width).zip(idx) {
            let at = if width == 8 {
                i64::from_be_bytes(raw.try_into().ok()?)
            } else {
                i64::from(i32::from_be_bytes(raw.try_into().ok()?))
            };
            transitions.push((at, locals.get(usize::from(ty))?.clone()));
        }
        Some(Self {
            name: None,
            transitions,
            initial: locals.first()?.clone(),
            rule,
        })
    }

    /// The IANA name the zone was loaded by.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The local time type at Unix time `t`.
    pub fn at(&self, t: i64) -> Local {
        if let Some(rule) = &self.rule
            && self.transitions.last().is_none_or(|(last, _)| t >= *last)
        {
            return rule.at(t);
        }
        match self.transitions.partition_point(|(at, _)| *at <= t) {
            0 => self.initial.clone(),
            n => self.transitions[n - 1].1.clone(),
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

/// A POSIX `TZ` rule: standard time and an optional daylight period.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    std: Local,
    dst: Option<Dst>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Dst {
    local: Local,
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
        let std = Local {
            abbrev: s.name()?,
            offset: -s.hms()?,
        };
        if s.0.is_empty() {
            return Some(Self { std, dst: None });
        }
        let abbrev = s.name()?;
        let offset = if s.0.first().is_some_and(|c| *c != b',') {
            -s.hms()?
        } else {
            std.offset + 3600
        };
        let start = s.transition()?;
        let end = s.transition()?;
        s.0.is_empty().then_some(Self {
            std,
            dst: Some(Dst {
                local: Local { offset, abbrev },
                start,
                end,
            }),
        })
    }

    fn at(&self, t: i64) -> Local {
        let Some(dst) = &self.dst else {
            return self.std.clone();
        };
        let (year, _, _) = civil_from_days((t + self.std.offset).div_euclid(86_400));
        // Start is given in standard local time, end in daylight local time.
        let start = year_start(year) + dst.start.0.day_of_year(year) * 86_400 + dst.start.1
            - self.std.offset;
        let end =
            year_start(year) + dst.end.0.day_of_year(year) * 86_400 + dst.end.1 - dst.local.offset;
        let in_dst = if start <= end {
            t >= start && t < end
        } else {
            t >= start || t < end
        };
        if in_dst {
            dst.local.clone()
        } else {
            self.std.clone()
        }
    }
}

impl DateRule {
    fn day_of_year(self, year: i64) -> i64 {
        match self {
            Self::Julian(n) => n - 1 + i64::from(is_leap(year) && n >= 60),
            Self::Zero(n) => n,
            Self::Month(m, w, d) => {
                let first = days_from_civil(year, m, 1);
                let mut day = 1 + (d - weekday(first)).rem_euclid(7) + (w - 1) * 7;
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
    fn name(&mut self) -> Option<String> {
        let (name, rest) = if self.0.first() == Some(&b'<') {
            let end = self.0.iter().position(|c| *c == b'>')?;
            (&self.0[1..end], &self.0[end + 1..])
        } else {
            let len = self
                .0
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .count();
            if len < 3 {
                return None;
            }
            self.0.split_at(len)
        };
        self.0 = rest;
        String::from_utf8(name.to_vec()).ok()
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

    fn hms(&mut self) -> Option<i64> {
        let sign = if self.eat(b'-') {
            -1
        } else {
            self.eat(b'+');
            1
        };
        let mut secs = self.number()? * 3600;
        if self.eat(b':') {
            secs += self.number()? * 60;
            if self.eat(b':') {
                secs += self.number()?;
            }
        }
        Some(sign * secs)
    }

    fn transition(&mut self) -> Option<(DateRule, i64)> {
        if !self.eat(b',') {
            return None;
        }
        let date = if self.eat(b'M') {
            let m = self.number()?;
            if !self.eat(b'.') {
                return None;
            }
            let w = self.number()?;
            if !self.eat(b'.') {
                return None;
            }
            DateRule::Month(m, w, self.number()?)
        } else if self.eat(b'J') {
            DateRule::Julian(self.number()?)
        } else {
            DateRule::Zero(self.number()?)
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

fn year_start(y: i64) -> i64 {
    days_from_civil(y, 1, 1) * 86_400
}

/// Day of the week of a day count since 1970-01-01, 0 for Sunday.
pub fn weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
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

/// Proleptic Gregorian `(year, month, day)` of a day count since 1970-01-01, by H. Hinnant's
/// `civil_from_days`.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Unix seconds of `text`, an RFC 3339 date-time or a `YYYY-MM-DD` date in UTC; a date stands
/// for its first second, or its last one with `end_of_day`. `None` for anything else.
pub fn parse_utc(text: &str, end_of_day: bool) -> Option<i64> {
    let b = text.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let digits = b.get(from..from + len)?;
        digits.iter().try_fold(0i64, |acc, &d| {
            d.is_ascii_digit().then(|| acc * 10 + i64::from(d - b'0'))
        })
    };
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if !(1..=12).contains(&m) || !(1..=month_len(y, m)).contains(&d) {
        return None;
    }
    let day = days_from_civil(y, m, d) * 86_400;
    if b.len() == 10 {
        return Some(if end_of_day { day + 86_399 } else { day });
    }
    if !matches!(b[10], b'T' | b't' | b' ') || b.get(13) != Some(&b':') || b.get(16) != Some(&b':')
    {
        return None;
    }
    let (hh, mm, ss) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let mut at = 19;
    if b.get(at) == Some(&b'.') {
        at += 1;
        let digits = b[at..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        at += digits;
    }
    let offset = match b.get(at..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let (oh, om) = (num(at + 1, 2)?, num(at + 4, 2)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let offset = oh * 3_600 + om * 60;
            if *sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    Some(day + hh * 3_600 + mm * 60 + ss - offset)
}
