//! `ulei_garden::embed::digest::verify` (what `ulei-garden-embed verify` and so the daemon's
//! check run) on a file of the structured layout, sealed or not, against the in-memory parser:
//! the two must agree. verify passes iff the bytes open AND carry their true digest; a digest
//! mismatch is `Corrupt`, never `Io` or `Format` for a version-3 file.
#![no_main]
use std::io::Write;

use libfuzzer_sys::fuzz_target;
use ulei_garden::embed::digest::{self, VerifyError};

fuzz_target!(|data: &[u8]| {
    let [flags, hi, lo, rest @ ..] = data else { return };
    let n = usize::from(u16::from_be_bytes([*hi, *lo])).min(rest.len());
    let (header, blobs) = rest.split_at(n);
    let mut bytes = ulei_fuzz::assemble(header, blobs, 3, flags & 1 != 0);
    if flags & 2 != 0 && bytes.len() > 64 {
        let at = 64 + usize::from(flags >> 2) % (bytes.len() - 64);
        bytes[at] ^= 0x01; // a flipped bit after sealing: must be Corrupt if it opened
    }
    let path = std::env::temp_dir().join(format!("ulei-fuzz-verify-{}.uiw", std::process::id()));
    std::fs::File::create(&path).and_then(|mut f| f.write_all(&bytes)).expect("temp file");
    let opens = ulei_infer::weights::file::WeightFile::read(&bytes[..]).is_ok();
    let sealed_ok = bytes[32..64] == ulei_fuzz::content_digest(&bytes) && bytes[32..64] != [0u8; 32];
    match digest::verify(&path) {
        Ok(v) => {
            assert!(opens && sealed_ok, "verify passed a file that does not open or is not sealed");
            assert_eq!(v.bytes, bytes.len() as u64);
        }
        Err(VerifyError::Corrupt(_)) => assert!(!(opens && sealed_ok), "verify refused a sound file"),
        Err(e) => panic!("version-3 file answered {e:?}, expected Ok or Corrupt"),
    }
    let _ = std::fs::remove_file(&path);
});
