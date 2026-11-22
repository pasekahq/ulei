// Port: THIRD_PARTY_NOTICES.md#psf
//! Python's `shlex.shlex` in POSIX mode with `whitespace_split`: the tokens of a shell text,
//! without expanding anything.

/// The tokens of `text` as Python's `shlex` splits them in POSIX mode with `whitespace_split`,
/// the characters of `punctuation` as `punctuation_chars` and, when `comments`, `#` comments
/// running to the end of their line; `None` on an unclosed quote or a trailing escape.
pub(crate) fn split(text: &str, punctuation: &str, comments: bool) -> Option<Vec<String>> {
    let mut lexer = Lexer {
        chars: text.chars(),
        punctuation,
        comments,
        pushback: None,
        ended: false,
    };
    let mut tokens = Vec::new();
    while !lexer.ended {
        lexer.read_token(&mut tokens)?;
    }
    Some(tokens)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LexState {
    Blank,
    Word,
    Punctuation,
    Quote(char),
    Escape,
}

struct Lexer<'a> {
    chars: std::str::Chars<'a>,
    punctuation: &'a str,
    comments: bool,
    /// The character a punctuation run or a word stopped at.
    pushback: Option<char>,
    ended: bool,
}

impl Lexer<'_> {
    /// Adds the next token to `tokens`, none at the end of the text; `None` on a lexing error.
    fn read_token(&mut self, tokens: &mut Vec<String>) -> Option<()> {
        use LexState::{Blank, Escape, Punctuation, Quote, Word};
        let blank = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
        let (comments, punctuation_chars) = (self.comments, self.punctuation);
        let comment = |c: char| comments && c == '#';
        let punctuation = |c: char| punctuation_chars.contains(c);
        let mut state = Blank;
        let mut escaped_from = Word;
        let mut quoted = false;
        let mut token = String::new();
        loop {
            let next = self.pushback.take().or_else(|| self.chars.next());
            match (state, next) {
                (Blank | Word | Punctuation, None) => {
                    self.ended = true;
                    break;
                }
                (Quote(_) | Escape, None) => return None,
                (Blank, Some(c)) if blank(c) => {
                    if !token.is_empty() || quoted {
                        break;
                    }
                }
                (Word | Punctuation, Some(c)) if blank(c) || comment(c) => {
                    if comment(c) {
                        skip_line(&mut self.chars);
                    }
                    state = Blank;
                    if !token.is_empty() || quoted {
                        break;
                    }
                }
                (Blank, Some(c)) if comment(c) => skip_line(&mut self.chars),
                (Punctuation, Some(c)) => {
                    if !punctuation(c) {
                        self.pushback = Some(c);
                        break;
                    }
                    token.push(c);
                }
                (Blank | Word, Some('\\')) => {
                    escaped_from = Word;
                    state = Escape;
                }
                (Blank | Word, Some(c @ ('\'' | '"'))) => state = Quote(c),
                (Blank, Some(c)) => {
                    token.push(c);
                    state = if punctuation(c) { Punctuation } else { Word };
                }
                (Word, Some(c)) if punctuation(c) => {
                    self.pushback = Some(c);
                    state = Blank;
                    if !token.is_empty() || quoted {
                        break;
                    }
                }
                (Word, Some(c)) => token.push(c),
                (Quote(q), Some(c)) => {
                    quoted = true;
                    if c == q {
                        state = Word;
                    } else if c == '\\' && q == '"' {
                        escaped_from = state;
                        state = Escape;
                    } else {
                        token.push(c);
                    }
                }
                (Escape, Some(c)) => {
                    if let Quote(q) = escaped_from
                        && c != '\\'
                        && c != q
                    {
                        token.push('\\');
                    }
                    token.push(c);
                    state = escaped_from;
                }
            }
        }
        if !token.is_empty() || quoted {
            tokens.push(token);
        }
        Some(())
    }
}

/// Consumes `chars` up to and including the next line end.
fn skip_line(chars: &mut std::str::Chars<'_>) {
    for c in chars.by_ref() {
        if c == '\n' {
            break;
        }
    }
}
