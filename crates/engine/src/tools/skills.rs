// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `skills_list` and `skill_view`: the skills of the skills directory, the `SKILL.md` of one
//! skill and the files inside its directory, never a file outside it.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::glob::Pattern;
use super::{
    Args, Context, Output, SKILL_VIEW, SKILLS_LIST, Tool, file_name, json_array, json_object,
    json_str, object_schema, os_error_text, py_strip, py_suffix, realpath, repr,
};
use crate::skills::frontmatter::{self, Mapping, Yaml};
use crate::skills::{
    SUPPORT_DIRS, Skills, in_excluded_dir, index_files, is_support_path, matches_platform, py_str,
    read_text, truthy,
};

/// Longest skill name `skills_list` shows; a longer one is cut.
pub const MAX_NAME_LENGTH: usize = 64;
/// Longest description `skills_list` shows; a longer one is cut to end in `...`.
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;
/// Characters of a `SKILL.md` that `skills_list` reads for its frontmatter.
const LIST_SCAN_CHARS: usize = 4000;
/// Most skills a not-found answer of `skill_view` names.
const NOT_FOUND_SUGGESTIONS: usize = 20;
/// Most served views the repeat-view dedup remembers.
const VIEW_DEDUP_CAP: usize = 200;

/// The answer to a repeat view of an unchanged skill file.
pub const VIEW_DEDUP_MESSAGE: &str = "Skill content unchanged since it was loaded earlier in this \
    conversation \u{2014} refer to the earlier skill_view result; it is still current and \
    complete. (Re-issued after context compression, this returns the full content again.)";

const LIST_DESCRIPTION: &str =
    "List available skills (name + description). Use skill_view(name) to load full content.";
const VIEW_DESCRIPTION: &str = "Skills allow for loading information about specific tasks and \
    workflows, as well as scripts and templates. Load a skill's full content or access its \
    linked files (references, templates, scripts). First call returns SKILL.md content plus a \
    'linked_files' dict showing available references/templates/scripts. To access those, call \
    again with file_path parameter.";
const LIST_HINT: &str = "Use skill_view(name) to see full content, tags, and linked files";
const LOOKUP_HINT: &str = "Use a skill name or relative path within the skills directory.";
const WITHIN_SKILL_HINT: &str = "Use a relative path within the skill directory";
const USAGE_HINT: &str = "To view linked files, call skill_view(name, file_path) where file_path \
    is e.g. 'references/api.md' or 'assets/config.yaml'";

/// Name endings of the files `skill_view` lists under `templates/`, and of other files a
/// missing linked file's answer lists.
const TEXT_EXTENSIONS: [&str; 7] = [".md", ".py", ".yaml", ".yml", ".json", ".tex", ".sh"];
/// Name endings of the files `skill_view` lists under `scripts/`.
const SCRIPT_EXTENSIONS: [&str; 6] = [".py", ".sh", ".bash", ".js", ".ts", ".rb"];

/// `skills_list` over `skills`.
pub(super) struct SkillsList {
    pub(super) skills: Skills,
}

/// `skill_view` over `skills`, with the files it served for the repeat-view dedup.
pub(super) struct SkillView {
    pub(super) skills: Skills,
    pub(super) views: Mutex<Vec<View>>,
}

/// A served view: the skill name and file path asked for, and the file with its stamp.
pub(super) struct View {
    name: String,
    file_path: String,
    source: PathBuf,
    stamp: (SystemTime, u64),
}

/// A skill as `skills_list` shows it.
struct Listed {
    name: String,
    description: Yaml,
    category: Option<String>,
}

impl Tool for SkillsList {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: SKILLS_LIST.into(),
            description: LIST_DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "category": {
                        "type": "string",
                        "description": "Optional category filter to narrow results"
                    }
                }),
                &[],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        let category = match Args(input).opt_str("category") {
            Ok(category) => category.filter(|c| !c.is_empty()),
            Err(e) => return Output::error(e),
        };
        let dir = &self.skills.dir;
        if !dir.exists()
            && let Err(e) = fs::create_dir_all(dir)
        {
            return Output::error_with(os_error_text(&e, Some(dir)), [("success", "false".into())]);
        }
        let mut skills = find_all(&self.skills);
        if skills.is_empty() {
            return Output::ok(json_object([
                ("success", "true".into()),
                ("skills", "[]".into()),
                ("categories", "[]".into()),
                ("message", json_str("No skills found in skills/ directory.")),
            ]));
        }
        if let Some(category) = category {
            skills.retain(|s| s.category.as_deref() == Some(category));
        }
        let mut categories: Vec<&str> = skills
            .iter()
            .filter_map(|s| s.category.as_deref())
            .collect();
        crate::sort::sort(&mut categories);
        categories.dedup();
        Output::ok(json_object([
            ("success", "true".into()),
            ("skills", json_array(skills.iter().map(Listed::json))),
            (
                "categories",
                json_array(categories.iter().map(|c| json_str(c))),
            ),
            ("count", skills.len().to_string()),
            ("hint", json_str(LIST_HINT)),
        ]))
    }
}

impl Listed {
    fn json(&self) -> String {
        json_object([
            ("name", json_str(&self.name)),
            ("description", yaml_json(&self.description)),
            (
                "category",
                self.category
                    .as_deref()
                    .map_or_else(|| "null".into(), json_str),
            ),
        ])
    }
}

/// Every listed skill of `skills` in category and name order: one per name, the first in path
/// order, none disabled or for another platform.
fn find_all(skills: &Skills) -> Vec<Listed> {
    let mut found: Vec<Listed> = Vec::new();
    if !skills.dir.exists() {
        return found;
    }
    for skill_md in index_files(&skills.dir, "SKILL.md") {
        if in_excluded_dir(&skill_md) {
            continue;
        }
        let Ok(text) = read_text(&skill_md, true) else {
            continue;
        };
        let head: String = text.chars().take(LIST_SCAN_CHARS).collect();
        let (fm, body) = frontmatter::parse(&head);
        if !matches_platform(&fm) {
            continue;
        }
        let name = match frontmatter::get(&fm, "name") {
            None => file_name(skill_md.parent().unwrap_or(&skills.dir)),
            Some(Yaml::Str(name)) => name.clone(),
            Some(_) => continue,
        };
        let name: String = name.chars().take(MAX_NAME_LENGTH).collect();
        if found.iter().any(|s| s.name == name) || skills.is_disabled(&name) {
            continue;
        }
        let mut description = frontmatter::get(&fm, "description")
            .cloned()
            .unwrap_or_else(|| Yaml::Str(String::new()));
        if !truthy(&description)
            && let Some(line) = py_strip(body)
                .split('\n')
                .map(py_strip)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
        {
            description = Yaml::Str(line.to_owned());
        }
        let description = match description {
            Yaml::Str(text) if text.chars().count() > MAX_DESCRIPTION_LENGTH => {
                let kept: String = text.chars().take(MAX_DESCRIPTION_LENGTH - 3).collect();
                Yaml::Str(kept + "...")
            }
            Yaml::List(items) if items.len() > MAX_DESCRIPTION_LENGTH => continue,
            Yaml::Map(entries) if entries.len() > MAX_DESCRIPTION_LENGTH => continue,
            other => other,
        };
        found.push(Listed {
            category: category(&skills.dir, &skill_md),
            name,
            description,
        });
    }
    crate::sort::sort_by(&mut found, |a, b| {
        (a.category.as_deref().unwrap_or(""), &a.name)
            .cmp(&(b.category.as_deref().unwrap_or(""), &b.name))
    });
    found
}

/// The top directory of `skill_md` under `dir` when the skill sits inside a category.
fn category(dir: &Path, skill_md: &Path) -> Option<String> {
    let rel = skill_md.strip_prefix(dir).ok()?;
    let mut parts = rel.components();
    let first = parts.next()?;
    (parts.count() >= 2).then(|| first.as_os_str().to_string_lossy().into_owned())
}

impl Tool for SkillView {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: SKILL_VIEW.into(),
            description: VIEW_DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "name": {
                        "type": "string",
                        "description": "The skill name (use skills_list to see available skills). \
                                        For plugin-provided skills, use the qualified form \
                                        'plugin:skill' (e.g. 'superpowers:writing-plans')."
                    },
                    "file_path": {
                        "type": "string",
                        "description": "OPTIONAL: Path to a linked file within the skill (e.g., \
                                        'references/api.md', 'templates/config.yaml', \
                                        'scripts/validate.py'). Omit to get the main SKILL.md \
                                        content."
                    }
                }),
                &["name"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        let args = Args(input);
        let (name, file_path) = match (args.str("name"), args.opt_str("file_path")) {
            (Ok(name), Ok(file_path)) => (name, file_path.unwrap_or("")),
            (Err(e), _) | (_, Err(e)) => return Output::error(e),
        };
        if let Some(stub) = self.unchanged(name, file_path) {
            return stub;
        }
        let (output, served) = self.view(name, file_path);
        if let Some((served_name, source)) = served
            && let Some(stamp) = stamp(&source)
        {
            self.record(View {
                name: served_name,
                file_path: file_path.to_owned(),
                source,
                stamp,
            });
        }
        output
    }
}

impl SkillView {
    fn views(&self) -> MutexGuard<'_, Vec<View>> {
        self.views.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The dedup answer when this file of this skill was served before and has not changed;
    /// a changed or vanished file drops its record.
    fn unchanged(&self, name: &str, file_path: &str) -> Option<Output> {
        let mut views = self.views();
        let at = views.iter().position(|view| {
            view.file_path == file_path
                && (view.name == name
                    || name.ends_with(&format!("/{}", view.name))
                    || view.name.ends_with(&format!("/{name}"))
                    || name.rsplit(':').next() == Some(view.name.as_str()))
        })?;
        if stamp(&views[at].source) != Some(views[at].stamp) {
            views.remove(at);
            return None;
        }
        let file = if file_path.is_empty() {
            "SKILL.md"
        } else {
            file_path
        };
        Some(Output::ok(json_object([
            ("success", "true".into()),
            ("status", json_str("unchanged")),
            ("name", json_str(&views[at].name)),
            ("file", json_str(file)),
            ("dedup", "true".into()),
            ("content_returned", "false".into()),
            ("message", json_str(VIEW_DEDUP_MESSAGE)),
        ])))
    }

    /// Remembers `view`, keeping the place of an earlier record of the same name and file
    /// and dropping the oldest records beyond [`VIEW_DEDUP_CAP`].
    fn record(&self, view: View) {
        let mut views = self.views();
        match views
            .iter_mut()
            .find(|v| v.name == view.name && v.file_path == view.file_path)
        {
            Some(old) => *old = view,
            None => views.push(view),
        }
        let excess = views.len().saturating_sub(VIEW_DEDUP_CAP);
        views.drain(..excess);
    }

    /// The answer to a view and, when it served a file, the name to record it under and the
    /// file.
    fn view(&self, name: &str, file_path: &str) -> (Output, Option<(String, PathBuf)>) {
        let (skill_dir, skill_md) = match self.find(name) {
            Ok(found) => found,
            Err(refusal) => return (refusal, None),
        };
        let content = match read_text(&skill_md, true) {
            Ok(content) => content,
            Err(e) => {
                let error = format!(
                    "Failed to read skill '{name}': {}",
                    os_error_text(&e, Some(&skill_md))
                );
                return (failure([("error", json_str(&error))]), None);
            }
        };
        let (fm, _) = frontmatter::parse(&content);
        if !matches_platform(&fm) {
            let error = format!("Skill '{name}' is not supported on this platform.");
            return (
                failure([
                    ("error", json_str(&error)),
                    ("readiness_status", json_str("unsupported")),
                ]),
                None,
            );
        }
        if let Some(Yaml::Str(resolved)) = frontmatter::get(&fm, "name")
            .cloned()
            .or_else(|| skill_md.parent().map(|p| Yaml::Str(file_name(p))))
            && self.skills.is_disabled(&resolved)
        {
            let error = format!(
                "Skill '{resolved}' is disabled. Enable it by removing it from the \
                 engine.disabled_skills setting or inspect the files directly on disk."
            );
            return (failure([("error", json_str(&error))]), None);
        }
        match skill_dir {
            Some(skill_dir) if !file_path.is_empty() => linked_file(name, file_path, &skill_dir),
            skill_dir => {
                let found = Found {
                    dir: &self.skills.dir,
                    skill_dir: skill_dir.as_deref(),
                    skill_md: &skill_md,
                };
                main_view(&found, &fm, &content, name)
            }
        }
    }

    /// The one skill `name` names, with its directory unless it is a flat `<name>.md`, or the
    /// refusal of the name: outside the skills directory, missing or ambiguous.
    fn find(&self, name: &str) -> Result<(Option<PathBuf>, PathBuf), Output> {
        if let Some(error) = lookup_error(name) {
            return Err(failure([
                ("error", json_str(error)),
                ("hint", json_str(LOOKUP_HINT)),
            ]));
        }
        let mut categorized = None;
        if let Some((namespace, bare)) = name.split_once(':') {
            if !valid_namespace(namespace) {
                let error = format!(
                    "Invalid namespace '{namespace}' in '{name}'. Namespaces must match \
                     [a-zA-Z0-9_-]+."
                );
                return Err(failure([("error", json_str(&error))]));
            }
            if !bare.is_empty() {
                categorized = Some(format!("{namespace}/{bare}"));
            }
        }
        if let Some(error) = categorized.as_deref().and_then(lookup_error) {
            return Err(failure([
                ("error", json_str(error)),
                ("hint", json_str(LOOKUP_HINT)),
            ]));
        }
        let dir = &self.skills.dir;
        if !dir.exists() {
            let error = "Skills directory does not exist yet. It will be created on first install.";
            return Err(failure([("error", json_str(error))]));
        }
        let mut candidates = candidates(dir, name, categorized.as_deref());
        if candidates.len() > 1 {
            let error = format!(
                "Ambiguous skill name '{name}': {} skills match across your local skills dir \
                 and external_dirs. Refusing to guess \u{2014} load one explicitly by its \
                 categorized path.",
                candidates.len()
            );
            let matches = candidates
                .iter()
                .map(|(_, md)| json_str(&md.to_string_lossy()));
            let hint = "Pass the full relative path instead of the bare name (e.g., \
                        'category/skill-name'), or rename one of the colliding skills so each \
                        name is unique.";
            return Err(failure([
                ("error", json_str(&error)),
                ("matches", json_array(matches)),
                ("hint", json_str(hint)),
            ]));
        }
        candidates
            .pop()
            .filter(|(_, md)| md.exists())
            .ok_or_else(|| {
                let available = find_all(&self.skills)
                    .into_iter()
                    .take(NOT_FOUND_SUGGESTIONS)
                    .map(|s| json_str(&s.name));
                failure([
                    ("error", json_str(&format!("Skill '{name}' not found."))),
                    ("available_skills", json_array(available)),
                    (
                        "hint",
                        json_str("Use skills_list to see all available skills"),
                    ),
                ])
            })
    }
}

/// Where a viewed skill lies: the skills directory, the skill's directory unless it is a flat
/// `<name>.md`, and its file.
struct Found<'a> {
    dir: &'a Path,
    skill_dir: Option<&'a Path>,
    skill_md: &'a Path,
}

/// A `{"success": false, ...}` answer with `fields` after the flag.
fn failure<'a>(fields: impl IntoIterator<Item = (&'a str, String)>) -> Output {
    Output {
        content: json_object(std::iter::once(("success", "false".into())).chain(fields)),
        is_error: true,
    }
}

/// Why `name` cannot be looked up: an absolute or drive path, or a `..` component.
fn lookup_error(name: &str) -> Option<&'static str> {
    let candidate = py_strip(name);
    let windows = candidate.replace('/', "\\");
    let mut chars = windows.chars();
    let drive =
        matches!((chars.next(), chars.next()), (Some(c), Some(':')) if c.is_ascii_alphabetic());
    if candidate.starts_with('/') || drive || unc_drive(&windows) {
        return Some("Skill name must be a relative path within the skills directory.");
    }
    has_parent_component(candidate)
        .then_some("Skill name cannot contain '..' path traversal components.")
}

/// Whether a backslash path starts with a UNC drive `\\machine\mount`.
fn unc_drive(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("\\\\") else {
        return false;
    };
    if rest.starts_with('\\') {
        return false;
    }
    rest.find('\\')
        .is_some_and(|at| !rest[at + 1..].starts_with('\\'))
}

fn has_parent_component(path: &str) -> bool {
    Path::new(path)
        .components()
        .any(|c| c == Component::ParentDir)
}

/// A namespace of a qualified name: letters, digits, `_` and `-`, a final line break allowed.
fn valid_namespace(namespace: &str) -> bool {
    let namespace = namespace.strip_suffix('\n').unwrap_or(namespace);
    !namespace.is_empty()
        && namespace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `rel` below `dir`, without its `.` and empty components.
fn child(dir: &Path, rel: &str) -> PathBuf {
    let rel: PathBuf = Path::new(rel)
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    if rel.as_os_str().is_empty() {
        dir.to_path_buf()
    } else {
        dir.join(rel)
    }
}

/// `path` with the suffix of its last component replaced by `.md`, or `.md` appended.
fn with_md(path: &Path) -> Option<PathBuf> {
    let name = file_name(path);
    if name.is_empty() {
        return None;
    }
    let stem = &name[..name.len() - py_suffix(&name).len()];
    Some(path.with_file_name(format!("{stem}.md")))
}

/// Skill files matching `name` under `dir`, each with its skill directory unless it is a flat
/// `<name>.md`: the path `name` (or `categorized`) below `dir`, a skill directory called
/// `name` or one whose frontmatter `name` is `name`, and any `<name>.md` outside support
/// directories. A file reached twice counts once.
fn candidates(
    dir: &Path,
    name: &str,
    categorized: Option<&str>,
) -> Vec<(Option<PathBuf>, PathBuf)> {
    let mut found: Vec<(Option<PathBuf>, PathBuf)> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut add = |skill_dir: Option<PathBuf>, skill_md: PathBuf| {
        let key = realpath(&skill_md);
        if !seen.contains(&key) {
            seen.push(key);
            found.push((skill_dir, skill_md));
        }
    };
    for rel in std::iter::once(name).chain(categorized) {
        let direct = child(dir, rel);
        if !is_support_path(&direct) && direct.is_dir() && direct.join("SKILL.md").exists() {
            let skill_md = direct.join("SKILL.md");
            add(Some(direct), skill_md);
        } else if let Some(md) = with_md(&direct)
            && md.exists()
            && !is_support_path(&md)
        {
            add(None, md);
        }
    }
    for skill_md in index_files(dir, "SKILL.md") {
        let Some(parent) = skill_md.parent().map(Path::to_path_buf) else {
            continue;
        };
        if file_name(&parent) == name {
            add(Some(parent), skill_md);
            continue;
        }
        let fm = read_text(&skill_md, true)
            .map(|text| frontmatter::parse(&text).0)
            .unwrap_or_default();
        if matches!(frontmatter::get(&fm, "name"), Some(Yaml::Str(n)) if n == name) {
            add(Some(parent), skill_md);
        }
    }
    if let Ok(pattern) = Pattern::parse(&format!("**/{name}.md")) {
        for (rel, path) in tree(dir, true) {
            if pattern.matches(&rel)
                && file_name(&path) != "SKILL.md"
                && path.exists()
                && !is_support_path(&path)
            {
                add(None, path);
            }
        }
    }
    found
}

/// The entries below `dir` with their components relative to it: those of `dir` in name
/// order, then, when `recursive`, those of each subdirectory in turn. Symlinked directories
/// are listed but not entered.
fn tree(dir: &Path, recursive: bool) -> Vec<(Vec<String>, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), Vec::new())];
    while let Some((at, prefix)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&at) else {
            continue;
        };
        let mut entries: Vec<(String, PathBuf, bool)> = entries
            .flatten()
            .map(|e| {
                let real_dir = e.file_type().is_ok_and(|t| t.is_dir());
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.path(),
                    real_dir,
                )
            })
            .collect();
        crate::sort::sort_by(&mut entries, |a, b| a.0.cmp(&b.0));
        let mut subdirs = Vec::new();
        for (name, path, real_dir) in entries {
            let mut rel: Vec<String> = prefix.clone();
            rel.push(name);
            if recursive && real_dir {
                subdirs.push((path.clone(), rel.clone()));
            }
            out.push((rel, path));
        }
        stack.extend(subdirs.into_iter().rev());
    }
    out
}

/// `rel` joined with `/`.
fn slash(rel: &[String]) -> String {
    rel.join("/")
}

/// One linked file of the skill in `skill_dir`, refused when the path climbs out of it.
fn linked_file(
    name: &str,
    file_path: &str,
    skill_dir: &Path,
) -> (Output, Option<(String, PathBuf)>) {
    if has_parent_component(file_path) {
        return (
            failure([
                ("error", json_str("Path traversal ('..') is not allowed.")),
                ("hint", json_str(WITHIN_SKILL_HINT)),
            ]),
            None,
        );
    }
    let target = child(skill_dir, file_path);
    let (resolved, root) = (realpath(&target), realpath(skill_dir));
    if !resolved.starts_with(&root) {
        let error = format!(
            "Path escapes allowed directory: {} is not in the subpath of {} OR one path is \
             relative and the other is absolute.",
            repr(&resolved.to_string_lossy()),
            repr(&root.to_string_lossy())
        );
        return (
            failure([
                ("error", json_str(&error)),
                ("hint", json_str(WITHIN_SKILL_HINT)),
            ]),
            None,
        );
    }
    if !target.exists() {
        let error = format!("File '{file_path}' not found in skill '{name}'.");
        return (
            failure([
                ("error", json_str(&error)),
                ("available_files", available_files(skill_dir)),
                (
                    "hint",
                    json_str("Use one of the available file paths listed above"),
                ),
            ]),
            None,
        );
    }
    let content = match read_text(&target, true) {
        Ok(content) => content,
        Err(e) => {
            return (
                Output::error_with(
                    os_error_text(&e, Some(&target)),
                    [("success", "false".into())],
                ),
                None,
            );
        }
    };
    let output = Output::ok(json_object([
        ("success", "true".into()),
        ("name", json_str(name)),
        ("file", json_str(file_path)),
        ("content", json_str(&content)),
        ("file_type", json_str(py_suffix(&file_name(&target)))),
        ("_source_path", json_str(&target.to_string_lossy())),
    ]));
    (output, Some((name.to_owned(), target)))
}

/// The files of a skill other than `SKILL.md`, by support directory, with the text files
/// outside them under `other`; empty groups left out.
fn available_files(skill_dir: &Path) -> String {
    let mut groups: Vec<(&str, Vec<String>)> = SUPPORT_DIRS
        .iter()
        .chain(&["other"])
        .map(|group| (*group, Vec::new()))
        .collect();
    for (rel, path) in tree(skill_dir, true) {
        if !path.is_file() || rel.last().is_some_and(|n| n == "SKILL.md") {
            continue;
        }
        let rel = slash(&rel);
        let group = SUPPORT_DIRS
            .iter()
            .position(|dir| rel.starts_with(&format!("{dir}/")))
            .or_else(|| {
                TEXT_EXTENSIONS
                    .contains(&py_suffix(&file_name(&path)))
                    .then_some(SUPPORT_DIRS.len())
            });
        if let Some(group) = group {
            groups[group].1.push(rel);
        }
    }
    json_object(
        groups
            .into_iter()
            .filter(|(_, files)| !files.is_empty())
            .map(|(group, files)| (group, json_array(files.iter().map(|f| json_str(f))))),
    )
}

/// The `SKILL.md` of a skill with its frontmatter fields and linked files.
fn main_view(
    found: &Found<'_>,
    fm: &Mapping,
    content: &str,
    name: &str,
) -> (Output, Option<(String, PathBuf)>) {
    let Found {
        dir,
        skill_dir,
        skill_md,
    } = *found;
    let linked = skill_dir.map(linked_files).unwrap_or_default();
    let rel_path = skill_md.strip_prefix(dir).unwrap_or(skill_md);
    let skill_name = frontmatter::get(fm, "name").cloned().unwrap_or_else(|| {
        Yaml::Str(skill_dir.map_or_else(
            || {
                let file = file_name(skill_md);
                file[..file.len() - py_suffix(&file).len()].to_owned()
            },
            file_name,
        ))
    });
    let tag_list = |key: &str| {
        let tags = frontmatter::get(fm, key).map_or_else(Vec::new, parse_tags);
        json_array(tags.iter().map(|t| json_str(t)))
    };
    let mut fields = vec![
        ("success", "true".to_owned()),
        ("name", yaml_json(&skill_name)),
        (
            "description",
            frontmatter::get(fm, "description").map_or_else(|| json_str(""), yaml_json),
        ),
        ("tags", tag_list("tags")),
        ("related_skills", tag_list("related_skills")),
        ("content", json_str(content)),
        ("path", json_str(&rel_path.to_string_lossy())),
        (
            "skill_dir",
            skill_dir.map_or_else(|| "null".into(), |d| json_str(&d.to_string_lossy())),
        ),
        ("org_provenance", "null".into()),
    ];
    if linked.is_empty() {
        fields.push(("linked_files", "null".into()));
        fields.push(("usage_hint", "null".into()));
    } else {
        let groups = linked
            .iter()
            .map(|(group, files)| (*group, json_array(files.iter().map(|f| json_str(f)))));
        fields.push(("linked_files", json_object(groups)));
        fields.push(("usage_hint", json_str(USAGE_HINT)));
    }
    for empty in [
        "required_environment_variables",
        "required_commands",
        "missing_required_environment_variables",
        "missing_credential_files",
        "missing_required_commands",
    ] {
        fields.push((empty, "[]".into()));
    }
    fields.extend([
        ("setup_needed", "false".into()),
        ("setup_skipped", "false".into()),
        ("readiness_status", json_str("available")),
        ("_source_path", json_str(&skill_md.to_string_lossy())),
    ]);
    if let Some(compatibility) = frontmatter::get(fm, "compatibility").filter(|v| truthy(v)) {
        fields.push(("compatibility", yaml_json(compatibility)));
    }
    if let Some(metadata @ Yaml::Map(_)) = frontmatter::get(fm, "metadata") {
        fields.push(("metadata", yaml_json(metadata)));
    }
    let served = if truthy(&skill_name) {
        py_str(&skill_name)
    } else {
        name.to_owned()
    };
    (
        Output::ok(json_object(fields)),
        Some((served, skill_md.to_path_buf())),
    )
}

/// The linked files of the skill in `skill_dir` by group, groups without files left out:
/// `references/*.md`, the text files anywhere under `templates/`, every file under
/// `assets/` and the scripts directly in `scripts/`.
fn linked_files(skill_dir: &Path) -> Vec<(&'static str, Vec<String>)> {
    let under = |group: &str, recursive: bool| -> Vec<(Vec<String>, PathBuf)> {
        tree(&skill_dir.join(group), recursive)
            .into_iter()
            .map(|(rel, path)| {
                let mut full = vec![group.to_owned()];
                full.extend(rel);
                (full, path)
            })
            .collect()
    };
    let by_ending = |entries: &[(Vec<String>, PathBuf)], endings: &[&str]| -> Vec<String> {
        endings
            .iter()
            .flat_map(|ending| {
                entries
                    .iter()
                    .filter(move |(rel, _)| rel.last().is_some_and(|n| n.ends_with(ending)))
                    .map(|(rel, _)| slash(rel))
            })
            .collect()
    };
    let assets = under("assets", true)
        .into_iter()
        .filter(|(_, path)| path.is_file())
        .map(|(rel, _)| slash(&rel))
        .collect();
    [
        (
            "references",
            by_ending(&under("references", false), &[".md"]),
        ),
        (
            "templates",
            by_ending(&under("templates", true), &TEXT_EXTENSIONS),
        ),
        ("assets", assets),
        (
            "scripts",
            by_ending(&under("scripts", false), &SCRIPT_EXTENSIONS),
        ),
    ]
    .into_iter()
    .filter(|(_, files)| !files.is_empty())
    .collect()
}

/// Tags from a list or from a text of comma-separated names, optionally in brackets.
fn parse_tags(value: &Yaml) -> Vec<String> {
    if !truthy(value) {
        return Vec::new();
    }
    if let Yaml::List(items) = value {
        return items
            .iter()
            .filter(|item| truthy(item))
            .map(|item| py_strip(&py_str(item)).to_owned())
            .collect();
    }
    let text = py_str(value);
    let text = py_strip(&text);
    let text = text
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .unwrap_or(text);
    text.split(',')
        .filter(|tag| !py_strip(tag).is_empty())
        .map(|tag| py_strip(tag).trim_matches(['"', '\'']).to_owned())
        .collect()
}

/// `value` as the reference's JSON writes it.
fn yaml_json(value: &Yaml) -> String {
    match value {
        Yaml::Null => "null".into(),
        Yaml::Str(text) => json_str(text),
        Yaml::List(items) => json_array(items.iter().map(yaml_json)),
        Yaml::Map(entries) => json_object(entries.iter().map(|(k, v)| (k.as_str(), yaml_json(v)))),
    }
}

/// Modification time and size of the file at `path`.
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}
