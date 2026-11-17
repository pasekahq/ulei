//! Tools the model may call: the [`Tool`] interface, the built-in tools and the [`Registry`]
//! that dispatches a model tool call to them.

mod files;
mod shell;

use std::fmt;
use std::path::Path;

use serde_json::{Map, Value};
use ulei_contracts::generated::c4g;

pub use files::MAX_READ;
pub use shell::{DEFAULT_TIMEOUT_MS, Interrupt, MAX_OUTPUT, MAX_TIMEOUT_MS};

/// Name of the built-in shell tool.
pub const SHELL: &str = "shell";
/// Name of the built-in file reading tool.
pub const READ: &str = "read";
/// Name of the built-in file writing tool.
pub const WRITE: &str = "write";
/// Name of the built-in exact-replacement editing tool.
pub const EDIT: &str = "edit";

/// Result of one tool call; it goes back to the model as a `tool_result` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Text the model sees.
    pub content: String,
    /// The call failed: invalid input, a failed operation, a timeout, an interrupt or a denial.
    pub is_error: bool,
}

impl Output {
    /// A successful result.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed result.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

impl From<Result<String, String>> for Output {
    fn from(result: Result<String, String>) -> Self {
        match result {
            Ok(content) => Self::ok(content),
            Err(content) => Self::error(content),
        }
    }
}

/// What a tool call runs against.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// Directory relative paths and shell commands resolve against.
    pub cwd: &'a Path,
    /// Handle another thread may use to stop the call.
    pub interrupt: &'a Interrupt,
}

/// A tool the model may call.
pub trait Tool {
    /// How the tool is advertised to the model; its `name` is the dispatch key.
    fn definition(&self) -> c4g::Tool;
    /// Runs one call; every failure, invalid input included, is an `is_error` output.
    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output;
}

/// A second tool was registered under a name already taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateTool(pub String);

impl fmt::Display for DuplicateTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tool `{}` is already registered", self.0)
    }
}

impl std::error::Error for DuplicateTool {}

/// Tools by name, in registration order.
#[derive(Default)]
pub struct Registry {
    entries: Vec<(c4g::Tool, Box<dyn Tool>)>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The built-in tools `shell`, `read`, `write` and `edit`.
    pub fn builtin() -> Self {
        let tools: [Box<dyn Tool>; 4] = [
            Box::new(shell::Shell),
            Box::new(files::Read),
            Box::new(files::Write),
            Box::new(files::Edit),
        ];
        Self {
            entries: tools.into_iter().map(|t| (t.definition(), t)).collect(),
        }
    }

    /// Adds a tool under the name of its definition.
    ///
    /// # Errors
    /// [`DuplicateTool`] when a tool of that name is already registered.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), DuplicateTool> {
        let definition = tool.definition();
        if self.entries.iter().any(|(d, _)| d.name == definition.name) {
            return Err(DuplicateTool(definition.name));
        }
        self.entries.push((definition, tool));
        Ok(())
    }

    /// Definitions of every registered tool, for a turn request.
    pub fn definitions(&self) -> Vec<c4g::Tool> {
        self.entries.iter().map(|(d, _)| d.clone()).collect()
    }

    /// Runs the tool `name`; an unknown name or an interrupted handle is an `is_error` output.
    pub fn execute(&self, name: &str, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let Some((_, tool)) = self.entries.iter().find(|(d, _)| d.name == name) else {
            return Output::error(format!("unknown tool `{name}`"));
        };
        if ctx.interrupt.is_interrupted() {
            return Output::error("interrupted before the tool ran");
        }
        tool.run(input, ctx)
    }
}

/// JSON Schema of an object with the given properties, no others allowed.
fn object_schema(properties: Value, required: &[&str]) -> Map<String, Value> {
    let mut schema = Map::new();
    schema.insert("type".into(), "object".into());
    schema.insert("properties".into(), properties);
    schema.insert(
        "required".into(),
        required.iter().map(|r| Value::from(*r)).collect(),
    );
    schema.insert("additionalProperties".into(), false.into());
    schema
}

/// Typed access to a tool input that has no fields besides the known ones.
struct Args<'a>(&'a Map<String, Value>);

impl<'a> Args<'a> {
    fn new(input: &'a Map<String, Value>, known: &[&str]) -> Result<Self, String> {
        match input.keys().find(|k| !known.contains(&k.as_str())) {
            Some(unknown) => Err(format!("unknown field `{unknown}`")),
            None => Ok(Self(input)),
        }
    }

    fn present(&self, name: &str) -> Option<&'a Value> {
        self.0.get(name).filter(|v| !v.is_null())
    }

    fn str(&self, name: &str) -> Result<&'a str, String> {
        self.present(name)
            .ok_or_else(|| format!("missing field `{name}`"))?
            .as_str()
            .ok_or_else(|| format!("field `{name}` must be a string"))
    }

    fn opt_u64(&self, name: &str) -> Result<Option<u64>, String> {
        self.present(name)
            .map(|v| {
                v.as_u64()
                    .ok_or_else(|| format!("field `{name}` must be a non-negative integer"))
            })
            .transpose()
    }

    fn opt_bool(&self, name: &str) -> Result<Option<bool>, String> {
        self.present(name)
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| format!("field `{name}` must be a boolean"))
            })
            .transpose()
    }
}
