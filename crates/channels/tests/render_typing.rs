//! Typing indicator cadence over an injected clock.

use std::time::{Duration, Instant};

use ulei_channels::render::{
    TYPING_COOLDOWN, TYPING_INTERVAL, TYPING_SEND_TIMEOUT, TypingFailure, TypingRefresh,
};

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn defaults_match_the_reference() {
    assert_eq!(TYPING_INTERVAL, secs(2));
    assert_eq!(TYPING_SEND_TIMEOUT, Duration::from_millis(1500));
    assert_eq!(TYPING_COOLDOWN, secs(30));
}

#[test]
fn first_action_is_immediate_then_every_interval_after_the_call_returns() {
    let t0 = Instant::now();
    let mut typing = TypingRefresh::start(t0, TYPING_COOLDOWN);
    assert_eq!(typing.next_due(), Some(t0));
    assert!(typing.poll(t0));
    let returned = t0 + Duration::from_millis(300);
    typing.sent(returned, Ok(()));
    assert_eq!(typing.next_due(), Some(returned + TYPING_INTERVAL));

    assert!(!typing.poll(returned + secs(1)), "not due yet");
    assert!(typing.poll(returned + TYPING_INTERVAL));
}

#[test]
fn paused_ticks_keep_the_cadence_without_sending() {
    let t0 = Instant::now();
    let mut typing = TypingRefresh::start(t0, TYPING_COOLDOWN);
    typing.pause();
    assert!(!typing.poll(t0));
    assert_eq!(typing.next_due(), Some(t0 + TYPING_INTERVAL));
    typing.resume();
    assert!(typing.poll(t0 + TYPING_INTERVAL));
}

#[test]
fn stop_ends_the_schedule() {
    let t0 = Instant::now();
    let mut typing = TypingRefresh::start(t0, TYPING_COOLDOWN);
    typing.stop();
    assert!(!typing.is_running());
    assert_eq!(typing.next_due(), None);
    assert!(!typing.poll(t0 + secs(10)));
    typing.sent(t0 + secs(10), Ok(()));
    assert_eq!(typing.next_due(), None, "a late result does not restart it");
}

#[test]
fn transient_failure_cools_down_then_recovers() {
    let t0 = Instant::now();
    let mut typing = TypingRefresh::start(t0, secs(10));
    assert!(typing.poll(t0));
    let failure = TypingFailure {
        status: Some(502),
        ..TypingFailure::default()
    };
    typing.sent(t0, Err(&failure));
    assert!(typing.in_cooldown(t0 + secs(9)));
    assert!(!typing.poll(t0 + secs(2)));
    assert!(!typing.poll(t0 + secs(8)));
    assert!(typing.poll(t0 + secs(10)), "cooldown over");
    assert!(!typing.in_cooldown(t0 + secs(10)));
}

#[test]
fn retry_after_sets_the_cooldown_within_bounds() {
    let t0 = Instant::now();
    let mut typing = TypingRefresh::start(t0, TYPING_COOLDOWN);
    let flood = TypingFailure {
        retry_after: Some(secs(5)),
        ..TypingFailure::default()
    };
    typing.sent(t0, Err(&flood));
    assert!(typing.in_cooldown(t0 + secs(4)));
    assert!(!typing.in_cooldown(t0 + secs(5)));

    let huge = TypingFailure {
        retry_after: Some(secs(3600)),
        ..TypingFailure::default()
    };
    typing.sent(t0, Err(&huge));
    assert!(typing.in_cooldown(t0 + secs(299)));
    assert!(!typing.in_cooldown(t0 + secs(300)));
}

#[test]
fn permanent_failure_does_not_cool_down() {
    let t0 = Instant::now();
    let mut typing = TypingRefresh::start(t0, TYPING_COOLDOWN);
    let forbidden = TypingFailure {
        status: Some(403),
        description: "Forbidden: bot was blocked by the user".into(),
        ..TypingFailure::default()
    };
    assert!(!forbidden.is_transient());
    typing.sent(t0, Err(&forbidden));
    assert!(!typing.in_cooldown(t0));
    assert!(typing.poll(t0 + TYPING_INTERVAL));
}

#[test]
fn transient_failures_are_classified_like_the_reference() {
    let by_text = |d: &str| TypingFailure {
        description: d.into(),
        ..TypingFailure::default()
    };
    assert!(by_text("Too Many Requests: retry later").is_transient());
    assert!(by_text("Timed out").is_transient());
    assert!(by_text("temporarily unavailable").is_transient());
    assert!(
        TypingFailure {
            status: Some(429),
            ..TypingFailure::default()
        }
        .is_transient()
    );
    assert!(
        TypingFailure {
            io: true,
            ..TypingFailure::default()
        }
        .is_transient()
    );
    assert!(!by_text("Bad Request: chat not found").is_transient());
}

#[test]
fn configured_cooldown_is_clamped() {
    let t0 = Instant::now();
    let failure = TypingFailure {
        io: true,
        ..TypingFailure::default()
    };
    let mut short = TypingRefresh::start(t0, Duration::ZERO);
    short.sent(t0, Err(&failure));
    assert!(short.in_cooldown(t0 + Duration::from_millis(999)));
    assert!(!short.in_cooldown(t0 + secs(1)));
}

#[test]
fn abandoned_call_reschedules_without_cooldown() {
    let t0 = Instant::now();
    let mut typing = TypingRefresh::start(t0, TYPING_COOLDOWN);
    assert!(typing.poll(t0));
    let gave_up = t0 + TYPING_SEND_TIMEOUT;
    typing.abandoned(gave_up);
    assert_eq!(typing.next_due(), Some(gave_up + TYPING_INTERVAL));
    assert!(!typing.in_cooldown(gave_up));
    assert!(
        typing.poll(gave_up + TYPING_INTERVAL),
        "the next tick sends again"
    );
}

#[test]
fn cooldown_carries_over_to_the_next_turn() {
    let t0 = Instant::now();
    let mut first = TypingRefresh::start(t0, TYPING_COOLDOWN);
    assert!(first.poll(t0));
    let flood = TypingFailure {
        status: Some(429),
        retry_after: Some(secs(20)),
        ..TypingFailure::default()
    };
    first.sent(t0, Err(&flood));
    first.stop();

    let t1 = t0 + secs(5);
    let mut next =
        TypingRefresh::start(t1, TYPING_COOLDOWN).with_cooldown_until(first.cooldown_until());
    assert!(!next.poll(t1), "still inside the flood-control window");
    assert!(next.poll(t0 + secs(21)));
}

#[test]
fn record_updates_the_cooldown_only() {
    let t0 = Instant::now();
    let mut typing = TypingRefresh::start(t0, secs(10));
    assert!(typing.poll(t0));
    typing.sent(t0, Ok(()));
    let due = typing.next_due();
    let failure = TypingFailure {
        io: true,
        ..TypingFailure::default()
    };
    typing.record(t0 + secs(1), Err(&failure));
    assert_eq!(typing.next_due(), due, "the cadence is untouched");
    assert!(typing.in_cooldown(t0 + secs(10)));
    assert!(!typing.in_cooldown(t0 + secs(11)));
}
