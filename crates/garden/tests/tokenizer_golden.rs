//! Token ids of the bge-m3 tokenizer against ids the reference `tokenizers` crate (0.23.2,
//! truncation at 512) produced for the same strings. Needs the model directory, so it is
//! ignored by default: `ULEI_GARDEN_MODEL_DIR=<dir with tokenizer.json> cargo test -p
//! ulei-garden --features onnx --test tokenizer_golden -- --ignored`.

#![cfg(feature = "onnx")]
#![allow(missing_docs, clippy::unwrap_used)]

use ulei_garden::embed::tokenizer::Tokenizer;

const GOLDEN: &[(&str, &[u32])] = &[
    (
        "Where is my passport?",
        &[0, 78662, 83, 759, 452, 10033, 32, 2],
    ),
    ("Где мой паспорт?", &[0, 105_267, 20351, 45858, 32, 2]),
    (
        "Встреча с Ilsa Varn в 18:30 — кафе «Moss».",
        &[
            0, 417, 37325, 2052, 135, 891, 433, 3323, 19, 49, 102_086, 292, 33063, 94, 116_128, 7,
            1251, 2,
        ],
    ),
    (
        "  multiple   spaces\tand\ttabs  ",
        &[0, 48716, 32628, 7, 136, 20928, 7, 6, 2],
    ),
    (
        "ﬁ ligature Ⅻ ＡＢＣ",
        &[0, 809, 19881, 6644, 39198, 47457, 2],
    ),
    ("", &[0, 2]),
    ("Ёлка и ёж", &[0, 6, 57498, 32889, 35, 6, 7785, 861, 2]),
];

#[test]
#[ignore = "needs the bge-m3 tokenizer.json in ULEI_GARDEN_MODEL_DIR"]
fn ids_match_the_reference_tokenizer() {
    let dir = std::env::var_os("ULEI_GARDEN_MODEL_DIR").expect("ULEI_GARDEN_MODEL_DIR");
    let json = std::fs::read_to_string(std::path::Path::new(&dir).join("tokenizer.json")).unwrap();
    let t = Tokenizer::from_json(&json).unwrap();
    for (text, ids) in GOLDEN {
        assert_eq!(t.encode(text, 512), *ids, "{text:?}");
    }
    let started = std::time::Instant::now();
    let huge = t.encode(&"паспорт".repeat(300_000), 512);
    assert_eq!(huge.len(), 512);
    eprintln!("4 MB without spaces: {:?}", started.elapsed());
}
