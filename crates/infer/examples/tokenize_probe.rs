//! Tokenizer probe of ADR-0086 E2 (feature `bert`); a diagnostic, not part of the library.
//!
//! ```text
//! tokenize_probe --weights FILE [--embed VECTORS_OUT] [--rounds N] < TEXTS > IDS
//! tokenize_probe --tokenizer-json FILE [--pipeline whitespace-split|collapse-spaces]
//!                [--max-tokens N] [--rounds N] < TEXTS > IDS
//! ```
//!
//! `TEXTS` holds one JSON string per line (the text as the model gets it, prefix included).
//! For each line the probe writes one JSON line `{"ids": [...], "ns": t}` with the token ids
//! and the median encode time over `--rounds` runs (default 1). The tokenizer comes from the
//! weight file's section, or is built in memory from a `tokenizer.json` (no model needed).
//! With `--embed` (weight file only) it also embeds each text through the encoder and writes the
//! pooled vectors to `VECTORS_OUT` as little-endian `f32`, in input order; the last stderr line
//! then sums the encode and embed times.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

use serde_json::json;
use ulei_infer::bert::Encoder;
use ulei_infer::text::{Pipeline, Tokenizer, convert};
use ulei_infer::weights::file::WeightFile;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut weights, mut json_path, mut embed) = (None, None, None);
    let mut pipeline = Pipeline::WhitespaceSplit;
    let mut max_tokens = 512;
    let mut rounds = 1usize;
    while let Some(flag) = args.next() {
        let value = args.next().expect("every flag takes a value");
        match flag.as_str() {
            "--weights" => weights = Some(PathBuf::from(value)),
            "--tokenizer-json" => json_path = Some(PathBuf::from(value)),
            "--embed" => embed = Some(PathBuf::from(value)),
            "--pipeline" => pipeline = Pipeline::parse(&value).expect("pipeline"),
            "--max-tokens" => max_tokens = value.parse().expect("max tokens"),
            "--rounds" => rounds = value.parse::<usize>().expect("rounds").max(1),
            other => panic!("unknown flag {other}"),
        }
    }
    let file = match (&weights, &json_path) {
        (Some(w), None) => WeightFile::open(w).expect("open the weight file"),
        (None, Some(j)) => {
            let text = std::fs::read_to_string(j).expect("read tokenizer.json");
            let section = convert::section(&text, pipeline, max_tokens).expect("tokenizer.json");
            let mut bytes = Vec::new();
            section.write_alone(&mut bytes).expect("write the section");
            WeightFile::read(bytes.as_slice()).expect("read the section")
        }
        _ => panic!("give --weights or --tokenizer-json"),
    };
    let encoder = embed
        .as_ref()
        .map(|_| Encoder::from_file(&file).expect("bind the encoder"));
    let tokenizer = Tokenizer::from_file(&file).expect("bind the tokenizer");
    let mut vectors = Vec::new();
    let (mut encode_ns, mut embed_ns, mut texts, mut tokens) = (0u128, 0u128, 0usize, 0usize);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("read a line");
        let text: String = serde_json::from_str(&line).expect("a JSON string per line");
        let mut times = Vec::with_capacity(rounds);
        let mut ids = Vec::new();
        for _ in 0..rounds {
            let t = Instant::now();
            ids = tokenizer.encode(&text).expect("encode");
            times.push(t.elapsed().as_nanos());
        }
        times.sort_unstable();
        let ns = times[times.len() / 2];
        encode_ns += ns;
        texts += 1;
        tokens += ids.len();
        writeln!(out, "{}", json!({"ids": ids, "ns": ns})).expect("write");
        if let Some(enc) = &encoder {
            let t = Instant::now();
            let v = enc.embed(&ids).expect("embed");
            embed_ns += t.elapsed().as_nanos();
            vectors.extend(v.iter().flat_map(|x| x.to_le_bytes()));
        }
    }
    out.flush().expect("flush");
    if let Some(path) = embed {
        std::fs::write(path, vectors).expect("write the vectors");
    }
    eprintln!(
        "{}",
        json!({"texts": texts, "tokens": tokens, "encode_ns": encode_ns, "embed_ns": embed_ns})
    );
}
