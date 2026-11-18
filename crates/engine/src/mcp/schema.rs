// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The input schema of an MCP tool as the model receives it: legacy `definitions` promoted to
//! `$defs`, nullable unions and same-typed const unions collapsed, object shapes repaired.

use serde_json::{Map, Value, json};

const META_KEYS: [&str; 4] = ["title", "description", "default", "examples"];

/// `schema` normalized for tool calling; a missing or empty one is an object without
/// properties.
pub(crate) fn normalize(schema: Option<&Value>) -> Map<String, Value> {
    let empty = || {
        let mut object = Map::new();
        object.insert("type".into(), "object".into());
        object.insert("properties".into(), json!({}));
        object
    };
    let Some(schema) = schema.filter(|s| crate::tools::truthy(s)) else {
        return empty();
    };
    let normalized = repair_object_shape(collapse_const_unions(strip_nullable_unions(
        rewrite_local_refs(schema),
    )));
    match normalized {
        Value::Object(mut object) => {
            if object.get("type").and_then(Value::as_str) == Some("object")
                && !object.contains_key("properties")
            {
                object.insert("properties".into(), json!({}));
            }
            object
        }
        _ => empty(),
    }
}

/// `definitions` as a schema keyword renamed to `$defs`, and `#/definitions/` refs pointed
/// there; property names stay as they are.
fn rewrite_local_refs(node: &Value) -> Value {
    match node {
        Value::Object(object) => {
            let mut out = Map::new();
            for (key, value) in object {
                if let ("properties" | "patternProperties", Value::Object(props)) =
                    (key.as_str(), value)
                {
                    let props = props
                        .iter()
                        .map(|(name, schema)| (name.clone(), rewrite_local_refs(schema)))
                        .collect();
                    out.insert(key.clone(), Value::Object(props));
                } else {
                    let key = if key == "definitions" { "$defs" } else { key };
                    out.insert(key.to_owned(), rewrite_local_refs(value));
                }
            }
            if let Some(Value::String(r)) = out.get("$ref")
                && let Some(rest) = r.strip_prefix("#/definitions/")
            {
                let rewritten = format!("#/$defs/{rest}");
                out.insert("$ref".into(), rewritten.into());
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(rewrite_local_refs).collect()),
        scalar => scalar.clone(),
    }
}

/// `anyOf` or `oneOf` of one non-null branch and a `null` branch replaced by that branch,
/// marked `nullable`, with the outer metadata it lacks.
fn strip_nullable_unions(node: Value) -> Value {
    let object = match node {
        Value::Array(items) => {
            return Value::Array(items.into_iter().map(strip_nullable_unions).collect());
        }
        Value::Object(object) => object,
        scalar => return scalar,
    };
    let stripped: Map<String, Value> = object
        .into_iter()
        .map(|(k, v)| (k, strip_nullable_unions(v)))
        .collect();
    for key in ["anyOf", "oneOf"] {
        let Some(Value::Array(variants)) = stripped.get(key) else {
            continue;
        };
        let non_null: Vec<&Value> = variants.iter().filter(|v| !is_null_type(v)).collect();
        if non_null.len() == 1 && non_null.len() != variants.len() {
            let mut replacement = match non_null[0] {
                Value::Object(branch) => branch.clone(),
                _ => Map::new(),
            };
            replacement.entry("nullable").or_insert(Value::Bool(true));
            for meta in META_KEYS {
                if let Some(value) = stripped.get(meta)
                    && !replacement.contains_key(meta)
                    && !(meta == "default" && replacement.contains_key("$ref"))
                {
                    replacement.insert(meta.into(), value.clone());
                }
            }
            return strip_nullable_unions(Value::Object(replacement));
        }
    }
    Value::Object(stripped)
}

fn is_null_type(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("null")
}

/// The JSON type of a branch that is only a `const` of a primitive value, with an agreeing
/// `type` and a `title` or `description` at most.
fn const_branch_type(branch: &Value) -> Option<&'static str> {
    let branch = branch.as_object()?;
    let value = branch.get("const")?;
    if branch
        .keys()
        .any(|k| !["const", "type", "title", "description"].contains(&k.as_str()))
    {
        return None;
    }
    let json_type = match value {
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        _ => return None,
    };
    match branch.get("type") {
        Some(declared) if declared != json_type => None,
        _ => Some(json_type),
    }
}

/// `anyOf` or `oneOf` whose branches are consts of one primitive type, plus at most one
/// `null` branch, replaced by an `enum` of that type.
fn collapse_const_unions(node: Value) -> Value {
    let object = match node {
        Value::Array(items) => {
            return Value::Array(items.into_iter().map(collapse_const_unions).collect());
        }
        Value::Object(object) => object,
        scalar => return scalar,
    };
    let out: Map<String, Value> = object
        .into_iter()
        .map(|(k, v)| (k, collapse_const_unions(v)))
        .collect();
    for key in ["anyOf", "oneOf"] {
        let Some(Value::Array(variants)) = out.get(key) else {
            continue;
        };
        let is_null = |v: &Value| is_null_type(v) && v.get("const").is_none();
        let nulls = variants.iter().filter(|v| is_null(v)).count();
        let consts: Vec<&Value> = variants.iter().filter(|v| !is_null(v)).collect();
        if nulls > 1 || consts.is_empty() {
            continue;
        }
        let types: Vec<Option<&str>> = consts.iter().map(|b| const_branch_type(b)).collect();
        let Some(Some(json_type)) = types.first().copied() else {
            continue;
        };
        if types.iter().any(|t| *t != Some(json_type)) {
            continue;
        }
        let mut replacement = Map::new();
        replacement.insert("type".into(), json_type.into());
        replacement.insert(
            "enum".into(),
            consts
                .iter()
                .filter_map(|b| b.get("const").cloned())
                .collect(),
        );
        if nulls > 0 {
            replacement.insert("nullable".into(), Value::Bool(true));
        }
        for meta in META_KEYS {
            if let Some(value) = out.get(meta)
                && !replacement.contains_key(meta)
            {
                replacement.insert(meta.into(), value.clone());
            }
        }
        return Value::Object(replacement);
    }
    Value::Object(out)
}

/// Objects given their `type` and `properties`, and `required` pruned to their properties.
fn repair_object_shape(node: Value) -> Value {
    let object = match node {
        Value::Array(items) => {
            return Value::Array(items.into_iter().map(repair_object_shape).collect());
        }
        Value::Object(object) => object,
        scalar => return scalar,
    };
    let mut repaired: Map<String, Value> = object
        .into_iter()
        .map(|(k, v)| (k, repair_object_shape(v)))
        .collect();
    if !repaired.get("type").is_some_and(crate::tools::truthy)
        && (repaired.contains_key("properties") || repaired.contains_key("required"))
    {
        repaired.insert("type".into(), "object".into());
    }
    if repaired.get("type").and_then(Value::as_str) == Some("object") {
        if !repaired.get("properties").is_some_and(Value::is_object) {
            repaired.insert("properties".into(), json!({}));
        }
        if let Some(Value::Array(required)) = repaired.get("required") {
            let props = repaired.get("properties").and_then(Value::as_object);
            let valid: Vec<Value> = required
                .iter()
                .filter(|r| {
                    r.as_str()
                        .is_some_and(|r| props.is_some_and(|p| p.contains_key(r)))
                })
                .cloned()
                .collect();
            if valid.len() != required.len() {
                if valid.is_empty() {
                    repaired.remove("required");
                } else {
                    repaired.insert("required".into(), Value::Array(valid));
                }
            }
        }
    }
    Value::Object(repaired)
}
