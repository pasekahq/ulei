//! The uncertain alerter against a fake C1 inbox and the fake Bot API: the library with an
//! injected time, and `ulei-tg uncertain` as the timer starts it.

#[allow(
    dead_code,
    reason = "the other test crates use other parts of the shared support"
)]
mod support {
    pub(crate) mod direct_config;
    pub(crate) mod fake_bot_api;
    pub(crate) mod fake_c6;
    pub(crate) mod fake_clock;
    pub(crate) mod process;
    pub(crate) mod scratch;
    pub(crate) mod sender;
}

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::{Value, json};
use support::direct_config::{owners, write_config};
use support::fake_bot_api::{FakeBotApi, Reply, fail};
use support::fake_c6::{Answer, FakeC6, error, ok};
use support::process::{stderr, stdout};
use support::scratch::Scratch;
use support::sender::{sender, sent};
use ulei_channels::alert::{AlertError, Alerter, REPEAT_SECS, Run};
use ulei_channels::bot_api::Sender;
use ulei_channels::notify::NotifyError;

const OWNER: i64 = 42;
const T0: u64 = 1_800_000_000;
const BLOCKED: &str = "hive:card-7:blocked";
const MISS: &str = "garden:miss-1";
const WEEKLY: &str = "weekly-3";

/// The C1 `UncertainEvent` of `id` with the source C1 derives from it.
fn event(id: &str, kind: &str, reason: &str, time: f64) -> Value {
    let source = id.split(':').next().unwrap_or_default();
    json!({ "id": id, "source": source, "kind": kind, "time": time, "reason": reason })
}

fn blocked() -> Value {
    event(
        BLOCKED,
        "task_blocked",
        "expired-processing",
        1_799_996_400.0,
    )
}

fn miss() -> Value {
    event(MISS, "garden_miss", "uncertain", 1_799_999_940.5)
}

fn weekly() -> Value {
    event(WEEKLY, "weekly", "deferred-exhausted", 1_800_000_000.0)
}

fn listed(events: &[Value]) -> Answer {
    ok(json!({ "events": events }))
}

/// A fake inbox and Bot API, the owner's sender and the state file in a scratch directory.
struct Bed {
    dir: Scratch,
    inbox: FakeC6,
    bot: FakeBotApi,
    sender: Sender,
}

impl Bed {
    fn new(tag: &str) -> Self {
        let dir = Scratch::new(tag);
        let inbox = FakeC6::start(&dir.socket("c1"));
        let bot = FakeBotApi::start();
        let (sender, _) = sender(&bot);
        Self {
            dir,
            inbox,
            bot,
            sender,
        }
    }

    fn state(&self) -> PathBuf {
        self.dir.path("alert.json")
    }

    /// One run at `now` whose `uncertain` call gets `answer`; a notice due gets message `id`.
    fn run(&self, now: u64, answer: Answer, id: i64) -> Result<Run, AlertError> {
        self.inbox.script("uncertain", answer);
        self.bot.script("sendMessage", sent(id));
        self.run_scripted(now)
    }

    fn run_scripted(&self, now: u64) -> Result<Run, AlertError> {
        Alerter::new(self.dir.socket("c1"), &self.sender, &[OWNER], self.state())
            .expect("alerter")
            .run(now)
    }

    /// The texts of the notices sent so far.
    fn notices(&self) -> Vec<String> {
        self.bot
            .calls("sendMessage")
            .iter()
            .map(|r| r.json()["text"].as_str().expect("text").to_owned())
            .collect()
    }
}

fn sent_one(run: &Run) -> bool {
    run.receipt
        .as_ref()
        .is_some_and(|r| r.chats.len() == 1 && r.chats[0].message_ids.len() == 1)
}

#[test]
fn new_uncertain_events_give_one_notice_naming_each_and_only_c1_uncertain_is_called() {
    let bed = Bed::new("alert-new");

    let run = bed.run(T0, listed(&[blocked(), miss()]), 7).expect("run");

    assert!(sent_one(&run), "{run:?}");
    assert_eq!(run.inbox_error, None);
    assert_eq!(
        bed.notices(),
        [format!(
            "ulei inbox: 2 uncertain events, 2 new. They may not have reached the agent.\n\n\
             {BLOCKED} | task_blocked | from hive | expired-processing | 2027-01-15 07:00 UTC | new\n\
             {MISS} | garden_miss | from garden | uncertain | 2027-01-15 07:59 UTC | new"
        )]
    );
    assert_eq!(
        bed.inbox.calls(),
        [("uncertain".to_owned(), json!({ "limit": 100 }))]
    );
    let methods: Vec<String> = bed
        .bot
        .requests()
        .iter()
        .map(|r| r.method().to_owned())
        .collect();
    assert_eq!(methods, ["sendMessage"]);
    assert_eq!(bed.bot.calls("sendMessage")[0].json()["chat_id"], OWNER);
}

#[test]
fn the_same_events_are_told_again_only_once_24_h_have_passed() {
    let bed = Bed::new("alert-repeat");
    let both = || listed(&[blocked(), miss()]);
    bed.run(T0, both(), 1).expect("first");

    let within = bed.run(T0 + REPEAT_SECS - 1, both(), 2).expect("within");
    let after = bed.run(T0 + REPEAT_SECS, both(), 3).expect("after");
    let soon = bed.run(T0 + REPEAT_SECS + 60, both(), 4).expect("soon");

    assert_eq!(within.receipt, None);
    assert!(sent_one(&after), "{after:?}");
    assert_eq!(soon.receipt, None);
    let notices = bed.notices();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(
        notices[1].starts_with("ulei inbox: 2 uncertain events, 0 new."),
        "{}",
        notices[1]
    );
    assert!(!notices[1].contains("| new"), "{}", notices[1]);
}

#[test]
fn no_uncertain_event_gives_no_notice_and_clears_the_repeat() {
    let bed = Bed::new("alert-none");

    let empty = bed.run(T0, listed(&[]), 1).expect("empty");
    assert_eq!(empty.receipt, None);
    assert!(!bed.state().exists(), "an empty run wrote the state");

    bed.run(T0 + 60, listed(&[blocked()]), 2).expect("one");
    let cleared = bed.run(T0 + 120, listed(&[]), 3).expect("cleared");
    let again = bed.run(T0 + 180, listed(&[blocked()]), 4).expect("again");

    assert_eq!(cleared.receipt, None);
    assert!(sent_one(&again), "{again:?}");
    let notices = bed.notices();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(
        notices[1].starts_with("ulei inbox: 1 uncertain event, 1 new."),
        "{}",
        notices[1]
    );
}

#[test]
fn an_event_that_left_uncertain_is_not_named_again() {
    let bed = Bed::new("alert-left");
    bed.run(T0, listed(&[blocked(), miss()]), 1).expect("both");

    let left = bed.run(T0 + 60, listed(&[miss()]), 2).expect("left");
    let added = bed
        .run(T0 + 120, listed(&[miss(), weekly()]), 3)
        .expect("added");
    let repeat = bed
        .run(T0 + 120 + REPEAT_SECS, listed(&[miss(), weekly()]), 4)
        .expect("repeat");

    assert_eq!(left.receipt, None);
    assert!(sent_one(&added) && sent_one(&repeat));
    let notices = bed.notices();
    assert_eq!(notices.len(), 3, "{notices:?}");
    for later in &notices[1..] {
        assert!(!later.contains(BLOCKED), "{later}");
        assert!(later.contains(MISS) && later.contains(WEEKLY), "{later}");
    }
    assert!(notices[1].contains(&format!("{WEEKLY} | weekly | from weekly-3")));
    assert!(notices[1].ends_with("| new") && !notices[1].contains("UTC | new\n"));
}

#[test]
fn a_failed_send_leaves_the_state_so_the_next_run_tells_again() {
    let bed = Bed::new("alert-retry");
    bed.run(T0, listed(&[blocked()]), 1).expect("first");
    let before = fs::read(bed.state()).expect("state");
    bed.inbox.script("uncertain", listed(&[blocked(), miss()]));
    for _ in 0..3 {
        bed.bot.script(
            "sendMessage",
            fail(403, "Forbidden: bot was blocked by the user", None),
        );
    }

    let failed = bed.run_scripted(T0 + 60).expect_err("blocked");
    assert!(
        matches!(failed, AlertError::Send(NotifyError::Send { sent: 0, .. })),
        "{failed}"
    );
    assert_eq!(fs::read(bed.state()).expect("state"), before);

    let retried = bed
        .run(T0 + 120, listed(&[blocked(), miss()]), 5)
        .expect("retried");

    assert!(sent_one(&retried), "{retried:?}");
    let notices = bed.notices();
    let last = notices.last().expect("a notice");
    assert!(
        last.starts_with("ulei inbox: 2 uncertain events, 1 new."),
        "{last}"
    );
    assert!(last.contains(&format!("{MISS} | garden_miss")), "{last}");
}

#[test]
fn an_unreadable_inbox_sends_one_notice_repeated_every_24_h() {
    let unreadable = [
        (
            "internal",
            error(500, "internal", "row synthetic-secret-payload is malformed"),
            "C1 inbox answered 500 internal",
        ),
        (
            "malformed",
            ok(json!({ "events": "synthetic-secret-payload" })),
            "malformed C1 answer",
        ),
        ("closed", Answer::Close, "C1 socket"),
    ];
    for (tag, answer, reason) in unreadable {
        let bed = Bed::new(tag);
        bed.run(T0, listed(&[blocked()]), 1).expect("first");

        let fault = bed.run(T0 + 60, answer.clone(), 2).expect("fault");
        let quiet = bed.run(T0 + 120, answer.clone(), 3).expect("quiet");
        let repeat = bed
            .run(T0 + 60 + REPEAT_SECS, answer.clone(), 4)
            .expect("repeat");
        let back = bed
            .run(T0 + 61 + REPEAT_SECS, listed(&[blocked()]), 5)
            .expect("back");

        assert!(sent_one(&fault) && sent_one(&repeat), "{tag}");
        assert_eq!(quiet.receipt, None, "{tag}");
        let error = fault.inbox_error.expect("inbox error");
        assert!(error.starts_with(reason), "{tag}: {error}");
        assert_eq!(quiet.inbox_error, Some(error.clone()), "{tag}");
        assert!(sent_one(&back), "{tag}: {back:?}");
        let notices = bed.notices();
        assert_eq!(notices.len(), 4, "{tag}: {notices:?}");
        assert!(
            notices[3].starts_with("ulei inbox: 1 uncertain event, 0 new."),
            "{tag}: the reported events survive the fault: {}",
            notices[3]
        );
        for notice in &notices[1..3] {
            assert!(
                notice.starts_with(&format!(
                    "ulei inbox: cannot read the inbox ({error}); uncertain events"
                )),
                "{tag}: {notice}"
            );
            assert!(
                !notice.contains("synthetic-secret-payload"),
                "{tag}: {notice}"
            );
        }
    }
}

#[test]
fn an_inbox_without_its_socket_is_a_notice_too() {
    let bed = Bed::new("alert-gone");
    bed.bot.script("sendMessage", sent(9));

    let run = Alerter::new(bed.dir.socket("absent"), &bed.sender, &[OWNER], bed.state())
        .expect("alerter")
        .run(T0)
        .expect("run");

    assert!(sent_one(&run), "{run:?}");
    assert!(
        run.inbox_error
            .as_deref()
            .is_some_and(|e| e.starts_with("C1 socket:")),
        "{run:?}"
    );
}

/// Runs `ulei-tg uncertain --config CONFIG --state STATE`.
fn alert(config: &Path, state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ulei-tg"))
        .arg("uncertain")
        .arg("--config")
        .arg(config)
        .arg("--state")
        .arg(state)
        .output()
        .expect("run ulei-tg uncertain")
}

#[test]
fn the_state_outlives_the_process_and_the_engine_is_never_called() {
    let dir = Scratch::new("alert-cli");
    let bot = FakeBotApi::start();
    bot.script("sendMessage", sent(31));
    let inbox = FakeC6::start(&dir.socket("c1"));
    let engine = FakeC6::start(&dir.socket("c6"));
    inbox.script("uncertain", listed(&[blocked()]));
    inbox.script("uncertain", listed(&[blocked()]));
    let config = write_config(&dir, &bot, &owners(&OWNER.to_string()));
    let state = dir.path("alert.json");

    let first = alert(&config, &state);
    let second = alert(&config, &state);

    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(
        stdout(&first),
        format!("{{\"chats\":[{{\"chat_id\":{OWNER},\"message_ids\":[31]}}]}}\n")
    );
    assert_eq!(stderr(&first), "");
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert_eq!(
        (stdout(&second), stderr(&second)),
        (String::new(), String::new())
    );
    assert_eq!(bot.calls("sendMessage").len(), 1);
    assert_eq!(inbox.methods(), ["uncertain", "uncertain"]);
    assert!(engine.calls().is_empty(), "{:?}", engine.calls());
}

#[test]
fn the_exit_code_says_whether_the_notice_went_out() {
    let dir = Scratch::new("alert-exit");
    let bot = FakeBotApi::start();
    let inbox = FakeC6::start(&dir.socket("c1"));
    let config = write_config(
        &dir,
        &bot,
        &format!("{}read_timeout = 1\n", owners(&OWNER.to_string())),
    );
    let state = dir.path("alert.json");

    // A response timeout is not retried: the message may have arrived.
    inbox.script("uncertain", listed(&[blocked()]));
    bot.script(
        "sendMessage",
        Reply::Delayed(Duration::from_secs(3), Box::new(sent(40))),
    );
    let failed = alert(&config, &state);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(stdout(&failed), "");
    assert!(
        stderr(&failed)
            .starts_with("ulei-tg uncertain: sent 0 of 1 messages, then telegram Timeout"),
        "{}",
        stderr(&failed)
    );
    assert!(!state.exists());

    inbox.script("uncertain", error(500, "internal", "synthetic"));
    bot.script("sendMessage", sent(41));
    let unreadable = alert(&config, &state);
    assert_eq!(unreadable.status.code(), Some(0), "{}", stderr(&unreadable));
    assert_eq!(
        stderr(&unreadable),
        "ulei-tg uncertain: C1 inbox answered 500 internal\n"
    );
    assert!(stdout(&unreadable).contains("\"message_ids\":[41]"));

    fs::write(&state, "{\"reported\":").expect("corrupt state");
    let refused = alert(&config, &state);
    assert_eq!(refused.status.code(), Some(3));
    assert!(
        stderr(&refused).starts_with(&format!("ulei-tg uncertain: {}: ", state.display())),
        "{}",
        stderr(&refused)
    );
    assert_eq!(inbox.calls().len(), 2, "the refused run called the inbox");
    assert_eq!(bot.calls("sendMessage").len(), 2);

    let usage = Command::new(env!("CARGO_BIN_EXE_ulei-tg"))
        .args(["uncertain", "--config"])
        .arg(&config)
        .output()
        .expect("run");
    assert_eq!(usage.status.code(), Some(2));
}

#[test]
fn a_full_page_of_events_is_told_as_at_least_the_limit() {
    let bed = Bed::new("alert-full");
    let page: Vec<Value> = (0..100)
        .map(|n| {
            event(
                &format!("hive:card-{n}:blocked"),
                "task_blocked",
                "uncertain",
                1e9,
            )
        })
        .collect();
    for id in 50..60 {
        bed.bot.script("sendMessage", sent(id));
    }
    bed.inbox.script("uncertain", listed(&page));

    let run = bed.run_scripted(T0).expect("run");

    assert!(run.receipt.is_some(), "{run:?}");
    let text = bed.notices().concat();
    assert!(
        text.starts_with("ulei inbox: at least 100 uncertain events, the oldest listed, 100 new."),
        "{text}"
    );
    assert!(text.contains("hive:card-0:blocked") && text.contains("hive:card-99:blocked"));
}
