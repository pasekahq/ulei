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
    static TOK: OnceLock<Tokenizer<'static>> = OnceLock::new();
    TOK.get_or_init(|| {
        let file: &'static WeightFile =
            Box::leak(Box::new(WeightFile::read(tokenizer_bytes()).expect("tokenizer file opens")));
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
