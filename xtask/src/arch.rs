//! Architecture boundaries (ADR-0001, ADR-0002), checked on `cargo metadata` and the sources.
//!
//! Crate graph:
//! - every crate lives in `crates/<dir>` and is named `ulei-<dir>` (plus the `xtask` tool);
//! - `ulei-contracts` depends on no workspace crate;
//! - a module crate (class 2) may depend on exactly one workspace crate: `ulei-contracts`;
//! - nothing depends on `xtask`, and `xtask` depends on no workspace crate;
//! - path dependencies stay inside the workspace;
//! - `[package.metadata.ulei]` declares `class`, `role` and the contracts the crate uses.
//!
//! Sources (`crates/**/*.rs`):
//! - no `#[path = ...]` and no `include!` (the Rust form of `sys.path` tricks);
//! - no `include_str!`/`include_bytes!` reaching outside the crate (`../`);
//! - no spawning another module's CLI (`Command::new("ulei...")`);
//! - only `crates/runtime-adapters` may mention Hermes;
//! - no personal paths (`/home/`, `~/.`); ids and tokens come from config (gitleaks covers secrets).

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::util::{self, Errors, Res};

pub const CONTRACTS_CRATE: &str = "ulei-contracts";
const CONTRACTS_HANDWRITTEN_BUDGET: usize = 1500;

pub fn run(root: &Path) -> Res {
    let mut errors = Errors::new();
    let meta = util::metadata(root).map_err(|e| vec![e])?;
    let index = util::read_json(&root.join("contracts/index.json")).map_err(|e| vec![e])?;
    let known: BTreeSet<String> = index
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    check_graph(root, &meta, &known, &mut errors);
    check_sources(root, &mut errors);
    check_contracts_budget(root, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Graph rules over `cargo metadata` output. Public for the unit tests below.
pub fn check_graph(root: &Path, meta: &Value, known: &BTreeSet<String>, errors: &mut Errors) {
    let empty = Vec::new();
    let packages = meta["packages"].as_array().unwrap_or(&empty);
    let names: BTreeSet<&str> = packages.iter().filter_map(|p| p["name"].as_str()).collect();
    let mut dirs_seen = BTreeSet::new();
    for pkg in packages {
        let name = pkg["name"].as_str().unwrap_or("?");
        let manifest = Path::new(pkg["manifest_path"].as_str().unwrap_or(""));
        let rel = util::rel(root, manifest);
        let deps = pkg["dependencies"].as_array().unwrap_or(&empty);
        let path_deps: Vec<(&str, &str)> = deps
            .iter()
            .filter_map(|d| Some((d["name"].as_str()?, d["path"].as_str()?)))
            .collect();
        for (dep, path) in &path_deps {
            if !Path::new(path).starts_with(root) {
                errors.push(format!(
                    "{name}: path dependency `{dep}` points outside the workspace ({path})"
                ));
            }
            if *dep == "xtask" {
                errors.push(format!("{name}: must not depend on the `xtask` tool"));
            }
        }
        if name == "xtask" {
            if rel != "xtask/Cargo.toml" {
                errors.push(format!("xtask must live in xtask/, found {rel}"));
            }
            for (dep, _) in &path_deps {
                errors.push(format!("xtask must not depend on workspace crate `{dep}`"));
            }
            continue;
        }
        let Some(dir) = rel
            .strip_prefix("crates/")
            .and_then(|r| r.strip_suffix("/Cargo.toml"))
        else {
            errors.push(format!(
                "{name}: crates must live in crates/<dir>/, found {rel}"
            ));
            continue;
        };
        dirs_seen.insert(dir.to_string());
        if name != format!("ulei-{dir}") {
            errors.push(format!(
                "{rel}: package must be named `ulei-{dir}`, found `{name}`"
            ));
        }
        let ulei = &pkg["metadata"]["ulei"];
        let class = &ulei["class"];
        let is_contracts = name == CONTRACTS_CRATE;
        if is_contracts {
            if class.as_str() != Some("contracts") {
                errors.push(format!(
                    "{name}: [package.metadata.ulei] class must be \"contracts\""
                ));
            }
        } else if class.as_i64() != Some(2) {
            errors.push(format!(
                "{name}: [package.metadata.ulei] class must be 2 (class-1 products live in their own repositories and are pinned in ecosystem.lock)"
            ));
        }
        if ulei["role"].as_str().is_none_or(|r| r.trim().is_empty()) {
            errors.push(format!("{name}: [package.metadata.ulei] role is missing"));
        }
        let used: Vec<&str> = ulei["contracts"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if ulei["contracts"].as_array().is_none() {
            errors.push(format!(
                "{name}: [package.metadata.ulei] contracts = [...] is missing"
            ));
        }
        for c in &used {
            if !known.contains(*c) {
                errors.push(format!(
                    "{name}: uses unknown contract `{c}` (not in contracts/index.json)"
                ));
            }
        }
        for (dep, _) in &path_deps {
            if is_contracts {
                errors.push(format!(
                    "{name}: the contracts crate must not depend on workspace crate `{dep}`"
                ));
            } else if *dep != CONTRACTS_CRATE && names.contains(dep) {
                errors.push(format!(
                    "{name} -> {dep}: modules talk to each other only through `{CONTRACTS_CRATE}` and the wire (ADR-0001)"
                ));
            }
        }
        if !is_contracts
            && !used.is_empty()
            && !path_deps.iter().any(|(d, _)| *d == CONTRACTS_CRATE)
        {
            errors.push(format!(
                "{name}: declares contracts but does not depend on `{CONTRACTS_CRATE}`"
            ));
        }
    }
    if let Ok(entries) = std::fs::read_dir(root.join("crates")) {
        for e in entries.flatten() {
            let dir = e.file_name().to_string_lossy().to_string();
            if e.path().join("Cargo.toml").exists() && !dirs_seen.contains(&dir) {
                errors.push(format!("crates/{dir} is not a workspace member"));
            }
        }
    }
}

/// Returns the violations of the source rules in one file's text.
pub fn source_violations(rel: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let in_adapters = rel.starts_with("crates/runtime-adapters/");
    for (n, line) in text.lines().enumerate() {
        let at = format!("{rel}:{}", n + 1);
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.contains("#[path") {
            out.push(format!(
                "{at}: `#[path]` is forbidden; use a normal module tree"
            ));
        }
        if compact.contains("include!(") && !compact.contains("_include!(") {
            out.push(format!("{at}: `include!` is forbidden"));
        }
        if (compact.contains("include_str!(") || compact.contains("include_bytes!("))
            && compact.contains("../")
        {
            out.push(format!("{at}: do not include files from outside the crate; read contracts through ulei-contracts"));
        }
        if compact.contains("Command::new(\"ulei") {
            out.push(format!(
                "{at}: do not spawn another module's CLI; use a contract"
            ));
        }
        if !in_adapters && line.to_ascii_lowercase().contains("hermes") {
            out.push(format!(
                "{at}: only crates/runtime-adapters may know about Hermes"
            ));
        }
        if line.contains("/home/") || line.contains("~/.") {
            out.push(format!(
                "{at}: personal paths and ids belong in config, not in code"
            ));
        }
    }
    out
}

fn check_sources(root: &Path, errors: &mut Errors) {
    for file in util::files_with_ext(&root.join("crates"), "rs") {
        let rel = util::rel(root, &file);
        match util::read(&file) {
            Ok(text) => errors.extend(source_violations(&rel, &text)),
            Err(e) => errors.push(e),
        }
    }
}

fn check_contracts_budget(root: &Path, errors: &mut Errors) {
    let src = root.join("crates/contracts/src");
    let generated = src.join("generated");
    let lines: usize = util::files_with_ext(&src, "rs")
        .iter()
        .filter(|f| !f.starts_with(&generated))
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .map(|t| t.lines().count())
        .sum();
    if lines > CONTRACTS_HANDWRITTEN_BUDGET {
        errors.push(format!(
            "crates/contracts: {lines} hand-written lines > budget {CONTRACTS_HANDWRITTEN_BUDGET} (no business logic in contracts)"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pkg(root: &str, dir: &str, class: Value, deps: &[&str]) -> Value {
        let deps: Vec<Value> = deps
            .iter()
            .map(|d| json!({"name": d, "path": format!("{root}/crates/{}", d.trim_start_matches("ulei-"))}))
            .collect();
        json!({
            "name": format!("ulei-{dir}"),
            "manifest_path": format!("{root}/crates/{dir}/Cargo.toml"),
            "dependencies": deps,
            "metadata": {"ulei": {"class": class, "role": "r", "contracts": ["C1"]}}
        })
    }

    fn graph(packages: Vec<Value>) -> Errors {
        let root = "/nonexistent-ulei-root";
        let mut errors = Errors::new();
        let known: BTreeSet<String> = ["C1".to_string()].into();
        check_graph(
            Path::new(root),
            &json!({"packages": packages}),
            &known,
            &mut errors,
        );
        errors
    }

    #[test]
    fn clean_graph_passes() {
        let r = "/nonexistent-ulei-root";
        let errors = graph(vec![
            pkg(r, "contracts", json!("contracts"), &[]),
            pkg(r, "core", json!(2), &["ulei-contracts"]),
            pkg(r, "hive", json!(2), &["ulei-contracts"]),
        ]);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn cross_module_dependency_is_red() {
        let r = "/nonexistent-ulei-root";
        let errors = graph(vec![
            pkg(r, "contracts", json!("contracts"), &[]),
            pkg(r, "core", json!(2), &["ulei-contracts"]),
            pkg(r, "hive", json!(2), &["ulei-contracts", "ulei-core"]),
        ]);
        assert!(
            errors.iter().any(|e| e.contains("ulei-hive -> ulei-core")),
            "{errors:?}"
        );
    }

    #[test]
    fn contracts_must_be_a_leaf() {
        let r = "/nonexistent-ulei-root";
        let errors = graph(vec![
            pkg(r, "contracts", json!("contracts"), &["ulei-core"]),
            pkg(r, "core", json!(2), &["ulei-contracts"]),
        ]);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("contracts crate must not depend")),
            "{errors:?}"
        );
    }

    #[test]
    fn source_bans() {
        let bad = "#[path = \"../../x.rs\"]\nmod x;\nconst P: &str = \"/home/someone\";\nfn f() { Command::new(\"ulei-hive\"); }\n// talks to hermes\n";
        let v = source_violations("crates/hive/src/lib.rs", bad);
        assert_eq!(v.len(), 4, "{v:?}");
        assert!(
            source_violations("crates/runtime-adapters/src/lib.rs", "// hermes plugin\n")
                .is_empty()
        );
    }
}
