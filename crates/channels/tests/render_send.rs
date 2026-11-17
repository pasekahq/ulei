//! Send plans of agent replies: chunking, reply threading, link previews, notifications and
//! the plain-text fallback decisions.

use ulei_channels::render::{
    MAX_MESSAGE_LEN, Notifications, ParseMode, ReplyToMode, SendOptions, SendPlan,
    is_markup_rejection, is_reply_target_missing, plan_reply, utf16_len,
};

fn long_reply() -> String {
    "Строка ответа с точкой.\n".repeat(400)
}

#[test]
fn blank_reply_sends_nothing() {
    for blank in ["", "  \n\t", "\u{3000}"] {
        let plan = plan_reply(blank, Some(7), false, &SendOptions::default());
        assert_eq!(plan, SendPlan::default());
    }
}

#[test]
fn short_reply_is_one_markdown_v2_message() {
    let plan = plan_reply(
        "**Готово** (1.5 сек)",
        Some(7),
        true,
        &SendOptions::default(),
    );
    assert_eq!(plan.messages.len(), 1);
    let m = &plan.messages[0];
    assert_eq!(m.text, "*Готово* \\(1\\.5 сек\\)");
    assert_eq!(m.parse_mode, Some(ParseMode::MarkdownV2));
    assert_eq!(ParseMode::MarkdownV2.as_str(), "MarkdownV2");
    assert_eq!(m.reply_to, Some(7));
    assert!(!m.disable_link_preview);
}

#[test]
fn long_reply_is_split_with_escaped_indicators() {
    let plan = plan_reply(&long_reply(), None, true, &SendOptions::default());
    let n = plan.messages.len();
    assert!(n >= 3, "{n} chunks");
    for (i, m) in plan.messages.iter().enumerate() {
        assert!(utf16_len(&m.text) <= MAX_MESSAGE_LEN);
        assert!(
            m.text.ends_with(&format!(" \\({}/{n}\\)", i + 1)),
            "{i}: {:?}",
            m.text
        );
    }
}

#[test]
fn reply_threading_follows_the_mode() {
    let threaded = |mode| {
        let options = SendOptions {
            reply_to_mode: mode,
            ..SendOptions::default()
        };
        plan_reply(&long_reply(), Some(99), true, &options)
            .messages
            .iter()
            .map(|m| m.reply_to)
            .collect::<Vec<_>>()
    };
    let first = threaded(ReplyToMode::First);
    assert_eq!(first[0], Some(99));
    assert!(first[1..].iter().all(Option::is_none));
    assert!(threaded(ReplyToMode::All).iter().all(|r| *r == Some(99)));
    assert!(threaded(ReplyToMode::Off).iter().all(Option::is_none));
    assert_eq!(ReplyToMode::default(), ReplyToMode::First);
}

#[test]
fn intermediate_replies_are_silent_and_retrigger_typing() {
    let options = SendOptions {
        disable_link_previews: true,
        ..SendOptions::default()
    };
    let progress = plan_reply("Проверяю:", None, false, &options);
    assert!(progress.messages[0].disable_notification);
    assert!(progress.messages[0].disable_link_preview);
    assert!(progress.retrigger_typing);

    let last = plan_reply("Ответ", None, true, &options);
    assert!(!last.messages[0].disable_notification);
    assert!(!last.retrigger_typing);

    let loud = SendOptions {
        notifications: Notifications::All,
        ..options
    };
    assert!(!plan_reply("Проверяю:", None, false, &loud).messages[0].disable_notification);
}

#[test]
fn plain_fallback_strips_markup_and_keeps_the_rest() {
    let options = SendOptions {
        reply_to_mode: ReplyToMode::All,
        ..SendOptions::default()
    };
    let plan = plan_reply(
        "**Итог**: `a_b` и [ссылка](http://e.org/x) (v1.2)",
        Some(5),
        false,
        &options,
    );
    let m = &plan.messages[0];
    let fallback = m.plain_fallback();
    assert_eq!(
        fallback.text,
        "Итог: `a_b` и [ссылка](http://e.org/x) (v1.2)"
    );
    assert_eq!(fallback.parse_mode, None);
    assert_eq!(fallback.reply_to, m.reply_to);
    assert_eq!(fallback.disable_notification, m.disable_notification);
}

#[test]
fn markup_rejection_is_recognised_from_the_error_text() {
    let text = "plain";
    assert!(is_markup_rejection(
        "Bad Request: can't parse entities: Character '.' is reserved",
        text
    ));
    assert!(is_markup_rejection(
        "Can't find end of the entity starting at byte offset 3 (Markdown)",
        text
    ));
    assert!(!is_markup_rejection("Bad Request: chat not found", text));
    assert!(!is_markup_rejection(
        "Forbidden: bot was blocked by the user",
        text
    ));

    let emoji = "![👍](tg://emoji?id=5)";
    assert!(is_markup_rejection(
        "Bad Request: not enough rights to send custom emoji",
        emoji
    ));
    assert!(is_markup_rejection("Premium required", emoji));
    assert!(!is_markup_rejection("Premium required", text));
}

#[test]
fn missing_reply_target_is_recognised() {
    assert!(is_reply_target_missing(
        "Bad Request: message to be replied not found"
    ));
    assert!(!is_reply_target_missing("Bad Request: message is too long"));
}
