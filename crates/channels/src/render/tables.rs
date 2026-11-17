// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! GFM pipe tables -> bold heading plus bullet groups, because `MarkdownV2` has no tables.

use super::pytext::{is_space, lstrip, strip};

/// Rewrites every GFM pipe table outside fenced code into one group per data row:
/// `**heading**` and `• header: value` bullets. Text without a table comes back unchanged.
pub(super) fn tables_to_bullets(text: &str) -> String {
    if !text.contains('|') || !text.contains('-') {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if lstrip(line).starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence
            && line.contains('|')
            && lines.get(i + 1).is_some_and(|l| is_separator(l))
        {
            let mut j = i + 2;
            while j < lines.len() && is_table_row(lines[j]) {
                j += 1;
            }
            out.push(render_block(&lines[i..j]));
            i = j;
            continue;
        }
        out.push(line.to_owned());
        i += 1;
    }
    out.join("\n")
}

fn is_table_row(line: &str) -> bool {
    let stripped = strip(line);
    !stripped.is_empty() && stripped.contains('|')
}

/// `| a | b |` -> `["a", "b"]`: one outer pipe trimmed on each side, cells stripped.
fn split_row(row: &str) -> Vec<String> {
    let s = strip(row);
    let s = s.strip_prefix('|').unwrap_or(s);
    let s = s.strip_suffix('|').unwrap_or(s);
    s.split('|').map(|c| strip(c).to_owned()).collect()
}

/// Delimiter row: optional outer pipes, at least two `:?-+:?` cells separated by `|`.
fn is_separator(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let skip_ws = |mut p: usize| {
        while p < chars.len() && is_space(chars[p]) {
            p += 1;
        }
        p
    };
    // `:?-+:?` then trailing whitespace; returns the position after it.
    let cell = |mut p: usize| {
        if chars.get(p) == Some(&':') {
            p += 1;
        }
        let dashes = p;
        while chars.get(p) == Some(&'-') {
            p += 1;
        }
        if p == dashes {
            return None;
        }
        if chars.get(p) == Some(&':') {
            p += 1;
        }
        Some(skip_ws(p))
    };
    let mut p = skip_ws(0);
    if chars.get(p) == Some(&'|') {
        p += 1;
    }
    let Some(mut p) = cell(skip_ws(p)) else {
        return false;
    };
    let mut more_cells = 0;
    while chars.get(p) == Some(&'|') {
        match cell(skip_ws(p + 1)) {
            Some(next) => {
                p = next;
                more_cells += 1;
            }
            None => break,
        }
    }
    if more_cells == 0 {
        return false;
    }
    if chars.get(p) == Some(&'|') {
        p += 1;
    }
    skip_ws(p) == chars.len()
}

fn render_block(block: &[&str]) -> String {
    if block.len() < 3 {
        return block.join("\n");
    }
    let headers = split_row(block[0]);
    if headers.len() < 2 {
        return block.join("\n");
    }
    let has_row_label_col = split_row(block[2]).len() == headers.len() + 1;
    let mut groups = Vec::with_capacity(block.len() - 2);
    for (index, row) in block[2..].iter().enumerate() {
        let index = index + 1;
        let cells = split_row(row);
        let (heading, mut data) = if has_row_label_col {
            let heading = cells
                .first()
                .filter(|c| !c.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Row {index}"));
            (heading, cells.get(1..).unwrap_or_default().to_vec())
        } else {
            let heading = cells
                .iter()
                .find(|c| !c.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Row {index}"));
            (heading, cells)
        };
        data.resize(headers.len(), String::new());
        let mut lines = vec![format!("**{heading}**")];
        for (header, value) in headers.iter().zip(&data) {
            if !has_row_label_col && *value == heading {
                continue;
            }
            lines.push(format!("• {header}: {value}"));
        }
        groups.push(lines.join("\n"));
    }
    groups.join("\n\n")
}
