//! Small helpers shared by the commands.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Errors are collected as human-readable lines.
pub type Errors = Vec<String>;
pub type Res = Result<(), Errors>;

pub fn root() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent().map(Path::to_path_buf).unwrap_or(here)
}

pub fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn read_json(path: &Path) -> Result<Value, String> {
    let text = read(path)?;
    serde_json::from_str(&text).map_err(|e| format!("{}: invalid JSON: {e}", path.display()))
}

/// Runs a command in `dir` and returns stdout; any failure is an error string.
pub fn output(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{program}: non-UTF-8 output: {e}"))
}

/// Contents of `path` at git revision `rev`, or `None` if it does not exist there.
pub fn git_show(root: &Path, rev: &str, path: &str) -> Option<String> {
    output(root, "git", &["show", &format!("{rev}:{path}")]).ok()
}

/// `cargo metadata --no-deps` of the workspace.
pub fn metadata(root: &Path) -> Result<Value, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let text = output(
        root,
        &cargo,
        &[
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ],
    )?;
    serde_json::from_str(&text).map_err(|e| format!("cargo metadata: {e}"))
}

/// All files under `dir` (recursive) with the given extension, sorted.
pub fn files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some(ext) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

pub fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// Compares or writes a generated file.
pub fn write_or_check(root: &Path, path: &Path, content: &str, check: bool, errors: &mut Errors) {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current == content {
        return;
    }
    if check {
        errors.push(format!(
            "{} is out of date; run `cargo xtask {}` and commit the result",
            rel(root, path),
            if rel(root, path).contains("module-map") {
                "module-map"
            } else {
                "gen-contracts"
            }
        ));
    } else {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(path, content) {
            errors.push(format!("{}: {e}", path.display()));
        } else {
            eprintln!("wrote {}", rel(root, path));
        }
    }
}

/// Parses `x.y.z`.
pub fn semver(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.split('.').map(|p| p.parse::<u64>().ok());
    let t = (it.next()??, it.next()??, it.next()??);
    if it.next().is_some() {
        return None;
    }
    Some(t)
}
