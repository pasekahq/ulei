// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! One extracted page as the model sees it: inline base64 images as placeholders, and a page
//! over its budget cut to a 75 % head and 25 % tail with a footer naming the stored full text.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use ring::digest::{SHA256, digest};

use crate::tools::redact::SPACE;
use crate::tools::{char_byte, hex, re, thousands, write_exclusive};

/// Most characters of a page the stored full text keeps; a marker line follows them.
const MAX_STORED_TEXT_CHARS: usize = 2_000_000;

/// The reference's `convert_base64_images_to_links`: a markdown image with a base64 source
/// becomes `[IMAGE: alt]` (`[IMAGE]` without alt text), any other base64 image `[IMAGE]`.
pub(crate) fn convert_base64_images_to_links(text: &str) -> String {
    let s = SPACE;
    let markdown = re(&format!(
        r"!\[([^\]]*)\]\([{s}]*data:image/[^;]+;base64,[A-Za-z0-9+/={s}]+\)"
    ));
    let out = markdown.replace_all(text, |c: &regex_lite::Captures<'_>| {
        let alt = crate::tools::py_strip(&c[1]);
        if alt.is_empty() {
            "[IMAGE]".to_owned()
        } else {
            format!("[IMAGE: {alt}]")
        }
    });
    let parenthesised = re(&format!(
        r"\([{s}]*data:image/[^;]+;base64,[A-Za-z0-9+/={s}]+\)"
    ));
    let out = parenthesised.replace_all(&out, "[IMAGE]");
    re("data:image/[^;]+;base64,[A-Za-z0-9+/=]+")
        .replace_all(&out, "[IMAGE]")
        .into_owned()
}

/// The reference's `_truncate_with_footer`: `content` whole within `char_limit` characters,
/// else its head and tail cut on line breaks and the footer; the full text goes to
/// `store_dir`. The flag tells whether it was cut.
pub(crate) fn truncate_with_footer(
    content: &str,
    url: Option<&str>,
    char_limit: usize,
    store_dir: &Path,
) -> (String, bool) {
    let total = content.chars().count();
    if total <= char_limit {
        return (content.to_owned(), false);
    }
    let head_budget = char_limit * 3 / 4;
    let tail_budget = char_limit - head_budget;
    let mut head = &content[..char_byte(content, head_budget)];
    let mut tail = &content[char_byte(content, total - tail_budget)..];
    if let Some(nl) = head.rfind('\n')
        && 2 * head[..nl].chars().count() > head_budget
    {
        head = &head[..nl];
    }
    if let Some(nl) = tail.find('\n')
        && 2 * tail[..nl].chars().count() < tail_budget
    {
        tail = &tail[nl + 1..];
    }
    let rule = "\u{2500}".repeat(8);
    let mut footer = format!(
        "\n{rule} [TRUNCATED] {rule}\nShowing {} chars (head) + {} chars (tail) of {} total clean \
         characters.",
        thousands(head.chars().count()),
        thousands(tail.chars().count()),
        thousands(total)
    );
    match store_full_text(url, content, total, store_dir) {
        Some(path) => {
            let path = path.display();
            let middle_start_line = head.matches('\n').count() + 2;
            let _ = write!(
                footer,
                "\nFull text saved to: {path}\nTo read the omitted middle: read_file \
                 path=\"{path}\" offset={middle_start_line} limit=200  (the file is the complete \
                 page; raise/lower offset to page through it)."
            );
        }
        None => footer.push_str(
            "\nFull text could not be stored; re-run web_extract on a more specific URL or use \
             browser_navigate for the complete page.",
        ),
    }
    footer.push('\n');
    footer.push_str(&"\u{2500}".repeat(29));
    let text =
        format!("{head}\n\n[... middle omitted \u{2014} see footer ...]\n\n{tail}\n{footer}");
    (text, true)
}

/// The reference's `_store_full_text`: the page, capped at [`MAX_STORED_TEXT_CHARS`], written
/// to `<host slug>-<sha256 prefix>.md` in `store_dir`; `None` when it could not be written or
/// the page has no URL string.
fn store_full_text(
    url: Option<&str>,
    content: &str,
    total: usize,
    store_dir: &Path,
) -> Option<PathBuf> {
    let url = url?;
    fs::create_dir_all(store_dir).ok()?;
    let host = super::url::urlsplit(url)
        .ok()?
        .hostname()
        .unwrap_or_else(|| "page".to_owned())
        .replace(':', "_");
    let slug: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(60)
        .collect();
    let slug = match slug.trim_matches('-') {
        "" => "page",
        slug => slug,
    };
    let hash = digest(&SHA256, url.as_bytes());
    let hex = hex(&hash.as_ref()[..5]);
    let path = store_dir.join(format!("{slug}-{hex}.md"));
    let stored = if total > MAX_STORED_TEXT_CHARS {
        format!(
            "{}\n\n[... stored copy truncated at {} chars of {}; re-extract a more specific URL \
             for the rest ...]",
            &content[..char_byte(content, MAX_STORED_TEXT_CHARS)],
            thousands(MAX_STORED_TEXT_CHARS),
            thousands(total)
        )
    } else {
        content.to_owned()
    };
    write_exclusive(&path, &stored, 0o666).ok()?;
    Some(path)
}
