//! Arbitrary bytes as a weight file (ADR-0086 section 2): `WeightFile::read` and everything a
//! model reads from it. Claim under test (weights/file.rs module docs): "Every malformed or
//! truncated file is an error, never a panic or an access out of bounds."
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ulei_fuzz::exercise_weight_file(data);
});
