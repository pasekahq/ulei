//! Shared bodies of the fuzz targets. Each target is: arbitrary bytes in, every public entry
//! point of the parser under test called, and only `Ok` or `Err` out. A panic, an abort, a
//! sanitizer report, an allocation above libFuzzer's `-malloc_limit_mb`, or an input that runs
//! past `-timeout` is a finding. Properties beyond "no crash" are asserted where the code
//! documents them (each assert names the documented claim).

use std::io::{Cursor, Read};
use std::sync::OnceLock;

use ulei_infer::bert::Encoder;
use ulei_infer::text::Tokenizer;
use ulei_infer::weights::file::{self, WeightFile};

/// Texts every model-level target encodes once a file opens: empty, ASCII, Cyrillic with a
/// combining mark, an emoji, controls, and one long run without spaces.
pub const PROBES: &[&str] = &[
    "",
    "query: a",
    "passage: Привет, мир — ёлка\u{301}",
    "😄\u{200d}👍 \u{feff}x\u{0}y",
    "\t\n\r  \u{a0}\u{3000}",
];

/// Opens `bytes` as a weight file and drives every accessor, the tokenizer and the encoder.
pub fn exercise_weight_file(bytes: &[u8]) {
    let Ok(file) = WeightFile::read(bytes) else {
        return;
    };
    let _ = file.digest();
    let _ = file.meta();
    for name in file.names() {
        // Each accessor is checked at open for range and alignment (module docs): Ok or Err.
        if let Ok(v) = file.f32(name) {
            assert_eq!(v.data.len(), v.shape.iter().product::<usize>(), "f32 view of {name}");
        }
        if let Ok(v) = file.i8(name) {
            assert_eq!(v.data.len(), v.shape.iter().product::<usize>(), "i8 view of {name}");
        }
        if let Ok(v) = file.u8(name) {
            assert_eq!(v.data.len(), v.shape.iter().product::<usize>(), "u8 view of {name}");
        }
        if let Ok(v) = file.u32(name) {
            assert_eq!(v.data.len(), v.shape.iter().product::<usize>(), "u32 view of {name}");
        }
        if let Ok(v) = file.f64(name) {
            assert_eq!(v.data.len(), v.shape.iter().product::<usize>(), "f64 view of {name}");
        }
    }
    if let Ok(tok) = Tokenizer::from_file(&file) {
        for text in PROBES {
            check_ids(&tok, text);
        }
    }
    if let Ok(encoder) = Encoder::from_file(&file) {
        for text in &PROBES[..3] {
            if let Ok(v) = encoder.embed_text(text) {
                // The pooled vector has the model's width (bert::Config).
                assert_eq!(v.len(), encoder.config().hidden_size, "pooled width");
            }
        }
    }
}

/// `Tokenizer::encode`'s documented shape: `<s>`, at most `max_tokens - 2` pieces, `</s>`; ids
/// inside the vocabulary; the same ids twice.
pub fn check_ids(tok: &Tokenizer<'_>, text: &str) {
    let first = tok.encode(text);
    let again = tok.encode(text);
    match (&first, &again) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a, b, "encode is deterministic");
            assert!(a.len() >= 2, "<s> and </s> at least: {a:?}");
            assert!(a.len() <= tok.max_tokens(), "{} ids > max_tokens {}", a.len(), tok.max_tokens());
            assert!(
                a.iter().all(|&id| (id as usize) < tok.vocab_size()),
                "an id outside the vocabulary of {}: {a:?}",
                tok.vocab_size()
            );
        }
        (Err(_), Err(_)) => {}
        _ => panic!("encode not deterministic: {first:?} then {again:?}"),
    }
}

/// A weight file built around `header` and `data`: a preamble whose lengths are right, so the
/// fuzzer's mutations reach the header and the tensor table instead of the length checks.
/// `version` replaces the format version; `seal` writes the true content digest.
pub fn assemble(header: &[u8], data: &[u8], version: u32, seal: bool) -> Vec<u8> {
    let pad = (64 - header.len() % 64) % 64;
    let total = 64 + header.len() + pad + data.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&file::MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(header.len() as u64).to_le_bytes());
    out.extend_from_slice(&(total as u64).to_le_bytes());
    out.extend_from_slice(&[0u8; 32]);
    out.extend_from_slice(header);
    out.extend(std::iter::repeat_n(0u8, pad));
    out.extend_from_slice(data);
    if seal {
        let digest = content_digest(&out);
        out[file::DIGEST].copy_from_slice(&digest);
    }
    out
}

/// SHA-256 of `bytes` with the digest's 32 bytes read as zeros, through `file::canonical`.
pub fn content_digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let n = file::canonical(Cursor::new(bytes), |chunk| h.update(chunk)).expect("in-memory read");
    assert_eq!(n, bytes.len() as u64, "canonical reads every byte");
    h.finalize().into()
}

/// The tokenizer-only weight file of `ULEI_FUZZ_TOKENIZER_FILE` (the fuzz job), loaded once.
pub fn tokenizer_bytes() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let path = std::env::var_os("ULEI_FUZZ_TOKENIZER_FILE").expect("ULEI_FUZZ_TOKENIZER_FILE");
        let mut v = Vec::new();
        std::fs::File::open(path).and_then(|mut f| f.read_to_end(&mut v)).expect("tokenizer file");
        v
    })
}

/// The real tokenizer over the bytes of [`tokenizer_bytes`], opened once.
pub fn real_tokenizer() -> &'static Tokenizer<'static> {
    // Held by statics, not leaked: LeakSanitizer counts what globals reach as alive (a leaked
    // Box failed the first run at exit).
    static FILE: OnceLock<WeightFile> = OnceLock::new();
    static TOK: OnceLock<Tokenizer<'static>> = OnceLock::new();
    TOK.get_or_init(|| {
        let file = FILE.get_or_init(|| WeightFile::read(tokenizer_bytes()).expect("tokenizer file opens"));
        Tokenizer::from_file(file).expect("tokenizer section reads")
    })
}

/// A reader that returns the bytes of `data` in chunks whose sizes `sizes` picks (1 to 256),
/// so a parser meets every split of its input across reads.
pub struct Chunky<'a> {
    pub data: &'a [u8],
    pub sizes: &'a [u8],
    pub at: usize,
    pub turn: usize,
}

impl Read for Chunky<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        let want = if self.sizes.is_empty() {
            buf.len()
        } else {
            usize::from(self.sizes[self.turn % self.sizes.len()]) + 1
        };
        self.turn += 1;
        let n = want.min(buf.len()).min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// The `tokenizer.json` of [`crafted_model`]: an empty character map (no normalisation), the
/// XLM-R specials and a handful of pieces; 14 ids.
const CRAFTED_TOKENIZER: &str = r#"{
  "added_tokens": [
    {"id": 0, "content": "<s>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
    {"id": 1, "content": "<pad>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
    {"id": 2, "content": "</s>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
    {"id": 3, "content": "<unk>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
    {"id": 13, "content": "<mask>", "single_word": false, "lstrip": true, "rstrip": false, "normalized": false, "special": true}
  ],
  "normalizer": {"type": "Precompiled", "precompiled_charsmap": "AAAAAA=="},
  "pre_tokenizer": {"type": "Metaspace", "replacement": "▁", "add_prefix_space": true},
  "post_processor": {"type": "RobertaProcessing", "sep": ["</s>", 2], "cls": ["<s>", 0]},
  "model": {"type": "Unigram", "unk_id": 3, "byte_fallback": false, "vocab": [
    ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0],
    ["▁", -2.0], ["▁a", -1.0], ["a", -3.0], ["b", -3.0], ["c", -3.0],
    ["▁q", -1.5], ["y", -3.0], ["e", -3.0], ["▁p", -2.5], ["<mask>", 0.0]]}
}"#;

/// The tokenizer section of [`CRAFTED_TOKENIZER`] (8 tokens at most), built once.
fn crafted_section() -> &'static ulei_infer::text::convert::Section {
    static SECTION: OnceLock<ulei_infer::text::convert::Section> = OnceLock::new();
    SECTION.get_or_init(|| {
        ulei_infer::text::convert::section(CRAFTED_TOKENIZER, ulei_infer::text::Pipeline::WhitespaceSplit, 8)
            .expect("the crafted tokenizer converts")
    })
}

/// A small BERT int8 weight file whose header and blobs the input chooses, through
/// `Encoder::from_file` and one forward pass per probe (E13r F7). Input: 12 bytes of config,
/// then the bytes every blob is filled from, cycled.
///
/// - config: model type (`bert` / `xlm-roberta`), pooling, every size of `config.json` from 0
///   up to a few (width <= 16, heads <= 4, feed-forward <= 32, layers <= 2, vocabulary 0, 13 or
///   14 to 28, positions <= 11, token types <= 2), `pad_token_id`, `layer_norm_eps` from a list
///   with 0, negative, overflowing and underflowing values, the activation block 0 to 8;
/// - tensors: every tensor the encoder binds, with the shapes the config implies, unless flag
///   bit 2 changes one dimension of one tensor; f32 blobs are raw bit patterns (NaN and
///   infinities included), i8 blobs raw bytes, the id -> row map a permutation unless flag
///   bit 3 takes it from the bytes.
///
/// A file that opens must embed without a panic, and every `Ok` vector has the model's width and
/// is finite (`Encoder::embed_with`'s contract since E13r F3).
pub fn crafted_model(data: &[u8]) {
    use serde_json::json;
    use ulei_infer::text::convert::Owned;
    use ulei_infer::weights::file::{Data, Entry};

    let Some((cfg, fill)) = data.split_first_chunk::<12>() else { return };
    let fill: &[u8] = if fill.is_empty() { &[0x3f, 0x80, 0x00, 0x00] } else { fill };
    let flags = cfg[0];
    let h = usize::from(cfg[1] % 17);
    let heads = usize::from(cfg[2] % 5);
    let ffn = usize::from(cfg[3] % 33);
    let layers = usize::from(cfg[4] % 3);
    let vocab = match cfg[5] % 16 {
        0 => 0,
        1 => 13,
        n => 12 + usize::from(n),
    };
    let positions = usize::from(cfg[6] % 12);
    let types = usize::from(cfg[7] % 3);
    let pad = cfg[8] % 4;
    let eps = [json!(1e-12), json!(1e-5), json!(0.0), json!(-1e-5), json!(1e39), json!(1e-50), json!(1)]
        [usize::from(cfg[9] % 7)]
    .clone();
    let block = usize::from(cfg[10] % 9);
    let reshape = (flags & 4 != 0).then_some((usize::from(cfg[11]), usize::from(cfg[11] >> 4)));

    let mut at = 0usize;
    let mut byte = || {
        let b = fill[at % fill.len()];
        at += 1;
        b
    };
    // The tensors as (name, shape, data); i8 and u32 filled after, from the same stream.
    enum Blob {
        F32(Vec<f32>),
        I8(Vec<i8>),
        U32(Vec<u32>),
    }
    let mut tensors: Vec<(String, Vec<usize>, Blob)> = Vec::new();
    let table = "embeddings.word_embeddings";
    tensors.push((format!("{table}.weight"), vec![vocab, h], Blob::I8(Vec::new())));
    tensors.push((format!("{table}.scale"), vec![vocab], Blob::F32(Vec::new())));
    tensors.push((format!("{table}.row_of_id"), vec![vocab], Blob::U32(Vec::new())));
    tensors.push(("embeddings.position_embeddings.weight".into(), vec![positions, h], Blob::F32(Vec::new())));
    tensors.push(("embeddings.token_type_embeddings.weight".into(), vec![types, h], Blob::F32(Vec::new())));
    tensors.push(("embeddings.LayerNorm.weight".into(), vec![h], Blob::F32(Vec::new())));
    tensors.push(("embeddings.LayerNorm.bias".into(), vec![h], Blob::F32(Vec::new())));
    for i in 0..layers {
        let p = format!("encoder.layer.{i}");
        for (name, out, inp) in [
            ("attention.self.query", h, h),
            ("attention.self.key", h, h),
            ("attention.self.value", h, h),
            ("attention.output.dense", h, h),
            ("intermediate.dense", ffn, h),
            ("output.dense", h, ffn),
        ] {
            tensors.push((format!("{p}.{name}.weight"), vec![out, inp], Blob::I8(Vec::new())));
            tensors.push((format!("{p}.{name}.scale"), vec![out], Blob::F32(Vec::new())));
            tensors.push((format!("{p}.{name}.bias"), vec![out], Blob::F32(Vec::new())));
        }
        for name in ["attention.output.LayerNorm", "output.LayerNorm"] {
            tensors.push((format!("{p}.{name}.weight"), vec![h], Blob::F32(Vec::new())));
            tensors.push((format!("{p}.{name}.bias"), vec![h], Blob::F32(Vec::new())));
        }
    }
    if let Some((which, dim)) = reshape {
        let n = tensors.len();
        let shape = &mut tensors[which % n].1;
        let d = shape.len();
        shape[dim % d] = usize::from(cfg[11] % 20);
    }
    for (_, shape, blob) in &mut tensors {
        let n: usize = shape.iter().product();
        *blob = match blob {
            Blob::F32(_) => Blob::F32((0..n).map(|_| f32::from_le_bytes([byte(), byte(), byte(), byte()])).collect()),
            Blob::I8(_) => Blob::I8((0..n).map(|_| byte() as i8).collect()),
            Blob::U32(_) if flags & 8 != 0 => {
                Blob::U32((0..n).map(|_| u32::from(byte()) % (n as u32 + 2)).collect())
            }
            Blob::U32(_) => Blob::U32((0..n as u32).rev().collect()),
        };
    }

    let section = crafted_section();
    let meta = json!({
        "format": ulei_infer::bert::convert::FORMAT,
        "config": {
            "model_type": if flags & 1 == 0 { "bert" } else { "xlm-roberta" },
            "hidden_act": "gelu",
            "hidden_size": h,
            "intermediate_size": ffn,
            "layer_norm_eps": eps,
            "max_position_embeddings": positions,
            "num_attention_heads": heads,
            "num_hidden_layers": layers,
            "pad_token_id": pad,
            "type_vocab_size": types,
            "vocab_size": vocab,
        },
        "pooling": if flags & 2 == 0 { "mean" } else { "cls" },
        "prefixes": {"query": "query: ", "passage": "passage: "},
        "act_block": block,
        "tokenizer": section.meta,
        "source": null,
    });
    let mut entries: Vec<Entry<'_>> = tensors
        .iter()
        .map(|(name, shape, blob)| Entry {
            name: name.clone(),
            shape: shape.clone(),
            data: match blob {
                Blob::F32(d) => Data::F32(d),
                Blob::I8(d) => Data::I8(d),
                Blob::U32(d) => Data::U32(d),
            },
        })
        .collect();
    entries.extend(section.tensors.iter().map(|(name, shape, d)| Entry {
        name: name.clone(),
        shape: shape.clone(),
        data: match d {
            Owned::U8(d) => Data::U8(d),
            Owned::U32(d) => Data::U32(d),
            Owned::F64(d) => Data::F64(d),
        },
    }));
    let mut bytes = Vec::new();
    file::write(&mut bytes, &meta, &entries).expect("an in-memory weight file writes");
    let file = WeightFile::read(&bytes[..]).expect("a written weight file opens");
    let Ok(encoder) = Encoder::from_file(&file) else { return };
    let check = |v: &ulei_infer::Result<Vec<f32>>| {
        if let Ok(v) = v {
            assert_eq!(v.len(), encoder.config().hidden_size, "pooled width");
            assert!(v.iter().all(|x| x.is_finite()), "an Ok vector is finite: {v:?}");
        }
    };
    for text in &PROBES[..3] {
        check(&encoder.embed_text(text));
    }
    check(&encoder.embed(&[0, 5, 2]));
}
