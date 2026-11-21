//! One ASCII spelling of a text, so that Latin and Cyrillic spellings of a name meet. Text is
//! lowercased and split into tokens, maximal runs of letters and digits; each token maps letter
//! by letter through [`LETTERS`], other Latin letters with diacritics fold to their base letter
//! and combining marks drop. Any other character stays as it is.

/// Cyrillic letters (Russian and Serbian) and the Latin letters with carons of Serbian, with
/// their ASCII spelling.
const LETTERS: [(char, &str); 44] = [
    ('а', "a"),
    ('б', "b"),
    ('в', "v"),
    ('г', "g"),
    ('д', "d"),
    ('е', "e"),
    ('ё', "e"),
    ('ж', "z"),
    ('з', "z"),
    ('и', "i"),
    ('й', "j"),
    ('к', "k"),
    ('л', "l"),
    ('м', "m"),
    ('н', "n"),
    ('о', "o"),
    ('п', "p"),
    ('р', "r"),
    ('с', "s"),
    ('т', "t"),
    ('у', "u"),
    ('ф', "f"),
    ('х', "h"),
    ('ц', "c"),
    ('ч', "c"),
    ('ш', "s"),
    ('щ', "sc"),
    ('ъ', ""),
    ('ы', "y"),
    ('ь', ""),
    ('э', "e"),
    ('ю', "ju"),
    ('я', "ja"),
    ('ђ', "dj"),
    ('ј', "j"),
    ('љ', "lj"),
    ('њ', "nj"),
    ('ћ', "c"),
    ('џ', "dz"),
    ('č', "c"),
    ('ć', "c"),
    ('š', "s"),
    ('ž', "z"),
    ('đ', "dj"),
];

/// Lowercase Latin letters whose canonical decomposition is a base letter and marks.
const FOLDS: [(&str, char); 19] = [
    ("àáâãäåāăąǎ", 'a'),
    ("çĉċ", 'c'),
    ("ď", 'd'),
    ("èéêëēĕėęě", 'e'),
    ("ĝğġģ", 'g'),
    ("ĥ", 'h'),
    ("ìíîïĩīĭįǐ", 'i'),
    ("ĵ", 'j'),
    ("ķ", 'k'),
    ("ĺļľ", 'l'),
    ("ñńņň", 'n'),
    ("òóôõöōŏő", 'o'),
    ("ŕŗř", 'r'),
    ("śŝş", 's'),
    ("ţť", 't'),
    ("ùúûüũūŭůűų", 'u'),
    ("ŵ", 'w'),
    ("ýÿŷ", 'y'),
    ("źż", 'z'),
];

/// The ASCII spelling of one lowercase token.
pub fn translit(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for c in token.chars() {
        if is_mark(c) {
            continue;
        }
        if let Some((_, to)) = LETTERS.iter().find(|(from, _)| *from == c) {
            out.push_str(to);
        } else if let Some((_, base)) = FOLDS.iter().find(|(from, _)| from.contains(c)) {
            out.push(*base);
        } else {
            out.push(c);
        }
    }
    out
}

/// The tokens of `text`: lowercased maximal runs of letters and digits, with the combining
/// marks that follow a letter.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut token = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || (is_mark(c) && !token.is_empty()) {
            token.extend(c.to_lowercase());
            compose_short_i(&mut token);
        } else if !token.is_empty() {
            out.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        out.push(token);
    }
    out
}

/// The transliterated tokens of `text` joined by single spaces.
pub fn key(text: &str) -> String {
    tokens(text)
        .iter()
        .map(|t| translit(t))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_mark(c: char) -> bool {
    ('\u{300}'..='\u{36f}').contains(&c)
}

/// `и` and a combining breve are `й`, a letter of its own with its own spelling.
fn compose_short_i(token: &mut String) {
    if token.ends_with("и\u{306}") {
        token.truncate(token.len() - "и\u{306}".len());
        token.push('й');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyrillic_and_latin_spellings_of_a_name_share_a_key() {
        assert_eq!(key("Мира Вельде"), "mira velde");
        assert_eq!(key("  MIRA   velde! "), "mira velde");
        assert_eq!(key("Тарас Щукин"), "taras scukin");
        assert_eq!(key("Юлия Ёжикова"), "julija ezikova");
        assert_eq!(key("Подъезд"), "podezd");
    }

    #[test]
    fn serbian_letters_and_diacritics_fold_to_the_base_letter() {
        assert_eq!(key("Ђорђе Љубић"), "djordje ljubic");
        assert_eq!(key("Đorđe Ljubić"), "djordje ljubic");
        assert_eq!(key("Žiga Šolc"), "ziga solc");
        assert_eq!(key("Renée Brontë"), "renee bronte");
    }

    #[test]
    fn combining_marks_join_their_letter() {
        assert_eq!(key("Rene\u{301}e"), "renee");
        assert_eq!(key("Андреи\u{306}"), key("Андрей"));
        assert_eq!(tokens("Rene\u{301}e"), ["rene\u{301}e"]);
    }

    #[test]
    fn tokens_are_runs_of_letters_and_digits() {
        assert_eq!(
            tokens("Kafe-Nova_2, дом 7"),
            ["kafe", "nova", "2", "дом", "7"]
        );
        assert_eq!(key("!!"), "");
        assert_eq!(translit("ωμεγα"), "ωμεγα");
    }
}
