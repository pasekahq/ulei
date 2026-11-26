//! The tokenizer on a hand-made `tokenizer.json`: both pipelines, added tokens, truncation,
//! random input and corrupt sections.

use serde_json::json;

use super::charsmap::tests::build;
use super::convert::{Section, section};
use super::*;
use crate::weights::file::WeightFile;

fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in bytes.chunks(3) {
        let n = c
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= c.len() {
                s.push(char::from(A[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// A character map: tab and the ideographic space to a space, full-width A to A, the zero
/// width space to nothing, and the circled one to 1.
fn charsmap_b64() -> String {
    let pairs = [
        ("\t", " "),
        ("\u{3000}", " "),
        ("\u{FF21}", "A"),
        ("\u{200B}", ""),
        ("\u{2460}", "1"),
    ];
    let mut strings = Vec::new();
    let mut keys = Vec::new();
    for (k, v) in pairs {
        keys.push((k, u32::try_from(strings.len()).unwrap()));
        strings.extend_from_slice(v.as_bytes());
        strings.push(0);
    }
    let trie = build(&keys);
    let mut blob = u32::try_from(trie.len() * 4)
        .unwrap()
        .to_le_bytes()
        .to_vec();
    blob.extend(trie.iter().flat_map(|u| u.to_le_bytes()));
    blob.extend_from_slice(&strings);
    b64(&blob)
}

pub(crate) fn tokenizer_json(mask_lstrip: bool) -> String {
    json!({
        "added_tokens": [
            {"id": 0, "content": "<s>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
            {"id": 1, "content": "<pad>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
            {"id": 2, "content": "</s>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
            {"id": 3, "content": "<unk>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
            {"id": 13, "content": "<mask>", "single_word": false, "lstrip": mask_lstrip, "rstrip": false, "normalized": false, "special": true}
        ],
        "normalizer": {"type": "Sequence", "normalizers": [
            {"type": "Precompiled", "precompiled_charsmap": charsmap_b64()},
            {"type": "Replace", "pattern": {"Regex": " {2,}"}, "content": " "}
        ]},
        "pre_tokenizer": {"type": "Metaspace", "replacement": "\u{2581}", "add_prefix_space": true},
        "post_processor": {"type": "TemplateProcessing",
            "single": [{"SpecialToken": {"id": "<s>", "type_id": 0}}, {"Sequence": {"id": "A", "type_id": 0}}, {"SpecialToken": {"id": "</s>", "type_id": 0}}],
            "special_tokens": {"<s>": {"id": "<s>", "ids": [0], "tokens": ["<s>"]}, "</s>": {"id": "</s>", "ids": [2], "tokens": ["</s>"]}}},
        "model": {"type": "Unigram", "unk_id": 3, "byte_fallback": false, "vocab": [
            ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0],
            ["\u{2581}", -2.0], ["\u{2581}ab", -1.0], ["a", -3.0], ["b", -3.0], ["c", -3.0],
            ["\u{2581}c", -1.5], ["A", -3.0], ["1", -3.0], ["\u{2581}A", -2.5], ["<mask>", 0.0]]}
    })
    .to_string()
}

fn file_of(s: &Section) -> WeightFile {
    let mut bytes = Vec::new();
    s.write_alone(&mut bytes).unwrap();
    WeightFile::read(bytes.as_slice()).unwrap()
}

fn file(pipeline: Pipeline, max_tokens: usize, mask_lstrip: bool) -> WeightFile {
    file_of(&section(&tokenizer_json(mask_lstrip), pipeline, max_tokens).unwrap())
}

fn enc(f: &WeightFile, text: &str) -> Vec<u32> {
    Tokenizer::from_file(f).unwrap().encode(text).unwrap()
}

const W: Pipeline = Pipeline::WhitespaceSplit;
const C: Pipeline = Pipeline::CollapseSpaces;

#[test]
fn whitespace_split_drops_every_whitespace() {
    let f = file(W, 512, false);
    assert_eq!(enc(&f, "abc"), vec![0, 5, 8, 2]);
    assert_eq!(enc(&f, "ab   c"), vec![0, 5, 9, 2]);
    assert_eq!(enc(&f, " \tab\n c \u{3000}"), vec![0, 5, 9, 2]);
    assert_eq!(enc(&f, ""), vec![0, 2]);
    assert_eq!(enc(&f, " \t\u{200B} "), vec![0, 2]);
    // A literal ▁ is not prepended to and splits a word.
    assert_eq!(enc(&f, "\u{2581}ab\u{2581}c"), vec![0, 5, 9, 2]);
    assert_eq!(enc(&f, "a\u{2581}"), vec![0, 4, 6, 4, 2]);
}

#[test]
fn collapse_spaces_keeps_one_space_as_a_piece() {
    let f = file(C, 512, false);
    assert_eq!(enc(&f, "ab   c"), vec![0, 5, 9, 2]);
    assert_eq!(enc(&f, "ab "), vec![0, 5, 4, 2]);
    assert_eq!(enc(&f, " ab"), vec![0, 5, 2]);
    // Tab maps to a space, then collapses with the one before it.
    assert_eq!(enc(&f, "ab \tc"), vec![0, 5, 9, 2]);
    assert_eq!(
        enc(&f, "ab\n"),
        vec![0, 5, 3, 2],
        "newline is not in this map"
    );
}

#[test]
fn normaliser_runs_per_cluster() {
    let f = file(W, 512, false);
    assert_eq!(enc(&f, "\u{FF21}"), vec![0, 12, 2]);
    // The circled one with a combining mark is one short cluster: the mark goes with it.
    assert_eq!(enc(&f, "\u{2460}\u{301}"), vec![0, 4, 11, 2]);
    assert_eq!(enc(&f, "a\u{301}"), vec![0, 4, 6, 3, 2]);
}

#[test]
fn added_tokens_split_the_raw_text() {
    for p in [W, C] {
        let f = file(p, 512, false);
        assert_eq!(enc(&f, "ab<mask>c"), vec![0, 5, 13, 9, 2]);
        assert_eq!(enc(&f, "<s></s><pad><unk>"), vec![0, 0, 2, 1, 3, 2]);
        assert_eq!(enc(&f, "<s<mask>"), vec![0, 4, 3, 13, 2]);
    }
    // Without lstrip the space before the token is a piece of its own under collapse-spaces.
    assert_eq!(enc(&file(C, 512, false), "ab <mask>"), vec![0, 5, 4, 13, 2]);
    assert_eq!(enc(&file(C, 512, true), "ab \t<mask>"), vec![0, 5, 13, 2]);
}

#[test]
fn truncation_keeps_the_frame() {
    let f = file(W, 4, false);
    assert_eq!(enc(&f, "ab ab ab ab"), vec![0, 5, 5, 2]);
    assert_eq!(enc(&f, "<mask><mask><mask>"), vec![0, 13, 13, 2]);
    let two = file(W, 2, false);
    assert_eq!(enc(&two, "ab ab"), vec![0, 2]);
    assert!(section(&tokenizer_json(false), W, 1).is_err());
    // Work stops at the budget: a huge input costs no more than a short one.
    let long = "ab ".repeat(1 << 20);
    assert_eq!(enc(&f, &long).len(), 4);
}

#[test]
fn only_the_head_of_a_long_text_is_read() {
    // E13r F6: the work grew with the text (a 20 MB word took a minute and 700 MB). The text
    // is cut at max(64 KiB, max_tokens x longest piece x 4) bytes, here 64 KiB.
    const CAP: usize = 64 * 1024;
    for p in [W, C] {
        let f = file(p, 8, false);
        // Ordinary text reaches the budget long before the cut: the ids are unchanged.
        let long = "ab c ".repeat(1 << 20);
        assert_eq!(enc(&f, &long), enc(&f, &long[..40]));
        // A pathological text whose content starts past the cut differs from the uncut one.
        let mut far = " ".repeat(CAP);
        far.push_str("ab");
        assert!(!enc(&f, &far).contains(&5), "{p:?}");
        assert_eq!(enc(&f, &far), enc(&f, &far[..CAP]), "{p:?}");
        let mut near = " ".repeat(CAP - 2);
        near.push_str("ab");
        assert!(enc(&f, &near).contains(&5), "{p:?}");
        // The cut never splits a character: 'é' is two bytes, the cut falls inside one.
        let mut odd = "x".to_owned();
        odd.push_str(&"é".repeat(CAP));
        assert_eq!(enc(&f, &odd).len(), 8);
        // One word of 20 MB is cheap now (it took a minute before the cut).
        let word = "a".repeat(20 << 20);
        assert_eq!(enc(&f, &word).len(), 8);
    }
}

type Edit = Box<dyn Fn(&mut serde_json::Value)>;

#[test]
fn bad_tokenizer_files_are_refused() {
    let good: serde_json::Value = serde_json::from_str(&tokenizer_json(false)).unwrap();
    let edits: Vec<(&str, Edit)> = vec![
        ("Unigram", Box::new(|v| v["model"]["type"] = json!("BPE"))),
        (
            "byte fallback",
            Box::new(|v| v["model"]["byte_fallback"] = json!(true)),
        ),
        ("unk_id", Box::new(|v| v["model"]["unk_id"] = json!(null))),
        (
            "normalized",
            Box::new(|v| v["added_tokens"][4]["normalized"] = json!(true)),
        ),
        (
            "post_processor",
            Box::new(|v| v["post_processor"] = json!({"type": "BertProcessing"})),
        ),
        (
            "collapse-spaces",
            Box::new(|v| v["normalizer"]["normalizers"][1]["content"] = json!("_")),
        ),
        (
            "collapse-spaces",
            Box::new(|v| v["pre_tokenizer"]["add_prefix_space"] = json!(false)),
        ),
        (
            "base64",
            Box::new(|v| v["normalizer"]["normalizers"][0]["precompiled_charsmap"] = json!("@@@@")),
        ),
    ];
    for (want, edit) in edits {
        let mut v = good.clone();
        edit(&mut v);
        let e = section(&v.to_string(), C, 512).unwrap_err().to_string();
        assert!(e.contains(want), "{want:?} not in {e:?}");
    }
    // whitespace-split ignores the file's own pipeline but for the character map.
    let mut v = good.clone();
    v["pre_tokenizer"] = json!({"type": "Whitespace"});
    assert!(section(&v.to_string(), W, 512).is_ok());
    v["normalizer"] = serde_json::Value::Null;
    let f = file_of(&section(&v.to_string(), W, 512).unwrap());
    assert_eq!(
        enc(&f, "\u{FF21}"),
        vec![0, 4, 3, 2],
        "no map, no normalisation"
    );
}

#[test]
fn duplicate_pieces_keep_the_last_id() {
    let mut v: serde_json::Value = serde_json::from_str(&tokenizer_json(false)).unwrap();
    v["model"]["vocab"][8] = json!(["\u{2581}c", -9.0]);
    let f = file_of(&section(&v.to_string(), W, 512).unwrap());
    assert_eq!(enc(&f, "c"), vec![0, 9, 2]);
}

/// A small xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }
}

/// A random string mixing the vocabulary, whitespace, added tokens and any code point.
fn random_text(rng: &mut Rng, max_len: usize) -> String {
    const PARTS: [&str; 14] = [
        "ab",
        "c",
        " ",
        "  ",
        "\t",
        "\n",
        "\u{2581}",
        "<mask>",
        "<s>",
        "\u{FF21}",
        "\u{2460}",
        "\u{301}",
        "\u{200D}",
        "\u{1F1F7}",
    ];
    let mut s = String::new();
    for _ in 0..rng.below(max_len + 1) {
        if rng.below(3) == 0 {
            s.push_str(PARTS[rng.below(PARTS.len())]);
        } else {
            let cp = u32::try_from(rng.below(0x11_0000)).unwrap();
            s.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
        }
    }
    s
}

#[test]
fn random_text_never_panics_and_stays_in_bounds() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for (p, max) in [(W, 512), (C, 512), (W, 6), (C, 3)] {
        let f = file(p, max, true);
        let t = Tokenizer::from_file(&f).unwrap();
        for _ in 0..3000 {
            let s = random_text(&mut rng, 40);
            let ids = t.encode(&s).unwrap();
            assert!(ids.len() >= 2 && ids.len() <= max, "{s:?}");
            assert_eq!((ids[0], ids[ids.len() - 1]), (0, 2));
            assert!(ids.iter().all(|&i| (i as usize) < t.vocab_size()));
        }
    }
}

#[test]
fn corrupt_sections_are_errors_not_panics() {
    let s = section(&tokenizer_json(false), W, 512).unwrap();
    let mut good = Vec::new();
    s.write_alone(&mut good).unwrap();
    let n = usize::try_from(u64::from_le_bytes(good[16..24].try_into().unwrap())).unwrap();
    // Every truncation is refused by the reader.
    for k in 0..good.len() {
        assert!(WeightFile::read(&good[..k]).is_err());
    }
    // Missing pieces of the section.
    let mut meta_only = s.clone();
    meta_only.tensors.pop();
    let f = file_of(&meta_only);
    assert!(Tokenizer::from_file(&f).is_err());
    let mut no_meta = Vec::new();
    crate::weights::file::write(&mut no_meta, &json!({}), &s.entries()).unwrap();
    let f = WeightFile::read(no_meta.as_slice()).unwrap();
    assert!(
        Tokenizer::from_file(&f)
            .unwrap_err()
            .to_string()
            .contains("convert the model again")
    );
    // Random byte changes, in the header and in the arrays: an error or some ids, never a panic.
    let mut rng = Rng(7);
    let mut opened = 0;
    for _ in 0..20_000 {
        let mut bad = good.clone();
        for _ in 0..=rng.below(3) {
            let at = if rng.below(2) == 0 {
                64 + rng.below(n)
            } else {
                rng.below(bad.len())
            };
            bad[at] = u8::try_from(rng.below(256)).unwrap();
        }
        let Ok(f) = WeightFile::read(bad.as_slice()) else {
            continue;
        };
        let Ok(t) = Tokenizer::from_file(&f) else {
            continue;
        };
        opened += 1;
        for text in ["abc", "\u{FF21} c\t<mask>", "\u{2460}\u{301}", "xyz"] {
            if let Ok(ids) = t.encode(text) {
                assert!(ids.iter().all(|&i| (i as usize) < t.vocab_size()));
            }
        }
    }
    assert!(opened > 0);
}
