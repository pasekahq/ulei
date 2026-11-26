//! The real e5 / bge-m3 tokenizer section (the two are the same XLM-R SentencePiece table) on
//! arbitrary text: valid UTF-8 as is, other bytes through `from_utf8_lossy`. On a sound section
//! `encode` must succeed (its only errors are a corrupt section), give `<s> ... </s>`, at most
//! `max_tokens` ids inside the vocabulary, deterministically. Inputs up to 1 MiB: the daemon's
//! queries are cut at 4 000 characters, passages are not.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let tok = ulei_fuzz::real_tokenizer();
    let text = String::from_utf8_lossy(data);
    let ids = tok.encode(&text).expect("a sound tokenizer section encodes every text");
    ulei_fuzz::check_ids(tok, &text);
    // The same text with the e5 prefixes, as the helper sends it.
    for prefix in ["query: ", "passage: "] {
        let with = format!("{prefix}{text}");
        tok.encode(&with).expect("prefixed text encodes");
    }
    // Every id sequence starts and ends with the same two specials.
    let empty = tok.encode("").expect("empty text");
    assert_eq!(ids.first(), empty.first(), "<s>");
    assert_eq!(ids.last(), empty.last(), "</s>");
});
