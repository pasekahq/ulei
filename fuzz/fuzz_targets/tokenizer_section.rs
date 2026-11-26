//! A damaged tokenizer section: the tokenizer-only weight file with bytes of its blobs replaced
//! (input = pairs of [u32 position][u8 value], positions inside the data section), then
//! `Tokenizer::from_file` and `encode` of the probe texts and of the input's tail as text. The
//! tokenizer reads its trie, offsets and piece table in place (text/mod.rs: "nothing is built at
//! load"), so every index it follows comes from the file: it must answer `Err`, never panic,
//! read out of bounds or loop.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let base = ulei_fuzz::tokenizer_bytes();
    let hdr = u64::from_le_bytes(base[16..24].try_into().unwrap()) as usize;
    let start = (64 + hdr).div_ceil(64) * 64;
    let span = base.len() - start;
    let mut file = base.to_vec();
    let (patches, text) = data.split_at(data.len().min(5 * 64));
    for p in patches.chunks_exact(5) {
        let at = u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as usize % span;
        file[start + at] = p[4];
    }
    let Ok(w) = ulei_infer::weights::file::WeightFile::read(&file[..]) else { return };
    let Ok(tok) = ulei_infer::text::Tokenizer::from_file(&w) else { return };
    for probe in ulei_fuzz::PROBES {
        let _ = tok.encode(probe);
    }
    let _ = tok.encode(&String::from_utf8_lossy(text));
});
