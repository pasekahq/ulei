// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Bot API failures as values, their classifiers and the token redaction of error text.

use std::fmt;
use std::time::Duration;

/// What failed, as the reference's Telegram library classifies a failed request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// HTTP 401 or 404: the token is malformed or revoked; never heals.
    Unauthorized,
    /// HTTP 403: the bot was blocked, kicked or lacks rights.
    Forbidden,
    /// HTTP 400: the request was refused; the description says why.
    BadRequest,
    /// HTTP 409: another `getUpdates` or a webhook holds the bot.
    Conflict,
    /// Flood control (`parameters.retry_after`, usually with HTTP 429).
    RetryAfter(Duration),
    /// The group became a supergroup (`parameters.migrate_to_chat_id`).
    ChatMigrated(i64),
    /// Any other non-2xx status (5xx, 429 without `retry_after`, ...).
    Server(u16),
    /// The connection failed, was reset or dropped, or the HTTP exchange was malformed.
    Network,
    /// Resolve or connect timed out: the request never left, so a resend cannot duplicate.
    ConnectTimeout,
    /// The response did not arrive in time: the request may have reached Telegram.
    Timeout,
    /// A 2xx response that is not a Bot API envelope or lacks the expected result.
    InvalidResponse,
    /// A file is larger than the download limit.
    TooLarge,
    /// A local file could not be read or written.
    Io,
}

/// A failed Bot API call. Display and `Debug` carry no token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// Class of the failure.
    pub kind: ErrorKind,
    /// Telegram's `description` for API errors, else a short local text; redacted.
    pub description: String,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, description: &str, token: &str) -> Self {
        Self {
            kind,
            description: redact(description, token),
        }
    }

    /// The server-requested wait of a flood-control failure.
    pub fn retry_after(&self) -> Option<Duration> {
        match self.kind {
            ErrorKind::RetryAfter(wait) => Some(wait),
            _ => None,
        }
    }

    /// Another poller or a webhook holds the bot (HTTP 409 or its texts).
    pub fn is_polling_conflict(&self) -> bool {
        let text = self.description.to_lowercase();
        self.kind == ErrorKind::Conflict
            || text.contains("terminated by other getupdates request")
            || text.contains("another bot instance is running")
    }

    /// A credential failure that reconnecting can never fix.
    pub fn is_auth_error(&self) -> bool {
        matches!(self.kind, ErrorKind::Unauthorized | ErrorKind::Forbidden)
    }

    /// A transient transport failure that warrants a reconnect.
    pub fn is_network_error(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Network
                | ErrorKind::Server(_)
                | ErrorKind::ConnectTimeout
                | ErrorKind::Timeout
                | ErrorKind::Io
        )
    }

    /// The connection was never established, so a resend is safe.
    pub fn is_connect_timeout(&self) -> bool {
        self.kind == ErrorKind::ConnectTimeout
    }

    /// Telegram refused the request (HTTP 400).
    pub fn is_bad_request(&self) -> bool {
        self.kind == ErrorKind::BadRequest
    }

    /// A transient network failure that never reached Telegram.
    pub fn is_retryable(&self) -> bool {
        matches!(self.kind, ErrorKind::Network | ErrorKind::ConnectTimeout)
    }

    /// A response timeout: not safe to resend, the message may have been delivered.
    pub fn is_timeout(&self) -> bool {
        self.kind == ErrorKind::Timeout
    }

    /// The text was longer than Telegram accepts for one message.
    pub fn is_message_too_long(&self) -> bool {
        let text = self.description.to_lowercase();
        text.contains("message_too_long") || text.contains("too long")
    }

    /// A typing failure worth a per-chat cooldown (flood, 5xx, timeouts, transport).
    pub fn is_transient_typing_error(&self) -> bool {
        let text = self.description.to_lowercase();
        let marked = [
            "too many requests",
            "rate limit",
            "timed out",
            "timeout",
            "temporar",
        ]
        .iter()
        .any(|marker| text.contains(marker));
        marked
            || match self.kind {
                ErrorKind::RetryAfter(_)
                | ErrorKind::Network
                | ErrorKind::ConnectTimeout
                | ErrorKind::Timeout
                | ErrorKind::Io => true,
                ErrorKind::Server(status) => status == 429 || status >= 500,
                _ => false,
            }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "telegram {:?}: {}", self.kind, self.description)
    }
}

impl std::error::Error for Error {}

/// Masks the configured token and anything shaped like a bot token
/// (`[bot]<8+ digits>:<30+ token chars>`) as `<digits>:***`.
pub fn redact(text: &str, token: &str) -> String {
    let masked = match token.split_once(':') {
        Some((id, _)) if !token.is_empty() => text.replace(token, &format!("{id}:***")),
        _ if !token.is_empty() => text.replace(token, "***"),
        _ => text.to_owned(),
    };
    redact_token_shapes(&masked)
}

fn redact_token_shapes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let starts_run = bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_digit());
        if starts_run {
            let digits_end = run_end(bytes, i, |b| b.is_ascii_digit());
            if digits_end - i >= 8 && bytes.get(digits_end) == Some(&b':') {
                let secret_end = run_end(bytes, digits_end + 1, is_token_char);
                if secret_end - digits_end > 30 {
                    out.push_str(&text[copied..=digits_end]);
                    out.push_str("***");
                    copied = secret_end;
                    i = secret_end;
                    continue;
                }
            }
            i = digits_end;
            continue;
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

fn run_end(bytes: &[u8], start: usize, keep: impl Fn(u8) -> bool) -> usize {
    bytes[start..]
        .iter()
        .position(|&b| !keep(b))
        .map_or(bytes.len(), |n| start + n)
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_configured_token_and_token_shapes() {
        let shaped = format!("bot12345678:{}", "A".repeat(32));
        let text = format!("GET /bot123456:TEST/getMe and {shaped}/sendMessage failed");
        assert_eq!(
            redact(&text, "123456:TEST"),
            "GET /bot123456:***/getMe and bot12345678:***/sendMessage failed"
        );
    }

    #[test]
    fn keeps_short_digit_runs() {
        assert_eq!(redact("chat 1001: ok 12:30", ""), "chat 1001: ok 12:30");
    }
}
