//! Golden tests: outputs of the reference implementation on the fixtures in
//! `tests/fixtures/render/` (see the README there) must be reproduced exactly.

use std::fs;
use std::path::{Path, PathBuf};

use ulei_channels::render::{
    MAX_MESSAGE_LEN, SendOptions, plan_reply, split_message, strip_markdown_v2, to_markdown_v2,
    utf16_len,
};

const SEP: &str = "-----8<----- ";

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/render")
}

fn read(name: &str) -> String {
    fs::read_to_string(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Splits a fixture into `(header, body)` records; each body ends with one extra newline.
fn records(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        if let Some(header) = line.strip_prefix(SEP) {
            out.push((header.trim_end().to_owned(), String::new()));
        } else {
            out.last_mut()
                .expect("record header first")
                .1
                .push_str(line);
        }
    }
    for (_, body) in &mut out {
        assert_eq!(body.pop(), Some('\n'), "record body ends with a newline");
    }
    out
}

fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir())
        .expect("fixture dir")
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".md").map(str::to_owned)
        })
        .filter(|n| n != "README")
        .collect();
    names.sort();
    names
}

#[test]
fn replies_render_and_split_as_the_reference_does() {
    let names = fixture_names();
    assert!(names.len() >= 15, "fixtures present: {names:?}");
    for name in names {
        let input = read(&format!("{name}.md"));
        let expected = records(&read(&format!("{name}.out")));
        let plan = plan_reply(&input, None, true, &SendOptions::default());
        assert_eq!(
            plan.messages.len() * 2,
            expected.len(),
            "{name}: chunk count"
        );
        for (i, message) in plan.messages.iter().enumerate() {
            let (head, mdv2) = &expected[2 * i];
            let (_, expected_plain) = &expected[2 * i + 1];
            assert_eq!(&message.text, mdv2, "{name}: {head}");
            assert_eq!(
                &message.plain_fallback().text,
                expected_plain,
                "{name}: {head} plain"
            );
            assert!(
                utf16_len(&message.text) <= MAX_MESSAGE_LEN,
                "{name}: {head} too long"
            );
        }
    }
}

#[test]
fn small_cases_convert_and_strip_as_the_reference_does() {
    let recs = records(&read("cases.txt"));
    assert!(recs.len() >= 3 * 80);
    for case in recs.chunks(3) {
        let [(_, input), (_, mdv2), (_, plain)] = case else {
            panic!("case triple");
        };
        let got = to_markdown_v2(input);
        assert_eq!(&got, mdv2, "markdownv2 of {input:?}");
        assert_eq!(&strip_markdown_v2(&got), plain, "plain of {input:?}");
    }
}

#[test]
fn splits_match_the_reference_at_small_limits() {
    let recs = records(&read("splits.txt"));
    let mut i = 0;
    let mut cases = 0;
    while i < recs.len() {
        let (head, input) = &recs[i];
        let limit: usize = head
            .strip_prefix("split ")
            .and_then(|n| n.parse().ok())
            .expect("limit");
        let mut expected = Vec::new();
        i += 1;
        while i < recs.len() && recs[i].0 == "chunk" {
            expected.push(recs[i].1.clone());
            i += 1;
        }
        assert_eq!(
            split_message(input, limit),
            expected,
            "split {limit} of {input:?}"
        );
        cases += 1;
    }
    assert!(cases >= 10);
}

/// Literal placeholder look-alikes (NUL can arrive in tool output echoed by the model) must
/// render as the reference does and never recurse: each key is restored once, newest first.
#[test]
fn placeholder_lookalikes_render_as_the_reference_does() {
    let cases = [
        ("`\0PH0\0`", "`\0PH0\0`", "`\0PH0\0`"),
        ("**`\0PH1\0`**", "*`\0PH1\0`*", "`\0PH1\0`"),
        ("a \0PH0\0 b", "a \0PH0\0 b", "a \0PH0\0 b"),
        ("**\0PH0\0** and `x`", "*`x`* and `x`", "`x` and `x`"),
    ];
    for (input, mdv2, plain) in cases {
        let got = to_markdown_v2(input);
        assert_eq!(got, mdv2, "markdownv2 of {input:?}");
        assert_eq!(strip_markdown_v2(&got), plain, "plain of {input:?}");
    }
}
