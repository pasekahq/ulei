//! A small BERT int8 weight file whose config, shapes and blobs the input chooses (zero and odd
//! sizes, bad `layer_norm_eps`, NaN and infinite weights, a wrong shape, a bad id -> row map),
//! through `Encoder::from_file` and a forward pass per probe text (ADR-0086 E12a, E13r F7). The
//! file is consistent where the fuzzer does not choose otherwise, so its inputs reach the
//! forward pass instead of stopping at the tensor table. Structure in `ulei_fuzz::crafted_model`.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ulei_fuzz::crafted_model(data));
