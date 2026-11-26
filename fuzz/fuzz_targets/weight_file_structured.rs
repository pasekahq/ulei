//! A weight file whose preamble is always consistent: input = [u16 header length][header][data].
//! The fuzzer's mutations then reach the JSON header, the tensor table (dtypes, shapes, offsets,
//! alignment, overlap, products that overflow) and `meta` (config, tokenizer, prefixes) that the
//! encoder and the tokenizer read. One flag byte picks the format version (3, or an older one,
//! which must be refused as "convert the model again", never parsed) and whether to seal.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [flags, hi, lo, rest @ ..] = data else { return };
    let n = usize::from(u16::from_be_bytes([*hi, *lo])).min(rest.len());
    let (header, blobs) = rest.split_at(n);
    let version = match flags & 3 { 0 => 1, 1 => 2, _ => 3 };
    let file = ulei_fuzz::assemble(header, blobs, version, flags & 4 != 0);
    if version < 3 {
        assert!(
            !matches!(ulei_infer::weights::file::WeightFile::read(&file[..]), Ok(_)),
            "an old format version opened"
        );
        return;
    }
    ulei_fuzz::exercise_weight_file(&file);
});
