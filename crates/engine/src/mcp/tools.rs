// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The tools of one MCP server: each listed tool and the resource and prompt utilities. A
//! call's answer is shaped as the reference shapes it, stripped of the server's header values,
//! and framed as untrusted data.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::client::{Failure, Session, media_type};
use super::prefixed_tool_name;
use super::recovery::Link;
use crate::tools::redact::{mask_credentials, mask_values};
use crate::tools::{
    Context, Output, Tool, json_array, json_object, json_str, py_json, re, strip_unicode_tags,
    thousands, truthy, untrusted,
};

/// Most characters of one text payload of a server before head and tail are kept.
const MAX_RESULT_CHARS: usize = 2_000_000;
/// Most decoded bytes of one resource or audio payload.
const MAX_RESOURCE_BYTES: usize = 50 * 1024 * 1024;
const MAX_RESOURCE_B64_CHARS: usize = MAX_RESOURCE_BYTES * 4 / 3 + 4;
/// Results shorter than this are not scanned for elision markers.
const ELISION_SCAN_MIN_CHARS: usize = 1_000;
/// How much of a result the elision scan reads.
const ELISION_SCAN_MAX_CHARS: usize = 65_536;
pub(crate) const ELISION_NOTICE: &str = "\n[note: this result contains provider-side elision markers (e.g. \
\"...N more items\" / has_more:true). The data shown is INCOMPLETE \u{2014} page/fetch the \
remainder before treating any enumeration as complete.]";

/// The resource and prompt utilities a server may get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Utility {
    /// `list_resources`.
    ListResources,
    /// `read_resource`.
    ReadResource,
    /// `list_prompts`.
    ListPrompts,
    /// `get_prompt`.
    GetPrompt,
}

impl Utility {
    /// Every utility, in registration order.
    pub(crate) const ALL: [Self; 4] = [
        Self::ListResources,
        Self::ReadResource,
        Self::ListPrompts,
        Self::GetPrompt,
    ];

    /// The tool name part after the server.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::ListResources => "list_resources",
            Self::ReadResource => "read_resource",
            Self::ListPrompts => "list_prompts",
            Self::GetPrompt => "get_prompt",
        }
    }

    /// The server capability the utility needs.
    pub(crate) fn capability(self) -> &'static str {
        match self {
            Self::ListResources | Self::ReadResource => "resources",
            Self::ListPrompts | Self::GetPrompt => "prompts",
        }
    }

    /// How the utility of `server` is advertised.
    pub(crate) fn definition(self, server: &str) -> c4g::Tool {
        let (description, schema) = match self {
            Self::ListResources => (
                format!("List available resources from MCP server '{server}'"),
                json!({"type": "object", "properties": {}}),
            ),
            Self::ReadResource => (
                format!("Read a resource by URI from MCP server '{server}'"),
                json!({
                    "type": "object",
                    "properties": {
                        "uri": {"type": "string", "description": "URI of the resource to read"}
                    },
                    "required": ["uri"]
                }),
            ),
            Self::ListPrompts => (
                format!("List available prompts from MCP server '{server}'"),
                json!({"type": "object", "properties": {}}),
            ),
            Self::GetPrompt => (
                format!("Get a prompt by name from MCP server '{server}'"),
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Name of the prompt to retrieve"},
                        "arguments": {
                            "type": "object",
                            "description": "Optional arguments to pass to the prompt",
                            "properties": {},
                            "additionalProperties": true
                        }
                    },
                    "required": ["name"]
                }),
            ),
        };
        c4g::Tool {
            name: prefixed_tool_name(server, self.key()),
            description,
            input_schema: match schema {
                Value::Object(schema) => schema,
                _ => Map::new(),
            },
            native: None,
        }
    }
}

/// One request of a utility on a session within a deadline.
type Call<'a> = &'a dyn Fn(&Session, Option<Instant>) -> Result<Output, Failure>;

/// A tool the server listed.
pub(crate) struct ServerTool {
    /// The server's link.
    pub(crate) link: Arc<Link>,
    /// The tool's name on the server.
    pub(crate) tool: String,
    /// How it is advertised.
    pub(crate) definition: c4g::Tool,
    /// Deadline of one call; `None` waits without one.
    pub(crate) timeout: Option<Duration>,
}

/// A resource or prompt utility of a server.
pub(crate) struct UtilityTool {
    /// The server's link.
    pub(crate) link: Arc<Link>,
    /// Which utility.
    pub(crate) utility: Utility,
    /// Deadline of one call; `None` waits without one.
    pub(crate) timeout: Option<Duration>,
}

impl Tool for ServerTool {
    fn definition(&self) -> c4g::Tool {
        self.definition.clone()
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let params = json!({"name": self.tool, "arguments": input});
        let server = &self.link.server.name;
        let output = self
            .link
            .call(true, self.timeout, ctx.interrupt, |session, deadline| {
                let params = Some(params.clone());
                let result =
                    session.request("tools/call", params, deadline, Some(ctx.interrupt))?;
                Ok(call_result(&result, server))
            });
        shape(&self.definition.name, &self.link, &output)
    }
}

impl Tool for UtilityTool {
    fn definition(&self) -> c4g::Tool {
        self.utility.definition(&self.link.server.name)
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let (link, interrupt) = (&self.link, ctx.interrupt);
        let server = &link.server.name;
        let call = |call: Call<'_>| link.call(false, self.timeout, interrupt, call);
        let required = |name: &str| {
            input
                .get(name)
                .filter(|v| truthy(v))
                .cloned()
                .ok_or_else(|| Output::error(format!("Missing required parameter '{name}'")))
        };
        let output = match self.utility {
            Utility::ListResources => call(&|session, deadline| {
                let all = session.list("resources/list", "resources", deadline, Some(interrupt))?;
                Ok(Output::ok(list_resources(&all)))
            }),
            Utility::ReadResource => match required("uri") {
                Err(missing) => missing,
                Ok(uri) => call(&|session, deadline| {
                    let params = Some(json!({ "uri": uri }));
                    let result =
                        session.request("resources/read", params, deadline, Some(interrupt))?;
                    Ok(Output::ok(read_resource(&result, server)))
                }),
            },
            Utility::ListPrompts => call(&|session, deadline| {
                let all = session.list("prompts/list", "prompts", deadline, Some(interrupt))?;
                Ok(Output::ok(list_prompts(&all)))
            }),
            Utility::GetPrompt => match required("name") {
                Err(missing) => missing,
                Ok(name) => call(&|session, deadline| {
                    let mut params = json!({ "name": name });
                    let arguments = input.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    if !arguments.is_null() {
                        params["arguments"] = arguments;
                    }
                    let result =
                        session.request("prompts/get", Some(params), deadline, Some(interrupt))?;
                    Ok(Output::ok(get_prompt(&result)))
                }),
            },
        };
        shape(
            &prefixed_tool_name(server, self.utility.key()),
            link,
            &output,
        )
    }
}

/// `output` of the tool `name` without the link's secrets, with the elision notice when the
/// server marked its data incomplete, framed as untrusted.
fn shape(name: &str, link: &Link, output: &Output) -> Output {
    let mut content = mask_values(&output.content, &link.secrets()).into_owned();
    if elided(&content) {
        content.push_str(ELISION_NOTICE);
    }
    untrusted(
        name,
        Output {
            content,
            is_error: output.is_error,
        },
    )
}

/// Whether `content` carries a marker of data the server left out.
fn elided(content: &str) -> bool {
    if content.chars().count() < ELISION_SCAN_MIN_CHARS {
        return false;
    }
    let window = &content[..crate::tools::char_byte(content, ELISION_SCAN_MAX_CHARS)];
    [
        r"(?i)\.\.\.\s*\d+\s+more\s+items?",
        r#"(?i)"has_more"\s*:\s*true"#,
        r"(?i)saved to sandbox",
        r"(?i)data_preview",
    ]
    .iter()
    .any(|pattern| re(pattern).is_match(window))
}

/// `text` cut to [`MAX_RESULT_CHARS`]: 40 % head and 60 % tail around an omission notice.
fn truncate(text: String) -> String {
    let total = text.chars().count();
    if total <= MAX_RESULT_CHARS {
        return text;
    }
    let head = MAX_RESULT_CHARS * 2 / 5;
    let tail = MAX_RESULT_CHARS - head;
    let omitted = total - head - tail;
    let head_end = crate::tools::char_byte(&text, head);
    let tail_start = crate::tools::char_byte(&text, total - tail);
    format!(
        "{}\n\n... [MCP RESULT TRUNCATED - {} chars omitted out of {} total] ...\n\n{}",
        &text[..head_end],
        thousands(omitted),
        thousands(total),
        &text[tail_start..]
    )
}

/// The result of a `tools/call` answer: its error text as a tool error, else its content as
/// `{"result": text}` with any structured content and metadata.
fn call_result(result: &Value, server: &str) -> Output {
    let blocks = result.get("content").and_then(Value::as_array);
    let blocks = blocks.map_or(&[][..], Vec::as_slice);
    if result.get("isError").is_some_and(truthy) {
        let mut text = String::new();
        for block in blocks {
            if let Some(t) = block
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                text.push_str(t);
            } else if let Some(t) = block.pointer("/resource/text").filter(|t| truthy(t)) {
                text.push_str(&crate::tools::py_str(t));
            }
        }
        if text.is_empty() {
            text = "MCP tool returned an error".into();
        }
        return Output::error(mask_credentials(&truncate(text)));
    }
    let parts: Vec<String> = blocks
        .iter()
        .filter_map(|block| render_block(block, server))
        .collect();
    let text = truncate(parts.join("\n"));
    let structured = result
        .get("structuredContent")
        .filter(|v| !v.is_null())
        .map(|value| {
            let dumped = py_json(value);
            if dumped.chars().count() > MAX_RESULT_CHARS {
                json_str(&truncate(dumped))
            } else {
                dumped
            }
        });
    let meta = result
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| {
            let mut kept = Map::new();
            for (key, value) in meta {
                if !reserved_meta_key(key) {
                    kept.insert(key.clone(), value.clone());
                }
            }
            (!kept.is_empty()).then(|| py_json(&Value::Object(kept)))
        });
    if structured.is_none() && meta.is_none() {
        return Output::ok(json_object([("result", json_str(&text))]));
    }
    let mut fields = Vec::new();
    if !text.is_empty() {
        fields.push(("result", json_str(&text)));
    }
    if let Some(structured) = structured {
        let key = if text.is_empty() {
            "result"
        } else {
            "structuredContent"
        };
        fields.push((key, structured));
    }
    if let Some(meta) = meta {
        fields.push(("_meta", meta));
    }
    if !fields.iter().any(|(key, _)| *key == "result") {
        fields.insert(0, ("result", json_str(&text)));
    }
    Output::ok(json_object(fields))
}

/// Whether a `_meta` key uses a protocol-reserved prefix: an `mcp` or `modelcontextprotocol`
/// label followed by another label before the `/`.
fn reserved_meta_key(key: &str) -> bool {
    let Some(slash) = key.find('/').filter(|s| *s > 0) else {
        return false;
    };
    let labels: Vec<&str> = key[..slash].split('.').collect();
    labels
        .iter()
        .enumerate()
        .any(|(i, l)| (*l == "modelcontextprotocol" || *l == "mcp") && i + 1 < labels.len())
}

/// The text of one content block of a call result, `None` when it renders empty. Images and
/// audio have no media cache to go to.
fn render_block(block: &Value, server: &str) -> Option<String> {
    if let Some(text) = block
        .get("text")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        return Some(strip_unicode_tags(text).into_owned());
    }
    if let Some(data) = block.get("data").and_then(Value::as_str)
        && media_type(block.get("mimeType").and_then(Value::as_str)).starts_with("audio/")
    {
        if data.len() > MAX_RESOURCE_B64_CHARS {
            return Some(format!(
                "[MCP audio resource too large to cache: ~{} bytes]",
                data.len() * 3 / 4
            ));
        }
        return match b64_len(data) {
            Some(n) if n > MAX_RESOURCE_BYTES => Some(format!(
                "[MCP audio resource too large to cache: {n} bytes]"
            )),
            _ => None,
        };
    }
    Some(render_resource(block, server)).filter(|text| !text.is_empty())
}

/// A resource link as a pointer at the server's `read_resource` tool, an embedded text
/// resource as its text, an embedded blob as the note the reference gives without a document
/// cache; empty for other blocks.
fn render_resource(block: &Value, server: &str) -> String {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has = |key: &str| block.get(key).is_some_and(|v| !v.is_null());
    if kind == "resource_link" || (has("uri") && !has("resource") && kind != "text") {
        let Some(uri) = block.get("uri").filter(|u| truthy(u)) else {
            return String::new();
        };
        let mut details = format!("uri={}", crate::tools::py_str(uri));
        let field = |key| {
            block
                .get(key)
                .filter(|v| truthy(v))
                .map(crate::tools::py_str)
        };
        if let Some(name) = field("name") {
            let _ = write!(details, ", name={name}");
        }
        if let Some(mime) = field("mimeType") {
            let _ = write!(details, ", mimeType={mime}");
        }
        return format!(
            "[MCP resource link: {details} \u{2014} fetch it with {}]",
            prefixed_tool_name(server, "read_resource")
        );
    }
    let Some(resource) = block.get("resource").filter(|r| !r.is_null()) else {
        return String::new();
    };
    if let Some(text) = resource.get("text").filter(|t| !t.is_null()) {
        return strip_unicode_tags(&crate::tools::py_str(text)).into_owned();
    }
    let Some(blob) = resource.get("blob").and_then(Value::as_str) else {
        return String::new();
    };
    let field = |key| {
        resource
            .get(key)
            .filter(|v| truthy(v))
            .map(crate::tools::py_str)
            .unwrap_or_default()
    };
    let (uri, mime) = (field("uri"), field("mimeType"));
    if blob.len() > MAX_RESOURCE_B64_CHARS {
        return format!(
            "[MCP embedded resource too large to cache: ~{} bytes, uri={uri}]",
            blob.len() * 3 / 4
        );
    }
    let what = if mime.is_empty() { &uri } else { &mime };
    match b64_len(blob) {
        None => format!("[MCP embedded resource could not be decoded: {what}]"),
        Some(n) if n > MAX_RESOURCE_BYTES => {
            format!("[MCP embedded resource too large to cache: {n} bytes, uri={uri}]")
        }
        Some(n) => format!(
            "[MCP embedded resource received ({n} bytes, {}) but document cache unavailable in \
             this process]",
            if mime.is_empty() {
                "unknown type"
            } else {
                &mime
            }
        ),
    }
}

/// The decoded length of base64 `data` as a lenient decoder reads it: characters outside the
/// alphabet skipped, data ending at the first `=`; `None` when the padding is wrong.
fn b64_len(data: &str) -> Option<usize> {
    let alphabet = |b: &u8| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/';
    let body = data.split('=').next().unwrap_or_default();
    let n = body.bytes().filter(alphabet).count();
    let pads = data.bytes().skip(body.len()).filter(|b| *b == b'=').count();
    match n % 4 {
        0 => Some(n / 4 * 3),
        1 => None,
        r if pads >= 4 - r => Some(n / 4 * 3 + r - 1),
        _ => None,
    }
}

/// `{"resources": [...]}`: the URI, name, description and MIME type of each resource.
fn list_resources(all: &[Value]) -> String {
    let entries = all.iter().map(|r| {
        let mut fields = Vec::new();
        if let Some(uri) = r.get("uri") {
            fields.push(("uri", json_str(&crate::tools::py_str(uri))));
        }
        if let Some(name) = r.get("name") {
            fields.push(("name", py_json(name)));
        }
        if let Some(d) = r.get("description").filter(|d| truthy(d)) {
            fields.push(("description", py_json(d)));
        }
        if let Some(m) = r.get("mimeType").filter(|m| truthy(m)) {
            fields.push(("mimeType", py_json(m)));
        }
        json_object(fields)
    });
    json_object([("resources", json_array(entries))])
}

/// `{"result": text}` of the contents of a `resources/read` answer.
fn read_resource(result: &Value, server: &str) -> String {
    let contents = result.get("contents").and_then(Value::as_array);
    let mut parts = Vec::new();
    for block in contents.map_or(&[][..], Vec::as_slice) {
        if let Some(text) = block.get("text").filter(|t| !t.is_null()) {
            parts.push(strip_unicode_tags(&crate::tools::py_str(text)).into_owned());
        } else if let Some(blob) = block.get("blob").filter(|b| !b.is_null()) {
            let wrapped = json!({"type": "resource", "resource": block});
            let rendered = render_resource(&wrapped, server);
            parts.push(if rendered.is_empty() {
                format!("[binary data, {} bytes]", blob.as_str().map_or(0, str::len))
            } else {
                rendered
            });
        }
    }
    json_object([("result", json_str(&parts.join("\n")))])
}

/// `{"prompts": [...]}`: the name, description and arguments of each prompt.
fn list_prompts(all: &[Value]) -> String {
    let entries = all.iter().map(|p| {
        let mut fields = Vec::new();
        if let Some(name) = p.get("name") {
            fields.push(("name", py_json(name)));
        }
        if let Some(d) = p.get("description").filter(|d| truthy(d)) {
            fields.push(("description", py_json(d)));
        }
        if let Some(Value::Array(args)) = p.get("arguments").filter(|a| truthy(a)) {
            let args = args.iter().map(|a| {
                let mut fields = vec![("name", py_json(a.get("name").unwrap_or(&Value::Null)))];
                if let Some(d) = a.get("description").filter(|d| truthy(d)) {
                    fields.push(("description", py_json(d)));
                }
                fields.push((
                    "required",
                    py_json(a.get("required").unwrap_or(&Value::Null)),
                ));
                json_object(fields)
            });
            fields.push(("arguments", json_array(args)));
        }
        json_object(fields)
    });
    json_object([("prompts", json_array(entries))])
}

/// `{"messages": [...]}` of a `prompts/get` answer, with its description.
fn get_prompt(result: &Value) -> String {
    let messages = result.get("messages").and_then(Value::as_array);
    let entries = messages.map_or(&[][..], Vec::as_slice).iter().map(|m| {
        let mut fields = Vec::new();
        if let Some(role) = m.get("role") {
            fields.push(("role", py_json(role)));
        }
        if let Some(content) = m.get("content") {
            let text = match content {
                Value::String(text) => text.clone(),
                _ => match content.get("text").and_then(Value::as_str) {
                    Some(text) => text.to_owned(),
                    None => content.to_string(),
                },
            };
            fields.push(("content", json_str(&strip_unicode_tags(&text))));
        }
        json_object(fields)
    });
    let mut fields = vec![("messages", json_array(entries))];
    if let Some(d) = result.get("description").filter(|d| truthy(d)) {
        fields.push(("description", py_json(d)));
    }
    json_object(fields)
}
