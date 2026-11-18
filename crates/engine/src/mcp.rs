// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The MCP client of the engine (ADR-0015 section 3): every configured loopback HTTP server is
//! connected when a run starts, its tools are listed, filtered and registered as
//! `mcp__<server>__<tool>` beside its resource and prompt utilities, its calls recover a lost
//! session, and its session ends with the run.

mod client;
mod recovery;
mod schema;
mod tools;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::Value;
use ulei_contracts::generated::c4g;

pub use client::Endpoint;
use recovery::Link;
pub use recovery::{Clock, SystemClock};
use tools::Utility;

use crate::config::{self, Config, Fault, ItemSetting};
use crate::tools::glob::fnmatch;
use crate::tools::{Registry, repr, strip_unicode_tags};

/// Deadline of one tool call of a server that sets no `timeout`.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);
/// Deadline of connecting to a server that sets no `connect_timeout`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// The start of every MCP tool name.
pub const TOOL_NAME_PREFIX: &str = "mcp__";

/// The settings of each server.
const FIELDS: [ItemSetting; 7] = [
    config::MCP_URL,
    config::MCP_ENV_FILE,
    config::MCP_TIMEOUT,
    config::MCP_CONNECT_TIMEOUT,
    config::MCP_TOOLS_INCLUDE,
    config::MCP_TOOLS_RESOURCES,
    config::MCP_TOOLS_PROMPTS,
];

/// Description shapes of a listed tool that are reported as suspicious, and why.
const INJECTION_PATTERNS: [(&str, &str); 10] = [
    (
        r"(?i)ignore\s+(all\s+)?previous\s+instructions",
        "prompt override attempt ('ignore previous instructions')",
    ),
    (
        r"(?i)you\s+are\s+now\s+a",
        "identity override attempt ('you are now a...')",
    ),
    (
        r"(?i)your\s+new\s+(task|role|instructions?)\s+(is|are)",
        "task override attempt",
    ),
    (r"(?i)system\s*:\s*", "system prompt injection attempt"),
    (
        r"(?i)<\s*(system|human|assistant)\s*>",
        "role tag injection attempt",
    ),
    (
        r"(?i)do\s+not\s+(tell|inform|mention|reveal)",
        "concealment instruction",
    ),
    (
        r"(?i)(curl|wget|fetch)\s+https?://",
        "network command in description",
    ),
    (
        r"(?i)base64\.(b64decode|decodebytes)",
        "base64 decode reference",
    ),
    (r"(?i)exec\s*\(|eval\s*\(", "code execution reference"),
    (
        r"(?i)import\s+(subprocess|os|shutil|socket)",
        "dangerous import reference",
    ),
];

/// The settings of one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// The configured name, which its tool names carry sanitized.
    pub name: String,
    /// Where it listens.
    pub endpoint: Endpoint,
    /// The env-file whose entries are its request headers.
    pub env_file: Option<PathBuf>,
    /// Deadline of one call; `None` waits without one.
    pub timeout: Option<Duration>,
    /// Deadline of connecting and listing its tools; `None` waits without one.
    pub connect_timeout: Option<Duration>,
    /// Names or globs of the tools to register; empty registers all.
    pub include: Vec<String>,
    /// Whether its resource utilities are registered.
    pub resources: bool,
    /// Whether its prompt utilities are registered.
    pub prompts: bool,
}

impl Server {
    /// The servers `engine.mcp_servers` names, each under the settings of `config`.
    ///
    /// # Errors
    /// [`config::Error`] of a setting, a name listed twice, two names that read the same
    /// environment variable, or a URL that is not on a loopback host.
    pub fn from_settings(
        config: &Config,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Vec<Self>, config::Error> {
        let names = config.names(config::MCP_SERVERS, env(config::MCP_SERVERS.env))?;
        let mut servers: Vec<Self> = Vec::with_capacity(names.len());
        for name in names {
            if servers.iter().any(|s| s.name == name) {
                return Err(config::Error::Item {
                    key: config::MCP_SERVERS.key.into(),
                    env: config::MCP_SERVERS.env.into(),
                    fault: Fault::Invalid("must not name a server twice"),
                });
            }
            for other in &servers {
                let shared = FIELDS.iter().find_map(|field| {
                    let env = field.env(&name);
                    FIELDS
                        .iter()
                        .any(|theirs| theirs.env(&other.name) == env)
                        .then_some(env)
                });
                if let Some(env) = shared {
                    return Err(config::Error::Clash {
                        setting: config::MCP_SERVERS,
                        first: other.name.clone(),
                        second: name,
                        env,
                    });
                }
            }
            let url = config
                .item_text(config::MCP_URL, &name, env)?
                .ok_or_else(|| config::MCP_URL.error(&name, Fault::Missing))?;
            let endpoint = Endpoint::parse(&url)
                .map_err(|rule| config::MCP_URL.error(&name, Fault::Invalid(rule)))?;
            servers.push(Self {
                endpoint,
                env_file: config
                    .item_text(config::MCP_ENV_FILE, &name, env)?
                    .map(PathBuf::from),
                timeout: config.item_timeout(
                    config::MCP_TIMEOUT,
                    &name,
                    env,
                    DEFAULT_TOOL_TIMEOUT,
                )?,
                connect_timeout: config.item_timeout(
                    config::MCP_CONNECT_TIMEOUT,
                    &name,
                    env,
                    DEFAULT_CONNECT_TIMEOUT,
                )?,
                include: config.item_names(config::MCP_TOOLS_INCLUDE, &name, env)?,
                resources: config.item_switch(config::MCP_TOOLS_RESOURCES, &name, env, true),
                prompts: config.item_switch(config::MCP_TOOLS_PROMPTS, &name, env, true),
                name,
            });
        }
        Ok(servers)
    }

    /// Whether the include filter keeps the listed tool `tool`: an exact name, or a glob with
    /// `*`, `?` or `[`.
    fn keeps(&self, tool: &str) -> bool {
        self.include.is_empty()
            || self.include.iter().any(|pattern| {
                pattern == tool || (pattern.contains(['*', '?', '[']) && fnmatch(pattern, tool))
            })
    }
}

/// `value` with every character outside `[A-Za-z0-9_]` as `_`.
pub fn sanitize_name_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `mcp__<server>__<tool>`, both parts sanitized.
pub fn prefixed_tool_name(server: &str, tool: &str) -> String {
    format!(
        "{TOOL_NAME_PREFIX}{}__{}",
        sanitize_name_component(server),
        sanitize_name_component(tool)
    )
}

/// The open sessions of a run.
pub struct Sessions(Vec<Arc<Link>>);

impl Sessions {
    /// Ends every session on its server.
    pub fn close(self) {
        for link in self.0 {
            link.close();
        }
    }
}

/// Connects every server of `servers` at once, each retrying on `clock`, then registers the
/// tools of each that connected into `registry` in their order. The warnings name each server
/// that failed and each tool skipped.
pub fn connect(
    servers: &[Server],
    registry: &mut Registry,
    clock: &Arc<dyn Clock>,
) -> (Sessions, Vec<String>) {
    let opened: Vec<Result<(Link, Vec<Value>), String>> = thread::scope(|scope| {
        let handles: Vec<_> = servers
            .iter()
            .map(|server| scope.spawn(|| Link::start(server, Arc::clone(clock))))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("the connecting thread panicked".into()))
            })
            .collect()
    });
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    let mut owners: HashMap<String, String> = HashMap::new();
    for (server, opened) in servers.iter().zip(opened) {
        match opened {
            Ok((link, listed)) => {
                let link = Arc::new(link);
                register(server, &link, &listed, registry, &mut owners, &mut warnings);
                sessions.push(link);
            }
            Err(message) => warnings.push(format!(
                "Failed to connect to MCP server '{}': {message}",
                server.name
            )),
        }
    }
    (Sessions(sessions), warnings)
}

/// Where a registration candidate comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    Native(String),
    Utility(&'static str),
}

impl Origin {
    fn label(&self) -> String {
        match self {
            Self::Native(name) => format!("tool {}", repr(name)),
            Self::Utility(key) => format!("generated utility {}", repr(key)),
        }
    }
}

/// A tool ready to register, where it comes from and its name.
type Candidate = (String, Origin, Box<dyn crate::tools::Tool>);

/// Registers the tools `listed` by the server of `link` that its filter keeps and the
/// utilities its switches and capabilities allow. Names that sanitize alike are skipped, except
/// that a listed tool wins over a utility; a name another server owns stays its.
fn register(
    server: &Server,
    link: &Arc<Link>,
    listed: &[Value],
    registry: &mut Registry,
    owners: &mut HashMap<String, String>,
    warnings: &mut Vec<String>,
) {
    let mut unique: Vec<Candidate> = Vec::new();
    for candidate in candidates(server, link, listed, warnings) {
        if !unique
            .iter()
            .any(|(name, origin, _)| *name == candidate.0 && *origin == candidate.1)
        {
            unique.push(candidate);
        }
    }
    let skipped = collisions(&server.name, &unique, warnings);
    for (name, origin, tool) in unique {
        if skipped.contains(&(name.clone(), origin.clone())) {
            continue;
        }
        // Built-in names never start with the prefix, so a taken name is another server's.
        if let Err(taken) = registry.register(tool) {
            let owner = owners.get(&taken.0).cloned().unwrap_or_default();
            warnings.push(format!(
                "MCP server '{}': {} normalizes to '{}', already owned by MCP toolset \
                 'mcp-{owner}' \u{2014} skipping to preserve the existing owner",
                server.name,
                origin.label(),
                taken.0
            ));
            continue;
        }
        owners.insert(name, server.name.clone());
    }
}

/// The listed tools the filter of `server` keeps, then the utilities its switches and the
/// capabilities of its session allow.
fn candidates(
    server: &Server,
    link: &Arc<Link>,
    listed: &[Value],
    warnings: &mut Vec<String>,
) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();
    for tool in listed {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !server.keeps(name) {
            continue;
        }
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        scan_description(&server.name, name, description, warnings);
        let description = if description.is_empty() {
            format!("MCP tool {name} from {}", server.name)
        } else {
            strip_unicode_tags(description).into_owned()
        };
        let definition = c4g::Tool {
            name: prefixed_tool_name(&server.name, name),
            description,
            input_schema: schema::normalize(tool.get("inputSchema")),
        };
        candidates.push((
            definition.name.clone(),
            Origin::Native(name.to_owned()),
            Box::new(tools::ServerTool {
                link: Arc::clone(link),
                tool: name.to_owned(),
                definition,
                timeout: server.timeout,
            }),
        ));
    }
    for utility in Utility::ALL {
        let enabled = match utility.capability() {
            "resources" => server.resources,
            _ => server.prompts,
        };
        if enabled && link.advertises(utility.capability()) {
            candidates.push((
                prefixed_tool_name(&server.name, utility.key()),
                Origin::Utility(utility.key()),
                Box::new(tools::UtilityTool {
                    link: Arc::clone(link),
                    utility,
                    timeout: server.timeout,
                }),
            ));
        }
    }
    candidates
}

/// The candidates of `server` to skip: each utility whose name one listed tool takes too, and
/// every candidate of a name several listed tools share.
fn collisions(
    server: &str,
    unique: &[Candidate],
    warnings: &mut Vec<String>,
) -> Vec<(String, Origin)> {
    let mut skipped = Vec::new();
    let mut names: Vec<&str> = unique.iter().map(|(name, _, _)| name.as_str()).collect();
    crate::sort::sort(&mut names);
    names.dedup();
    for name in names {
        let origins: Vec<&Origin> = unique
            .iter()
            .filter(|(n, _, _)| n == name)
            .map(|(_, o, _)| o)
            .collect();
        if origins.len() <= 1 {
            continue;
        }
        let natives = origins
            .iter()
            .filter(|o| matches!(o, Origin::Native(_)))
            .count();
        if natives == 1 {
            skipped.extend(
                origins
                    .iter()
                    .filter(|o| matches!(o, Origin::Utility(_)))
                    .map(|o| (name.to_owned(), (*o).clone())),
            );
            continue;
        }
        let mut labels: Vec<String> = origins.iter().map(|o| o.label()).collect();
        crate::sort::sort(&mut labels);
        warnings.push(format!(
            "MCP server '{server}': name normalization collision for '{name}' from {}; \
             skipping every colliding entry instead of choosing an arbitrary handler",
            labels.join(", ")
        ));
        skipped.extend(origins.into_iter().map(|o| (name.to_owned(), o.clone())));
    }
    skipped
}

/// Reports a description of a listed tool that matches a prompt-injection shape.
fn scan_description(server: &str, tool: &str, description: &str, warnings: &mut Vec<String>) {
    if description.is_empty() {
        return;
    }
    let findings: Vec<&str> = INJECTION_PATTERNS
        .iter()
        .filter(|(pattern, _)| crate::tools::re(pattern).is_match(description))
        .map(|(_, reason)| *reason)
        .collect();
    if !findings.is_empty() {
        let shown = &description[..crate::tools::char_byte(description, 200)];
        warnings.push(format!(
            "MCP server '{server}' tool '{tool}': suspicious description content \u{2014} {}. \
             Description: {shown}",
            findings.join("; ")
        ));
    }
}
