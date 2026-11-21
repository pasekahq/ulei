// Port: THIRD_PARTY_NOTICES.md#snowball
//! The Snowball Russian stemmer (`algorithms/russian.sbl` of Snowball 3.2.0): the stem of one
//! lowercase token, so that the case and number forms of a Russian word meet in the FTS index.
//! A token without a Russian vowel, a Latin word or a number, comes back unchanged.

/// Russian vowels, the grouping `v`.
const VOWELS: &[char] = &['а', 'е', 'и', 'о', 'у', 'ы', 'э', 'ю', 'я'];

/// Perfective gerund endings removed only after `а` or `я`.
const GERUND_AFTER_A: &[&str] = &["в", "вши", "вшись"];
const GERUND: &[&str] = &["ив", "ивши", "ившись", "ыв", "ывши", "ывшись"];

const ADJECTIVE: &[&str] = &[
    "ее", "ие", "ые", "ое", "ими", "ыми", "ей", "ий", "ый", "ой", "ем", "им", "ым", "ом", "его",
    "ого", "ему", "ому", "их", "ых", "ую", "юю", "ая", "яя", "ою", "ею",
];

/// Participle endings removed only after `а` or `я`.
const PARTICIPLE_AFTER_A: &[&str] = &["ем", "нн", "вш", "ющ", "щ"];
const PARTICIPLE: &[&str] = &["ивш", "ывш", "ующ"];

const REFLEXIVE: &[&str] = &["ся", "сь"];

/// Verb endings removed only after `а` or `я`.
const VERB_AFTER_A: &[&str] = &[
    "ла", "на", "ете", "йте", "ли", "й", "л", "ем", "н", "ло", "но", "ет", "ют", "ны", "ть", "ешь",
    "нно",
];
const VERB: &[&str] = &[
    "ила", "ыла", "ена", "ейте", "уйте", "ите", "или", "ыли", "ей", "уй", "ил", "ыл", "им", "ым",
    "ен", "ило", "ыло", "ено", "ят", "ует", "уют", "ит", "ыт", "ены", "ить", "ыть", "ишь", "ую",
    "ю",
];

const NOUN: &[&str] = &[
    "а", "ев", "ов", "ие", "ье", "е", "иями", "ями", "ами", "еи", "ии", "и", "ией", "ей", "ой",
    "ий", "й", "иям", "ям", "ием", "ем", "ам", "ом", "о", "у", "ах", "иях", "ях", "ы", "ь", "ию",
    "ью", "ю", "ия", "ья", "я",
];

const DERIVATIONAL: &[&str] = &["ост", "ость"];

const SUPERLATIVE: &[&str] = &["ейш", "ейше"];

/// Whether `token` has a letter of the Russian alphabet: the tokens [`stem`] is for.
pub fn is_russian(token: &str) -> bool {
    token.chars().any(|c| matches!(c, 'а'..='я' | 'ё'))
}

/// The Snowball Russian stem of the lowercase `token`, with `ё` read as `е`.
pub fn stem(token: &str) -> String {
    let mut w: Vec<char> = token
        .chars()
        .map(|c| if c == 'ё' { 'е' } else { c })
        .collect();
    let (rv, r2) = regions(&w);
    let mut word = Word { w: &mut w, rv };
    if !word.perfective_gerund() {
        word.strip(&[REFLEXIVE]);
        let _ = word.adjectival() || word.verb() || word.strip(&[NOUN]).is_some();
    }
    if word.ends_with("и") {
        word.w.pop();
    }
    if let Some((_, len)) = word.longest(&[DERIVATIONAL])
        && word.w.len() - len >= r2
    {
        word.cut(len);
    }
    word.tidy_up();
    w.into_iter().collect()
}

/// The starts of RV (after the first vowel) and of R2 (after the next vowel and consonant
/// past the first vowel and consonant); the word's length where the word has none.
fn regions(w: &[char]) -> (usize, usize) {
    let past = |from: usize, vowel: bool| {
        w.get(from..)?
            .iter()
            .position(|c| VOWELS.contains(c) == vowel)
            .map(|i| from + i + 1)
    };
    let Some(rv) = past(0, true) else {
        return (w.len(), w.len());
    };
    let r2 = past(rv, false)
        .and_then(|c| past(c, true))
        .and_then(|c| past(c, false))
        .unwrap_or(w.len());
    (rv, r2)
}

/// A word whose endings are matched and removed inside RV only.
struct Word<'a> {
    w: &'a mut Vec<char>,
    rv: usize,
}

impl Word<'_> {
    fn ends_with(&self, suffix: &str) -> bool {
        let n = suffix.chars().count();
        self.w.len() >= self.rv + n
            && self
                .w
                .iter()
                .rev()
                .zip(suffix.chars().rev())
                .all(|(a, b)| *a == b)
    }

    /// The group and the length of the longest ending of `groups` the word ends with in RV.
    fn longest(&self, groups: &[&[&str]]) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize)> = None;
        for (g, list) in groups.iter().enumerate() {
            for s in *list {
                let n = s.chars().count();
                if self.ends_with(s) && best.is_none_or(|(_, b)| n > b) {
                    best = Some((g, n));
                }
            }
        }
        best
    }

    /// Whether the character before the last `len` ones is `а` or `я`, inside RV.
    fn after_a(&self, len: usize) -> bool {
        let end = self.w.len() - len;
        end > self.rv && matches!(self.w[end - 1], 'а' | 'я')
    }

    fn cut(&mut self, len: usize) {
        let end = self.w.len() - len;
        self.w.truncate(end);
    }

    /// Removes the longest ending of `groups`; its group, or `None` without one.
    fn strip(&mut self, groups: &[&[&str]]) -> Option<usize> {
        let (g, len) = self.longest(groups)?;
        self.cut(len);
        Some(g)
    }

    /// Removes the longest ending of `after_a` and `any` when it is of `any` or follows `а` or
    /// `я`; false when there is none or it does not.
    fn strip_after_a(&mut self, after_a: &[&str], any: &[&str]) -> bool {
        match self.longest(&[after_a, any]) {
            Some((g, len)) if g == 1 || self.after_a(len) => {
                self.cut(len);
                true
            }
            _ => false,
        }
    }

    fn perfective_gerund(&mut self) -> bool {
        self.strip_after_a(GERUND_AFTER_A, GERUND)
    }

    fn adjectival(&mut self) -> bool {
        if self.strip(&[ADJECTIVE]).is_none() {
            return false;
        }
        self.strip_after_a(PARTICIPLE_AFTER_A, PARTICIPLE);
        true
    }

    fn verb(&mut self) -> bool {
        self.strip_after_a(VERB_AFTER_A, VERB)
    }

    /// `ейш(е)` goes and a following `нн` becomes `н`; `нн` becomes `н`; `ь` goes.
    fn tidy_up(&mut self) {
        match self.longest(&[SUPERLATIVE, &["н"], &["ь"]]) {
            Some((0, len)) => {
                self.cut(len);
                if self.ends_with("нн") {
                    self.w.pop();
                }
            }
            Some((1, _)) if self.ends_with("нн") => {
                self.w.pop();
            }
            Some((2, _)) => {
                self.w.pop();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pairs of the published vocabulary and output of the algorithm.
    fn assert_stems(pairs: &[(&str, &str)]) {
        for (form, want) in pairs {
            assert_eq!(stem(form), *want, "{form}");
        }
    }

    #[test]
    fn noun_endings_go() {
        assert_stems(&[
            ("книгами", "книг"),
            ("окнах", "окн"),
            ("армией", "арм"),
            ("балалайками", "балалайк"),
            ("деревня", "деревн"),
        ]);
        assert_eq!(stem("стоматологу"), stem("стоматологи"));
        assert_eq!(stem("стоматологу"), "стоматолог");
    }

    #[test]
    fn gerunds_reflexives_and_verbs_go() {
        assert_stems(&[
            ("бросившись", "брос"),
            ("ворвавшись", "ворва"),
            ("прочитав", "прочита"),
            ("купаться", "купа"),
            ("улыбнулась", "улыбнул"),
            ("читаешь", "чита"),
        ]);
    }

    #[test]
    fn adjectives_participles_and_superlatives_go() {
        assert_stems(&[
            ("длинная", "длин"),
            ("бушующих", "буш"),
            ("бегущий", "бегущ"),
            ("стеклянный", "стекля"),
            ("главнейшая", "главн"),
            ("новейшее", "нов"),
            ("благонамереннейший", "благонамерен"),
        ]);
    }

    #[test]
    fn ost_goes_only_in_r2() {
        assert_stems(&[
            ("бездарность", "бездарн"),
            ("влюбленности", "влюблен"),
            ("бедность", "бедност"),
            ("гордость", "гордост"),
        ]);
    }

    #[test]
    fn yo_reads_as_ye_and_other_tokens_stay() {
        assert_stems(&[("взлёт", "взлет"), ("ёлка", "елк")]);
        for t in ["dentist", "2048", "кт", ""] {
            assert_eq!(stem(t), t);
        }
    }
}
