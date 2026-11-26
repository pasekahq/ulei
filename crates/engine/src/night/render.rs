//! Archive history as transcripts (ADR-0078 section 2, stage c): entries grouped into sessions
//! by the part of their `external_id` before the last separator, the messages of the kept roles
//! in time order, each cut to a character cap, numbered, and split into parts that fit the
//! prompt's token budget at message boundaries.

use std::fmt::Write as _;

use crate::night::spend::estimate_tokens;
use crate::sort;
use crate::wire::c10::Listed;
use crate::zone;

/// What a cut message ends with.
pub const CUT_MARK: &str = " [...]";

/// One message of a transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The entry's `seq`.
    pub seq: u64,
    /// The item's id.
    pub id: String,
    /// The author.
    pub who: String,
    /// Unix time of the message, when the source gave one.
    pub at: Option<f64>,
    /// The text, cut to the cap and marked when cut.
    pub text: String,
}

/// The messages of one session in one window of entries.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The session: the `external_id` before its last separator.
    pub key: String,
    /// Its messages of the kept roles, in time order; empty when it has none.
    pub messages: Vec<Message>,
    /// Entries of the session left out by their role.
    pub left_out: usize,
}

/// One extraction unit: a session, or a part of one that does not fit one call.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    /// The unit's key in the cursor: the session, with `#<n>` for part `n` of a split session.
    pub key: String,
    /// The session.
    pub session: String,
    /// Its place among the session's parts, from 1.
    pub index: usize,
    /// The number of parts of the session.
    pub count: usize,
    /// The messages, numbered from 1 in the transcript.
    pub messages: Vec<Message>,
    /// The transcript the model reads.
    pub transcript: String,
    /// Its estimated tokens.
    pub tokens: u64,
}

impl Part {
    /// The time span of its messages: the earliest and the latest `at`, when any has one.
    #[must_use]
    pub fn span(&self) -> Option<(f64, f64)> {
        self.messages
            .iter()
            .filter_map(|m| m.at)
            .fold(None, |span, at| match span {
                None => Some((at, at)),
                Some((from, to)) => Some((f64::min(from, at), f64::max(to, at))),
            })
    }
}

/// The session of `external_id`: the part before the last `separator`; `None` for an id without
/// it, the session's own row.
#[must_use]
pub fn session_of<'a>(external_id: &'a str, separator: &str) -> Option<&'a str> {
    if separator.is_empty() {
        return None;
    }
    external_id
        .rfind(separator)
        .map(|at| &external_id[..at])
        .filter(|session| !session.is_empty())
}

/// `text` cut to `chars` characters, marked when cut or when the archive cut it already.
#[must_use]
pub fn cut(text: &str, chars: usize, truncated: bool) -> String {
    let text = text.trim();
    match text.char_indices().nth(chars) {
        Some((at, _)) => format!("{}{CUT_MARK}", &text[..at]),
        None if truncated => format!("{text}{CUT_MARK}"),
        None => text.to_owned(),
    }
}

/// The sessions of `entries`, in the order of their first entry, each with the messages whose
/// `who` is one of `roles`, cut to `message_chars`, in time order (by `at`, then `seq`) when
/// every message has a time, else in `seq` order. A session's own row is left out.
#[must_use]
pub fn group(
    entries: &[Listed],
    separator: &str,
    roles: &[String],
    message_chars: usize,
) -> Vec<Session> {
    let mut sessions: Vec<Session> = Vec::new();
    for entry in entries {
        let Some(key) = session_of(&entry.external_id, separator) else {
            continue;
        };
        let at = if let Some(at) = sessions.iter().rposition(|s| s.key == key) {
            at
        } else {
            sessions.push(Session {
                key: key.to_owned(),
                messages: Vec::new(),
                left_out: 0,
            });
            sessions.len() - 1
        };
        let session = &mut sessions[at];
        let who = entry.who.as_deref().unwrap_or_default();
        if !roles.iter().any(|r| r == who) || entry.text.trim().is_empty() {
            session.left_out += 1;
            continue;
        }
        session.messages.push(Message {
            seq: entry.seq,
            id: entry.id.clone(),
            who: who.to_owned(),
            at: entry.at,
            text: cut(
                &entry.text,
                message_chars,
                entry.text_truncated == Some(true),
            ),
        });
    }
    for session in &mut sessions {
        if session.messages.iter().all(|m| m.at.is_some()) {
            sort::sort_by(&mut session.messages, |a, b| {
                a.at.unwrap_or_default()
                    .total_cmp(&b.at.unwrap_or_default())
                    .then(a.seq.cmp(&b.seq))
            });
        }
    }
    sessions
}

/// The minute of `at` in UTC, `YYYY-MM-DD HH:MM`.
fn minute(at: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let stamp = zone::rfc3339(at.max(0.0) as u64);
    format!("{} {}", &stamp[..10], &stamp[11..16])
}

/// One numbered line of a transcript.
fn line(number: usize, message: &Message) -> String {
    let when = message.at.map(minute).unwrap_or_default();
    let space = if when.is_empty() { "" } else { " " };
    format!(
        "[{number}]{space}{when} {}: {}\n",
        message.who, message.text
    )
}

/// The transcript of `messages`, part `index` of `count` of `session` of `source`.
#[must_use]
pub fn transcript(
    source: &str,
    session: &str,
    index: usize,
    count: usize,
    messages: &[Message],
) -> String {
    let mut text = format!("Source: {source}\nSession: {session}\n");
    if count > 1 {
        let _ = writeln!(text, "Part {index} of {count}");
    }
    let times: Vec<f64> = messages.iter().filter_map(|m| m.at).collect();
    if let (Some(from), Some(to)) = (
        times.iter().copied().reduce(f64::min),
        times.iter().copied().reduce(f64::max),
    ) {
        let _ = writeln!(text, "Time (UTC): {} to {}", minute(from), minute(to));
    }
    text.push('\n');
    for (n, message) in messages.iter().enumerate() {
        text.push_str(&line(n + 1, message));
    }
    text
}

/// The parts of `session` of `source`: one part while its transcript's estimate stays within
/// `budget` tokens, else consecutive runs of messages that each fit (at least one message each).
/// A session without messages has none.
#[must_use]
pub fn parts(source: &str, session: &Session, budget: u64) -> Vec<Part> {
    if session.messages.is_empty() {
        return Vec::new();
    }
    let header = estimate_tokens(&transcript(source, &session.key, 1, 2, &[]));
    let mut runs: Vec<Vec<Message>> = vec![Vec::new()];
    let mut used = header;
    for message in &session.messages {
        let cost = estimate_tokens(&line(99_999, message));
        let current = runs.last().map_or(0, Vec::len);
        if current > 0 && used + cost > budget {
            runs.push(Vec::new());
            used = header;
        }
        used += cost;
        if let Some(run) = runs.last_mut() {
            run.push(message.clone());
        }
    }
    let count = runs.len();
    runs.into_iter()
        .enumerate()
        .map(|(i, messages)| {
            let transcript = transcript(source, &session.key, i + 1, count, &messages);
            Part {
                key: if count == 1 {
                    session.key.clone()
                } else {
                    format!("{}#{}", session.key, i + 1)
                },
                session: session.key.clone(),
                index: i + 1,
                count,
                tokens: estimate_tokens(&transcript),
                transcript,
                messages,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, external_id: &str, who: &str, at: Option<f64>, text: &str) -> Listed {
        Listed {
            seq,
            id: format!("{seq:064x}"),
            external_id: external_id.to_owned(),
            who: Some(who.to_owned()),
            at,
            kind: "text/plain".to_owned(),
            put_at: 1.0,
            text: text.to_owned(),
            text_truncated: None,
        }
    }

    fn roles() -> Vec<String> {
        vec!["user".to_owned(), "assistant".to_owned()]
    }

    #[test]
    fn the_session_is_the_id_before_the_last_separator() {
        assert_eq!(session_of("s-1/m-2", "/"), Some("s-1"));
        assert_eq!(session_of("key/7/12", "/"), Some("key/7"));
        assert_eq!(session_of("s-1", "/"), None);
        assert_eq!(session_of("/m", "/"), None);
        assert_eq!(session_of("s-1::m-2", "::"), Some("s-1"));
        assert_eq!(session_of("s-1/m", ""), None);
    }

    #[test]
    fn cut_marks_long_and_archive_cut_text() {
        assert_eq!(cut("  hello  ", 10, false), "hello");
        assert_eq!(cut("привет мир", 6, false), "привет [...]");
        assert_eq!(cut("short", 10, true), "short [...]");
    }

    #[test]
    fn group_keeps_roles_orders_by_time_and_skips_session_rows() {
        let entries = vec![
            entry(1, "s-1", "system", None, "session row"),
            entry(2, "s-1/m-2", "assistant", Some(200.0), "second"),
            entry(3, "s-2/m-1", "user", Some(50.0), "other session"),
            entry(4, "s-1/m-1", "user", Some(100.0), "first"),
            entry(5, "s-1/m-3", "tool", Some(300.0), "tool output"),
            entry(6, "s-1/m-4", "system", Some(400.0), "system text"),
            entry(7, "s-1/m-5", "user", Some(500.0), "   "),
        ];
        let sessions = group(&entries, "/", &roles(), 100);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].key, "s-1");
        let texts: Vec<&str> = sessions[0]
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(sessions[0].left_out, 3);
        assert_eq!(sessions[1].key, "s-2");
    }

    #[test]
    fn group_keeps_seq_order_without_times() {
        let entries = vec![
            entry(1, "s/b", "user", None, "one"),
            entry(2, "s/a", "assistant", Some(1.0), "two"),
        ];
        let sessions = group(&entries, "/", &roles(), 100);
        assert_eq!(sessions[0].messages[0].text, "one");
    }

    #[test]
    fn transcript_numbers_messages_with_their_minute() {
        let entries = vec![
            entry(
                1,
                "s/1",
                "user",
                Some(1_700_000_000.0),
                "Ada moved to Sample Road.",
            ),
            entry(2, "s/2", "assistant", Some(1_700_000_060.0), "Noted."),
        ];
        let session = &group(&entries, "/", &roles(), 100)[0];
        let parts = parts("old-runtime", session, 60_000);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].key, "s");
        assert_eq!(
            parts[0].transcript,
            "Source: old-runtime\nSession: s\nTime (UTC): 2023-11-14 22:13 to 2023-11-14 22:14\n\n\
             [1] 2023-11-14 22:13 user: Ada moved to Sample Road.\n\
             [2] 2023-11-14 22:14 assistant: Noted.\n"
        );
        assert_eq!(parts[0].span(), Some((1_700_000_000.0, 1_700_000_060.0)));
    }

    #[test]
    fn a_long_session_splits_at_message_boundaries() {
        let text = "x".repeat(700);
        let entries: Vec<Listed> = (1..=10)
            .map(|n| {
                entry(
                    n,
                    &format!("s/{n}"),
                    "user",
                    Some(f64::from(u32::try_from(n).unwrap())),
                    &text,
                )
            })
            .collect();
        let session = &group(&entries, "/", &roles(), 1_000)[0];
        let parts = parts("src", session, 500);
        assert_eq!(parts.len(), 5);
        assert!(parts.iter().all(|p| p.messages.len() == 2 && p.count == 5));
        assert_eq!(parts[0].key, "s#1");
        assert_eq!(parts[4].key, "s#5");
        assert!(parts[0].transcript.contains("Part 1 of 5"));
        assert!(parts.iter().all(|p| p.tokens <= 500));
        // One message over the budget still makes a part of its own.
        let one = &group(&entries[..1], "/", &roles(), 1_000)[0];
        assert_eq!(super::parts("src", one, 10).len(), 1);
        let empty = Session {
            key: "e".to_owned(),
            messages: Vec::new(),
            left_out: 2,
        };
        assert_eq!(super::parts("src", &empty, 100).len(), 0);
    }
}
