//! Glob patterns over `/`-separated relative paths: `*` and `?` within one component, `[...]`
//! classes (`[!...]` negated) and `**` for any number of components.

/// A glob over `/`-separated relative paths.
#[derive(Debug)]
pub(super) struct Pattern {
    segments: Vec<Segment>,
}

#[derive(Debug)]
enum Segment {
    /// `**`: zero or more whole components.
    AnyDirs,
    Name(Vec<Token>),
}

#[derive(Debug)]
enum Token {
    Char(char),
    AnyChar,
    Star,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl Token {
    fn matches(&self, c: char) -> bool {
        match self {
            Self::Char(want) => *want == c,
            Self::AnyChar => true,
            Self::Star => false,
            Self::Class { negated, ranges } => {
                ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&c)) != *negated
            }
        }
    }
}

impl Pattern {
    /// Parses a relative glob; empty components and `.` are ignored.
    ///
    /// # Errors
    /// The text of the problem: an empty or absolute pattern, an unclosed `[`, a trailing `\`.
    pub(super) fn parse(pattern: &str) -> Result<Self, String> {
        if pattern.starts_with('/') {
            return Err("glob pattern must be relative; put the directory in `path`".into());
        }
        let mut segments = Vec::new();
        for part in pattern.split('/').filter(|p| !p.is_empty() && *p != ".") {
            if part == "**" {
                if !matches!(segments.last(), Some(Segment::AnyDirs)) {
                    segments.push(Segment::AnyDirs);
                }
            } else {
                segments.push(Segment::Name(tokens(part)?));
            }
        }
        if segments.is_empty() {
            return Err("glob pattern is empty".into());
        }
        Ok(Self { segments })
    }

    /// Whether the pattern names a single component, so a filter applies it to file names.
    pub(super) fn is_name(&self) -> bool {
        matches!(self.segments.as_slice(), [Segment::Name(_)])
    }

    /// Whether the path with these components matches.
    pub(super) fn matches(&self, comps: &[String]) -> bool {
        self.accepts(comps, false)
    }

    /// Whether some path below the directory with these components can match.
    pub(super) fn may_match_below(&self, comps: &[String]) -> bool {
        self.accepts(comps, true)
    }

    /// `at[i][j]`: segments `i..` match components `j..`, or with `below` can match them
    /// followed by at least one more component.
    fn accepts(&self, comps: &[String], below: bool) -> bool {
        let (s, c) = (self.segments.len(), comps.len());
        let names: Vec<Vec<char>> = comps.iter().map(|n| n.chars().collect()).collect();
        let mut at = vec![false; (s + 1) * (c + 1)];
        let idx = |i: usize, j: usize| i * (c + 1) + j;
        for i in (0..=s).rev() {
            at[idx(i, c)] = if below {
                i < s
            } else {
                self.segments[i..]
                    .iter()
                    .all(|seg| matches!(seg, Segment::AnyDirs))
            };
        }
        for j in (0..c).rev() {
            for i in (0..s).rev() {
                at[idx(i, j)] = match &self.segments[i] {
                    Segment::AnyDirs => at[idx(i + 1, j)] || at[idx(i, j + 1)],
                    Segment::Name(tokens) => {
                        at[idx(i + 1, j + 1)] && name_matches(tokens, &names[j])
                    }
                };
            }
        }
        at[idx(0, 0)]
    }
}

fn tokens(part: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = part.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        i += 1;
        let token = match c {
            '*' if matches!(out.last(), Some(Token::Star)) => continue,
            '*' => Token::Star,
            '?' => Token::AnyChar,
            '\\' => {
                let escaped = chars
                    .get(i)
                    .ok_or_else(|| format!("glob component `{part}` ends with `\\`"))?;
                i += 1;
                Token::Char(*escaped)
            }
            '[' => {
                let (token, next) =
                    class(&chars, i).ok_or_else(|| format!("unclosed `[` in `{part}`"))?;
                i = next;
                token
            }
            c => Token::Char(c),
        };
        out.push(token);
    }
    Ok(out)
}

/// The class whose body starts at `i`, and the index after its `]`; a `]` first in the body
/// (after any `!` or `^`) is literal.
fn class(chars: &[char], mut i: usize) -> Option<(Token, usize)> {
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let body = i;
    let mut ranges = Vec::new();
    loop {
        let lo = *chars.get(i)?;
        i += 1;
        if lo == ']' && i - 1 > body {
            return Some((Token::Class { negated, ranges }, i));
        }
        let hi = match (chars.get(i), chars.get(i + 1)) {
            (Some('-'), Some(&hi)) if hi != ']' => {
                i += 2;
                hi
            }
            _ => lo,
        };
        ranges.push((lo, hi));
    }
}

/// Wildcard match of one component; `*` backtracks to its latest position only.
fn name_matches(tokens: &[Token], name: &[char]) -> bool {
    let (mut t, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match tokens.get(t) {
            Some(Token::Star) => {
                star = Some((t, n));
                t += 1;
                continue;
            }
            Some(token) if token.matches(name[n]) => {
                t += 1;
                n += 1;
                continue;
            }
            _ => {}
        }
        match star {
            Some((st, sn)) => {
                star = Some((st, sn + 1));
                t = st + 1;
                n = sn + 1;
            }
            None => return false,
        }
    }
    tokens[t..].iter().all(|token| matches!(token, Token::Star))
}
