// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Intentional silence: the agent answers with a bare marker when it chooses not to reply.

use super::pytext::{is_punctuation, is_space, strip};

/// Whole-answer markers of intentional silence.
const SILENT_MARKERS: [&str; 4] = ["[SILENT]", "SILENT", "NO_REPLY", "NO REPLY"];
/// Longest answer, in characters, that can still be a marker.
const MAX_MARKER_LEN: usize = 64;

/// True only when `answer` is exactly a silence marker, up to case, inner whitespace and stray
/// edge punctuation; square brackets stay structural. A blank answer and prose that mentions a
/// marker are not silence.
pub fn is_intentional_silence(answer: &str) -> bool {
    let stripped = strip(answer);
    if stripped.is_empty() || stripped.chars().count() > MAX_MARKER_LEN {
        return false;
    }
    [stripped, strip_edge_punctuation(stripped)]
        .into_iter()
        .any(|candidate| SILENT_MARKERS.contains(&canonical(candidate).as_str()))
}

/// Upper case with whitespace runs collapsed to one space and the ends trimmed.
fn canonical(text: &str) -> String {
    text.to_uppercase()
        .split(is_space)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` without the punctuation at its ends, except square brackets, so `[SILENT` keeps its
/// bracket and does not become `SILENT`.
fn strip_edge_punctuation(text: &str) -> &str {
    let stray = |c: char| c != '[' && c != ']' && is_punctuation(c);
    strip(text.trim_start_matches(stray).trim_end_matches(stray))
}
