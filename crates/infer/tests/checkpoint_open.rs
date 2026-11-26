//! `Checkpoint::open` checks every tensor's end against the file's length before it allocates
//! the tensor's buffer (E13r F4: an 85-byte file whose header named 1 TiB aborted the reader).

use std::path::PathBuf;

use ulei_infer::weights::Checkpoint;

/// A `safetensors` file of the header `json` and `data`.
fn raw(json: &str, data: &[u8]) -> Vec<u8> {
    let mut out = (json.len() as u64).to_le_bytes().to_vec();
    out.extend_from_slice(json.as_bytes());
    out.extend_from_slice(data);
    out
}

#[test]
fn a_header_naming_more_than_the_file_is_refused_before_allocating() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("checkpoint_open");
    std::fs::create_dir_all(&dir).unwrap();
    let (huge, fine) = (dir.join("huge.safetensors"), dir.join("fine.safetensors"));
    let json = r#"{"a":{"dtype":"F32","shape":[274877906944],"data_offsets":[0,1099511627776]}}"#;
    std::fs::write(&huge, raw(json, &[0u8; 64])).unwrap();
    let mut data = 1.0f32.to_le_bytes().to_vec();
    data.extend_from_slice(&2.0f32.to_le_bytes());
    let json = r#"{"a":{"dtype":"F32","shape":[2],"data_offsets":[0,8]}}"#;
    std::fs::write(&fine, raw(json, &data)).unwrap();
    let e = Checkpoint::open(&huge).unwrap_err();
    assert!(e.to_string().contains("truncated"), "{e}");
    let c = Checkpoint::open(&fine).unwrap();
    assert_eq!(c.get("a").unwrap().data, [1.0, 2.0]);
    // One byte short of the data is truncated too.
    let short = dir.join("short.safetensors");
    std::fs::write(&short, raw(json, &data[..7])).unwrap();
    assert!(Checkpoint::open(&short).is_err());
}
