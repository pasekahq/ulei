//! The daemon's side of the pipe (embed/pipe.rs `Helper`): a helper process that writes the
//! fuzz input to its stdout and exits (the replay helper of fuzz/replay), then a query and
//! a passage call. The daemon must refuse a bad ready line as a `StartError`, and fail a call on
//! a malformed, mismatched or cut answer, without a panic, an unbounded allocation or waiting
//! past its deadline. The input's first line should be a ready line (the seeds are), so the
//! answers behind it get exercised too.
#![no_main]
use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use ulei_garden::embed::Embedder;
use ulei_garden::embed::pipe::Helper;

fuzz_target!(init: {
    // The daemon is a Rust main, which starts with SIGPIPE ignored, so a write to a helper that
    // died is an error, not a signal; a libFuzzer process starts with the default action.
    // SAFETY: setting a signal's disposition to SIG_IGN at start, before any thread exists.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
}, |data: &[u8]| {
    let replay = std::env::var_os("ULEI_FUZZ_REPLAY_HELPER").expect("ULEI_FUZZ_REPLAY_HELPER");
    let path = std::env::temp_dir().join(format!("ulei-fuzz-replay-{}.bin", std::process::id()));
    std::fs::File::create(&path).and_then(|mut f| f.write_all(data)).expect("temp file");
    let mut cmd = Command::new(replay);
    cmd.arg(&path);
    let t = Instant::now();
    if let Ok(helper) = Helper::start(cmd, Duration::from_secs(5), Duration::from_secs(2)) {
        let helper = helper.with_passage_deadline(Duration::from_secs(2));
        let dim = helper.dim();
        if let Ok(v) = helper.embed_query("q") {
            assert_eq!(v.len(), dim, "query vector of the announced width");
        }
        if let Ok(vs) = helper.embed_passages(&["a", "b"]) {
            assert_eq!(vs.len(), 2, "one vector per passage");
            assert!(vs.iter().all(|v| v.len() == dim));
        }
        let _ = helper.fault();
    }
    // Start (5 s) and two calls (2 s each) bound the run; the replay helper exits at once.
    assert!(t.elapsed() < Duration::from_secs(12), "pipe exceeded its deadlines: {:?}", t.elapsed());
    let _ = std::fs::remove_file(&path);
});
