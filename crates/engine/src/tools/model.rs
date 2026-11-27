//! The `set_model` tool: the model's own `/model <alias>`. It stores a qb route alias with the
//! current session of the turn's key, as C6 `set_route` does for the channel's `/model`, so the
//! session's later turns run on it until a new session of the key; with `global` it also stores
//! the alias as the engine's default route, as a global `set_route` does for `/model --global`,
//! so new sessions and the engine after a restart run on it too (ADR-0094). Only in a turn whose
//! prompt is the owner's, and only an alias the gateway lists.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::{Args, Context, Output, SET_MODEL, json_array, json_object, json_str, object_schema};
use crate::gateway;
use crate::journal::Journal;
use crate::policy::Origin;

/// How long a journal write waits for a lock held by another turn.
const JOURNAL_WAIT: Duration = Duration::from_secs(5);

const DESCRIPTION: &str = "Switch the model the owner's chat runs on, as the owner's `/model \
    <alias> [--global]` command does. Use only when the owner asks for another model in words \
    (e.g. \"switch to opus\"). Without `model` it lists the route aliases the gateway serves, \
    the one in force and the default. With `model` it stores that alias for this session: the \
    current turn finishes on the model it started on, and every later turn of the session runs \
    on the new one until the owner starts a new session with /new, which returns to the \
    default. With `global: true` it also makes the alias the default: new sessions after /new \
    and the engine after a restart run on it, until another global switch. Set `global` only \
    when the owner asks for the model by default, from now on, everywhere or globally. Tell \
    the owner the alias you set and whether it holds until /new or is the new default.";

const NOT_OWNER: &str = "set_model runs only when the owner asks: this turn's prompt is not the \
    owner's, so the model was not changed";

/// The tool of one turn: the journal and session key it stores the alias under, the route in
/// force without a stored one, the configured default a global switch to it restores, the
/// gateway whose routes it checks against, and the turn's origin.
pub struct SetModel {
    journal: PathBuf,
    key: String,
    default_route: String,
    configured: String,
    gateway: gateway::Client,
    origin: Origin,
}

impl SetModel {
    /// The tool of a turn of session key `key`, whose prompt has `origin`, over the journal
    /// file `journal`, with `default_route` in force when the session stores none and
    /// `configured` the configured default, checking aliases against the routes of `gateway`.
    pub fn new(
        journal: PathBuf,
        key: String,
        (default_route, configured): (String, String),
        gateway: gateway::Client,
        origin: Origin,
    ) -> Self {
        Self {
            journal,
            key,
            default_route,
            configured,
            gateway,
            origin,
        }
    }

    fn call(&self, args: &Args<'_>) -> Result<Output, String> {
        let current = self.current()?;
        let routes = self.gateway.routes().map_err(|e| {
            format!("could not list the gateway's routes ({e}); the model was not changed")
        })?;
        let aliases: Vec<&str> = routes.routes.iter().map(|r| r.alias.as_str()).collect();
        let listed = || json_array(aliases.iter().map(|a| json_str(a)));
        let Some(alias) = args.opt_str("model")?.map(str::trim) else {
            let routes = routes.routes.iter().map(|r| {
                let mut fields = vec![
                    ("alias", json_str(&r.alias)),
                    ("state", json_str(r.state.as_str())),
                ];
                if let Some(model) = &r.model {
                    fields.push(("model", json_str(model)));
                }
                json_object(fields)
            });
            return Ok(Output::ok(json_object([
                ("success", "true".to_owned()),
                ("current", json_str(&current)),
                ("default", json_str(&self.default_route)),
                ("routes", json_array(routes)),
            ])));
        };
        if !aliases.contains(&alias) {
            return Err(format!(
                "unknown model alias {alias:?}; the gateway serves {}; the model was not changed",
                listed()
            ));
        }
        let global = args.opt_bool("global")?.unwrap_or(false);
        let stored_default = (alias != self.configured).then_some(alias);
        Journal::open(&self.journal)
            .and_then(|mut journal| {
                if global {
                    journal.store_default_route(stored_default, JOURNAL_WAIT)?;
                }
                journal.store_route(&self.key, alias, JOURNAL_WAIT)
            })
            .map_err(|e| format!("journal write failed ({e}); the model was not changed"))?;
        let note = if global {
            "Stored for this session and as the default: the next turn, new sessions after /new \
             and the engine after a restart run on it."
        } else {
            "Stored for this session: the next turn runs on it, until /new returns to the default."
        };
        Ok(Output::ok(json_object([
            ("success", "true".to_owned()),
            ("previous", json_str(&current)),
            ("model", json_str(alias)),
            ("global", global.to_string()),
            ("note", json_str(note)),
        ])))
    }

    /// The alias in force for the session's next turn: the stored one, else the default.
    fn current(&self) -> Result<String, String> {
        let stored = Journal::open(&self.journal)
            .and_then(|journal| journal.route(&self.key, JOURNAL_WAIT))
            .map_err(|e| format!("journal read failed: {e}"))?;
        Ok(stored.unwrap_or_else(|| self.default_route.clone()))
    }
}

impl super::Tool for SetModel {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: SET_MODEL.into(),
            description: DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "model": {
                        "type": "string",
                        "description": "Route alias to switch to, e.g. opus, sonnet or haiku; omit to list the aliases and the current one."
                    },
                    "global": {
                        "type": "boolean",
                        "description": "Also make the alias the default model for new sessions and after a restart, as `/model <alias> --global`. Default false: this session only."
                    }
                }),
                &[],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        if self.origin != Origin::TopLevelOwner {
            return Output::error(NOT_OWNER);
        }
        self.call(&Args(input)).unwrap_or_else(Output::error)
    }
}
