//! The weight file read through its read-only `mmap` (ADR-0087 E4): an on-disk round trip, and
//! files that are truncated, empty or not files refused with an error.

use std::path::PathBuf;

use serde_json::json;
use ulei_infer::Error;
use ulei_infer::weights::file::{Data, Entry, WeightFile, write};

fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("test directory");
    d
}

fn sample() -> Vec<u8> {
    let f = [1.5f32, -0.0, f32::MIN_POSITIVE, 3.0e38, f32::NAN];
    let i = [-128i8, 127, 0, 5, -3];
    let u = [7u32, 0, u32::MAX];
    let tensors = [
        Entry {
            name: "w".into(),
            shape: vec![5],
            data: Data::I8(&i),
        },
        Entry {
            name: "a".into(),
            shape: vec![5],
            data: Data::F32(&f),
        },
        Entry {
            name: "map".into(),
            shape: vec![3],
            data: Data::U32(&u),
        },
    ];
    let mut out = Vec::new();
    write(&mut out, &json!({"m": 1}), &tensors).expect("write the sample");
    out
}

#[test]
fn mapped_file_reads_in_place() {
    let d = dir("mapped_file_reads_in_place");
    let path = d.join("w.uiw");
    let bytes = sample();
    std::fs::write(&path, &bytes).unwrap();
    let file = WeightFile::open(&path).unwrap();
    assert_eq!(file.meta(), &json!({"m": 1}));
    let a = file.f32("a").unwrap();
    assert_eq!(a.shape, &[5]);
    assert_eq!(a.data[0].to_bits(), 1.5f32.to_bits());
    assert_eq!(a.data[1].to_bits(), (-0.0f32).to_bits());
    assert!(a.data[4].is_nan());
    assert_eq!(file.i8("w").unwrap().data, &[-128, 127, 0, 5, -3]);
    assert_eq!(file.u32("map").unwrap().data, &[7, 0, u32::MAX]);
    // In place: every blob is 64-byte aligned in memory (the mapping is page aligned).
    for p in [
        a.data.as_ptr().cast::<u8>(),
        file.i8("w").unwrap().data.as_ptr().cast(),
        file.u32("map").unwrap().data.as_ptr().cast(),
    ] {
        assert_eq!(p.align_offset(64), 0);
    }
    // Same tensors as the in-memory reader.
    let mem = WeightFile::read(bytes.as_slice()).unwrap();
    assert_eq!(mem.names(), file.names());
    assert_eq!(mem.u32("map").unwrap(), file.u32("map").unwrap());
    // Replacing the file by a rename leaves the mapping's bytes unchanged.
    let next = d.join("w.uiw.part");
    std::fs::write(&next, b"not a weight file").unwrap();
    std::fs::rename(&next, &path).unwrap();
    assert_eq!(file.i8("w").unwrap().data, &[-128, 127, 0, 5, -3]);
    drop(file);
    assert!(WeightFile::open(&path).is_err());
}

#[test]
fn bad_files_on_disk_are_refused() {
    let d = dir("bad_files_on_disk_are_refused");
    let bytes = sample();
    for k in [0, 1, 23, 24, 40, bytes.len() / 2, bytes.len() - 1] {
        let path = d.join(format!("cut-{k}.uiw"));
        std::fs::write(&path, &bytes[..k]).unwrap();
        assert!(
            matches!(WeightFile::open(&path), Err(Error::Weights(_))),
            "{k} bytes"
        );
    }
    let mut longer = bytes.clone();
    longer.extend_from_slice(&[0; 4096]);
    let path = d.join("longer.uiw");
    std::fs::write(&path, &longer).unwrap();
    assert!(matches!(WeightFile::open(&path), Err(Error::Weights(_))));
    assert!(matches!(
        WeightFile::open(&d.join("missing.uiw")),
        Err(Error::Io(_))
    ));
    // A directory is not a file.
    assert!(WeightFile::open(&d).is_err());
}
