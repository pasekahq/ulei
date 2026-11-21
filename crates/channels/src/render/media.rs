// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `MEDIA:<path>` tags and the `[[audio_as_voice]]` / `[[as_document]]` directives of a final
//! reply: the files it asks to deliver, in order, and the text left to show. The tag grammar is
//! matched by hand with the reference patterns' semantics; offsets are in characters.

use std::ops::Range;

use super::pytext::{extension, is_space, path_suffix, strip};

const VOICE_DIRECTIVE: &str = "[[audio_as_voice]]";
const DOCUMENT_DIRECTIVE: &str = "[[as_document]]";
const TAG: &str = "MEDIA:";

/// Extensions a tag is recognised by without a file check, longest first.
const DELIVERY_EXTS: &[&str] = &[
    "geojson", "jpeg", "tiff", "webm", "opus", "flac", "docx", "epub", "xlsx", "yaml", "pptx",
    "html", "json", "png", "jpg", "gif", "bmp", "svg", "mp4", "mov", "avi", "mkv", "3gp", "mp3",
    "m2a", "wav", "ogg", "m4a", "pdf", "doc", "odt", "rtf", "txt", "xls", "ods", "csv", "tsv",
    "xml", "yml", "kmz", "kml", "gpx", "ppt", "odp", "key", "zip", "tar", "tgz", "bz2", "rar",
    "apk", "ipa", "htm", "webp", "md", "gz", "xz", "7z",
];

/// Audio extensions; `[[audio_as_voice]]` marks only these files as voice.
pub(crate) const AUDIO_EXTS: &[&str] = &[".ogg", ".opus", ".mp3", ".m2a", ".wav", ".m4a", ".flac"];

/// Quote and emphasis marks a tag may be wrapped in.
const MARKS: &[char] = &['`', '"', '\'', '*', '_'];

/// Full-width punctuation that ends a tag's path.
const CJK_ENDS: &str = "（）〈〉《》：，。；！？、\u{201c}\u{201d}\u{2018}\u{2019}【】";

/// One file a reply asks to deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFile {
    /// The path as the tag gave it with `~` expanded, or the checked path of an extensionless tag.
    pub path: String,
    /// `[[audio_as_voice]]` was set and the file has an audio extension.
    pub is_voice: bool,
}

/// A final reply split into its files and the text to show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaReply {
    /// Requested files in delivery order, each once.
    pub files: Vec<MediaFile>,
    /// The reply without the tags of `files` and without the directives.
    pub text: String,
    /// `[[as_document]]` was set: images go as documents.
    pub as_document: bool,
}

/// Splits `content` into its `MEDIA:` files and text, as the reference's `extract_media`.
/// `home` expands a leading `~`; `deliverable` checks a path on disk and returns the path to
/// send, for tags whose extension alone does not make them deliverable.
pub fn extract_media(
    content: &str,
    home: Option<&str>,
    deliverable: &dyn Fn(&str) -> Option<String>,
) -> MediaReply {
    let as_document = content.contains(DOCUMENT_DIRECTIVE);
    let has_voice = content.contains(VOICE_DIRECTIVE);
    let cleaned = content
        .replace(VOICE_DIRECTIVE, "")
        .replace(DOCUMENT_DIRECTIVE, "");
    let original: Vec<char> = content.chars().collect();
    let scan = mask(&original, deliverable);
    let mut files: Vec<MediaFile> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for found in matches(&scan, known_tag) {
        let path = normalize(&collect(&scan[found.path.clone()]));
        if path.is_empty() {
            continue;
        }
        let is_voice = has_voice && AUDIO_EXTS.contains(&extension(&path).as_str());
        let expanded = expand_user(&path, home);
        if !seen.contains(&expanded) {
            seen.push(expanded.clone());
            files.push(MediaFile {
                path: expanded,
                is_voice,
            });
        }
    }
    for (_, safe) in extensionless(&scan, deliverable) {
        if !seen.contains(&safe) {
            let ext = extension(&safe);
            files.push(MediaFile {
                path: safe.clone(),
                is_voice: has_voice && AUDIO_EXTS.contains(&ext.as_str()),
            });
            seen.push(safe);
        }
    }
    if files.is_empty() {
        return MediaReply {
            files,
            text: cleaned,
            as_document,
        };
    }
    let text = match without_tags(&cleaned, deliverable) {
        Some(text) => strip(&collapse_blank_lines(&text)).to_owned(),
        None => cleaned,
    };
    MediaReply {
        files,
        text,
        as_document,
    }
}

/// The part of a tag after `MEDIA:` and its spaces: the path range and the match end.
type TagMatcher = fn(&[char], usize) -> Option<(Range<usize>, usize)>;

/// `text` as the reference's `strip_media_directives_for_display` shows it, for a text whose
/// files are not delivered: the directives and the tags of deliverable files removed, runs of
/// blank lines collapsed and trailing spaces trimmed.
pub fn strip_media_for_display(text: &str, deliverable: &dyn Fn(&str) -> Option<String>) -> String {
    if !text.contains(TAG) && !text.contains(VOICE_DIRECTIVE) && !text.contains(DOCUMENT_DIRECTIVE)
    {
        return text.to_owned();
    }
    let cleaned = text
        .replace(VOICE_DIRECTIVE, "")
        .replace(DOCUMENT_DIRECTIVE, "");
    let stripped = without_tags(&cleaned, deliverable).unwrap_or(cleaned);
    collapse_blank_lines(&stripped)
        .trim_end_matches(is_space)
        .to_owned()
}

/// `cleaned` without its tags, located on a masked copy; `None` when it has none.
fn without_tags(cleaned: &str, deliverable: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut chars: Vec<char> = cleaned.chars().collect();
    let masked = mask(&chars, deliverable);
    let mut spans: Vec<(usize, usize)> = matches(&masked, known_tag)
        .into_iter()
        .map(|found| (found.start, found.end))
        .collect();
    spans.extend(
        extensionless(&masked, deliverable)
            .into_iter()
            .map(|(span, _)| span),
    );
    if spans.is_empty() {
        return None;
    }
    for (start, end) in merge(spans).into_iter().rev() {
        chars.drain(start..end);
    }
    Some(collect(&chars))
}

/// A tag match: the whole match and its path group.
struct Found {
    start: usize,
    end: usize,
    path: Range<usize>,
}

/// Non-overlapping matches of `tag` left to right, as `finditer` reports them; `tag` matches
/// the part after `MEDIA:` at an index and returns its path range and the match end.
fn matches(text: &[char], tag: TagMatcher) -> Vec<Found> {
    let mut found = Vec::new();
    let mut from = 0;
    let mut at = 0;
    while at + TAG.len() <= text.len() {
        if !starts_with_ci(text, at, TAG) {
            at += 1;
            continue;
        }
        let marks = text[from..at]
            .iter()
            .rev()
            .take_while(|c| MARKS.contains(c))
            .take(3)
            .count();
        let mut after = at + TAG.len();
        while after < text.len() && is_space(text[after]) {
            after += 1;
        }
        match tag(text, after) {
            Some((path, end)) => {
                found.push(Found {
                    start: at - marks,
                    end,
                    path,
                });
                from = end;
                at = end;
            }
            None => at += 1,
        }
    }
    found
}

/// The tag of a known deliverable extension: a quoted path, or `~/`, `/` or `X:\` and a path
/// that may hold single-line spaces and ends in a known extension; then trailing marks and an
/// optional period.
fn known_tag(text: &[char], at: usize) -> Option<(Range<usize>, usize)> {
    let path_end = quoted(text, at)
        .filter(|&end| known_boundary(text, end))
        .or_else(|| {
            let mut i = anchor(text, at)?;
            if text.get(i).is_none_or(|&c| is_space(c)) {
                return None;
            }
            i += 1;
            while i < text.len() && text[i] != '\n' {
                if text[i] == '.'
                    && !is_space(text[i - 1])
                    && let Some(end) = DELIVERY_EXTS.iter().find_map(|ext| {
                        let end = i + 1 + ext.len();
                        (starts_with_ci(text, i + 1, ext) && known_boundary(text, end))
                            .then_some(end)
                    })
                {
                    return Some(end);
                }
                i += 1;
            }
            None
        })?;
    let mut end = path_end + count_marks(text, path_end);
    if text.get(end) == Some(&'.') {
        end += 1;
    }
    Some((at..path_end, end))
}

/// The tag form checked on disk: a quoted path, or `~/`, `/` or `X:\` and a run without spaces
/// or quotes up to the first boundary; then trailing marks and spaces.
fn extensionless_tag(text: &[char], at: usize) -> Option<(Range<usize>, usize)> {
    let path_end = quoted(text, at)
        .filter(|&end| plain_boundary(text, end))
        .or_else(|| {
            let start = anchor(text, at)?;
            let allowed = |c: char| !is_space(c) && !matches!(c, '`' | '"' | '\'');
            if !text.get(start).is_some_and(|&c| allowed(c)) {
                return None;
            }
            let mut i = start + 1;
            loop {
                if plain_boundary(text, i) {
                    return Some(i);
                }
                if !allowed(text[i]) {
                    return None;
                }
                i += 1;
            }
        })?;
    let mut end = path_end + count_marks(text, path_end);
    while end < text.len() && is_space(text[end]) {
        end += 1;
    }
    Some((at..path_end, end))
}

/// The extensionless tags whose path has no known extension and checks out on disk: the span
/// to remove and the path to send, as `_match_extensionless_path` resolves them.
fn extensionless(
    text: &[char],
    deliverable: &dyn Fn(&str) -> Option<String>,
) -> Vec<((usize, usize), String)> {
    let mut resolved = Vec::new();
    for found in matches(text, extensionless_tag) {
        let path = normalize(&collect(&text[found.path.clone()]));
        if path.is_empty() || !lacks_known_extension(&path) {
            continue;
        }
        if let Some(safe) = deliverable(&path) {
            resolved.push(((found.start, found.path.end), safe));
            continue;
        }
        if let Some((safe, end)) = spaced_path(text, &found.path, deliverable) {
            resolved.push(((found.start, end), safe));
        }
    }
    resolved
}

/// Extends a path that did not check out across single spaces, up to 8 tokens, never past a
/// line end or the next `MEDIA:`; the first candidate that checks out wins.
fn spaced_path(
    text: &[char],
    path: &Range<usize>,
    deliverable: &dyn Fn(&str) -> Option<String>,
) -> Option<(String, usize)> {
    let start = path.start;
    let line_end = text[start..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(text.len(), |n| start + n);
    let mut segment = &text[start..line_end];
    if let Some(next) = (1..segment.len()).find(|&i| starts_with(segment, i, TAG)) {
        segment = &segment[..next];
    }
    let mut pos = path.end - start;
    for _ in 0..8 {
        while pos < segment.len() && matches!(segment[pos], ' ' | '\t') {
            pos += 1;
        }
        if pos >= segment.len() {
            break;
        }
        let mut token_end = pos;
        while token_end < segment.len() && !matches!(segment[token_end], ' ' | '\t') {
            token_end += 1;
        }
        let candidate = normalize(&collect(&segment[..token_end]));
        if let Some(safe) = deliverable(&candidate) {
            return Some((safe, start + token_end));
        }
        pos = token_end;
    }
    None
}

/// End of a `` `...` ``, `"..."` or `'...'` path on one line with at least one character.
fn quoted(text: &[char], at: usize) -> Option<usize> {
    let quote = *text.get(at)?;
    if !matches!(quote, '`' | '"' | '\'') {
        return None;
    }
    let close = text[at + 1..]
        .iter()
        .position(|&c| c == quote || c == '\n')
        .map(|n| at + 1 + n)?;
    (text[close] == quote && close > at + 1).then_some(close + 1)
}

/// The index after a `~/`, `/` or drive-letter anchor at `at`.
fn anchor(text: &[char], at: usize) -> Option<usize> {
    match (text.get(at), text.get(at + 1), text.get(at + 2)) {
        (Some('~'), Some('/'), _) => Some(at + 2),
        (Some('/'), _, _) => Some(at + 1),
        (Some(c), Some(':'), Some('/' | '\\')) if c.is_ascii_alphabetic() => Some(at + 3),
        _ => None,
    }
}

/// What may follow the path of a known-extension tag.
fn known_boundary(text: &[char], at: usize) -> bool {
    let Some(&c) = text.get(at) else {
        return true;
    };
    is_space(c)
        || "`\"'*_,;:)]}[".contains(c)
        || CJK_ENDS.contains(c)
        || starts_with_ci(text, at, TAG)
        || (c == '.' && text.get(at + 1).is_none_or(|&n| is_space(n)))
}

/// What may follow the path of an extensionless tag.
fn plain_boundary(text: &[char], at: usize) -> bool {
    let Some(&c) = text.get(at) else {
        return true;
    };
    is_space(c) || "`\"',;:)]}".contains(c) || CJK_ENDS.contains(c) || starts_with_ci(text, at, TAG)
}

/// Up to three quote or emphasis marks at `at`.
fn count_marks(text: &[char], at: usize) -> usize {
    text[at.min(text.len())..]
        .iter()
        .take_while(|c| MARKS.contains(c))
        .take(3)
        .count()
}

/// `text` with code blocks, inline code, blockquote lines and JSON string values holding a bare
/// `MEDIA:` path blanked to spaces (line feeds kept), so example and stored tags are not
/// delivered; offsets are unchanged.
fn mask(text: &[char], deliverable: &dyn Fn(&str) -> Option<String>) -> Vec<char> {
    let mut masked = text.to_vec();
    for (start, end) in protected_spans(text, deliverable) {
        blank(&mut masked, start, end);
    }
    for (start, end) in json_media_values(&masked.clone()) {
        blank(&mut masked, start, end);
    }
    masked
}

fn blank(chars: &mut [char], start: usize, end: usize) {
    for c in &mut chars[start..end] {
        if *c != '\n' {
            *c = ' ';
        }
    }
}

/// Fenced code blocks, inline code that is not a tag's quoted path nor a whole tag whose path
/// checks out, and lines starting with `>`.
fn protected_spans(
    text: &[char],
    deliverable: &dyn Fn(&str) -> Option<String>,
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut at = 0;
    while at + 3 <= text.len() {
        if !starts_with(text, at, "```") {
            at += 1;
            continue;
        }
        let fence = text[at + 3..]
            .iter()
            .position(|&c| c == '\n')
            .map(|n| at + 3 + n + 1)
            .and_then(|body| {
                (body..text.len())
                    .find(|&i| starts_with(text, i, "```"))
                    .map(|close| close + 3)
            });
        match fence {
            Some(end) => {
                spans.push((at, end));
                at = end;
            }
            None => at += 1,
        }
    }
    let mut at = 0;
    while at < text.len() {
        if text[at] != '`' {
            at += 1;
            continue;
        }
        let Some(end) = quoted(text, at) else {
            at += 1;
            continue;
        };
        let head: String = collect(&text[at.saturating_sub(20)..at]);
        let inner = collect(&text[at + 1..end - 1]);
        let inner = strip(&inner);
        let tag_path = starts_with_ci(&inner.chars().collect::<Vec<_>>(), 0, TAG)
            .then(|| normalize(&inner.chars().skip(TAG.len()).collect::<String>()));
        let deliverable_tag = tag_path.is_some_and(|p| !p.is_empty() && deliverable(&p).is_some());
        if !head.trim_end_matches(is_space).ends_with(TAG) && !deliverable_tag {
            spans.push((at, end));
        }
        at = end;
    }
    let mut line = 0;
    while line < text.len() {
        let end = text[line..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(text.len(), |n| line + n);
        if text[line] == '>' {
            spans.push((line, end));
        }
        line = end + 1;
    }
    spans
}

/// Bodies of JSON string values (a quote after `:`, `,`, `{` or `[` and optional spaces) that
/// hold `MEDIA:` with a bare path.
fn json_media_values(text: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut at = 1;
    while at < text.len() {
        if !":,{[".contains(text[at - 1]) {
            at += 1;
            continue;
        }
        let mut open = at;
        while open < text.len() && is_space(text[open]) {
            open += 1;
        }
        let Some(close) = json_string_end(text, open) else {
            at += 1;
            continue;
        };
        let body = &text[open + 1..close];
        let bare = (0..body.len()).any(|i| {
            if !starts_with(body, i, TAG) {
                return false;
            }
            let mut j = i + TAG.len();
            while j < body.len() && is_space(body[j]) {
                j += 1;
            }
            anchor(body, j).is_some()
        });
        if bare {
            spans.push((open + 1, close));
        }
        at = close + 1;
    }
    spans
}

/// Index of the closing quote of a one-line JSON string opening at `open`.
fn json_string_end(text: &[char], open: usize) -> Option<usize> {
    if text.get(open) != Some(&'"') {
        return None;
    }
    let mut i = open + 1;
    loop {
        match text.get(i)? {
            '"' => return Some(i),
            '\n' => return None,
            '\\' => {
                if matches!(text.get(i + 1), None | Some('\n')) {
                    return None;
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
}

/// Sorted spans with overlapping and nested ones joined.
fn merge(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Runs of three or more line feeds become two.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut feeds = 0;
    for c in text.chars() {
        if c == '\n' {
            feeds += 1;
            if feeds <= 2 {
                out.push(c);
            }
        } else {
            feeds = 0;
            out.push(c);
        }
    }
    out
}

/// A tag's path without its quotes and trailing punctuation.
pub(crate) fn normalize(raw: &str) -> String {
    let mut path = strip(raw);
    let mut ends = path.chars();
    if let (Some(first), Some(last)) = (ends.next(), ends.next_back())
        && first == last
        && "`\"'".contains(first)
    {
        path = strip(&path[first.len_utf8()..path.len() - last.len_utf8()]);
    }
    path.trim_start_matches(['`', '"', '\''])
        .trim_end_matches(['`', '"', '\'', ',', '.', ';', ':', ')', '}', ']'])
        .to_owned()
}

/// `os.path.expanduser` for `~` alone or followed by `/`, with `home` as `$HOME`; other paths
/// unchanged.
pub(crate) fn expand_user(path: &str, home: Option<&str>) -> String {
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            let joined = format!("{}{rest}", home.trim_end_matches('/'));
            if joined.is_empty() {
                "/".to_owned()
            } else {
                joined
            }
        }
        _ => path.to_owned(),
    }
}

/// The path's `PurePath.suffix` is empty or not a known deliverable extension.
fn lacks_known_extension(path: &str) -> bool {
    let ext = path_suffix(path);
    ext.is_empty() || !DELIVERY_EXTS.contains(&&ext[1..])
}

fn starts_with(text: &[char], at: usize, lit: &str) -> bool {
    (at..)
        .zip(lit.chars())
        .all(|(i, c)| text.get(i) == Some(&c))
}

fn starts_with_ci(text: &[char], at: usize, lit: &str) -> bool {
    (at..)
        .zip(lit.chars())
        .all(|(i, c)| text.get(i).is_some_and(|t| t.eq_ignore_ascii_case(&c)))
}

fn collect(chars: &[char]) -> String {
    chars.iter().collect()
}
