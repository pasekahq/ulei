//! The embedder helper `ulei-garden-embed --embedder fake` behind `ulei_garden::embed::pipe`,
//! over synthetic artifact files, and scripted helpers that hang or answer slowly; no model, no
//! network.

#![allow(missing_docs, clippy::unwrap_used)]

mod scripted;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ulei_garden::embed::pipe::{CALL_DEADLINE, Helper, Identity};
use ulei_garden::embed::{EmbedError, Embedder, HashEmbedder, ONNX_FILE, TOKENIZER_FILE};

const EMBED: &str = env!("CARGO_BIN_EXE_ulei-garden-embed");
/// SHA-256 of `abc`.
const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
/// SHA-256 of the empty file.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A model directory holding `abc` as the model and an empty tokenizer, removed on drop.
struct ModelDir(PathBuf);

impl ModelDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ulei-garden-embed-pipe-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(ONNX_FILE).parent().unwrap()).unwrap();
        fs::write(dir.join(ONNX_FILE), "abc").unwrap();
        fs::write(dir.join(TOKENIZER_FILE), "").unwrap();
        Self(dir)
    }
}

impl Drop for ModelDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_ready_line_carries_both_artifact_digests_and_vectors_cross_the_pipe_bit_for_bit() {
    let dir = ModelDir::new();
    let mut command = Command::new(EMBED);
    command
        .args(["--embedder", "fake", "--model-dir"])
        .arg(&dir.0);
    let helper = Helper::start(command, Duration::from_secs(30), CALL_DEADLINE).unwrap();
    assert!(
        helper.has_lanes(),
        "the fake helper announces its two lanes"
    );
    let reference = HashEmbedder::new(256);
    assert_eq!(
        helper.identity(),
        &Identity {
            name: reference.model_id().to_owned(),
            model_sha256: ABC.to_owned(),
            tokenizer_sha256: EMPTY.to_owned(),
            dim: 256,
        }
    );
    assert_eq!(
        helper.model_id(),
        format!("{}/{ABC}/{EMPTY}", reference.model_id())
    );
    assert_eq!(helper.dim(), 256);

    let query = "Bram Sample sees the dentist on Fridays";
    assert_eq!(
        helper.embed_query(query).unwrap(),
        reference.embed_query(query).unwrap()
    );
    let passages = [
        "Cafe Lumen has a green door.",
        "Fogtown expects drizzle.",
        "",
    ];
    assert_eq!(
        helper.embed_passages(&passages).unwrap(),
        reference.embed_passages(&passages).unwrap()
    );
}

#[test]
fn a_helper_that_misses_the_call_deadline_is_killed_and_reaped_and_every_later_call_fails() {
    let dir = ModelDir::new();
    let path = scripted::script(&dir.0, "hangs", scripted::HANG);
    let helper = scripted::start(&path, Duration::from_millis(200)).unwrap();
    let pid = scripted::pid(&dir.0, "hangs");
    assert_eq!(helper.fault(), None);

    let asked = Instant::now();
    let e = helper.embed_query("Fogtown weather").unwrap_err();
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "{:?}",
        asked.elapsed()
    );
    let why = "helper pipe: no answer within 200ms";
    assert!(e.to_string().ends_with(why), "{e}");
    assert_eq!(helper.fault().as_deref(), Some(why));
    assert!(!scripted::alive(pid), "the helper is killed and reaped");

    let again = Instant::now();
    assert!(
        helper
            .embed_passages(&["Cafe Lumen has a green door."])
            .is_err()
    );
    assert!(again.elapsed() < Duration::from_millis(100));
    assert_eq!(scripted::log(&dir.0, "hangs").len(), 1, "no second request");
}

#[test]
fn a_helper_that_exits_on_a_call_breaks_the_pipe_and_one_that_exits_while_idle_reports_its_exit() {
    let dir = ModelDir::new();
    let path = scripted::script(&dir.0, "exits", &scripted::exits_after(1));
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert_eq!(helper.embed_query("first").unwrap(), [1.0]);
    let e = helper.embed_query("second").unwrap_err();
    assert!(
        e.to_string().ends_with("helper pipe: the helper closed it"),
        "{e}"
    );
    assert!(!scripted::alive(scripted::pid(&dir.0, "exits")));

    let path = scripted::script(&dir.0, "idle", &format!("{}; exit 0", scripted::ANSWER));
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert_eq!(helper.embed_query("first").unwrap(), [1.0]);
    let asked = Instant::now();
    while helper.fault().is_none() && asked.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        helper.fault().as_deref(),
        Some("the helper exited (exit status: 0)")
    );
    assert!(!scripted::alive(scripted::pid(&dir.0, "idle")));
}

#[test]
fn a_waiting_query_takes_the_pipe_before_a_waiting_passage() {
    let dir = ModelDir::new();
    let path = scripted::script(&dir.0, "slow", &scripted::slow("0.4"));
    let helper = Arc::new(scripted::start(&path, CALL_DEADLINE).unwrap());
    assert!(
        !helper.has_lanes(),
        "a ready line without lanes keeps one call at a time"
    );
    let passage = |text: &'static str| {
        let helper = Arc::clone(&helper);
        thread::spawn(move || helper.embed_passages(&[text]).unwrap())
    };
    let first = passage("first passage");
    thread::sleep(Duration::from_millis(100));
    let second = passage("second passage");
    thread::sleep(Duration::from_millis(100));
    assert_eq!(helper.embed_query("the query").unwrap(), [1.0]);
    first.join().unwrap();
    second.join().unwrap();
    let texts: Vec<String> = scripted::log(&dir.0, "slow")
        .iter()
        .map(|line| {
            let request: serde_json::Value = serde_json::from_str(line).unwrap();
            request["texts"][0].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(texts, ["first passage", "the query", "second passage"]);
}

/// The texts of the requests the script `name` of `dir` has read, in order.
fn texts(dir: &std::path::Path, name: &str) -> Vec<String> {
    scripted::log(dir, name)
        .iter()
        .map(|line| {
            let request: serde_json::Value = serde_json::from_str(line).unwrap();
            request["texts"][0].as_str().unwrap().to_owned()
        })
        .collect()
}

#[test]
fn with_lanes_a_query_is_answered_while_a_long_passage_is_in_flight() {
    let dir = ModelDir::new();
    scripted::set_lanes_identity(&dir.0);
    let per_call = scripted::by_lane(&scripted::later("1.5"), scripted::ANSWER_ID);
    let path = scripted::script(&dir.0, "lanes", &per_call);
    // The query's deadline is shorter than the passage: only the passage's own counts for it.
    let helper = Arc::new(
        scripted::start(&path, Duration::from_millis(500))
            .unwrap()
            .with_passage_deadline(Duration::from_secs(5)),
    );
    assert!(helper.has_lanes());
    let started = Instant::now();
    let passage = {
        let helper = Arc::clone(&helper);
        thread::spawn(move || {
            let v = helper.embed_passages(&["a long passage"]).unwrap();
            (v, started.elapsed())
        })
    };
    thread::sleep(Duration::from_millis(100));
    for n in 0..3 {
        let asked = Instant::now();
        assert_eq!(helper.embed_query(&format!("query {n}")).unwrap(), [1.0]);
        assert!(
            asked.elapsed() < Duration::from_millis(400),
            "{:?}",
            asked.elapsed()
        );
    }
    let queried = started.elapsed();
    let (vectors, passage_took) = passage.join().unwrap();
    assert_eq!(vectors, [[1.0]]);
    assert!(queried < Duration::from_millis(1000), "{queried:?}");
    assert!(
        passage_took >= Duration::from_millis(1500),
        "{passage_took:?}"
    );
    assert_eq!(
        texts(&dir.0, "lanes"),
        ["a long passage", "query 0", "query 1", "query 2"]
    );
    let ids: Vec<u64> = scripted::log(&dir.0, "lanes")
        .iter()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["id"]
                .as_u64()
                .unwrap()
        })
        .collect();
    let mut distinct = ids.clone();
    distinct.dedup();
    assert_eq!(distinct, ids, "every request has an id of its own");
    assert_eq!(helper.fault(), None);
}

#[test]
fn answers_on_the_two_lanes_never_mix_up_under_concurrent_calls() {
    let dir = ModelDir::new();
    let mut command = Command::new(EMBED);
    command
        .args(["--embedder", "fake", "--model-dir"])
        .arg(&dir.0);
    let helper = Arc::new(Helper::start(command, Duration::from_secs(30), CALL_DEADLINE).unwrap());
    let reference = HashEmbedder::new(256);
    let rounds = 300;
    let queries = {
        let helper = Arc::clone(&helper);
        let reference = reference.clone();
        thread::spawn(move || {
            for n in 0..rounds {
                let q = format!("query number {n} about Fogtown");
                assert_eq!(
                    helper.embed_query(&q).unwrap(),
                    reference.embed_query(&q).unwrap()
                );
            }
        })
    };
    for n in 0..rounds {
        let p = [
            format!("passage {n}: Cafe Lumen"),
            format!("and its door {}", n * 7),
        ];
        let p: Vec<&str> = p.iter().map(String::as_str).collect();
        assert_eq!(
            helper.embed_passages(&p).unwrap(),
            reference.embed_passages(&p).unwrap()
        );
    }
    queries.join().unwrap();
    assert_eq!(helper.fault(), None);
}

#[test]
fn a_helper_with_lanes_that_dies_mid_flight_fails_both_callers_and_starts_again() {
    let dir = ModelDir::new();
    scripted::set_lanes_identity(&dir.0);
    let per_call = scripted::by_lane(scripted::SWALLOW, scripted::SWALLOW);
    let path = scripted::script(&dir.0, "dies", &per_call);
    let helper = Arc::new(
        scripted::start(&path, Duration::from_secs(10))
            .unwrap()
            .with_passage_deadline(Duration::from_secs(10)),
    );
    let pid = scripted::pid(&dir.0, "dies");
    let call = |query: bool| {
        let helper = Arc::clone(&helper);
        thread::spawn(move || {
            if query {
                helper.embed_query("the query").map(|_| ())
            } else {
                helper.embed_passages(&["the passage"]).map(|_| ())
            }
        })
    };
    let passage = call(false);
    let query = call(true);
    let since = Instant::now();
    while scripted::log(&dir.0, "dies").len() < 2 {
        assert!(
            since.elapsed() < Duration::from_secs(3),
            "both requests reach the helper"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        helper.fault(),
        None,
        "nothing looks for an exit while calls are in flight"
    );
    let killed = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    for (lane, call) in [("passage", passage), ("query", query)] {
        let e = call.join().unwrap().unwrap_err();
        assert!(
            e.to_string().ends_with("helper pipe: the helper closed it"),
            "{lane}: {e}"
        );
    }
    assert!(
        since.elapsed() < Duration::from_secs(3),
        "{:?}",
        since.elapsed()
    );
    assert!(helper.fault().is_some());
    assert!(!scripted::alive(pid), "the helper is reaped");
    assert!(
        helper.embed_query("later").is_err(),
        "every later call fails"
    );

    // A new start of the same helper serves again.
    let answers = scripted::by_lane(scripted::ANSWER_ID, scripted::ANSWER_ID);
    let path = scripted::script(&dir.0, "again", &answers);
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert_eq!(helper.embed_passages(&["p"]).unwrap(), [[1.0]]);
    assert_eq!(helper.embed_query("q").unwrap(), [1.0]);
}

#[test]
fn a_helper_with_lanes_whose_answer_names_no_call_in_flight_is_killed() {
    let dir = ModelDir::new();
    scripted::set_lanes_identity(&dir.0);
    let wrong = r#"printf '{"id":999,"count":1,"dim":1}\n\000\000\200\077'"#;
    let path = scripted::script(&dir.0, "wrong", &scripted::by_lane(wrong, wrong));
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    let e = helper.embed_query("q").unwrap_err();
    assert!(
        e.to_string()
            .ends_with("helper pipe: an answer for no call in flight (id Some(999))"),
        "{e}"
    );
    assert!(!scripted::alive(scripted::pid(&dir.0, "wrong")));
    assert!(helper.embed_passages(&["p"]).is_err());
}

#[test]
fn a_call_to_a_helper_that_died_while_idle_is_down_and_never_reaches_it() {
    let dir = ModelDir::new();
    let path = scripted::script(&dir.0, "dies", scripted::ANSWER);
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert_eq!(helper.embed_query("first").unwrap(), [1.0]);
    scripted::kill_idle(scripted::pid(&dir.0, "dies"));
    let e = helper
        .embed_passages(&["Cafe Lumen has a green door."])
        .unwrap_err();
    assert!(matches!(e, EmbedError::Down(_)), "{e}");
    assert_eq!(scripted::log(&dir.0, "dies").len(), 1, "no second request");
}

#[cfg(not(feature = "native"))]
#[test]
fn a_build_without_the_native_feature_refuses_the_engines_weight_file() {
    let dir = ModelDir::new();
    fs::remove_file(dir.0.join(ONNX_FILE)).unwrap();
    fs::write(dir.0.join(ulei_garden::embed::ENGINE_FILE), "x").unwrap();
    for embedder in [None, Some("native")] {
        let mut command = Command::new(EMBED);
        if let Some(e) = embedder {
            command.args(["--embedder", e]);
        }
        command.arg("--model-dir").arg(&dir.0);
        match Helper::start(command, Duration::from_secs(30), CALL_DEADLINE) {
            Err(ulei_garden::embed::pipe::StartError::Refused(reason)) => assert_eq!(
                reason, "--embedder native: this build has no native feature",
                "{embedder:?}"
            ),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("ready without the native feature"),
        }
    }
}

/// A helper killed with SIGKILL that the daemon did not send (the OOM killer, an operator)
/// while a passage was in flight: the call is `Down`, not a fault of the text, so the worker does
/// not count it toward `PASSAGE_TRIES` and three such kills never skip the passage for good
/// (ADR-0086 E12a, finding F3). Both exchanges: one call at a time, and the lanes.
#[test]
fn a_helper_killed_from_outside_during_a_call_is_down_not_a_failure_of_the_text() {
    let dir = ModelDir::new();
    let path = scripted::script(&dir.0, "oom", "kill -KILL $$");
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    let e = helper.embed_passages(&["a long passage"]).unwrap_err();
    assert!(matches!(e, EmbedError::Down(_)), "{e:?}");

    let dir = ModelDir::new();
    scripted::set_lanes_identity(&dir.0);
    let path = scripted::script(&dir.0, "oom", "kill -KILL $$");
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert!(helper.has_lanes());
    let e = helper.embed_passages(&["a long passage"]).unwrap_err();
    assert!(matches!(e, EmbedError::Down(_)), "{e:?}");
}

/// A helper that crashes by itself on a text (here SIGSEGV) or hangs past the deadline stays a
/// failure of that text (`Run`): those are what `PASSAGE_TRIES` is for.
#[test]
fn a_helper_that_crashes_on_a_call_is_a_failure_of_the_text() {
    let dir = ModelDir::new();
    let path = scripted::script(&dir.0, "crash", "kill -SEGV $$");
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    let e = helper.embed_passages(&["a text"]).unwrap_err();
    assert!(matches!(e, EmbedError::Run(_)), "{e:?}");

    let path = scripted::script(&dir.0, "hang", scripted::HANG);
    let helper = scripted::start(&path, Duration::from_millis(300)).unwrap();
    let e = helper.embed_passages(&["a text"]).unwrap_err();
    assert!(matches!(e, EmbedError::Run(_)), "{e:?}");
}
