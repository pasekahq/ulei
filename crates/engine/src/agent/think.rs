// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Reasoning that a model writes into its reply text instead of a thinking block, and the
//! tool-call markup some models write there instead of a tool call.

use std::sync::OnceLock;

use regex_lite::Regex;

use crate::tools::redact::SPACE;

const REASONING_TAGS: [&str; 5] = [
    "think",
    "thinking",
    "reasoning",
    "REASONING_SCRATCHPAD",
    "thought",
];
const TOOL_CALL_TAGS: [&str; 5] = [
    "tool_call",
    "tool_calls",
    "tool_result",
    "function_call",
    "function_calls",
];

struct Patterns {
    reasoning_blocks: Vec<Regex>,
    tool_call_blocks: Vec<Regex>,
    /// The named function block with the boundary character before it in group 1.
    named_function: Regex,
    unterminated: Regex,
    orphan: Regex,
    stray_closer: Regex,
    think_pair: Regex,
}

/// The compiled patterns, built by the first reply that needs them.
fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let names = REASONING_TAGS.join("|");
        Patterns {
            reasoning_blocks: REASONING_TAGS
                .iter()
                .map(|name| re(&format!("(?is)<{name}>.*?</{name}>")))
                .collect(),
            tool_call_blocks: TOOL_CALL_TAGS
                .iter()
                .map(|name| re(&format!(r"(?is)<{name}\b[^>]*>.*?</{name}>")))
                .collect(),
            named_function: re(&format!(
                r"(?is)(^|[\n\r.!?:])[ \t]*<function\b[^>]*\bname[{SPACE}]*=[^>]*>.*?</function>"
            )),
            unterminated: re(&format!(r"(?is)(?:^|\n)[ \t]*<(?:{names})\b[^>]*>.*$")),
            orphan: re(&format!("(?i)</?(?:{names})>[{SPACE}]*")),
            stray_closer: re(&format!(
                "(?i)</(?:{}|function)>[{SPACE}]*",
                TOOL_CALL_TAGS.join("|")
            )),
            think_pair: re(&format!("(?s)<think>.*?</think>[{SPACE}]*")),
        }
    })
}

/// A constant pattern; one that does not compile is a defect every empty-reply test hits.
fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("think pattern {pattern}: {e}"))
}

/// `text` without reasoning blocks, closed or left open at a line start, stray reasoning tags
/// and tool-call markup.
pub(super) fn strip_blocks(text: &str) -> String {
    let p = patterns();
    let mut text = text.to_owned();
    for pattern in p.reasoning_blocks.iter().chain(&p.tool_call_blocks) {
        text = pattern.replace_all(&text, "").into_owned();
    }
    text = p.named_function.replace_all(&text, "${1}").into_owned();
    for pattern in [&p.unterminated, &p.orphan, &p.stray_closer] {
        text = pattern.replace_all(&text, "").into_owned();
    }
    text
}

/// `text` without its closed `<think>` blocks and the whitespace after each.
pub(super) fn strip_think_pairs(text: &str) -> String {
    patterns().think_pair.replace_all(text, "").into_owned()
}

/// Whether `text` opens a `<think>`, `<thinking>` or `<reasoning>` block.
pub(super) fn has_inline(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["<think>", "<thinking>", "<reasoning>"]
        .iter()
        .any(|tag| lower.contains(tag))
}
