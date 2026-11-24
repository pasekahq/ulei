//! Architecture boundaries (ADR-0001, ADR-0002, ADR-0072), checked on `cargo metadata` and the
//! sources.
//!
//! Crate graph:
//! - every crate lives in `crates/<dir>` and is named `ulei-<dir>` (plus the `xtask` tool);
//! - `ulei-contracts` depends on no workspace crate;
//! - a library crate (class `"lib"`, ADR-0072) depends on no workspace crate, uses no contract,
//!   has no binary or build script, and names the modules that may use it in `users = [...]`;
//! - a module crate (class 2) may depend on `ulei-contracts` and on the library crates that list
//!   it in their `users`, and on no other workspace crate;
//! - nothing depends on `xtask`, and `xtask` depends on no workspace crate;
//! - path dependencies stay inside the workspace;
//! - `[package.metadata.ulei]` declares `class`, `role` and the contracts the crate uses.
//!
//! Sources (`crates/**/*.rs`):
//! - no `#[path = ...]` and no `include!` (the Rust form of `sys.path` tricks);
//! - no `include_str!`/`include_bytes!` reaching outside the crate (`../`);
//! - no spawning another module's CLI (`Command::new("ulei...")`);
//! - only `crates/runtime-adapters` may mention Hermes;
//! - no personal paths (`/home/`, `~/.`); ids and tokens come from config (gitleaks covers secrets);
//! - no model-vendor hosts or vendor credential paths (ADR-0009): models are reached only
//!   through the qb gateway.
//! - a library's `src/` reads no environment or `ulei.toml`, spawns no process and opens no
//!   socket (ADR-0072): it holds computation, the caller hands it paths and values.
//!
//! Python tree `hive-py/` (ADR-0068):
//! - `hive-py/**/*.py` and `hive-py/**/*.toml` keep the bans above that are not Rust syntax;
//! - `module.toml` names only contracts of `contracts/index.json`;
//! - `requirements.lock` pins only admitted packages.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::pytree;
use crate::util::{self, Errors, Res};

pub const CONTRACTS_CRATE: &str = "ulei-contracts";
/// `[package.metadata.ulei] class` of a library crate (ADR-0072).
pub const LIBRARY_CLASS: &str = "lib";
/// Cargo target kinds a library crate may have: no `bin`, no `custom-build` (ADR-0072).
const LIBRARY_TARGET_KINDS: &[&str] = &["lib", "rlib", "test", "example", "bench"];
/// What a library's `src/` must not contain (ADR-0072): environment and config reading,
/// processes, sockets.
const LIBRARY_SOURCE_BANS: &[&str] = &[
    "std::env",
    "env::var",
    "std::process",
    "process::Command",
    "std::net",
    "os::unix::net",
    "UnixStream",
    "UnixListener",
    "TcpStream",
    "TcpListener",
    "UdpSocket",
    "ulei.toml",
];
const CONTRACTS_HANDWRITTEN_BUDGET: usize = 1500;

/// Model-vendor endpoints, lowercase; ulei reaches models only through the qb gateway.
const VENDOR_HOSTS: &[&str] = &[
    "api.anthropic.com",
    "platform.claude.com",
    "console.anthropic.com",
    "api.openai.com",
    "auth.openai.com",
    "chatgpt.com",
    "openrouter.ai",
];

/// Vendor credential files, lowercase; credentials live in qb and its config.
const VENDOR_CREDENTIAL_PATHS: &[&str] = &[".claude/.credentials.json", ".codex/auth.json"];

pub fn run(root: &Path) -> Res {
    let mut errors = Errors::new();
    let meta = util::metadata(root).map_err(|e| vec![e])?;
    let index = util::read_json(&root.join("contracts/index.json")).map_err(|e| vec![e])?;
    let known: BTreeSet<String> = index
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    check_graph(root, &meta, &known, &mut errors);
    check_sources(root, &library_dirs(&meta), &mut errors);
    check_python_tree(root, &known, &mut errors);
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
    let libraries: BTreeMap<&str, Vec<&str>> = packages
        .iter()
        .filter(|p| p["metadata"]["ulei"]["class"].as_str() == Some(LIBRARY_CLASS))
        .filter_map(|p| {
            let users = p["metadata"]["ulei"]["users"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            Some((p["name"].as_str()?, users))
        })
        .collect();
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
        let is_library = libraries.contains_key(name);
        if is_contracts {
            if class.as_str() != Some("contracts") {
                errors.push(format!(
                    "{name}: [package.metadata.ulei] class must be \"contracts\""
                ));
            }
        } else if is_library {
            check_library(name, pkg, &names, errors);
        } else if class.as_i64() != Some(2) {
            errors.push(format!(
                "{name}: [package.metadata.ulei] class must be 2 or \"{LIBRARY_CLASS}\" (class-1 products live in their own repositories and are pinned in ecosystem.lock; libraries: ADR-0072)"
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
            } else if is_library {
                errors.push(format!(
                    "{name}: a library crate must not depend on workspace crate `{dep}` (ADR-0072)"
                ));
            } else if let Some(users) = libraries.get(dep) {
                if !users.contains(&name) {
                    errors.push(format!(
                        "{name} -> {dep}: the library does not list `{name}` in its [package.metadata.ulei] users; adding a user needs an ADR (ADR-0072)"
                    ));
                }
            } else if *dep != CONTRACTS_CRATE && names.contains(dep) {
                errors.push(format!(
                    "{name} -> {dep}: modules talk to each other only through `{CONTRACTS_CRATE}` and the wire (ADR-0001)"
                ));
            }
        }
        if !is_contracts
            && !is_library
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

/// The rules of a library crate (ADR-0072) besides its workspace dependencies.
fn check_library(name: &str, pkg: &Value, names: &BTreeSet<&str>, errors: &mut Errors) {
    let ulei = &pkg["metadata"]["ulei"];
    if ulei["contracts"].as_array().is_some_and(|a| !a.is_empty()) {
        errors.push(format!(
            "{name}: a library crate uses no contract; contracts = [] (ADR-0072)"
        ));
    }
    match ulei["users"].as_array() {
        None => errors.push(format!(
            "{name}: [package.metadata.ulei] users = [...] is missing (ADR-0072)"
        )),
        Some(users) => {
            for u in users {
                let u = u.as_str().unwrap_or("?");
                if !names.contains(u) || u == name || u == CONTRACTS_CRATE || u == "xtask" {
                    errors.push(format!(
                        "{name}: users names `{u}`, which is not a module crate (ADR-0072)"
                    ));
                }
            }
        }
    }
    let empty = Vec::new();
    for t in pkg["targets"].as_array().unwrap_or(&empty) {
        for kind in t["kind"].as_array().unwrap_or(&empty) {
            let kind = kind.as_str().unwrap_or("?");
            if !LIBRARY_TARGET_KINDS.contains(&kind) {
                errors.push(format!(
                    "{name}: a library crate has no `{kind}` target `{}` (ADR-0072)",
                    t["name"].as_str().unwrap_or("?")
                ));
            }
        }
    }
}

/// `crates/<dir>/` of every library crate.
fn library_dirs(meta: &Value) -> Vec<String> {
    let empty = Vec::new();
    meta["packages"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter(|p| p["metadata"]["ulei"]["class"].as_str() == Some(LIBRARY_CLASS))
        .filter_map(|p| p["name"].as_str()?.strip_prefix("ulei-"))
        .map(|dir| format!("crates/{dir}/"))
        .collect()
}

/// Returns the violations of the library source bans (ADR-0072) in one file of a library's `src/`.
pub fn library_violations(rel: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if let Some(ban) = LIBRARY_SOURCE_BANS.iter().find(|b| line.contains(*b)) {
            out.push(format!(
                "{rel}:{}: `{ban}` in a library crate; config, environment, processes and sockets belong to the module that links it (ADR-0072)",
                n + 1
            ));
        }
    }
    out
}

/// Returns the violations of the source rules in one Rust file's text.
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
        line_bans(&at, line, in_adapters, &mut out);
    }
    out
}

/// Returns the violations of the bans that are not Rust syntax in one file's text.
fn text_violations(rel: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        line_bans(&format!("{rel}:{}", n + 1), line, false, &mut out);
    }
    out
}

fn line_bans(at: &str, line: &str, may_name_hermes: bool, out: &mut Vec<String>) {
    let lower = line.to_ascii_lowercase();
    if !may_name_hermes && lower.contains("hermes") {
        out.push(format!(
            "{at}: only crates/runtime-adapters may know about Hermes"
        ));
    }
    if line.contains("/home/") || line.contains("~/.") {
        out.push(format!(
            "{at}: personal paths and ids belong in config, not in code"
        ));
    }
    if let Some(name) = VENDOR_HOSTS
        .iter()
        .chain(VENDOR_CREDENTIAL_PATHS)
        .find(|name| lower.contains(*name))
    {
        out.push(format!(
            "{at}: model vendor `{name}` is forbidden; talk to the qb gateway instead (ADR-0009)"
        ));
    }
}

fn check_sources(root: &Path, libraries: &[String], errors: &mut Errors) {
    for file in util::files_with_ext(&root.join("crates"), "rs") {
        let rel = util::rel(root, &file);
        match util::read(&file) {
            Ok(text) => {
                errors.extend(source_violations(&rel, &text));
                if libraries
                    .iter()
                    .any(|dir| rel.starts_with(&format!("{dir}src/")))
                {
                    errors.extend(library_violations(&rel, &text));
                }
            }
            Err(e) => errors.push(e),
        }
    }
}

/// The rules of the Python tree `hive-py/`.
fn check_python_tree(root: &Path, known: &BTreeSet<String>, errors: &mut Errors) {
    let dir = root.join(pytree::DIR);
    for ext in ["py", "toml"] {
        for file in util::files_with_ext(&dir, ext) {
            let rel = util::rel(root, &file);
            match util::read(&file) {
                Ok(text) => errors.extend(text_violations(&rel, &text)),
                Err(e) => errors.push(e),
            }
        }
    }
    let tree = match pytree::load(root) {
        Ok(tree) => tree,
        Err(e) => {
            errors.extend(e);
            return;
        }
    };
    for c in &tree.contracts {
        if !known.contains(c) {
            errors.push(format!(
                "{}/module.toml: uses unknown contract `{c}` (not in contracts/index.json)",
                pytree::DIR
            ));
        }
    }
    for (name, version) in &tree.packages {
        if !pytree::ADMITTED.contains(&name.as_str()) {
            errors.push(format!(
                "{}/requirements.lock: `{name}=={version}` is not admitted; a package needs an ADR with `Affects: {}, dep:py:{name}` (ADR-0068 section 3.5)",
                pytree::DIR,
                pytree::DIR
            ));
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
            "crates/contracts: {lines} hand-written lines > budget {CONTRACTS_HANDWRITTEN_BUDGET} (only thin wire helpers and the two C9 decoders, config and env_file, may be hand-written)"
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

    fn lib(root: &str, dir: &str, deps: &[&str], users: &[&str]) -> Value {
        let mut p = pkg(root, dir, json!(LIBRARY_CLASS), deps);
        p["metadata"]["ulei"]["contracts"] = json!([]);
        p["metadata"]["ulei"]["users"] = json!(users);
        p["targets"] = json!([{"name": format!("ulei_{dir}"), "kind": ["lib"]}, {"name": "golden", "kind": ["test"]}]);
        p
    }

    #[test]
    fn module_may_depend_on_a_library_that_lists_it() {
        let r = "/nonexistent-ulei-root";
        let errors = graph(vec![
            pkg(r, "contracts", json!("contracts"), &[]),
            pkg(r, "garden", json!(2), &["ulei-contracts", "ulei-infer"]),
            lib(r, "infer", &[], &["ulei-garden"]),
        ]);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn module_not_in_the_users_of_a_library_is_red() {
        let r = "/nonexistent-ulei-root";
        let errors = graph(vec![
            pkg(r, "contracts", json!("contracts"), &[]),
            pkg(r, "garden", json!(2), &["ulei-contracts"]),
            pkg(r, "hive", json!(2), &["ulei-contracts", "ulei-infer"]),
            lib(r, "infer", &[], &["ulei-garden"]),
        ]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("ulei-hive -> ulei-infer: the library does not list"));
    }

    #[test]
    fn library_depends_on_no_workspace_crate() {
        let r = "/nonexistent-ulei-root";
        for dep in ["ulei-contracts", "ulei-garden", "ulei-tokens"] {
            let errors = graph(vec![
                pkg(r, "contracts", json!("contracts"), &[]),
                pkg(r, "garden", json!(2), &["ulei-contracts"]),
                lib(r, "infer", &[dep], &["ulei-garden"]),
                lib(r, "tokens", &[], &["ulei-garden"]),
            ]);
            assert_eq!(
                errors,
                [format!(
                    "ulei-infer: a library crate must not depend on workspace crate `{dep}` (ADR-0072)"
                )]
            );
        }
    }

    #[test]
    fn library_has_users_no_contracts_and_no_binary() {
        let r = "/nonexistent-ulei-root";
        let mut bad = lib(r, "infer", &[], &["ulei-hive", "ulei-contracts"]);
        bad["metadata"]["ulei"]["contracts"] = json!(["C1"]);
        bad["targets"] = json!([
            {"name": "ulei_infer", "kind": ["lib"]},
            {"name": "encode", "kind": ["bin"]},
            {"name": "build-script-build", "kind": ["custom-build"]},
            {"name": "demo", "kind": ["example"]},
        ]);
        let errors = graph(vec![pkg(r, "contracts", json!("contracts"), &[]), bad]);
        assert_eq!(errors.len(), 5, "{errors:?}");
        let mut none = lib(r, "infer", &[], &[]);
        none["metadata"]["ulei"]
            .as_object_mut()
            .unwrap()
            .remove("users");
        let errors = graph(vec![none]);
        assert_eq!(
            errors,
            ["ulei-infer: [package.metadata.ulei] users = [...] is missing (ADR-0072)"]
        );
    }

    #[test]
    fn library_sources_read_no_environment_and_open_no_socket() {
        let bad = "use std::env;
let p = std::process::Command::new(\"x\");
let s = UnixStream::connect(p);
let c = \"ulei.toml\";
let f = std::fs::File::open(path)?;
";
        let v = library_violations("crates/infer/src/weights.rs", bad);
        let at: Vec<&str> = v.iter().map(|e| e.split(": ").next().unwrap()).collect();
        assert_eq!(
            at,
            [
                "crates/infer/src/weights.rs:1",
                "crates/infer/src/weights.rs:2",
                "crates/infer/src/weights.rs:3",
                "crates/infer/src/weights.rs:4",
            ],
            "{v:?}"
        );
        let dirs = library_dirs(
            &json!({"packages": [lib("/r", "infer", &[], &[]), pkg("/r", "garden", json!(2), &[])]}),
        );
        assert_eq!(dirs, ["crates/infer/"]);
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

    const MODULE: &str = "role = \"hive\"\ncontracts = [\"C1\"]\n";
    const LOCK: &str = "# runtime\ncronsim==2.7 --hash=sha256:00  # BSD-3-Clause\n";

    /// Errors of `check_python_tree` on a temporary tree holding `files` under `hive-py/`.
    fn python_tree(name: &str, files: &[(&str, &str)]) -> Errors {
        let root = std::env::temp_dir().join(format!("xtask-arch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, text) in files {
            let path = root.join("hive-py").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let known: BTreeSet<String> = ["C1".to_string()].into();
        let mut errors = Errors::new();
        check_python_tree(&root, &known, &mut errors);
        std::fs::remove_dir_all(&root).unwrap();
        errors
    }

    #[test]
    fn clean_python_tree_passes() {
        let errors = python_tree(
            "clean",
            &[
                ("module.toml", MODULE),
                ("requirements.lock", LOCK),
                ("hive/store.py", "import sqlite3\nROOT = 'tmp/hive'\n"),
            ],
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn python_tree_keeps_the_text_bans() {
        let errors = python_tree(
            "bans",
            &[
                ("module.toml", MODULE),
                ("requirements.lock", LOCK),
                (
                    "hive/heads.py",
                    "ok = 1\nROOT = '/home/someone/hive'\n# like Hermes does\nURL = 'https://api.openai.com/v1'\n",
                ),
                ("config.example.toml", "auth = \"~/.codex/auth.json\"\n"),
            ],
        );
        let at: Vec<&str> = errors
            .iter()
            .map(|e| e.split(": ").next().unwrap())
            .collect();
        assert_eq!(
            at,
            [
                "hive-py/hive/heads.py:2",
                "hive-py/hive/heads.py:3",
                "hive-py/hive/heads.py:4",
                "hive-py/config.example.toml:1",
                "hive-py/config.example.toml:1",
            ],
            "{errors:?}"
        );
        assert!(errors[2].contains("api.openai.com") && errors[4].contains(".codex/auth.json"));
    }

    #[test]
    fn python_tree_contract_must_be_in_the_index() {
        let errors = python_tree(
            "contract",
            &[
                (
                    "module.toml",
                    "role = \"hive\"\ncontracts = [\"C1\", \"C99\"]\n",
                ),
                ("requirements.lock", LOCK),
            ],
        );
        assert_eq!(
            errors,
            ["hive-py/module.toml: uses unknown contract `C99` (not in contracts/index.json)"]
        );
    }

    #[test]
    fn python_tree_lock_holds_only_admitted_packages() {
        let errors = python_tree(
            "admitted",
            &[
                ("module.toml", MODULE),
                (
                    "requirements.lock",
                    "CronSim==2.7 --hash=sha256:00  # BSD-3-Clause\nrequests==2.32.3 --hash=sha256:00  # Apache-2.0\n",
                ),
            ],
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].starts_with("hive-py/requirements.lock: `requests==2.32.3` is not admitted"),
            "{errors:?}"
        );
    }

    #[test]
    fn python_tree_lock_pins_are_exact() {
        let errors = python_tree(
            "exact",
            &[
                ("module.toml", MODULE),
                ("requirements.lock", "# runtime\ncronsim>=2.7\n"),
            ],
        );
        assert_eq!(
            errors,
            ["hive-py/requirements.lock:2: `cronsim>=2.7` is not an exact `name==version` pin"]
        );
    }

    #[test]
    fn vendor_hosts_and_credential_paths_are_red_in_every_crate() {
        let names = [
            "api.anthropic.com",
            "Platform.Claude.com",
            "console.anthropic.com",
            "API.OPENAI.COM",
            "auth.openai.com",
            "chatgpt.com",
            "openrouter.ai",
            ".claude/.credentials.json",
            ".Codex/auth.json",
        ];
        let mut text = String::from("let vendor = \"anthropic\"; // model from openai\n");
        for name in names {
            text.push_str(&format!("const U: &str = \"https://{name}/v1\";\n"));
        }
        for rel in [
            "crates/engine/src/lib.rs",
            "crates/runtime-adapters/src/lib.rs",
        ] {
            let v = source_violations(rel, &text);
            assert_eq!(v.len(), names.len(), "{v:?}");
            for (n, name) in names.iter().enumerate() {
                let at = format!("{rel}:{}:", n + 2);
                let named = v[n].contains(&name.to_ascii_lowercase());
                assert!(
                    v[n].starts_with(&at) && named && v[n].contains("ADR-0009"),
                    "{v:?}"
                );
            }
        }
    }
}
