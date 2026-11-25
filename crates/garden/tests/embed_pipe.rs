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
