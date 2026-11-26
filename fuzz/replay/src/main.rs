//! `replay_helper FILE`: copies FILE to stdout and closes it, then reads stdin to its end; stands
//! in for `ulei-garden-embed` so the daemon's pipe code reads exactly the bytes the fuzzer
//! chose. Keeping stdin open until the daemon closes it means the daemon's writes never meet a
//! closed pipe (a libFuzzer process does not ignore SIGPIPE as a Rust `main` does), and closing
//! stdout at once makes every read after the given bytes end without waiting for a deadline.
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;

fn main() {
    let path = std::env::args_os().nth(1).expect("FILE");
    let bytes = std::fs::read(path).unwrap_or_default();
    // SAFETY: descriptor 1 is this process's stdout, open at start; nothing else uses it after
    // this File takes it, and dropping the File closes it.
    let mut out = unsafe { File::from_raw_fd(1) };
    let _ = out.write_all(&bytes);
    drop(out);
    let mut sink = Vec::new();
    let _ = std::io::stdin().lock().read_to_end(&mut sink);
}
