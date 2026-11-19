// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The owner chat's engine session over C6 (ADR-0033): the session key of the chat, turns with
//! their context, and the owner commands mapped onto C6 methods.

use ulei_contracts::generated::c6::{
    InterruptRequest, SessionStatus, SetRouteRequest, StatusRequest, SubmitTurnRequest,
};

use super::client::{Client, Error};
use crate::ingress::{
    Engine, EngineError, EngineStatus, Interrupted, NewSession, RouteSwitch, Turn,
};

/// The `channel` of every turn the chat submits: the reference's platform name.
pub const CHANNEL: &str = "telegram";

/// The session key of a private chat: the reference's key of a direct chat in the default
/// namespace.
pub fn session_key(chat_id: i64) -> String {
    format!("agent:main:{CHANNEL}:dm:{chat_id}")
}

/// The engine as one owner chat sees it.
#[derive(Debug)]
pub struct ChatSession {
    client: Client,
    key: String,
    /// `/new` ran: the next accepted turn starts a new session of the key.
    fresh: bool,
    failure: Option<Error>,
}

impl ChatSession {
    /// The session of `chat_id` on the engine `client` reaches.
    pub fn new(client: Client, chat_id: i64) -> Self {
        Self {
            client,
            key: session_key(chat_id),
            fresh: false,
            failure: None,
        }
    }

    /// The C6 failure of the last failed call since the previous take, if any.
    pub fn take_failure(&mut self) -> Option<Error> {
        self.failure.take()
    }

    fn failed(&mut self, error: Error) -> EngineError {
        let failed = EngineError {
            message: match &error {
                Error::Remote { message, .. } => message.clone(),
                other => other.to_string(),
            },
        };
        self.failure = Some(error);
        failed
    }

    fn session_status(&mut self) -> Result<SessionStatus, EngineError> {
        let request = StatusRequest {
            session: self.key.clone(),
            turn: None,
        };
        self.client.status(&request).map_err(|e| self.failed(e))
    }
}

impl Engine for ChatSession {
    fn submit_turn(&mut self, turn: &Turn) -> Result<(), EngineError> {
        let request = SubmitTurnRequest {
            session: self.key.clone(),
            prompt: turn.text.clone(),
            channel: Some(CHANNEL.to_owned()),
            new: self.fresh.then_some(true),
            context: turn.context.clone().filter(|c| !c.is_empty()),
        };
        let answer = self
            .client
            .submit_turn(&request)
            .map_err(|e| self.failed(e))?;
        if !answer.accepted {
            return Err(EngineError {
                message: format!("the session queue is full ({})", answer.queue_depth),
            });
        }
        self.fresh = false;
        Ok(())
    }

    fn interrupt(&mut self) -> Result<Interrupted, EngineError> {
        let request = InterruptRequest {
            session: self.key.clone(),
        };
        let answer = self
            .client
            .interrupt(&request)
            .map_err(|e| self.failed(e))?;
        Ok(if answer.interrupted {
            Interrupted::Running
        } else {
            Interrupted::Nothing
        })
    }

    fn status(&mut self) -> Result<EngineStatus, EngineError> {
        let status = self.session_status()?;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a checked non-negative Unix time in seconds fits i64"
        )]
        let since = status.since as i64;
        Ok(EngineStatus {
            session_id: status.session,
            title: None,
            created: None,
            last_activity: since,
            model: Some(status.route),
            context_used: 0,
            context_total: 0,
            tokens_billed: None,
            running: status.running,
            queued: status.queue_depth,
        })
    }

    fn set_route(&mut self, alias: &str) -> Result<RouteSwitch, EngineError> {
        let request = SetRouteRequest {
            session: self.key.clone(),
            route: alias.to_owned(),
        };
        let answer = self
            .client
            .set_route(&request)
            .map_err(|e| self.failed(e))?;
        Ok(RouteSwitch {
            alias: answer.route,
            context_total: None,
        })
    }

    /// Stops the running turn, as the reference's reset invalidates the running generation,
    /// and makes the chat's next turn start a new session of the key.
    fn new_session(&mut self) -> Result<NewSession, EngineError> {
        let status = self.session_status()?;
        if status.running {
            self.interrupt()?;
        }
        self.fresh = true;
        Ok(NewSession {
            reset_existing: status.running || status.queue_depth > 0 || status.last_turn.is_some(),
            model: None,
            context_total: None,
        })
    }
}
