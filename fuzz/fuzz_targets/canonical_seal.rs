//! `file::canonical` over every split of its input across reads, and `file::seal`: the sink must
//! get exactly the input with bytes 32..64 read as zeros, whatever the chunk sizes; sealing then
//! changes only those 32 bytes, so the canonical stream is unchanged.
#![no_main]
use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use ulei_infer::weights::file;

fuzz_target!(|data: &[u8]| {
    let [k, rest @ ..] = data else { return };
    let (sizes, body) = rest.split_at(usize::from(*k).min(rest.len()));
    let mut got = Vec::new();
    let n = file::canonical(
        ulei_fuzz::Chunky { data: body, sizes, at: 0, turn: 0 },
        |c| got.extend_from_slice(c),
    )
    .expect("in-memory read");
    assert_eq!(n as usize, body.len(), "canonical returns the bytes read");
    let mut want = body.to_vec();
    let hi = file::DIGEST.end.min(want.len());
    if want.len() > file::DIGEST.start {
        want[file::DIGEST.start..hi].fill(0);
    }
    assert_eq!(got, want, "canonical zeroes exactly bytes 32..64");
    if body.len() >= file::DIGEST.end {
        let digest: [u8; 32] = body[..32].try_into().unwrap();
        let mut c = Cursor::new(body.to_vec());
        file::seal(&mut c, &digest).expect("seal into memory");
        let sealed = c.into_inner();
        assert_eq!(&sealed[file::DIGEST], &digest[..], "seal writes the digest");
        assert_eq!(sealed.len(), body.len(), "seal does not grow the file");
        let mut again = Vec::new();
        file::canonical(Cursor::new(&sealed), |c| again.extend_from_slice(c)).unwrap();
        assert_eq!(again, want, "sealing leaves the canonical bytes unchanged");
    }
});
