//! Code spans with backticks inside: a span opened by two or more backticks keeps the single
//! backticks it holds and becomes one `MarkdownV2` code entity (ADR-0019, item 4).

use ulei_channels::render::{split_markdown_v2, strip_markdown_v2, to_markdown_v2};

/// The code entities Telegram reads from `MarkdownV2` text: `\` escapes the next character in
/// and out of code, a lone backtick opens or closes a span.
fn code_entities(md: &str) -> Vec<String> {
    let (mut spans, mut cur, mut open) = (Vec::new(), String::new(), false);
    let mut chars = md.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let next = chars.next().expect("escape has a character");
                if open {
                    cur.push(next);
                }
            }
            '`' if open => {
                spans.push(std::mem::take(&mut cur));
                open = false;
            }
            '`' => open = true,
            other if open => cur.push(other),
            _ => {}
        }
    }
    assert!(!open, "unclosed code span in {md:?}");
    spans
}

const COMMAND: &str =
    "Use the terminal tool to run `sleep 120; echo late`, then reply with its output only.";

#[test]
fn a_double_backtick_span_with_inner_backticks_is_one_code_entity() {
    let md = to_markdown_v2(&format!("7. ``{COMMAND}``, then `/stop`"));
    assert_eq!(
        md,
        "7\\. `Use the terminal tool to run \\`sleep 120; echo late\\`, then reply with its output \
         only.`, then `/stop`"
    );
    assert_eq!(code_entities(&md), [COMMAND, "/stop"]);
}

#[test]
fn one_space_at_each_end_of_a_double_backtick_span_is_dropped() {
    let md = to_markdown_v2("run `` `ls` `` now");
    assert_eq!(code_entities(&md), ["`ls`"]);
    assert_eq!(strip_markdown_v2(&md), "run ``ls`` now");
}

#[test]
fn the_plain_fallback_and_the_splitter_keep_the_whole_span() {
    let md = to_markdown_v2(&format!("``{COMMAND}``"));
    // The plain text keeps the code span's own backticks, as the reference does for `code`.
    assert_eq!(strip_markdown_v2(&md), format!("`{COMMAND}`"));
    assert_eq!(split_markdown_v2(&md), std::slice::from_ref(&md));
}

#[test]
fn spans_without_a_matching_run_stay_as_the_reference_renders_them() {
    assert_eq!(to_markdown_v2("``"), "\\`\\`");
    assert_eq!(to_markdown_v2("`a``b`"), "`a``b`");
    assert_eq!(to_markdown_v2("`code`"), "`code`");
    // A paragraph break ends the search for the closing run; the single-backtick rule applies.
    assert_eq!(to_markdown_v2("``a\n\nb``"), "\\``a\n\nb`\\`");
}
