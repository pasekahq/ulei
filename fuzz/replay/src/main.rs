//! `replay_helper FILE`: copies FILE to stdout, then exits; stands in for `ulei-garden-embed`
//! so the daemon's pipe code reads exactly the bytes the fuzzer chose.
fn main() {
    let path = std::env::args_os().nth(1).expect("FILE");
    let bytes = std::fs::read(path).unwrap_or_default();
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&bytes);
    let _ = out.flush();
}
