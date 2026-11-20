// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The work of the `ulei-web` helper: one `web_search` or `web_extract` [`Request`] on Exa,
//! answered with the reference result text. The binary supplies the HTTPS transport.

mod page;
mod url;

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::tools::redact::{has_prefixed_token, unquote};
use crate::tools::web::{MAX_EXTRACT_CHAR_LIMIT, MIN_EXTRACT_CHAR_LIMIT, Op, Request};
use crate::tools::{json_ascii, py_str, repr, tool_error, truthy, type_name};

/// The reference's error when no Exa key is set.
const MISSING_KEY: &str =
    "EXA_API_KEY environment variable not set. Get your API key at https://exa.ai";

const SECRET_IN_URL: &str = "Blocked: URL contains what appears to be an API key or token. \
Secrets must not be sent in URLs.";
const PRIVATE_TARGET: &str = "Blocked: URL targets a private or internal network address";
const NO_RESULT: &str = "Extract backend returned no result for this URL";
const INACCESSIBLE: &str = "Content was inaccessible or not found";

/// One POST to the Exa API.
#[derive(Debug, Clone, PartialEq)]
pub struct Call<'a> {
    /// The base URL followed by `/search` or `/contents`.
    pub url: String,
    /// Value of the `x-api-key` header.
    pub key: &'a str,
    /// The JSON body.
    pub body: Value,
    /// Overall limit of the call.
    pub timeout: Duration,
}

/// Why an Exa call failed, in the two classes the reference's handlers tell apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExaError {
    /// An answer of status 400 or above, or a body that is no JSON: the text stands alone.
    Value(String),
    /// Any other failure: connect, TLS, timeout, redirect, size limit; the text follows
    /// `Exa search failed: ` or `Exa extract failed: `.
    Other(String),
}

/// The helper's answer: the tool result text, and whether the call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The result the model sees.
    pub text: String,
    /// The call failed.
    pub failed: bool,
}

/// The base URL a call may use: `https`, or under the test switch `http` to a loopback address;
/// the error is the reason it is refused.
fn check_base_url(base_url: &str) -> Result<(), String> {
    let parsed = url::urlsplit(base_url).map_err(|e| format!("invalid Exa base URL: {e}"))?;
    if parsed.scheme == "https"
        || (cfg!(feature = "web-loopback-http") && is_loopback_http(&parsed))
    {
        return Ok(());
    }
    Err("the Exa base URL must be an https:// URL".into())
}

/// Whether `base_url` is an `http://` URL whose host is a loopback address literal, which the
/// `web-loopback-http` test switch lets the engine and the helper use.
#[cfg(feature = "web-loopback-http")]
pub(crate) fn is_loopback_http_url(base_url: &str) -> bool {
    url::urlsplit(base_url).is_ok_and(|parsed| is_loopback_http(&parsed))
}

fn is_loopback_http(parsed: &url::Split) -> bool {
    parsed.scheme == "http"
        && parsed
            .hostname()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback())
}

/// Runs the request in `input` against Exa through `post`.
pub fn serve(input: &[u8], post: &mut dyn FnMut(&Call<'_>) -> Result<Value, ExaError>) -> Reply {
    let request = match Request::from_json(input) {
        Ok(request) => request,
        Err(e) => {
            return Reply {
                text: tool_error(format!("invalid web helper request: {e}")),
                failed: true,
            };
        }
    };
    match &request.op {
        Op::Search { query, limit } => search(&request, query, *limit, post),
        Op::Extract { urls, char_limit } => extract(&request, urls, *char_limit, post),
    }
}

/// `EXA_API_KEY` from the env-file; its last assignment wins.
fn load_key(env_file: &Path) -> Result<String, String> {
    let entries = ulei_contracts::env_file::read(env_file)
        .map_err(|e| format!("cannot read the env-file {}: {e}", env_file.display()))?;
    let key = entries
        .into_iter()
        .rev()
        .find(|(name, _)| name == "EXA_API_KEY")
        .map(|(_, value)| value);
    match key.as_deref().map(crate::tools::py_strip) {
        Some(key) if !key.is_empty() => Ok(key.to_owned()),
        _ => Err(MISSING_KEY.into()),
    }
}

/// One POST of `body` to `<base_url><path>` with the key; the key is read first.
fn exa(
    request: &Request,
    path: &str,
    body: Value,
    post: &mut dyn FnMut(&Call<'_>) -> Result<Value, ExaError>,
) -> Result<Value, ExaError> {
    let key = load_key(&request.env_file).map_err(ExaError::Value)?;
    check_base_url(&request.base_url).map_err(ExaError::Other)?;
    post(&Call {
        url: format!("{}{path}", request.base_url),
        key: &key,
        body,
        timeout: request.timeout,
    })
}

fn search(
    request: &Request,
    query: &Value,
    limit: i64,
    post: &mut dyn FnMut(&Call<'_>) -> Result<Value, ExaError>,
) -> Reply {
    let error = match search_results(request, query, limit, post) {
        Ok(web) => {
            let web = web
                .into_iter()
                .map(|fields| {
                    Ordered::Obj(
                        fields
                            .into_iter()
                            .map(|(k, v)| (k, Ordered::Value(v)))
                            .collect(),
                    )
                })
                .collect();
            return Reply {
                text: pretty(&Ordered::Obj(vec![
                    ("success", Ordered::Value(Value::Bool(true))),
                    ("data", Ordered::Obj(vec![("web", Ordered::Arr(web))])),
                ])),
                failed: false,
            };
        }
        Err(ExaError::Value(e)) => e,
        Err(ExaError::Other(e)) => format!("Exa search failed: {e}"),
    };
    Reply {
        text: pretty(&Ordered::Obj(vec![
            ("success", Ordered::Value(Value::Bool(false))),
            ("error", Ordered::Value(error.into())),
        ])),
        failed: true,
    }
}

/// The `web` entries of a successful search, as the provider shapes Exa's results.
fn search_results(
    request: &Request,
    query: &Value,
    limit: i64,
    post: &mut dyn FnMut(&Call<'_>) -> Result<Value, ExaError>,
) -> Result<Vec<Vec<(&'static str, Value)>>, ExaError> {
    let mut body = serde_json::Map::new();
    match query {
        Value::Null => {}
        Value::String(_) => {
            body.insert("query".into(), query.clone());
        }
        other => {
            return Err(ExaError::Value(format!(
                "Invalid value for option 'query': {}. Expected one of [<class 'str'>]",
                py_str(other)
            )));
        }
    }
    body.insert("contents".into(), json!({"highlights": true}));
    body.insert("numResults".into(), limit.into());
    let data = exa(request, "/search", Value::Object(body), post)?;
    let mut web = Vec::new();
    for (i, result) in results(data)?.into_iter().enumerate() {
        let mut result = item(result)?;
        let highlights = match result.get("highlights") {
            Some(h) if truthy(h) => join_highlights(h)?,
            _ => String::new(),
        };
        web.push(vec![
            ("url", or_empty(result.remove("url"))),
            ("title", or_empty(result.remove("title"))),
            ("description", highlights.into()),
            ("position", (i + 1).into()),
        ]);
    }
    Ok(web)
}

/// `data["results"]` as the SDK iterates it.
fn results(mut data: Value) -> Result<Vec<Value>, ExaError> {
    match data.get_mut("results").map(Value::take) {
        Some(Value::Array(results)) => Ok(results),
        Some(other) => Err(ExaError::Other(format!(
            "{} object is not iterable",
            repr(type_name(Some(&other)))
        ))),
        None => Err(ExaError::Other("'results'".into())),
    }
}

/// One result object as the SDK reads its fields.
fn item(result: Value) -> Result<serde_json::Map<String, Value>, ExaError> {
    match result {
        Value::Object(fields) => Ok(fields),
        other => Err(ExaError::Other(format!(
            "{} object has no attribute 'get'",
            repr(type_name(Some(&other)))
        ))),
    }
}

/// Python's `" ".join(highlights)`.
fn join_highlights(highlights: &Value) -> Result<String, ExaError> {
    match highlights {
        Value::String(s) => Ok(s.chars().map(String::from).collect::<Vec<_>>().join(" ")),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    ExaError::Other(format!(
                        "sequence item {i}: expected str instance, {} found",
                        type_name(Some(item))
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| parts.join(" ")),
        Value::Object(o) => Ok(o.keys().cloned().collect::<Vec<_>>().join(" ")),
        _ => Err(ExaError::Other("can only join an iterable".into())),
    }
}

/// One `web_extract` result entry.
struct Page {
    url: Value,
    title: Value,
    content: Value,
    error: Option<String>,
}

impl Page {
    fn failed(url: impl Into<Value>, error: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: "".into(),
            content: "".into(),
            error: Some(error.into()),
        }
    }
}

/// The reference's `_web_extract_url`: a URL string, or the `url` or `href` of an object.
fn extract_url(item: &Value) -> Option<String> {
    let value = match item {
        Value::Object(o) => match o.get("url") {
            Some(url) if truthy(url) => Some(url),
            _ => o.get("href"),
        },
        other => Some(other),
    };
    let url = crate::tools::py_strip(value?.as_str()?);
    (!url.is_empty()).then(|| url.to_owned())
}

fn extract(
    request: &Request,
    items: &[Value],
    char_limit: usize,
    post: &mut dyn FnMut(&Call<'_>) -> Result<Value, ExaError>,
) -> Reply {
    let (mut slots, wanted) = match screen(items) {
        Ok(screened) => screened,
        Err(refusal) => return refusal,
    };
    let mut safe = Vec::new();
    for (index, normalized) in wanted {
        if url::is_safe_url(&normalized, &url::resolve) {
            safe.push((index, normalized));
        } else {
            slots[index] = Some(Page::failed(normalized, PRIVATE_TARGET));
        }
    }
    let safe_urls: Vec<String> = safe.iter().map(|(_, u)| u.clone()).collect();
    let fetched = if safe_urls.is_empty() {
        Ok(Vec::new())
    } else {
        contents(request, &safe_urls, post)
    };
    let mut pages = match fetched {
        Ok(pages) => pages,
        Err(error) => safe_urls
            .iter()
            .map(|u| Page::failed(u.as_str(), error.clone()))
            .collect(),
    };
    if slots.iter().any(Option::is_some) {
        let mut fetched = pages.into_iter();
        for (index, url) in safe {
            slots[index] = Some(
                fetched
                    .next()
                    .unwrap_or_else(|| Page::failed(url, NO_RESULT)),
            );
        }
        pages = slots.into_iter().flatten().collect();
    }
    let char_limit = char_limit.clamp(MIN_EXTRACT_CHAR_LIMIT, MAX_EXTRACT_CHAR_LIMIT);
    for page in &mut pages {
        if page.error.as_deref().is_some_and(|e| !e.is_empty()) || !truthy(&page.content) {
            continue;
        }
        let Some(raw) = page.content.as_str() else {
            return Reply {
                text: tool_error(format!(
                    "Error extracting content: expected string or bytes-like object, got {}",
                    repr(type_name(Some(&page.content)))
                )),
                failed: true,
            };
        };
        let clean = page::convert_base64_images_to_links(raw);
        let (text, _) =
            page::truncate_with_footer(&clean, page.url.as_str(), char_limit, &request.store_dir);
        page.content = text.into();
    }
    if pages.is_empty() {
        return Reply {
            text: tool_error(INACCESSIBLE),
            failed: true,
        };
    }
    let failed = pages
        .iter()
        .all(|p| p.error.as_deref().is_some_and(|e| !e.is_empty()));
    let results = pages
        .into_iter()
        .map(|p| {
            Ordered::Obj(vec![
                ("url", Ordered::Value(p.url)),
                ("title", Ordered::Value(p.title)),
                ("content", Ordered::Value(p.content)),
                (
                    "error",
                    Ordered::Value(p.error.map_or(Value::Null, Value::from)),
                ),
            ])
        })
        .collect();
    let text = pretty(&Ordered::Obj(vec![("results", Ordered::Arr(results))]));
    Reply {
        text: page::convert_base64_images_to_links(&text),
        failed,
    }
}

/// Result slots in call order, and the index and normalized URL of each item still wanted.
type Screened = (Vec<Option<Page>>, Vec<(usize, String)>);

/// The items in call order: a slot filled with the failure of an item that is no URL, an
/// empty slot and the normalized URL of every other; the refusal of the whole call when a URL
/// carries a credential.
fn screen(items: &[Value]) -> Result<Screened, Reply> {
    let mut slots: Vec<Option<Page>> = Vec::with_capacity(items.len());
    let mut wanted = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(url) = extract_url(item) else {
            slots.push(Some(Page::failed(
                "",
                format!(
                    "Invalid URL item at index {index}: expected a URL string or an object with \
                     a string 'url' or 'href' field"
                ),
            )));
            continue;
        };
        let normalized = url::normalize_url_for_request(&url);
        if [&url, &unquote(&url), &normalized, &unquote(&normalized)]
            .into_iter()
            .any(|text| has_prefixed_token(text))
        {
            return Err(blocked(SECRET_IN_URL));
        }
        if let Some(key) = url::sensitive_query_param_name(&normalized) {
            return Err(blocked(&format!(
                "Blocked: URL contains a credential-like query parameter ({key}). Web extract \
                 backends are third-party readers; remove the sensitive query parameter or use a \
                 local browser session when this access is explicitly required."
            )));
        }
        slots.push(None);
        wanted.push((index, normalized));
    }
    Ok((slots, wanted))
}

/// The early refusal of a whole extract call, as `json.dumps` writes it.
fn blocked(error: &str) -> Reply {
    Reply {
        text: format!("{{\"success\": false, \"error\": {}}}", json_ascii(error)),
        failed: true,
    }
}

/// The pages Exa returns for `urls`, or the error text each of them gets.
fn contents(
    request: &Request,
    urls: &[String],
    post: &mut dyn FnMut(&Call<'_>) -> Result<Value, ExaError>,
) -> Result<Vec<Page>, String> {
    let body = json!({"urls": urls, "text": true});
    let data = exa(request, "/contents", body, post).map_err(|e| match e {
        ExaError::Value(e) => e,
        ExaError::Other(e) => format!("Exa extract failed: {e}"),
    })?;
    let fail = |e: ExaError| match e {
        ExaError::Value(e) | ExaError::Other(e) => format!("Exa extract failed: {e}"),
    };
    results(data)
        .map_err(fail)?
        .into_iter()
        .map(|result| {
            let mut result = item(result).map_err(fail)?;
            Ok(Page {
                url: or_empty(result.remove("url")),
                title: or_empty(result.remove("title")),
                content: or_empty(result.remove("text")),
                error: None,
            })
        })
        .collect()
}

/// `value or ""` in Python.
fn or_empty(value: Option<Value>) -> Value {
    value.filter(truthy).unwrap_or_else(|| "".into())
}

/// JSON whose object keys keep the reference's order.
enum Ordered {
    Value(Value),
    Arr(Vec<Ordered>),
    Obj(Vec<(&'static str, Ordered)>),
}

/// `value` as `json.dumps(value, indent=2, ensure_ascii=False)` writes it.
fn pretty(value: &Ordered) -> String {
    let mut out = String::new();
    write_pretty(value, 0, &mut out);
    out
}

fn write_pretty(value: &Ordered, depth: usize, out: &mut String) {
    let pad = |depth: usize| "  ".repeat(depth);
    match value {
        Ordered::Value(Value::Array(items)) => write_pretty(
            &Ordered::Arr(items.iter().cloned().map(Ordered::Value).collect()),
            depth,
            out,
        ),
        Ordered::Value(Value::Object(o)) if !o.is_empty() => {
            out.push_str("{\n");
            for (i, (k, v)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                let _ = write!(out, "{}{}: ", pad(depth + 1), Value::from(k.as_str()));
                write_pretty(&Ordered::Value(v.clone()), depth + 1, out);
            }
            let _ = write!(out, "\n{}}}", pad(depth));
        }
        Ordered::Value(scalar) => out.push_str(&scalar.to_string()),
        Ordered::Arr(items) if items.is_empty() => out.push_str("[]"),
        Ordered::Arr(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad(depth + 1));
                write_pretty(item, depth + 1, out);
            }
            let _ = write!(out, "\n{}]", pad(depth));
        }
        Ordered::Obj(fields) if fields.is_empty() => out.push_str("{}"),
        Ordered::Obj(fields) => {
            out.push_str("{\n");
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                let _ = write!(out, "{}{}: ", pad(depth + 1), Value::from(*k));
                write_pretty(v, depth + 1, out);
            }
            let _ = write!(out, "\n{}}}", pad(depth));
        }
    }
}
