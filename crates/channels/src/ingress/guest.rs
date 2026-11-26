//! The owner's protocol traffic of the guest gate (ADR-0083): which owner updates go to the gate
//! before the ingress sees them, and the payload they go as. A port of the reference's
//! `guest-gate-ingress` plugin, rule for rule: the same button prefixes, command, words and
//! fields; plus the private-chat check the rest of the ingress applies.
//!
//! Pure: it reads the raw update object of `getUpdates` and builds a JSON value. The call to the
//! gate is `crate::tg`'s.

use serde_json::{Map, Value, json};

/// `callback_data` prefixes of the gate's buttons: approve, block, postpone, request.
pub const CALLBACK_PREFIXES: [&str; 4] = ["ap:", "bi:", "po:", "rq:"];
/// The command that lists the gate's pending requests.
pub const REQUESTS_COMMAND: &str = "/requests";
/// Owner words that answer the gate's last preview, compared after trim and lower case.
pub const ANSWER_WORDS: [&str; 4] = ["шли", "отправь", "отправить", "не надо"];
/// Verbs of `<verb> <24 hex id>` that act on one request, compared in lower case.
const ID_VERBS: [&str; 2] = ["отменить", "ответить"];
/// The fields of a message the gate gets; it never gets media bytes or the rest of the update.
pub const MESSAGE_FIELDS: [&str; 8] = [
    "message_id",
    "from",
    "chat",
    "text",
    "caption",
    "document",
    "voice",
    "audio",
];

/// Why an owner update is the gate's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// A button of the gate (`callback_query` with one of [`CALLBACK_PREFIXES`]).
    Button,
    /// `/requests`, `отменить|ответить <id>` or one of [`ANSWER_WORDS`].
    Command,
    /// Any other reply: the gate tells whether it answers one of its previews.
    Reply,
}

impl Protocol {
    /// The name in log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Command => "command",
            Self::Reply => "reply",
        }
    }

    /// Whether a failed gate call eats the update: a button or a command must never reach the
    /// model; a reply that the gate could not judge goes the usual way.
    pub fn eaten_on_failure(self) -> bool {
        !matches!(self, Self::Reply)
    }
}

/// An owner update for the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct Forward {
    /// Why it is the gate's.
    pub protocol: Protocol,
    /// The JSON body of the gate call.
    pub payload: Value,
}

/// The gate's part of `update`, a raw `getUpdates` object, if it is one of the owner's protocol
/// updates in a private chat; `None` sends it the usual way. Only `message` and `callback_query`
/// are looked at: an edited message is never the gate's.
pub fn classify(update: &Value, owner_ids: &[i64]) -> Option<Forward> {
    let update_id = update.get("update_id")?.as_i64()?;
    let owner = |user: Option<&Value>| {
        user.and_then(|u| u.get("id"))
            .and_then(Value::as_i64)
            .is_some_and(|id| owner_ids.contains(&id))
    };
    let private =
        |message: &Value| message.pointer("/chat/type").and_then(Value::as_str) == Some("private");
    if let Some(callback) = update.get("callback_query").filter(|c| c.is_object()) {
        let from = callback.get("from");
        let data = callback.get("data").and_then(Value::as_str).unwrap_or("");
        let message = callback.get("message").filter(|m| m.is_object())?;
        if !owner(from)
            || !CALLBACK_PREFIXES.iter().any(|p| data.starts_with(p))
            || !private(message)
        {
            return None;
        }
        let payload = json!({
            "update_id": update_id,
            "callback_query": {
                "id": callback.get("id").cloned().unwrap_or(Value::Null),
                "from": from.cloned().unwrap_or(Value::Null),
                "data": data,
                "message": kept(message),
            },
        });
        return Some(Forward {
            protocol: Protocol::Button,
            payload,
        });
    }
    let message = update.get("message").filter(|m| m.is_object())?;
    if !owner(message.get("from")) || !private(message) {
        return None;
    }
    let text = message.get("text").and_then(Value::as_str).unwrap_or("");
    let protocol = if is_command(text) {
        Protocol::Command
    } else if message
        .get("reply_to_message")
        .is_some_and(Value::is_object)
    {
        Protocol::Reply
    } else {
        return None;
    };
    Some(Forward {
        protocol,
        payload: json!({"update_id": update_id, "message": kept(message)}),
    })
}

/// The fields of [`MESSAGE_FIELDS`] that `message` has, and of the message it replies to only
/// its id.
fn kept(message: &Value) -> Value {
    let mut keep = Map::new();
    for field in MESSAGE_FIELDS {
        if let Some(value) = message.get(field) {
            keep.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(reply) = message.get("reply_to_message").filter(|r| r.is_object()) {
        let id = reply.get("message_id").cloned().unwrap_or(Value::Null);
        keep.insert("reply_to_message".to_owned(), json!({"message_id": id}));
    }
    Value::Object(keep)
}

/// Whether the owner's `text` is a command of the gate: `/requests` or `/requests@<bot>` as the
/// whole text; `отменить|ответить <24 hex digits>` at its start, in any case; or one of
/// [`ANSWER_WORDS`] as the whole text, in any case. Leading and trailing whitespace is ignored
/// but for `/requests`, as in the reference.
pub fn is_command(text: &str) -> bool {
    if text.split('@').next() == Some(REQUESTS_COMMAND) {
        return true;
    }
    let text = text.trim_matches(is_space);
    acts_on_id(text) || ANSWER_WORDS.contains(&text.to_lowercase().as_str())
}

/// `^(?:отменить|ответить)\s+[0-9a-f]{24}(?:\s|$)` with Python's `re.IGNORECASE`.
fn acts_on_id(text: &str) -> bool {
    let Some(verb_end) = text.find(is_space) else {
        return false;
    };
    if !ID_VERBS.contains(&text[..verb_end].to_lowercase().as_str()) {
        return false;
    }
    let rest = text[verb_end..].trim_start_matches(is_space);
    let hex = rest.bytes().take_while(u8::is_ascii_hexdigit).count();
    if hex < 24 {
        return false;
    }
    // Exactly 24 digits, then the end or whitespace.
    rest[24..].chars().next().is_none_or(is_space)
}

/// Python's `str.isspace` (and `re`'s `\s` on text): Unicode whitespace and the ASCII
/// separators `\x1c` to `\x1f`.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const OWNER: i64 = 1001;
    const ID: &str = "0123456789abcdef01234567";

    fn message(text: &str) -> Value {
        json!({
            "update_id": 7,
            "message": {
                "message_id": 42, "date": 1_700_000_000,
                "from": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
                "chat": {"id": OWNER, "type": "private", "first_name": "Owner"},
                "text": text,
                "entities": [{"type": "bot_command", "offset": 0, "length": 9}],
            }
        })
    }

    fn callback(data: &str) -> Value {
        json!({
            "update_id": 8,
            "callback_query": {
                "id": "cb-1", "chat_instance": "ci",
                "from": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
                "data": data,
                "message": {
                    "message_id": 50, "date": 1_700_000_000,
                    "from": {"id": 999, "is_bot": true, "first_name": "Bot"},
                    "chat": {"id": OWNER, "type": "private"},
                    "text": "Preview", "reply_markup": {"inline_keyboard": []},
                },
            }
        })
    }

    fn protocol(update: &Value) -> Option<Protocol> {
        classify(update, &[OWNER]).map(|f| f.protocol)
    }

    #[test]
    fn buttons_of_the_gate_are_its() {
        for data in ["ap:1", "bi:x", "po:", "rq:abc"] {
            assert_eq!(protocol(&callback(data)), Some(Protocol::Button), "{data}");
        }
        for data in ["", "ap", "AP:1", "xx:1", " ap:1", "model:x"] {
            assert_eq!(protocol(&callback(data)), None, "{data}");
        }
        let mut no_data = callback("ap:1");
        no_data["callback_query"]
            .as_object_mut()
            .unwrap()
            .remove("data");
        assert_eq!(protocol(&no_data), None);
        let mut stranger = callback("ap:1");
        stranger["callback_query"]["from"]["id"] = json!(5);
        assert_eq!(protocol(&stranger), None);
        let mut group = callback("ap:1");
        group["callback_query"]["message"]["chat"]["type"] = json!("group");
        assert_eq!(protocol(&group), None);
        // An inline message's button has no message: the reference forwards nothing.
        let mut inline = callback("ap:1");
        inline["callback_query"]
            .as_object_mut()
            .unwrap()
            .remove("message");
        assert_eq!(protocol(&inline), None);
    }

    #[test]
    fn commands_and_words_of_the_gate_are_its() {
        for text in [
            "/requests",
            "/requests@synthetic_bot",
            "/requests@",
            &format!("ответить {ID}"),
            &format!("Отменить {ID} текст"),
            &format!("ОТВЕТИТЬ\t{}\nи ещё", ID.to_uppercase()),
            &format!("  отменить   {ID}  "),
            "шли",
            "Шли",
            " Не надо ",
            "ОТПРАВЬ",
            "отправить\n",
        ] {
            assert_eq!(
                protocol(&message(text)),
                Some(Protocol::Command),
                "{text:?}"
            );
        }
        for text in [
            "/requests ",
            " /requests",
            "/requests list",
            "/request",
            &format!("ответить {}", &ID[..23]),
            &format!("ответить {ID}0"),
            &format!("ответить {ID}x"),
            &format!("ответить{ID}"),
            &format!("ответь {ID}"),
            &format!("пожалуйста ответить {ID}"),
            "ответить",
            "шли его",
            "не  надо",
            "ненадо",
            "отправь письмо",
            "",
        ] {
            assert_eq!(protocol(&message(text)), None, "{text:?}");
        }
    }

    #[test]
    fn owner_replies_are_the_gates_to_judge() {
        let mut reply = message("Да, пусть приходит");
        reply["message"]["reply_to_message"] = json!({
            "message_id": 41, "chat": {"id": OWNER, "type": "private"}, "text": "Preview body"});
        assert_eq!(protocol(&reply), Some(Protocol::Reply));
        // A command that is also a reply counts as a command: eaten when the gate fails.
        let mut both = message("шли");
        both["message"]["reply_to_message"] = json!({"message_id": 41});
        assert_eq!(protocol(&both), Some(Protocol::Command));
        assert!(Protocol::Command.eaten_on_failure() && Protocol::Button.eaten_on_failure());
        assert!(!Protocol::Reply.eaten_on_failure());
    }

    #[test]
    fn others_edits_and_other_chats_are_not_forwarded() {
        let plain = message("Hello");
        assert_eq!(protocol(&plain), None);
        let mut stranger = message("шли");
        stranger["message"]["from"]["id"] = json!(5);
        assert_eq!(protocol(&stranger), None);
        let mut nobody = message("шли");
        nobody["message"].as_object_mut().unwrap().remove("from");
        assert_eq!(protocol(&nobody), None);
        let mut group = message("/requests");
        group["message"]["chat"]["type"] = json!("supergroup");
        assert_eq!(protocol(&group), None);
        let edited = json!({"update_id": 9, "edited_message": message("шли")["message"].clone()});
        assert_eq!(protocol(&edited), None);
        let mut edited_reply = edited.clone();
        edited_reply["edited_message"]["reply_to_message"] = json!({"message_id": 1});
        assert_eq!(protocol(&edited_reply), None);
        assert_eq!(classify(&message("шли"), &[]), None);
        assert_eq!(classify(&json!({"message": {}}), &[OWNER]), None);
    }

    #[test]
    fn the_payload_holds_only_the_protocol_fields() {
        let mut update = message("шли");
        let m = &mut update["message"];
        m["caption"] = json!("cap");
        m["document"] = json!({"file_id": "d", "file_size": 3});
        m["voice"] = json!({"file_id": "v", "duration": 2});
        m["audio"] = json!({"file_id": "a"});
        m["photo"] = json!([{"file_id": "p"}]);
        m["forward_origin"] = json!({"type": "hidden_user", "sender_user_name": "X"});
        m["reply_to_message"] = json!({"message_id": 41, "text": "Preview body",
            "from": {"id": 999}, "chat": {"id": OWNER, "type": "private"}});
        let forward = classify(&update, &[OWNER]).unwrap();
        assert_eq!(
            forward.payload,
            json!({"update_id": 7, "message": {
                "message_id": 42,
                "from": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
                "chat": {"id": OWNER, "type": "private", "first_name": "Owner"},
                "text": "шли", "caption": "cap",
                "document": {"file_id": "d", "file_size": 3},
                "voice": {"file_id": "v", "duration": 2},
                "audio": {"file_id": "a"},
                "reply_to_message": {"message_id": 41},
            }})
        );
        let forward = classify(&callback("ap:9"), &[OWNER]).unwrap();
        assert_eq!(
            forward.payload,
            json!({"update_id": 8, "callback_query": {
                "id": "cb-1",
                "from": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
                "data": "ap:9",
                "message": {
                    "message_id": 50,
                    "from": {"id": 999, "is_bot": true, "first_name": "Bot"},
                    "chat": {"id": OWNER, "type": "private"},
                    "text": "Preview",
                },
            }})
        );
    }
}
