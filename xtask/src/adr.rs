//! ADR gate (ADR-0001). Every ADR is `docs/architecture/adr/NNNN-slug.md` with `# ADR-NNNN: `,
//! `Status:`, `Date:` and `Affects:` lines. Against a base revision, a new crate, an added or
//! removed dependency, a new or removed `ecosystem.lock` product or contract requires a new
//! ADR in the same change whose `Affects:` line names the object:
//! `crates/<dir>`, `dep:<name>`, `lock:<product>`, `contract:<id>`.

use std::collections::BTreeSet;
use std::path::Path;

use crate::util::{self, Errors, Res};

const ADR_DIR: &str = "docs/architecture/adr";

pub fn run(root: &Path, base: Option<&str>) -> Res {
    let mut errors = Errors::new();
    check_format(root, &mut errors);
    if let Some(base) = base {
        if util::output(
            root,
            "git",
            &["cat-file", "-e", &format!("{base}^{{commit}}")],
        )
        .is_err()
        {
            errors.push(format!(
                "base revision {base} is not available (fetch full history)"
            ));
        } else {
            let needed = changed_objects(root, base);
            let affects = added_affects(root, base);
            for obj in needed.difference(&affects) {
                errors.push(format!(
                    "`{obj}` changed without an ADR: add docs/architecture/adr/NNNN-*.md with `Affects: {obj}`"
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn parse_affects(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("Affects:"))
        .flat_map(|rest| rest.split(','))
        .map(|t| t.trim().trim_matches('`').to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn check_format(root: &Path, errors: &mut Errors) {
    let files = util::files_with_ext(&root.join(ADR_DIR), "md");
    let mut numbers = Vec::new();
    for f in &files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "README.md" || name == "template.md" {
            continue;
        }
        let rel = util::rel(root, f);
        let num = name.get(..4).and_then(|n| n.parse::<u32>().ok());
        let slug_ok = name.len() > 8
            && name.as_bytes().get(4) == Some(&b'-')
            && name[5..name.len() - 3]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        let Some(num) = num.filter(|_| slug_ok) else {
            errors.push(format!("{rel}: name must be NNNN-lowercase-slug.md"));
            continue;
        };
        numbers.push(num);
        let text = util::read(f).unwrap_or_default();
        if !text.starts_with(&format!("# ADR-{num:04}: ")) {
            errors.push(format!(
                "{rel}: first line must be `# ADR-{num:04}: <title>`"
            ));
        }
        for key in ["Status:", "Date:", "Affects:"] {
            if !text.lines().any(|l| l.trim().starts_with(key)) {
                errors.push(format!("{rel}: missing `{key}` line"));
            }
        }
    }
    numbers.sort_unstable();
    for (i, n) in numbers.iter().enumerate() {
        let want = u32::try_from(i + 1).unwrap_or(u32::MAX);
        if *n != want {
            errors.push(format!("ADR numbers must be unique and contiguous from 0001; expected {want:04}, found {n:04}"));
            break;
        }
    }
}

/// `Affects:` objects of ADR files added between `base` and the working tree.
pub fn added_affects(root: &Path, base: &str) -> BTreeSet<String> {
    let Ok(list) = util::output(
        root,
        "git",
        &[
            "diff",
            "--name-only",
            "--diff-filter=A",
            base,
            "--",
            ADR_DIR,
        ],
    ) else {
        return BTreeSet::new();
    };
    list.lines()
        .filter_map(|p| util::read(&root.join(p)).ok())
        .flat_map(|t| parse_affects(&t))
        .collect()
}

fn deps_of(manifest: &str) -> BTreeSet<String> {
    let Ok(t) = manifest.parse::<toml::Table>() else {
        return BTreeSet::new();
    };
    let mut out = BTreeSet::new();
    let mut collect = |tbl: &toml::Table| {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(d) = tbl.get(key).and_then(|v| v.as_table()) {
                out.extend(d.keys().map(|k| format!("dep:{k}")));
            }
        }
    };
    collect(&t);
    if let Some(ws) = t.get("workspace").and_then(|v| v.as_table()) {
        collect(ws);
    }
    if let Some(targets) = t.get("target").and_then(|v| v.as_table()) {
        for tt in targets.values().filter_map(|v| v.as_table()) {
            collect(tt);
        }
    }
    out
}

fn manifests(root: &Path, rev: Option<&str>) -> Vec<String> {
    let listing = match rev {
        Some(r) => util::output(root, "git", &["ls-tree", "-r", "--name-only", r]),
        None => util::output(
            root,
            "git",
            &["ls-files", "--cached", "--others", "--exclude-standard"],
        ),
    }
    .unwrap_or_default();
    listing
        .lines()
        .filter(|p| *p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
        .map(str::to_string)
        .collect()
}

fn changed_objects(root: &Path, base: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let old_m = manifests(root, Some(base));
    let new_m = manifests(root, None);
    for m in &new_m {
        if let Some(dir) = m
            .strip_prefix("crates/")
            .and_then(|r| r.strip_suffix("/Cargo.toml"))
            && !old_m.contains(m)
        {
            out.insert(format!("crates/{dir}"));
        }
        let new = deps_of(&util::read(&root.join(m)).unwrap_or_default());
        let old = deps_of(&util::git_show(root, base, m).unwrap_or_default());
        out.extend(new.symmetric_difference(&old).cloned());
    }
    for m in old_m.iter().filter(|m| !new_m.contains(m)) {
        out.extend(deps_of(&util::git_show(root, base, m).unwrap_or_default()));
    }
    let products = |text: Option<String>| -> BTreeSet<String> {
        text.and_then(|t| t.parse::<toml::Table>().ok())
            .and_then(|t| {
                t.get("products")
                    .and_then(|p| p.as_table())
                    .map(|p| p.keys().map(|k| format!("lock:{k}")).collect())
            })
            .unwrap_or_default()
    };
    let new_p = products(util::read(&root.join("ecosystem.lock")).ok());
    let old_p = products(util::git_show(root, base, "ecosystem.lock"));
    out.extend(new_p.symmetric_difference(&old_p).cloned());
    let ids = |text: Option<String>| -> BTreeSet<String> {
        text.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| {
                v.as_object()
                    .map(|o| o.keys().map(|k| format!("contract:{k}")).collect())
            })
            .unwrap_or_default()
    };
    let new_c = ids(util::read(&root.join("contracts/index.json")).ok());
    let old_c = ids(util::git_show(root, base, "contracts/index.json"));
    out.extend(new_c.symmetric_difference(&old_c).cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affects_and_deps() {
        let a = parse_affects("# ADR-0007: x\nAffects: `crates/forest`, dep:rusqlite\n");
        assert!(a.contains("crates/forest") && a.contains("dep:rusqlite"));
        let d = deps_of(
            "[dependencies]\nserde = \"1\"\n[dev-dependencies]\nproptest = \"1\"\n[workspace.dependencies]\nx = { path = \"x\" }\n",
        );
        assert_eq!(d.len(), 3);
    }
}
