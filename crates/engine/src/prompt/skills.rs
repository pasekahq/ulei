// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The skills index of the system prompt: each category of the skills directory with its
//! `DESCRIPTION.md` text and its skills, names in order, each with the head of its description.

use std::collections::BTreeMap;
use std::path::Path;

use crate::skills::frontmatter::{self, Mapping};
use crate::skills::{Skills, index_files, matches_platform, py_str, read_text, truthy};
use crate::tools::{file_name, py_strip};

/// Longest description of a skill in the index; a longer one is cut to end in `...`.
const SKILL_PROMPT_DESC_LIMIT: usize = 60;

const HEAD: &str = "## Skills (mandatory)\n\
Before replying, scan the skills below. If a skill matches or is even partially relevant to \
your task, you MUST load it with skill_view(name) and follow its instructions. Err on the side \
of loading \u{2014} it is always better to have context you don't need than to miss critical \
steps, pitfalls, or established workflows. Skills contain specialized knowledge \u{2014} API \
endpoints, tool-specific commands, and proven workflows that outperform general-purpose \
approaches. Load the skill even if you think you could handle the task with basic tools like \
web_search or terminal. Skills also encode the user's preferred approach, conventions, and \
quality standards for tasks like code review, planning, and testing \u{2014} load them even for \
tasks you already know how to do, because the skill defines how it should be done here.\n\
After difficult/iterative tasks, offer to save as a skill. If a skill you loaded was missing \
steps, had wrong commands, or needed pitfalls you discovered, update it before finishing.\n\
\n\
<available_skills>\n";

const TAIL: &str = "\n</available_skills>\n\
\n\
Only proceed without loading a skill if genuinely none are relevant to the task.";

/// The index of `skills`; empty when the directory is absent or shows no skill.
pub(super) fn index(skills: &Skills) -> String {
    let dir = &skills.dir;
    if !dir.exists() {
        return String::new();
    }
    let mut categories: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for skill_file in index_files(dir, "SKILL.md") {
        let Some((frontmatter, description)) = parse_skill_file(&skill_file) else {
            continue;
        };
        let parts = relative_parts(dir, &skill_file);
        let (skill_name, category) = match parts.as_slice() {
            [.., skill, _] => {
                let category = if parts.len() > 2 {
                    parts[..parts.len() - 2].join("/")
                } else {
                    parts[0].clone()
                };
                (skill.clone(), category)
            }
            _ => (
                skill_file.parent().map(file_name).unwrap_or_default(),
                "general".to_owned(),
            ),
        };
        let frontmatter_name =
            frontmatter::get(&frontmatter, "name").map_or_else(|| skill_name.clone(), py_str);
        if skills.is_disabled(&frontmatter_name) || skills.is_disabled(&skill_name) {
            continue;
        }
        let shown = if frontmatter_name.is_empty() {
            skill_name
        } else {
            frontmatter_name
        };
        let category = if category.is_empty() {
            "general".to_owned()
        } else {
            category
        };
        categories
            .entry(category)
            .or_default()
            .push((shown, description));
    }
    if categories.is_empty() {
        return String::new();
    }
    let descriptions = category_descriptions(dir);
    let mut lines = Vec::new();
    for (category, mut entries) in categories {
        match descriptions.get(&category).filter(|d| !d.is_empty()) {
            Some(description) => lines.push(format!("  {category}: {description}")),
            None => lines.push(format!("  {category}:")),
        }
        crate::sort::sort_by(&mut entries, |a, b| a.0.cmp(&b.0));
        let mut seen: Vec<&str> = Vec::new();
        for (name, description) in &entries {
            if seen.contains(&name.as_str()) {
                continue;
            }
            seen.push(name);
            if description.is_empty() {
                lines.push(format!("    - {name}"));
            } else {
                lines.push(format!("    - {name}: {description}"));
            }
        }
    }
    format!("{HEAD}{}{TAIL}", lines.join("\n"))
}

/// The frontmatter and index description of a skill file shown on this platform; a file
/// that cannot be read shows with neither.
fn parse_skill_file(path: &Path) -> Option<(Mapping, String)> {
    let Ok(text) = read_text(path, false) else {
        return Some((Vec::new(), String::new()));
    };
    let (frontmatter, _) = frontmatter::parse(&text);
    if !matches_platform(&frontmatter) {
        return None;
    }
    let description = prompt_description(&frontmatter);
    Some((frontmatter, description))
}

/// The `description` stripped of whitespace and quotes, cut to [`SKILL_PROMPT_DESC_LIMIT`].
fn prompt_description(frontmatter: &Mapping) -> String {
    let Some(raw) = frontmatter::get(frontmatter, "description").filter(|v| truthy(v)) else {
        return String::new();
    };
    let text = unquote(&py_str(raw));
    if text.chars().count() > SKILL_PROMPT_DESC_LIMIT {
        let kept: String = text.chars().take(SKILL_PROMPT_DESC_LIMIT - 3).collect();
        return kept + "...";
    }
    text
}

/// The description of each category directory from its `DESCRIPTION.md`, the top directory's
/// under `general`.
fn category_descriptions(dir: &Path) -> BTreeMap<String, String> {
    let mut descriptions = BTreeMap::new();
    for file in index_files(dir, "DESCRIPTION.md") {
        let Ok(text) = read_text(&file, false) else {
            continue;
        };
        let (frontmatter, _) = frontmatter::parse(&text);
        let Some(description) = frontmatter::get(&frontmatter, "description").filter(|v| truthy(v))
        else {
            continue;
        };
        let parts = relative_parts(dir, &file);
        let category = if parts.len() > 1 {
            parts[..parts.len() - 1].join("/")
        } else {
            "general".to_owned()
        };
        descriptions.insert(category, unquote(&py_str(description)));
    }
    descriptions
}

/// `text` stripped of whitespace, then of quote characters.
fn unquote(text: &str) -> String {
    py_strip(text).trim_matches(['\'', '"']).to_owned()
}

/// The components of `path` below `dir`.
fn relative_parts(dir: &Path, path: &Path) -> Vec<String> {
    path.strip_prefix(dir)
        .map(|rel| {
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}
