// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Skills as files: a directory holding one directory per skill with its `SKILL.md`, grouped
//! under category directories. The system prompt indexes them and the skill tools list and
//! read them; both scan the directory only when they run.

pub mod frontmatter;

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use frontmatter::{Mapping, Yaml};

use crate::tools::{normalize_line_endings, repr};

/// Directories no scan enters: version control, environments, caches and archives.
const EXCLUDED_DIRS: [&str; 14] = [
    ".git",
    ".github",
    ".hub",
    ".archive",
    ".venv",
    "venv",
    "node_modules",
    "site-packages",
    "__pycache__",
    ".tox",
    ".nox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];

/// Directories of a skill holding its linked files, which `skill_view` reads on request.
pub(crate) const SUPPORT_DIRS: [&str; 4] = ["references", "templates", "assets", "scripts"];

/// The mirror of organisation-shared skills at the top of the directory; never scanned.
const ORG_MIRROR_DIR: &str = "_org";

/// Platform names of the `platforms` frontmatter key and the platform prefix each stands for.
const PLATFORM_MAP: [(&str, &str); 3] = [
    ("macos", "darwin"),
    ("linux", "linux"),
    ("windows", "win32"),
];

/// The platform the engine runs on, as the `platforms` key names it after mapping.
const PLATFORM: &str = "linux";

/// The skills directory and the skills turned off in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skills {
    /// The directory of skill directories.
    pub dir: PathBuf,
    /// Names of disabled skills: a frontmatter `name` or a skill directory name.
    pub disabled: Vec<String>,
}

impl Skills {
    pub(crate) fn is_disabled(&self, name: &str) -> bool {
        self.disabled.iter().any(|d| d == name)
    }
}

/// Paths of the files named `filename` under `dir` in path order. The walk follows symlinks
/// but never into a directory it is already inside, and skips [`EXCLUDED_DIRS`], the
/// organisation mirror and the support directories of a skill.
pub(crate) fn index_files(dir: &Path, filename: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let top: Vec<PathBuf> = fs::canonicalize(dir).into_iter().collect();
    let mut stack = vec![(dir.to_path_buf(), top)];
    while let Some((root, inside)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        let has_skill = files.iter().any(|f| f == "SKILL.md");
        dirs.retain(|d| {
            !(EXCLUDED_DIRS.contains(&d.as_str())
                || (root == dir && d == ORG_MIRROR_DIR)
                || (has_skill && SUPPORT_DIRS.contains(&d.as_str())))
        });
        if files.iter().any(|f| f == filename) {
            found.push(root.join(filename));
        }
        for d in dirs {
            let path = root.join(d);
            if let Ok(real) = fs::canonicalize(&path)
                && !inside.contains(&real)
            {
                let mut chain = inside.clone();
                chain.push(real);
                stack.push((path, chain));
            }
        }
    }
    crate::sort::sort_by(&mut found, |a, b| a.as_os_str().cmp(b.as_os_str()));
    found
}

/// Whether a component of `path` is in [`EXCLUDED_DIRS`].
pub(crate) fn in_excluded_dir(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(name) if EXCLUDED_DIRS.iter().any(|d| name == *d)))
}

/// Whether `path` lies in a support directory directly inside a directory with a `SKILL.md`.
pub(crate) fn is_support_path(path: &Path) -> bool {
    let parts: Vec<Component<'_>> = path
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    (1..parts.len().saturating_sub(1)).any(|at| {
        matches!(parts[at], Component::Normal(name) if SUPPORT_DIRS.iter().any(|d| name == *d))
            && parts[..at]
                .iter()
                .collect::<PathBuf>()
                .join("SKILL.md")
                .exists()
    })
}

/// Whether the `platforms` of `frontmatter` admit the engine's platform; none admits all.
pub(crate) fn matches_platform(frontmatter: &Mapping) -> bool {
    let platforms = match frontmatter::get(frontmatter, "platforms") {
        Some(value) if truthy(value) => value,
        _ => return true,
    };
    let list = match platforms {
        Yaml::List(items) => items.as_slice(),
        other => std::slice::from_ref(other),
    };
    list.iter().any(|platform| {
        let normalized = py_str(platform).to_lowercase();
        let normalized = normalized.trim();
        let mapped = PLATFORM_MAP
            .iter()
            .find(|(name, _)| *name == normalized)
            .map_or(normalized, |(_, prefix)| prefix);
        PLATFORM.starts_with(mapped)
    })
}

/// The text of the file at `path` with universal newlines: lossy UTF-8 without a leading byte
/// order mark when `lossy`, else strict UTF-8.
///
/// # Errors
/// The read failure; invalid UTF-8 when not `lossy`; a file that is neither regular nor a
/// directory, which a read could block on.
pub(crate) fn read_text(path: &Path, lossy: bool) -> io::Result<String> {
    let kind = fs::metadata(path)?.file_type();
    if !kind.is_file() && !kind.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let bytes = fs::read(path)?;
    let text = if lossy {
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    };
    Ok(normalize_line_endings(&text, "\n"))
}

/// Python's truth value of `value`: null and empty values are false.
pub(crate) fn truthy(value: &Yaml) -> bool {
    match value {
        Yaml::Null => false,
        Yaml::Str(s) => !s.is_empty(),
        Yaml::List(items) => !items.is_empty(),
        Yaml::Map(entries) => !entries.is_empty(),
    }
}

/// Python's `str()` of `value`.
pub(crate) fn py_str(value: &Yaml) -> String {
    match value {
        Yaml::Str(s) => s.clone(),
        other => py_repr(other),
    }
}

fn py_repr(value: &Yaml) -> String {
    match value {
        Yaml::Null => "None".to_owned(),
        Yaml::Str(s) => repr(s),
        Yaml::List(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Yaml::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{}: {}", repr(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
