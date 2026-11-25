// Port: THIRD_PARTY_NOTICES.md#unicode-segmentation
//! Extended grapheme clusters (Unicode Standard Annex #29) as the `unicode-segmentation` crate
//! 1.13.3 finds them (Unicode 17.0.0): the reference normaliser looks a character map up per
//! cluster, so its boundaries decide the output.
//!
//! The rules are those of that crate's `check_pair` (`GB3` to `GB13`, `GB9c` with its look-back
//! over `InCB=Linker` / `InCB=Extend`), evaluated left to right: every look-back of the crate is
//! a small state carried forward here.

use super::grapheme_table::RUNS;

/// `Grapheme_Cluster_Break`, with `Extended_Pictographic` and `InCB=Consonant` as categories of
/// their own (as the crate tabulates them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cat {
    Any,
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    RegionalIndicator,
    Prepend,
    SpacingMark,
    L,
    V,
    T,
    Lv,
    Lvt,
    ExtendedPictographic,
    InCbConsonant,
}

/// The categories in the order of the table's low four bits.
const CATS: [Cat; 16] = [
    Cat::Any,
    Cat::Cr,
    Cat::Lf,
    Cat::Control,
    Cat::Extend,
    Cat::Zwj,
    Cat::RegionalIndicator,
    Cat::Prepend,
    Cat::SpacingMark,
    Cat::L,
    Cat::V,
    Cat::T,
    Cat::Lv,
    Cat::Lvt,
    Cat::ExtendedPictographic,
    Cat::InCbConsonant,
];

/// The segmentation properties of one character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Props {
    cat: Cat,
    incb_extend: bool,
    incb_linker: bool,
}

fn props(c: char) -> Props {
    let cp = u32::from(c);
    // The first run starts at U+0000, so there is always one at or before `cp`.
    let i = RUNS.partition_point(|&r| r >> 8 <= cp).saturating_sub(1);
    let r = RUNS.get(i).copied().unwrap_or(0);
    Props {
        cat: CATS[(r & 0xF) as usize],
        incb_extend: r & 0x10 != 0,
        incb_linker: r & 0x20 != 0,
    }
}

/// What the characters before a boundary say, carried forward.
#[derive(Clone, Copy, Debug, Default)]
struct Context {
    /// Regional indicators in a row, ending at the previous character.
    regional: usize,
    /// The last character other than `Extend` was `Extended_Pictographic`.
    pictographic: bool,
    /// At the previous character, a `ZWJ`: whether `pictographic` held just before it.
    zwj_after_pictographic: bool,
    /// `InCB=Consonant` followed only by `InCB=Linker` / `InCB=Extend`: the linkers seen.
    linkers: Option<usize>,
}

impl Context {
    fn after(&mut self, p: Props) {
        self.regional = if p.cat == Cat::RegionalIndicator {
            self.regional + 1
        } else {
            0
        };
        self.zwj_after_pictographic = p.cat == Cat::Zwj && self.pictographic;
        match p.cat {
            Cat::ExtendedPictographic => self.pictographic = true,
            Cat::Extend => {}
            _ => self.pictographic = false,
        }
        // As the crate's look-back: a linker counts first, then an extender is skipped, then
        // the character decides.
        self.linkers = if p.incb_linker {
            self.linkers.map(|n| n + 1)
        } else if p.incb_extend {
            self.linkers
        } else if p.cat == Cat::InCbConsonant {
            Some(0)
        } else {
            None
        };
    }

    /// Whether a cluster boundary lies between a character of `before` and one of `after`.
    fn breaks(&self, before: Cat, after: Cat) -> bool {
        use Cat::{
            Control, Cr, Extend, ExtendedPictographic, InCbConsonant, L, Lf, Lv, Lvt, Prepend,
            RegionalIndicator, SpacingMark, T, V, Zwj,
        };
        match (before, after) {
            (Cr, Lf) => false,                                       // GB3
            (Control | Cr | Lf, _) | (_, Control | Cr | Lf) => true, // GB4, GB5
            // GB6 to GB8, then GB9, GB9a, GB9b.
            (L, L | V | Lv | Lvt)
            | (Lv | V, V | T)
            | (Lvt | T, T)
            | (_, Extend | Zwj | SpacingMark)
            | (Prepend, _) => false,
            (_, InCbConsonant) => self.linkers.is_none_or(|n| n == 0), // GB9c
            (Zwj, ExtendedPictographic) => !self.zwj_after_pictographic, // GB11
            (RegionalIndicator, RegionalIndicator) => self.regional.is_multiple_of(2), // GB12, GB13
            _ => true,                                                 // GB999
        }
    }
}

/// The extended grapheme clusters of a string, in order.
pub(super) struct Graphemes<'a> {
    s: &'a str,
    pos: usize,
    context: Context,
}

impl<'a> Graphemes<'a> {
    pub(super) fn new(s: &'a str) -> Self {
        Self {
            s,
            pos: 0,
            context: Context::default(),
        }
    }
}

impl<'a> Iterator for Graphemes<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let rest = self.s.get(self.pos..)?;
        let mut chars = rest.char_indices();
        let (_, first) = chars.next()?;
        let mut before = props(first);
        self.context.after(before);
        let mut end = rest.len();
        for (i, c) in chars {
            let p = props(c);
            if self.context.breaks(before.cat, p.cat) {
                end = i;
                break;
            }
            self.context.after(p);
            before = p;
        }
        let cluster = &rest[..end];
        self.pos += end;
        Some(cluster)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<&str> {
        Graphemes::new(s).collect()
    }

    #[test]
    fn table_spot_checks() {
        assert_eq!(props('a').cat, Cat::Any);
        assert_eq!(props('\r').cat, Cat::Cr);
        assert_eq!(props('\n').cat, Cat::Lf);
        assert_eq!(props('\0').cat, Cat::Control);
        assert_eq!(props('\u{301}').cat, Cat::Extend);
        assert_eq!(props('\u{200D}').cat, Cat::Zwj);
        assert_eq!(props('\u{1F1F7}').cat, Cat::RegionalIndicator);
        assert_eq!(props('\u{1100}').cat, Cat::L);
        assert_eq!(props('\u{1161}').cat, Cat::V);
        assert_eq!(props('\u{11A8}').cat, Cat::T);
        assert_eq!(props('\u{AC00}').cat, Cat::Lv);
        assert_eq!(props('\u{AC01}').cat, Cat::Lvt);
        assert_eq!(props('\u{1F600}').cat, Cat::ExtendedPictographic);
        assert_eq!(props('\u{0915}').cat, Cat::InCbConsonant);
        assert!(props('\u{094D}').incb_linker);
        assert!(props('\u{0301}').incb_extend);
        assert_eq!(props('\u{0E33}').cat, Cat::SpacingMark);
        assert_eq!(props('\u{0600}').cat, Cat::Prepend);
        assert_eq!(props('\u{10FFFF}').cat, Cat::Any);
    }

    #[test]
    fn clusters_follow_uax29() {
        assert_eq!(split(""), Vec::<&str>::new());
        assert_eq!(split("ab"), vec!["a", "b"]);
        assert_eq!(split("e\u{301}x"), vec!["e\u{301}", "x"]);
        assert_eq!(split("\r\n\n"), vec!["\r\n", "\n"]);
        assert_eq!(split("\u{301}a"), vec!["\u{301}", "a"]);
        // Hangul jamo sequences and syllables.
        assert_eq!(
            split("\u{1100}\u{1161}\u{11A8}\u{1100}"),
            vec!["\u{1100}\u{1161}\u{11A8}", "\u{1100}"]
        );
        // Regional indicators pair up.
        let flags = "\u{1F1F7}\u{1F1FA}\u{1F1FA}\u{1F1F8}\u{1F1E9}";
        assert_eq!(
            split(flags),
            vec!["\u{1F1F7}\u{1F1FA}", "\u{1F1FA}\u{1F1F8}", "\u{1F1E9}"]
        );
        // Emoji ZWJ sequences with skin tones; a ZWJ without a pictograph before it breaks.
        let family = "\u{1F468}\u{1F3FD}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(split(family), vec![family]);
        assert_eq!(split("a\u{200D}\u{1F600}"), vec!["a\u{200D}", "\u{1F600}"]);
        // Indic conjuncts (GB9c): consonant, virama, consonant stay together.
        assert_eq!(
            split("\u{915}\u{94D}\u{937}"),
            vec!["\u{915}\u{94D}\u{937}"]
        );
        assert_eq!(split("\u{94D}\u{937}"), vec!["\u{94D}", "\u{937}"]);
        // Prepend and spacing marks.
        assert_eq!(split("\u{600}1"), vec!["\u{600}1"]);
        assert_eq!(split("\u{E01}\u{E33}"), vec!["\u{E01}\u{E33}"]);
    }
}
