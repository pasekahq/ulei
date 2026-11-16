//! Contract checks: index, metaschema, fixtures, CHANGELOG, generated code and schema
//! compatibility against a base revision (ADR-0005).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::util::{self, Errors, Res};

pub const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// One entry of `contracts/index.json` with its parsed document.
pub struct Contract {
    pub id: String,
    pub file: String,
    pub kind: String,
    pub version: String,
    pub status: String,
    pub title: String,
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
                out.insert(k.clone(), rewrite_refs(v));
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
    pub fn schema_for(&self, def: &str) -> Option<Value> {
        let defs: Map<String, Value> = self.defs().into_iter().collect();
        if def == "root" {
            let (_, mut root) = self.root()?;
            root["$schema"] = json!(DRAFT_2020_12);
            root["$defs"] = Value::Object(defs);
            return Some(root);
        }
        if !defs.contains_key(def) {
            return None;
        }
        Some(json!({"$schema": DRAFT_2020_12, "$defs": defs, "$ref": format!("#/$defs/{def}")}))
    }
}

fn rewrite_refs(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| {
                    let v = match (k.as_str(), v.as_str()) {
                        ("$ref", Some(r)) => {
                            Value::String(r.replace("#/components/schemas/", "#/$defs/"))
                        }
                        _ => rewrite_refs(v),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(rewrite_refs).collect()),
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
        out.push(Contract {
            id: id.clone(),
            kind: field("kind")?,
            version: field("version")?,
            status: field("status")?,
            title: field("title")?,
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
    // Fixtures: fixtures/<cN>/{valid,invalid}/<Def>.<case>.json
    let dir = root.join("contracts/fixtures").join(c.module());
    for (sub, want_valid) in [("valid", true), ("invalid", false)] {
        let files = util::files_with_ext(&dir.join(sub), "json");
        if files.is_empty() {
            errors.push(format!(
                "{id}: no {sub} fixtures in {}",
                util::rel(root, &dir.join(sub))
            ));
        }
        for f in files {
            let rel = util::rel(root, &f);
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let def = stem.split('.').next().unwrap_or("");
            let Some(schema) = c.schema_for(def) else {
                errors.push(format!("{rel}: `{def}` is not a definition of {id}"));
                continue;
            };
            let instance = match util::read_json(&f) {
                Ok(v) => v,
                Err(e) => {
                    errors.push(e);
                    continue;
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
        if old.doc == new.doc && old.version == new.version {
            continue;
        }
        let (Some(ov), Some(nv)) = (util::semver(&old.version), util::semver(&new.version)) else {
            continue;
        };
        if old.doc != new.doc && nv <= ov {
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
            continue;
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
        if !affects.contains(&format!("contract:{id}")) {
            errors.push(format!(
                "{id}: breaking change needs a new ADR with `Affects: contract:{id}`: {}",
                breaks.join("; ")
            ));
        }
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
    fn additive_change_is_compatible() {
        let old = json!({"type": "object", "properties": {"a": {"type": "string", "enum": ["x"]}}});
        let new = json!({"type": "object", "properties": {"a": {"type": "string", "enum": ["x", "y"]}, "c": {"type": "boolean"}}});
        let mut out = Vec::new();
        breaking("T", &old, &new, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }
}
