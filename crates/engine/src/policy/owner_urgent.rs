//! `owner-urgent`: only a turn of the owner on the main channel may ask for urgent work
//! (ADR-0103). A tool call whose arguments set `urgent` to true, such as the hive's
//! `hive_create`, is refused on every other turn; this is the one place the flag is enforced.

use std::path::Path;

use serde_json::Value;
use ulei_contracts::generated::c4g::{Message, ToolCall};

use super::{Origin, Policy, Verdict};

/// The text of the refusal the model sees.
const REFUSAL: &str = "`urgent` is set only on a turn of the owner on the main channel; \
    create the card without it";

/// Denies a call with `"urgent": true` outside a [`Origin::TopLevelOwner`] turn.
#[derive(Debug, Clone)]
pub struct OwnerUrgent {
    origin: Origin,
}

impl Default for OwnerUrgent {
    fn default() -> Self {
        Self {
            origin: Origin::Other,
        }
    }
}

impl Policy for OwnerUrgent {
    fn name(&self) -> &'static str {
        "owner-urgent"
    }

    fn on_turn_start(&mut self, origin: Origin, _input: &Message) {
        self.origin = origin;
    }

    fn pre_tool(&mut self, call: &ToolCall, _cwd: &Path) -> Verdict {
        if self.origin != Origin::TopLevelOwner
            && call.input.get("urgent") == Some(&Value::Bool(true))
        {
            Verdict::Deny(REFUSAL.to_owned())
        } else {
            Verdict::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};
    use ulei_contracts::generated::c4g::Role;

    use super::super::Chain;
    use super::*;

    fn call(input: Value) -> ToolCall {
        let Value::Object(input) = input else {
            unreachable!("test inputs are objects")
        };
        ToolCall {
            id: "call-1".to_owned(),
            name: "mcp_hive_hive_create".to_owned(),
            input,
        }
    }

    fn turn(origin: Origin) -> Chain {
        let mut chain = Chain::new(vec![Box::new(OwnerUrgent::default())]);
        let input = Message {
            role: Role::User,
            content: Vec::new(),
        };
        chain.on_turn_start(origin, &input);
        chain
    }

    fn create(urgent: &Value) -> ToolCall {
        call(
            json!({"title": "t", "goal": "g", "step": 3, "source": "msg:1", "acceptance": "true",
            "urgent": urgent}),
        )
    }

    #[test]
    fn urgent_is_refused_outside_a_top_level_owner_turn() {
        let verdict = turn(Origin::Other).pre_tool(&create(&json!(true)), Path::new("/"));
        assert_eq!(
            verdict,
            Verdict::Deny(format!("denied by policy owner-urgent: {REFUSAL}"))
        );
    }

    #[test]
    fn urgent_passes_on_a_top_level_owner_turn() {
        let mut chain = turn(Origin::TopLevelOwner);
        assert_eq!(
            chain.pre_tool(&create(&json!(true)), Path::new("/")),
            Verdict::Allow
        );
    }

    #[test]
    fn a_call_without_urgent_true_passes_on_any_turn() {
        for origin in [Origin::Other, Origin::TopLevelOwner] {
            let mut chain = turn(origin);
            for input in [create(&json!(false)), call(Value::Object(Map::new()))] {
                assert_eq!(chain.pre_tool(&input, Path::new("/")), Verdict::Allow);
            }
        }
    }

    #[test]
    fn the_origin_follows_each_turn() {
        let mut chain = turn(Origin::TopLevelOwner);
        let input = Message {
            role: Role::User,
            content: Vec::new(),
        };
        chain.on_turn_start(Origin::Other, &input);
        assert!(matches!(
            chain.pre_tool(&create(&json!(true)), Path::new("/")),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn before_any_turn_urgent_is_refused() {
        let mut policy = OwnerUrgent::default();
        assert!(matches!(
            policy.pre_tool(&create(&json!(true)), Path::new("/")),
            Verdict::Deny(_)
        ));
    }
}
