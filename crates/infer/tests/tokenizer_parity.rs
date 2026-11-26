//! The tokenizer of a real weight file against the reference's ids (ADR-0087 E2), and random
//! text through it. Ignored: needs `ULEI_INFER_TOKENIZER_FILE` (a weight file with its tokenizer
//! section, or a weight file of the tokenizer alone) and, for the parity test,
//! `ULEI_INFER_TOKENIZER_CASES`: JSON lines `{"text": ..., "ids": [...]}` of the reference (the
//! `transformers` tokenizer of the checkpoint with truncation at the file's `max_tokens`).
//! Run at opt-level 3.

#![allow(missing_docs, clippy::unwrap_used)]

use std::io::BufRead;
use std::path::PathBuf;

use serde_json::Value;
use ulei_infer::text::Tokenizer;
use ulei_infer::weights::file::WeightFile;

fn weight_file() -> Option<WeightFile> {
    let path = std::env::var_os("ULEI_INFER_TOKENIZER_FILE").map(PathBuf::from)?;
    Some(WeightFile::open(&path).unwrap())
}

#[test]
#[ignore = "needs ULEI_INFER_TOKENIZER_FILE and ULEI_INFER_TOKENIZER_CASES"]
fn ids_equal_the_reference() {
    let (Some(file), Some(cases)) = (
        weight_file(),
        std::env::var_os("ULEI_INFER_TOKENIZER_CASES").map(PathBuf::from),
    ) else {
        panic!("set ULEI_INFER_TOKENIZER_FILE and ULEI_INFER_TOKENIZER_CASES");
    };
    let t = Tokenizer::from_file(&file).unwrap();
    let reader = std::io::BufReader::new(std::fs::File::open(&cases).unwrap());
    let (mut texts, mut tokens, mut bad) = (0usize, 0usize, Vec::new());
    for line in reader.lines() {
        let case: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let text = case["text"].as_str().unwrap();
        let want: Vec<u32> = case["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| u32::try_from(v.as_u64().unwrap()).unwrap())
            .collect();
        let got = t.encode(text).unwrap();
        texts += 1;
        tokens += want.len();
        if got != want {
            bad.push(text.chars().take(60).collect::<String>());
        }
    }
    eprintln!(
        "texts {texts}, reference tokens {tokens}, mismatching {}",
        bad.len()
    );
    assert!(
        bad.is_empty(),
        "first mismatches: {:?}",
        &bad[..bad.len().min(5)]
    );
}

#[test]
#[ignore = "needs ULEI_INFER_TOKENIZER_FILE"]
fn random_text_stays_in_bounds() {
    let file = weight_file().expect("set ULEI_INFER_TOKENIZER_FILE");
    let t = Tokenizer::from_file(&file).unwrap();
    let rounds: usize = std::env::var("ULEI_INFER_FUZZ_ROUNDS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(100_000);
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut tokens = 0usize;
    for _ in 0..rounds {
        let len = next() % 300;
        let text: String = (0..len)
            .map(|_| {
                let r = next();
                // Mostly the planes the vocabulary covers, with whitespace and special tokens.
                let cp = match r % 8 {
                    0 => u32::try_from(r >> 40).unwrap() % 0x11_0000,
                    1 => [0x20, 0x09, 0x0A, 0x3000, 0x2581, 0x200D, 0x301][(r >> 8) as usize % 7],
                    2 => 0x0400 + u32::try_from(r >> 40).unwrap() % 0x100,
                    3 => 0x1F300 + u32::try_from(r >> 40).unwrap() % 0x700,
                    4 => 0x4E00 + u32::try_from(r >> 40).unwrap() % 0x5200,
                    _ => 0x20 + u32::try_from(r >> 40).unwrap() % 0x5F,
                };
                char::from_u32(cp).unwrap_or('\u{FFFD}')
            })
            .collect();
        let ids = t.encode(&text).unwrap();
        assert!(ids.len() >= 2 && ids.len() <= t.max_tokens());
        assert!(ids.iter().all(|&i| (i as usize) < t.vocab_size()));
        tokens += ids.len();
    }
    eprintln!("{rounds} random texts, {tokens} ids, all in bounds");
}
