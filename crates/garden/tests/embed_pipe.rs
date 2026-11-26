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
/// (ADR-0087 E12a, finding F3). Both exchanges: one call at a time, and the lanes.
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

/// The daemon's peak memory, kB (`VmHWM` of this test process).
fn peak_kb() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap()
}

/// A helper that writes an endless line (no newline) as its ready line or as an answer header:
/// the daemon must refuse it after a bounded read, not hold all of it in memory until the
/// deadline (a broken helper could take a 1 GB host's memory with it; ADR-0087 E12a, F2).
#[test]
fn a_helper_line_without_end_is_refused_without_reading_it_all() {
    const FLOOD: &str = "head -c 268435456 /dev/zero | tr '\\000' a";
    let before = peak_kb();
    let dir = ModelDir::new();
    let path = scripted::script(&dir.0, "flood", FLOOD);
    let helper = scripted::start(&path, Duration::from_secs(20)).unwrap();
    assert!(helper.embed_query("x").is_err());
    let flood_ready = dir.0.join("flood-ready.sh");
    let wrote = Command::new("/bin/sh")
        .args([
            "-c",
            &format!("printf '#!/bin/sh\\n{FLOOD}\\n' > \"$1\" && chmod 700 \"$1\""),
            "sh",
        ])
        .arg(&flood_ready)
        .status()
        .unwrap();
    assert!(wrote.success());
    assert!(
        Helper::start(
            Command::new(&flood_ready),
            Duration::from_secs(20),
            CALL_DEADLINE
        )
        .is_err()
    );
    let grew = peak_kb().saturating_sub(before);
    assert!(
        grew < 64 * 1024,
        "the daemon's peak grew by {grew} kB reading a 256 MiB line"
    );
}

/// The single-call helper side (`pipe::serve`, what the ONNX backend runs) frames its answers:
/// the ready line, then a header and exactly count x dim x 4 bytes per request, and a refusal
/// for a bad request with the pipe still in step. A surviving mutant (`serve` returning at once)
/// showed no test ran it.
#[test]
fn the_single_call_helper_side_frames_every_answer() {
    let embedder = HashEmbedder::new(4);
    let identity = Identity {
        name: "hash".into(),
        model_sha256: ABC.into(),
        tokenizer_sha256: EMPTY.into(),
        dim: 4,
    };
    let input = b"{\"kind\":\"query\",\"texts\":[\"a\"]}\nnot json\n{\"kind\":\"passage\",\"texts\":[\"b\",\"c\"]}\n";
    let mut out = Vec::new();
    ulei_garden::embed::pipe::serve(&embedder, &identity, &input[..], &mut out).unwrap();
    // Walk the frames: each header line, then exactly its vectors' bytes.
    let mut at = 0;
    let mut headers = Vec::new();
    while at < out.len() {
        let nl = at + out[at..].iter().position(|&b| b == b'\n').unwrap();
        let header: serde_json::Value = serde_json::from_slice(&out[at..nl]).unwrap();
        at = nl + 1;
        let count = header
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        at += usize::try_from(count).unwrap() * 4 * 4;
        headers.push(header);
    }
    assert_eq!(at, out.len(), "the answers fill the output exactly");
    assert_eq!(
        headers.len(),
        4,
        "ready line and three answers: {headers:?}"
    );
    assert_eq!(headers[0]["dim"], 4);
    assert!(
        headers[0].get("lanes").is_none(),
        "the single-call side announces no lanes"
    );
    assert_eq!(headers[1], serde_json::json!({"count": 1, "dim": 4}));
    assert!(
        headers[2]["error"]
            .as_str()
            .unwrap()
            .starts_with("request: ")
    );
    assert_eq!(headers[3], serde_json::json!({"count": 2, "dim": 4}));
}

/// A header of exactly 64 KiB with its newline (a long refusal) is read whole; the bound only
/// refuses a longer line (F2's bound is not a smaller one, and a line that ends at the bound is
/// not cut).
#[test]
fn a_helper_header_up_to_the_bound_is_read_whole() {
    let dir = ModelDir::new();
    // {"error":"xxx…"}\n is 64 KiB in all.
    let reason = "x".repeat(64 * 1024 - r#"{"error":""}"#.len() - 1);
    fs::write(
        dir.0.join("long-refusal"),
        format!("{{\"error\":\"{reason}\"}}\n"),
    )
    .unwrap();
    let path = scripted::script(
        &dir.0,
        "long",
        &format!("cat '{}'", dir.0.join("long-refusal").display()),
    );
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    match helper.embed_query("q") {
        Err(EmbedError::Run(r)) => assert_eq!(r, reason, "the refusal's reason, whole"),
        other => panic!("{other:?}"),
    }
    assert!(
        helper.fault().is_none(),
        "a refusal leaves the pipe in step"
    );
}

/// An answer of the right count but another width is a broken pipe, never vectors of the
/// wrong width (a surviving mutant accepted a header unless both were wrong).
#[test]
fn an_answer_of_another_width_is_refused_even_with_the_right_count() {
    let dir = ModelDir::new();
    let path = scripted::script(
        &dir.0,
        "wide",
        r#"printf '{"count":1,"dim":2}\n\000\000\200\077\000\000\200\077'"#,
    );
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert!(helper.embed_query("q").is_err());
    let dir = ModelDir::new();
    let path = scripted::script(
        &dir.0,
        "more",
        r#"printf '{"count":2,"dim":1}\n\000\000\200\077\000\000\200\077'"#,
    );
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert!(helper.embed_query("q").is_err());
}

/// A helper with lanes that died while idle reports its exit through `fault` (a surviving
/// mutant inverted the idle test for the lanes).
#[test]
fn a_helper_with_lanes_that_died_while_idle_reports_its_exit() {
    let dir = ModelDir::new();
    scripted::set_lanes_identity(&dir.0);
    let per_call = scripted::by_lane(scripted::ANSWER_ID, scripted::ANSWER_ID);
    let path = scripted::script(&dir.0, "idle-lanes", &per_call);
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert!(helper.has_lanes());
    assert_eq!(helper.embed_query("q").unwrap(), [1.0]);
    assert_eq!(helper.fault(), None);
    scripted::kill_idle(scripted::pid(&dir.0, "idle-lanes"));
    let asked = Instant::now();
    while helper.fault().is_none() && asked.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        helper
            .fault()
            .is_some_and(|f| f.starts_with("the helper exited"))
    );
}

/// The ready line of a helper with a vector length of `dim`.
fn identity_of_dim(dim: &str) -> String {
    let digest: String = std::iter::repeat_n('a', 64).collect();
    format!(
        "{{\"name\":\"scripted\",\"model_sha256\":\"{digest}\",\"tokenizer_sha256\":\"{digest}\",\"dim\":{dim}}}\n"
    )
}

/// The vector length a helper declares bounds what the daemon allocates for every answer: a
/// length of 0 or above 4096 is a malformed ready line, as any other garbage on it, instead of
/// an allocation of gigabytes or an overflow of the answer's size (review E13r, finding 8).
#[test]
fn a_ready_line_with_a_vector_length_out_of_bounds_is_malformed() {
    use ulei_garden::embed::pipe::StartError;
    for dim in ["0", "4097", "100000000", "4611686018427387904"] {
        let dir = ModelDir::new();
        fs::write(dir.0.join("identity"), identity_of_dim(dim)).unwrap();
        let path = scripted::script(&dir.0, "wide", scripted::ANSWER);
        match scripted::start(&path, CALL_DEADLINE) {
            Err(StartError::Malformed) => {}
            Err(e) => panic!("dim {dim}: {e}"),
            Ok(_) => panic!("dim {dim}: ready"),
        }
        assert!(
            !scripted::alive(scripted::pid(&dir.0, "wide")),
            "dim {dim}: the helper is reaped"
        );
    }
    let dir = ModelDir::new();
    fs::write(dir.0.join("identity"), identity_of_dim("4096")).unwrap();
    let path = scripted::script(&dir.0, "widest", scripted::ANSWER);
    let helper = scripted::start(&path, CALL_DEADLINE).unwrap();
    assert_eq!(helper.dim(), 4096);
}

/// One call at a time: a call that misses its deadline leaves the helper unusable before it
/// hands the pipe on, so the passage that waited for the pipe fails (or gets its own vector)
/// and never receives the late answer of the query, which the worker would store as the
/// passage's vector (review E13r, finding 1).
#[test]
fn a_late_answer_of_a_timed_out_call_never_reaches_the_next_caller() {
    let dir = ModelDir::new();
    let per_call = format!(
        "case \"$line\" in *slow*) sleep 0.3; {} ;; *) {} ;; esac",
        scripted::ANSWER,
        scripted::OPPOSITE
    );
    let path = scripted::script(&dir.0, "late", &per_call);
    let helper = Arc::new(scripted::start(&path, Duration::from_millis(200)).unwrap());
    assert!(!helper.has_lanes());
    let h = Arc::clone(&helper);
    let query = thread::spawn(move || h.embed_query("slow query text"));
    thread::sleep(Duration::from_millis(50));
    let h = Arc::clone(&helper);
    let passage = thread::spawn(move || h.embed_passages(&["fact text"]));
    let query = query.join().unwrap();
    let passage = passage.join().unwrap();
    assert!(query.is_err(), "{query:?}");
    match passage {
        Err(_) => {}
        Ok(v) => assert_eq!(v, [[-1.0]], "the passage got the query's answer"),
    }
    assert!(helper.fault().is_some());
}

/// The same with lanes: a query that missed its deadline leaves the helper unusable before the
/// next query takes the lane, so a late answer left on the lane is never taken as its own.
#[test]
fn with_lanes_a_late_answer_of_a_timed_out_query_never_reaches_the_next_query() {
    let dir = ModelDir::new();
    scripted::set_lanes_identity(&dir.0);
    let opposite = r#"printf '{"id":%s,"count":1,"dim":1}\n\000\000\200\277' "$id""#;
    let per_call = format!(
        "{}; case \"$line\" in *slow*) {} ;; *) {opposite} ;; esac",
        scripted::ID,
        scripted::later("0.25")
    );
    let path = scripted::script(&dir.0, "late-lanes", &per_call);
    let helper = Arc::new(scripted::start(&path, Duration::from_millis(200)).unwrap());
    assert!(helper.has_lanes());
    let started = Instant::now();
    let h = Arc::clone(&helper);
    let first = thread::spawn(move || h.embed_query("slow query text"));
    // After the late answer came in (250 ms), before the grace after the deadline ends (400 ms).
    thread::sleep(Duration::from_millis(320).saturating_sub(started.elapsed()));
    let second = helper.embed_query("other query");
    let first = first.join().unwrap();
    assert!(first.is_err(), "{first:?}");
    match second {
        Err(_) => {}
        Ok(v) => assert_eq!(v, [-1.0], "the second query got the first one's answer"),
    }
}
