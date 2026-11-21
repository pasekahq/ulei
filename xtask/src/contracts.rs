//! Contract checks: index, metaschema, fixtures, CHANGELOG, generated code and schema
//! compatibility against a base revision (ADR-0005).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::util::{self, Errors, Res};

pub const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// What `gen-contracts` writes for a contract (index field `generate`, default `types`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generate {
    /// Wire types, and a round trip of every valid fixture through them.
    Types,
    /// The key catalogue of a TOML file; no types and no round trip.
    Keys,
}

/// One entry of `contracts/index.json` with its parsed document.
pub struct Contract {
    pub id: String,
    pub file: String,
    pub kind: String,
    pub version: String,
    pub status: String,
    pub title: String,
    /// Where the document comes from; for a vendored one it names the pin.
    pub source: String,
    pub generate: Generate,
    /// A repository file that must validate as the document root (index field `example`).
    pub example: Option<String>,
    pub doc: Value,
}

impl Contract {
    /// Module name of the generated Rust code, e.g. `c1`.
    pub fn module(&self) -> String {
        self.id.to_ascii_lowercase()
    }

    /// Named definitions: `$defs` of a JSON Schema or `components.schemas` of an `OpenAPI`
    /// document, with `$ref`s rewritten to `#/$defs/...`. The typed root of a JSON Schema
    /// document is included under its `title`.
    pub fn defs(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        let source = if self.kind == "openapi" {
            &self.doc["components"]["schemas"]
        } else {
            &self.doc["$defs"]
        };
        if let Some(map) = source.as_object() {
            for (k, v) in map {
                out.insert(
                    k.clone(),
                    map_refs(v, &|r| r.replace("#/components/schemas/", "#/$defs/")),
                );
            }
        }
        out
    }

    /// The typed root schema (JSON Schema documents with a `type`), named by `title`.
    pub fn root(&self) -> Option<(String, Value)> {
        if self.kind != "json-schema" || self.doc.get("type").is_none() {
            return None;
        }
        let name = self.doc["title"].as_str()?.to_string();
        let mut root = self.doc.clone();
        if let Some(o) = root.as_object_mut() {
            for k in ["$schema", "$id", "$defs", "title"] {
                o.remove(k);
            }
            o.retain(|k, _| !k.starts_with("x-"));
        }
        Some((name, root))
    }

    /// A standalone schema validating instances of `def` (`root` = the document root).
    /// A `$ref` of `#` inside a definition keeps naming the typed document root.
    pub fn schema_for(&self, def: &str) -> Option<Value> {
        let mut defs: Map<String, Value> = self.defs().into_iter().collect();
        if def == "root" {
            let (_, mut root) = self.root()?;
            root["$schema"] = json!(DRAFT_2020_12);
            root["$defs"] = Value::Object(defs);
            return Some(root);
        }
        if !defs.contains_key(def) {
            return None;
        }
        if let Some((name, root)) = self.root() {
            let to_root = format!("#/$defs/{name}");
            let retarget = |r: &str| {
                if r == "#" {
                    to_root.clone()
                } else {
                    r.to_string()
                }
            };
            defs = defs
                .into_iter()
                .map(|(k, v)| (k, map_refs(&v, &retarget)))
                .collect();
            defs.insert(name, map_refs(&root, &retarget));
        }
        Some(json!({"$schema": DRAFT_2020_12, "$defs": defs, "$ref": format!("#/$defs/{def}")}))
    }
}

/// `v` with every `$ref` string replaced by `f(ref)`.
fn map_refs(v: &Value, f: &dyn Fn(&str) -> String) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| {
                    let v = match (k.as_str(), v.as_str()) {
                        ("$ref", Some(r)) => Value::String(f(r)),
                        _ => map_refs(v, f),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| map_refs(x, f)).collect()),
        _ => v.clone(),
    }
}

/// Loads `contracts/index.json` and every contract document from `read(path)`.
pub fn load_with(read: &dyn Fn(&str) -> Option<String>) -> Result<Vec<Contract>, String> {
    let index_text = read("contracts/index.json").ok_or("contracts/index.json is missing")?;
    let index: Value =
        serde_json::from_str(&index_text).map_err(|e| format!("contracts/index.json: {e}"))?;
    let mut out = Vec::new();
    for (id, entry) in index
        .as_object()
        .ok_or("contracts/index.json must be an object")?
    {
        let field = |k: &str| {
            entry[k]
                .as_str()
                .map(str::to_string)
                .ok_or(format!("index {id}: `{k}` missing"))
        };
        let file = field("file")?;
        let text =
            read(&format!("contracts/{file}")).ok_or(format!("contracts/{file} is missing"))?;
        let doc = serde_json::from_str(&text).map_err(|e| format!("contracts/{file}: {e}"))?;
        let generate = match entry.get("generate").map(Value::as_str) {
            None | Some(Some("types")) => Generate::Types,
            Some(Some("keys")) => Generate::Keys,
            Some(_) => return Err(format!("index {id}: `generate` must be `types` or `keys`")),
        };
        let example = match entry.get("example") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .map(str::to_string)
                    .ok_or(format!("index {id}: `example` must be a path"))?,
            ),
        };
        out.push(Contract {
            id: id.clone(),
            kind: field("kind")?,
            version: field("version")?,
            status: field("status")?,
            title: field("title")?,
            source: field("source")?,
            generate,
            example,
            file,
            doc,
        });
    }
    out.sort_by_key(|c| c.id[1..].parse::<u32>().unwrap_or(u32::MAX));
    Ok(out)
}

pub fn load(root: &Path) -> Result<Vec<Contract>, String> {
    load_with(&|p| std::fs::read_to_string(root.join(p)).ok())
}

pub fn run(root: &Path, base: Option<&str>) -> Res {
    let mut errors = Errors::new();
    let contracts = load(root).map_err(|e| vec![e])?;
    let changelog = util::read(&root.join("contracts/CHANGELOG.md")).unwrap_or_default();
    for c in &contracts {
        check_contract(root, c, &changelog, &mut errors);
    }
    if let Err(e) = crate::codegen::run(root, true) {
        errors.extend(e);
    }
    if let Some(base) = base {
        compat(root, base, &contracts, &mut errors);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_contract(root: &Path, c: &Contract, changelog: &str, errors: &mut Errors) {
    let id = &c.id;
    if !["json-schema", "openapi"].contains(&c.kind.as_str()) {
        errors.push(format!("{id}: unknown kind `{}`", c.kind));
    }
    if !["stable", "draft", "vendored"].contains(&c.status.as_str()) {
        errors.push(format!("{id}: unknown status `{}`", c.status));
    }
    if util::semver(&c.version).is_none() {
        errors.push(format!("{id}: version `{}` is not x.y.z", c.version));
    }
    if !changelog
        .lines()
        .any(|l| l.trim() == format!("## {id} {}", c.version))
    {
        errors.push(format!(
            "{id}: contracts/CHANGELOG.md has no `## {id} {}` entry",
            c.version
        ));
    }
    if c.kind == "openapi" {
        if !c.doc["openapi"]
            .as_str()
            .is_some_and(|v| v.starts_with("3.1"))
        {
            errors.push(format!("{id}: `openapi` must be 3.1.x"));
        }
        if c.doc["info"]["version"].as_str() != Some(c.version.as_str()) {
            errors.push(format!(
                "{id}: info.version must equal the index version {}",
                c.version
            ));
        }
        if c.doc["paths"].as_object().is_none_or(Map::is_empty) {
            errors.push(format!("{id}: OpenAPI document has no paths"));
        }
    } else if c.doc["$schema"].as_str() != Some(DRAFT_2020_12) {
        errors.push(format!("{id}: `$schema` must be {DRAFT_2020_12}"));
    }
    // Metaschema: the whole document (JSON Schema) or every component schema (OpenAPI 3.1).
    let mut to_check: Vec<(String, Value)> = Vec::new();
    if c.kind == "json-schema" {
        to_check.push((c.file.clone(), c.doc.clone()));
    } else {
        for (name, s) in c.defs() {
            let mut s = s;
            s["$schema"] = json!(DRAFT_2020_12);
            to_check.push((format!("{}#{name}", c.file), s));
        }
    }
    for (what, s) in &to_check {
        if let Err(e) = jsonschema::meta::validate(s) {
            errors.push(format!("{id}: {what} fails the 2020-12 metaschema: {e}"));
        }
    }
    // Fixtures: fixtures/<cN>/{valid,invalid}/<Def>.<case>.{json,toml}
    let dir = root.join("contracts/fixtures").join(c.module());
    for (sub, want_valid) in [("valid", true), ("invalid", false)] {
        let mut files = util::files_with_ext(&dir.join(sub), "json");
        files.extend(util::files_with_ext(&dir.join(sub), "toml"));
        if files.is_empty() {
            errors.push(format!(
                "{id}: no {sub} fixtures in {}",
                util::rel(root, &dir.join(sub))
            ));
        }
        for f in files {
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let def = stem.split('.').next().unwrap_or("");
            check_instance(root, c, &f, def, want_valid, errors);
        }
    }
    if let Some(example) = &c.example {
        check_instance(root, c, &root.join(example), "root", true, errors);
    }
}

/// The JSON document of `path`, or of its TOML when the extension is `toml`.
fn read_instance(path: &Path) -> Result<Value, String> {
    if path.extension().and_then(|e| e.to_str()) != Some("toml") {
        return util::read_json(path);
    }
    toml_instance(&util::read(path)?).map_err(|e| format!("{}: {e}", path.display()))
}

/// The JSON document of a TOML text. Floats, datetimes and arrays are refused: the subset of
/// `ulei.toml` has none, and JSON Schema would take a float such as `1.0` as an integer.
fn toml_instance(text: &str) -> Result<Value, String> {
    fn outside(key: &str, value: &toml::Value) -> Option<String> {
        let what = match value {
            toml::Value::Float(_) => "a float",
            toml::Value::Datetime(_) => "a datetime",
            toml::Value::Array(_) => "an array",
            toml::Value::Table(t) => {
                return t
                    .iter()
                    .find_map(|(k, v)| outside(&format!("{key}.{k}"), v));
            }
            toml::Value::String(_) | toml::Value::Integer(_) | toml::Value::Boolean(_) => {
                return None;
            }
        };
        Some(format!(
            "{} is {what}, outside the subset of ulei.toml",
            key.trim_start_matches('.')
        ))
    }
    let table: toml::Table = text.parse().map_err(|e| format!("invalid TOML: {e}"))?;
    if let Some(e) = table.iter().find_map(|(k, v)| outside(&format!(".{k}"), v)) {
        return Err(e);
    }
    serde_json::to_value(table).map_err(|e| e.to_string())
}

/// Checks that the schema of `def` accepts the instance at `path` exactly when `want_valid`.
fn check_instance(
    root: &Path,
    c: &Contract,
    path: &Path,
    def: &str,
    want_valid: bool,
    errors: &mut Errors,
) {
    let id = &c.id;
    let rel = util::rel(root, path);
    let Some(schema) = c.schema_for(def) else {
        errors.push(format!("{rel}: `{def}` is not a definition of {id}"));
        return;
    };
    let instance = match read_instance(path) {
        Ok(v) => v,
        Err(e) => {
            errors.push(e);
            return;
        }
    };
    match jsonschema::validator_for(&schema) {
        Ok(v) => {
            let ok = v.is_valid(&instance);
            if ok != want_valid {
                errors.push(format!(
                    "{rel}: expected {} but the schema {}",
                    if want_valid { "valid" } else { "invalid" },
                    if ok { "accepts it" } else { "rejects it" }
                ));
            }
        }
        Err(e) => errors.push(format!("{id}: cannot compile schema for `{def}`: {e}")),
    }
}

/// Breaking changes between two versions of one schema, as human-readable paths.
pub fn breaking(path: &str, old: &Value, new: &Value, out: &mut Vec<String>) {
    for k in [
        "type",
        "$ref",
        "const",
        "pattern",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "format",
    ] {
        if old.get(k) != new.get(k) {
            out.push(format!("{path}: `{k}` changed"));
        }
    }
    if let (Some(o), Some(n)) = (old["enum"].as_array(), new["enum"].as_array()) {
        for v in o {
            if !n.contains(v) {
                out.push(format!("{path}: enum value {v} removed"));
            }
        }
    } else if old.get("enum").is_some() != new.get("enum").is_some() {
        out.push(format!("{path}: enum added or removed"));
    }
    if old.get("additionalProperties") != Some(&json!(false))
        && new.get("additionalProperties") == Some(&json!(false))
    {
        out.push(format!("{path}: additionalProperties became false"));
    }
    let req = |v: &Value| -> BTreeSet<String> {
        v["required"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    for r in req(new).difference(&req(old)) {
        out.push(format!("{path}: `{r}` became required"));
    }
    if let Some(op) = old["properties"].as_object() {
        let empty = Map::new();
        let np = new["properties"].as_object().unwrap_or(&empty);
        for (k, ov) in op {
            match np.get(k) {
                None => out.push(format!("{path}.{k}: property removed")),
                Some(nv) => breaking(&format!("{path}.{k}"), ov, nv, out),
            }
        }
    }
    if old.get("items").is_some() || new.get("items").is_some() {
        breaking(&format!("{path}[]"), &old["items"], &new["items"], out);
    }
}

fn compat(root: &Path, base: &str, head: &[Contract], errors: &mut Errors) {
    let Ok(old_contracts) = load_with(&|p| util::git_show(root, base, p)) else {
        eprintln!("xtask contracts: no contracts at {base}; compatibility check skipped");
        return;
    };
    let affects = crate::adr::added_affects(root, base);
    for old in &old_contracts {
        let id = &old.id;
        let Some(new) = head.iter().find(|c| &c.id == id) else {
            if !affects.contains(&format!("contract:{id}")) {
                errors.push(format!(
                    "{id} was removed without an ADR (`Affects: contract:{id}`)"
                ));
            }
            continue;
        };
        let adr_covers = || {
            affects.contains(&format!("contract:{id}"))
                || crate::adr::declares_version(root, id, &new.version)
        };
        compare(old, new, &adr_covers, errors);
    }
}

/// Errors of one contract changed from `old` to `new`; `adr_covers` says whether an ADR
/// accepts a breaking change of it at the new version.
fn compare(old: &Contract, new: &Contract, adr_covers: &dyn Fn() -> bool, errors: &mut Errors) {
    let id = &new.id;
    if old.doc == new.doc && old.version == new.version {
        return;
    }
    let (Some(ov), Some(nv)) = (util::semver(&old.version), util::semver(&new.version)) else {
        return;
    };
    // A vendored document carries its upstream version, so a new pin in `source` identifies it.
    let repinned = new.status == "vendored" && old.source != new.source;
    if old.doc != new.doc && nv <= ov && !repinned {
        errors.push(format!(
            "{id}: schema changed but version {} was not increased",
            new.version
        ));
    }
    let mut breaks = Vec::new();
    let (od, nd) = (old.defs(), new.defs());
    for (name, os) in &od {
        match nd.get(name) {
            None => breaks.push(format!("{name}: definition removed")),
            Some(ns) => breaking(name, os, ns, &mut breaks),
        }
    }
    if let (Some((_, o)), Some((_, n))) = (old.root(), new.root()) {
        breaking("root", &o, &n, &mut breaks);
    }
    if old.kind == "openapi" {
        let empty = Map::new();
        let np = new.doc["paths"].as_object().unwrap_or(&empty);
        for (p, ops) in old.doc["paths"].as_object().unwrap_or(&empty) {
            for m in ops
                .as_object()
                .map(|o| o.keys().collect::<Vec<_>>())
                .unwrap_or_default()
            {
                if np.get(p).and_then(|x| x.get(m)).is_none() {
                    breaks.push(format!("{m} {p}: operation removed"));
                }
            }
        }
    }
    if breaks.is_empty() {
        return;
    }
    let major_bump = if ov.0 == 0 {
        nv.0 > 0 || nv.1 > ov.1
    } else {
        nv.0 > ov.0
    };
    if !major_bump {
        errors.push(format!(
            "{id}: breaking change without a major version bump ({} -> {}): {}",
            old.version,
            new.version,
            breaks.join("; ")
        ));
    }
    if !adr_covers() {
        errors.push(format!(
            "{id}: breaking change needs a new ADR with `Affects: contract:{id}`, or an ADR with it that names `{id} {}`: {}",
            new.version,
            breaks.join("; ")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_breaking_changes() {
        let old = json!({"type": "object", "required": ["a"], "properties": {"a": {"type": "string", "enum": ["x", "y"]}, "b": {"type": "integer"}}});
        let new = json!({"type": "object", "required": ["a", "b"], "additionalProperties": false, "properties": {"a": {"type": "string", "enum": ["x"]}, "b": {"type": "integer"}}});
        let mut out = Vec::new();
        breaking("T", &old, &new, &mut out);
        assert_eq!(out.len(), 3, "{out:?}");
    }

    #[test]
    fn definition_ref_to_root_validates_as_root() {
        let c = Contract {
            id: "C9".into(),
            file: "c9.schema.json".into(),
            kind: "json-schema".into(),
            version: "1.0.0".into(),
            status: "draft".into(),
            title: "Nine".into(),
            source: "test".into(),
            generate: Generate::Types,
            example: None,
            doc: json!({
                "title": "Packet", "type": "object", "required": ["n"],
                "properties": {"n": {"$ref": "#/$defs/Count"}},
                "$defs": {
                    "Count": {"type": "integer"},
                    "Wrap": {"type": "object", "properties": {"p": {"$ref": "#"}}}
                }
            }),
        };
        let schema = c.schema_for("Wrap").expect("Wrap is a definition");
        let v = jsonschema::validator_for(&schema).expect("schema compiles");
        assert!(v.is_valid(&json!({"p": {"n": 1}})));
        assert!(!v.is_valid(&json!({"p": {"n": "x"}})));
        assert!(!v.is_valid(&json!({"p": {}})));
    }

    #[test]
    fn toml_instances_refuse_floats_datetimes_and_arrays() {
        let refused = [
            (
                "[engine.routes.main]\nmax_tokens = 1.0\n",
                "engine.routes.main.max_tokens is a float",
            ),
            (
                "[inbox]\nroot = 1979-05-27T07:32:00Z\n",
                "inbox.root is a datetime",
            ),
            (
                "[engine]\nmax_steps = [1, 2]\n",
                "engine.max_steps is an array",
            ),
            (
                "[engine]\nr = { a = { b = 1.5 } }\n",
                "engine.r.a.b is a float",
            ),
        ];
        for (text, want) in refused {
            let err = toml_instance(text).expect_err(text);
            assert!(err.starts_with(want), "{err}");
        }
        assert_eq!(
            toml_instance("[engine.routes.main]\nmax_tokens = 1\n[inbox]\ndeliver = true\n"),
            Ok(
                json!({"engine": {"routes": {"main": {"max_tokens": 1}}}, "inbox": {"deliver": true}})
            )
        );
    }

    #[test]
    fn additive_change_is_compatible() {
        let old = json!({"type": "object", "properties": {"a": {"type": "string", "enum": ["x"]}}});
        let new = json!({"type": "object", "properties": {"a": {"type": "string", "enum": ["x", "y"]}, "c": {"type": "boolean"}}});
        let mut out = Vec::new();
        breaking("T", &old, &new, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    fn gateway(status: &str, source: &str, cache: &[&str]) -> Contract {
        Contract {
            id: "C4G".into(),
            file: "c4g.openapi.json".into(),
            kind: "openapi".into(),
            version: "1.0.0".into(),
            status: status.into(),
            title: "Gateway".into(),
            source: source.into(),
            generate: Generate::Types,
            example: None,
            doc: json!({
                "openapi": "3.1.0",
                "info": {"title": "Gateway", "version": "1.0.0"},
                "paths": {},
                "components": {"schemas": {"CacheMode": {"type": "string", "enum": cache}}}
            }),
        }
    }

    fn compare_errors(old: &Contract, new: &Contract) -> Errors {
        let mut errors = Errors::new();
        compare(old, new, &|| false, &mut errors);
        errors
    }

    #[test]
    fn vendored_additive_change_at_a_new_pin_keeps_the_upstream_version() {
        let old = gateway("vendored", "qb@aaaa", &["auto"]);
        let new = gateway("vendored", "qb@bbbb", &["auto", "auto_1h"]);
        assert_eq!(compare_errors(&old, &new), Errors::new());
    }

    #[test]
    fn vendored_change_at_the_same_pin_needs_a_new_version() {
        let old = gateway("vendored", "qb@aaaa", &["auto"]);
        let new = gateway("vendored", "qb@aaaa", &["auto", "auto_1h"]);
        assert_eq!(
            compare_errors(&old, &new),
            vec!["C4G: schema changed but version 1.0.0 was not increased".to_string()]
        );
    }

    #[test]
    fn vendored_breaking_change_at_a_new_pin_needs_a_major_bump() {
        let old = gateway("vendored", "qb@aaaa", &["auto", "auto_1h"]);
        let new = gateway("vendored", "qb@bbbb", &["auto"]);
        let errors = compare_errors(&old, &new);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0]
                .starts_with("C4G: breaking change without a major version bump (1.0.0 -> 1.0.0)"),
            "{errors:?}"
        );
        assert!(
            errors[1].starts_with("C4G: breaking change needs a new ADR"),
            "{errors:?}"
        );
    }

    #[test]
    fn draft_change_with_a_new_source_needs_a_new_version() {
        let old = gateway("draft", "qb@aaaa", &["auto"]);
        let new = gateway("draft", "qb@bbbb", &["auto", "auto_1h"]);
        assert_eq!(
            compare_errors(&old, &new),
            vec!["C4G: schema changed but version 1.0.0 was not increased".to_string()]
        );
    }
}
