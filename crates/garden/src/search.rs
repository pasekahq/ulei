//! Hybrid retrieval over the garden: lexical candidates from the store's FTS5 index and dense
//! candidates from cosine over stored vectors, fused by weighted reciprocal rank (RRF). Without
//! an embedder the search is lexical only. Passive pointers (one line per fact, no text) are
//! built on the same ranking with a score threshold.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;

use crate::embed::{EmbedError, Embedder, dot};
use crate::model::{FactId, UnixTime};
use crate::store::{Store, StoreError};

/// Knobs of one hybrid search. Defaults are the promoted profile of the reference prototype.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchParams {
    /// Results to return.
    pub k: usize,
    /// Candidates taken from each source before fusion.
    pub candidates: usize,
    /// RRF constant: a hit at rank `r` adds `weight / (rrf_k + r)`.
    pub rrf_k: f32,
    /// Weight of the dense ranking.
    pub dense_weight: f32,
    /// Weight of the lexical ranking.
    pub lexical_weight: f32,
    /// Clean the lexical query (drop request words and e-mail addresses, add loanword forms).
    pub rewrite: bool,
    /// World time the facts must hold at.
    pub valid_at: UnixTime,
    /// Transaction time: what the garden knew then (`as_of`).
    pub known_at: UnixTime,
    /// Whether private facts may be returned.
    pub include_private: bool,
}

impl SearchParams {
    /// The default profile at time `now` on both axes.
    pub fn at(now: UnixTime) -> Self {
        Self {
            k: 3,
            candidates: 100,
            rrf_k: 0.0,
            dense_weight: 1.0,
            lexical_weight: 1.0,
            rewrite: true,
            valid_at: now,
            known_at: now,
            include_private: false,
        }
    }
}

/// Failure of a search.
#[derive(Debug)]
pub enum SearchError {
    /// The store failed.
    Store(StoreError),
    /// The embedder failed.
    Embed(EmbedError),
    /// A parameter is out of range.
    Invalid(String),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "search: {e}"),
            Self::Embed(e) => write!(f, "search: {e}"),
            Self::Invalid(m) => write!(f, "search: {m}"),
        }
    }
}

impl std::error::Error for SearchError {}

impl From<StoreError> for SearchError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<EmbedError> for SearchError {
    fn from(e: EmbedError) -> Self {
        Self::Embed(e)
    }
}

/// Weighted reciprocal rank fusion. Each ranking counts an item once, at its first position
/// (rank from 1); the fused score is the sum of `weight / (k + rank)` over rankings with a
/// non-zero weight. Best first, ties by the item's order.
///
/// # Errors
///
/// [`SearchError::Invalid`] when `k` is negative or not finite, or the weights are negative,
/// not finite or all zero.
pub fn rrf<T: Clone + Eq + Hash + Ord>(
    rankings: &[(f32, &[T])],
    k: f32,
) -> Result<Vec<(T, f32)>, SearchError> {
    if !k.is_finite() || k < 0.0 {
        return Err(SearchError::Invalid(
            "RRF k must be finite and non-negative".into(),
        ));
    }
    let weights_ok = rankings.iter().all(|(w, _)| w.is_finite() && *w >= 0.0)
        && rankings.iter().any(|(w, _)| *w > 0.0);
    if !weights_ok {
        return Err(SearchError::Invalid(
            "RRF weights must be finite, non-negative and not all zero".into(),
        ));
    }
    let mut scores: HashMap<&T, f32> = HashMap::new();
    for (weight, items) in rankings {
        if *weight == 0.0 {
            continue;
        }
        let mut seen = std::collections::HashSet::new();
        let mut rank = 0_u32;
        for item in *items {
            if seen.insert(item) {
                rank += 1;
                #[allow(clippy::cast_precision_loss)] // ranks stay far below 2^24
                let r = rank as f32;
                *scores.entry(item).or_insert(0.0) += weight / (k + r);
            }
        }
    }
    let mut out: Vec<(T, f32)> = scores.into_iter().map(|(t, s)| (t.clone(), s)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

/// Request and grammar words that only broaden an OR query (Russian), kept from the reference
/// prototype. Content words, names, numbers and negation stay.
const STOP: &[&str] = &[
    "а",
    "и",
    "или",
    "но",
    "в",
    "во",
    "на",
    "по",
    "у",
    "с",
    "со",
    "к",
    "ко",
    "до",
    "от",
    "для",
    "из",
    "за",
    "про",
    "об",
    "о",
    "через",
    "мой",
    "моя",
    "моё",
    "твой",
    "твоя",
    "мы",
    "вы",
    "я",
    "ты",
    "он",
    "она",
    "оно",
    "они",
    "мне",
    "меня",
    "тебя",
    "его",
    "её",
    "их",
    "нам",
    "нас",
    "ваш",
    "этот",
    "это",
    "тот",
    "то",
    "такой",
    "как",
    "какой",
    "какая",
    "какие",
    "когда",
    "где",
    "сколько",
    "который",
    "что",
    "чтобы",
    "если",
    "есть",
    "быть",
    "был",
    "была",
    "были",
    "будет",
    "тут",
    "там",
    "вот",
    "уже",
    "ещё",
    "еще",
    "просто",
    "пожалуйста",
    "дай",
    "дать",
    "найти",
    "найди",
    "скинь",
    "скинуть",
    "пришли",
    "отправь",
    "добавь",
];

/// Generic technical slang and its standard forms; no person aliases or saved answers.
const LOANWORDS: &[(&str, &str)] = &[
    ("сервак", "сервер server"),
    ("юзер", "пользователь user"),
    ("фаервол", "firewall"),
    ("фаерволл", "firewall"),
    ("файрвол", "firewall"),
    ("пдф", "pdf"),
    ("ворк", "work"),
    ("пермит", "permit"),
    ("телега", "telegram"),
    ("линк", "ссылка link"),
    ("репо", "репозиторий repository"),
];

/// Casefolded runs of letters and digits.
pub fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Lexical form of a query: e-mail addresses and request words dropped, duplicates removed,
/// loanword forms appended. Falls back to the original query when nothing would remain.
pub fn clean_query(query: &str) -> String {
    let without_email: Vec<&str> = query.split_whitespace().filter(|w| !is_email(w)).collect();
    let mut kept: Vec<String> = Vec::new();
    for t in tokens(&without_email.join(" ")) {
        if !STOP.contains(&t.as_str()) && !kept.contains(&t) {
            kept.push(t);
        }
    }
    if kept.is_empty() {
        return query.to_owned();
    }
    let mut extra: Vec<&str> = Vec::new();
    for t in &kept {
        if let Some((_, forms)) = LOANWORDS.iter().find(|(w, _)| w == t)
            && !extra.contains(forms)
        {
            extra.push(forms);
        }
    }
    let mut out = kept.join(" ");
    for e in extra {
        out.push(' ');
        out.push_str(e);
    }
    out
}

fn is_email(word: &str) -> bool {
    let w = word.trim_matches(|c: char| !c.is_alphanumeric());
    match w.split_once('@') {
        Some((user, host)) => {
            !user.is_empty() && host.contains('.') && !host.starts_with('.') && !host.ends_with('.')
        }
        None => false,
    }
}

/// One fused result.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// The fact.
    pub fact: FactId,
    /// Fused RRF score, higher = better.
    pub score: f32,
}

/// Ranked candidates of each source before fusion, best first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Candidates {
    /// From the FTS5 index, with bm25-derived scores.
    pub lexical: Vec<(FactId, f32)>,
    /// From cosine over stored vectors; empty without an embedder.
    pub dense: Vec<(FactId, f32)>,
}

/// Hybrid search over one store. Dense ranking is brute force over every vector that holds at
/// the requested times; fine up to some ten thousand facts.
pub struct Searcher<'a> {
    store: &'a Store,
    embedder: Option<&'a dyn Embedder>,
}

impl<'a> Searcher<'a> {
    /// Searcher over `store`; lexical only when `embedder` is `None`.
    pub fn new(store: &'a Store, embedder: Option<&'a dyn Embedder>) -> Self {
        Self { store, embedder }
    }

    /// Candidates of each source for `query`, `p.candidates` per source.
    ///
    /// # Errors
    ///
    /// [`SearchError`] when the store or the embedder fails.
    pub fn candidates(&self, query: &str, p: &SearchParams) -> Result<Candidates, SearchError> {
        let lexical_query = if p.rewrite {
            clean_query(query)
        } else {
            query.to_owned()
        };
        let lexical = self.store.lexical(
            &lexical_query,
            p.candidates,
            p.valid_at,
            p.known_at,
            p.include_private,
        )?;
        let dense = match self.embedder {
            Some(e) if p.dense_weight > 0.0 => {
                // The dense side sees the original text, as in the prototype.
                let q = e.embed_query(query)?;
                let mut scored: Vec<(FactId, f32)> = self
                    .store
                    .vectors(e.model_id(), p.valid_at, p.known_at, p.include_private)?
                    .into_iter()
                    .map(|(id, v)| {
                        let s = dot(&q, &v);
                        (id, s)
                    })
                    .collect();
                scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                scored.truncate(p.candidates);
                scored
            }
            _ => Vec::new(),
        };
        Ok(Candidates { lexical, dense })
    }

    /// Best `p.k` facts for `query` by RRF of the dense and the lexical ranking.
    ///
    /// # Errors
    ///
    /// [`SearchError`] when the store or the embedder fails or the weights are invalid.
    pub fn search(&self, query: &str, p: &SearchParams) -> Result<Vec<Hit>, SearchError> {
        let c = self.candidates(query, p)?;
        let mut hits = fuse(&c, p)?;
        hits.truncate(p.k);
        Ok(hits)
    }
}

/// RRF of the candidates of both sources with the weights of `p`, dense first.
///
/// # Errors
///
/// [`SearchError::Invalid`] for invalid weights or `rrf_k`.
pub fn fuse(c: &Candidates, p: &SearchParams) -> Result<Vec<Hit>, SearchError> {
    let dense: Vec<FactId> = c.dense.iter().map(|(id, _)| id.clone()).collect();
    let lexical: Vec<FactId> = c.lexical.iter().map(|(id, _)| id.clone()).collect();
    let dense_weight = if dense.is_empty() {
        0.0
    } else {
        p.dense_weight
    };
    if dense_weight == 0.0 && (lexical.is_empty() || p.lexical_weight == 0.0) {
        return Ok(Vec::new());
    }
    Ok(rrf(
        &[(dense_weight, &dense), (p.lexical_weight, &lexical)],
        p.rrf_k,
    )?
    .into_iter()
    .map(|(fact, score)| Hit { fact, score })
    .collect())
}

/// Embeds every fact of `store` that has no vector of `embedder`'s model yet, `batch` texts at
/// a time; returns how many were embedded.
///
/// # Errors
///
/// [`SearchError`] when the store or the embedder fails.
pub fn index_missing(
    store: &mut Store,
    embedder: &dyn Embedder,
    batch: usize,
) -> Result<usize, SearchError> {
    index_missing_where(store, embedder, batch, &|_| true)
}

/// [`index_missing`] restricted to the facts `keep` accepts.
///
/// # Errors
///
/// [`SearchError`] when the store or the embedder fails.
pub fn index_missing_where(
    store: &mut Store,
    embedder: &dyn Embedder,
    batch: usize,
    keep: &dyn Fn(&FactId) -> bool,
) -> Result<usize, SearchError> {
    let mut missing = store.missing_vectors(embedder.model_id())?;
    missing.retain(|(id, _)| keep(id));
    for chunk in missing.chunks(batch.max(1)) {
        let texts: Vec<&str> = chunk.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = embedder.embed_passages(&texts)?;
        if vectors.len() != chunk.len() {
            return Err(SearchError::Embed(EmbedError::Run(format!(
                "{} vectors for {} texts",
                vectors.len(),
                chunk.len()
            ))));
        }
        for ((id, _), v) in chunk.iter().zip(&vectors) {
            store.put_vector(id, embedder.model_id(), v)?;
        }
    }
    Ok(missing.len())
}

/// A passive pointer: "there is a fact about X", without the fact's text.
#[derive(Clone, Debug, PartialEq)]
pub struct Pointer {
    /// The fact to fetch with the active search.
    pub fact: FactId,
    /// One line naming the entity the fact is about.
    pub line: String,
    /// Fused score.
    pub score: f32,
}

/// Tier (b) of retrieval: up to `n` pointers for `query` whose fused score reaches
/// `min_score`, one per entity, best first. No LLM; facts' texts are not returned.
///
/// # Errors
///
/// [`SearchError`] when the store or the embedder fails.
pub fn pointers(
    searcher: &Searcher<'_>,
    query: &str,
    p: &SearchParams,
    n: usize,
    min_score: f32,
) -> Result<Vec<Pointer>, SearchError> {
    let c = searcher.candidates(query, p)?;
    let mut out: Vec<Pointer> = Vec::new();
    let mut entities = Vec::new();
    for hit in fuse(&c, p)? {
        if out.len() == n || hit.score < min_score {
            break;
        }
        let Some(fact) = searcher.store.fact(&hit.fact)? else {
            continue;
        };
        if entities.contains(&fact.entity) {
            continue;
        }
        let name = searcher
            .store
            .entity(&fact.entity)?
            .map_or_else(|| fact.entity.0.clone(), |e| e.name);
        out.push(Pointer {
            line: format!("there is a fact about {name} ({})", hit.fact.0),
            fact: hit.fact,
            score: hit.score,
        });
        entities.push(fact.entity);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_sums_weighted_reciprocal_ranks() {
        let dense = ["a", "b", "c"];
        let lexical = ["b", "d"];
        let out = rrf(&[(1.0, &dense[..]), (1.0, &lexical[..])], 0.0).unwrap();
        // b: 1/2 + 1/1, a: 1/1, d: 1/2, c: 1/3
        let names: Vec<&str> = out.iter().map(|(t, _)| *t).collect();
        assert_eq!(names, ["b", "a", "d", "c"]);
        assert!((out[0].1 - 1.5).abs() < 1e-6);
    }

    #[test]
    fn rrf_counts_first_occurrence_and_breaks_ties_by_item() {
        let one = ["x", "x", "y"];
        let out = rrf(&[(1.0, &one[..])], 60.0).unwrap();
        assert!((out[1].1 - 1.0 / 62.0).abs() < 1e-7, "y is rank 2, not 3");
        let a = ["p"];
        let b = ["o"];
        let tie = rrf(&[(1.0, &a[..]), (1.0, &b[..])], 0.0).unwrap();
        assert_eq!(tie[0].0, "o");
    }

    #[test]
    fn rrf_validates_parameters() {
        let a = ["p"];
        assert!(rrf(&[(1.0, &a[..])], -1.0).is_err());
        assert!(rrf(&[(1.0, &a[..])], f32::NAN).is_err());
        assert!(rrf(&[(0.0, &a[..])], 0.0).is_err());
        assert!(rrf(&[(-1.0, &a[..]), (1.0, &a[..])], 0.0).is_err());
        let zero_weight_source = rrf(&[(0.0, &a[..]), (2.0, &["q"][..])], 0.0).unwrap();
        assert_eq!(zero_weight_source, vec![("q", 2.0)]);
    }

    #[test]
    fn cleanup_drops_request_words_and_emails() {
        assert_eq!(
            clean_query("Найди, пожалуйста, мой договор аренды"),
            "договор аренды"
        );
        assert_eq!(
            clean_query("письмо от a.b@example.org про счёт"),
            "письмо счёт"
        );
        assert_eq!(clean_query("где мой"), "где мой", "falls back when empty");
        assert_eq!(clean_query("dentist Dentist"), "dentist");
    }

    #[test]
    fn cleanup_appends_loanword_forms() {
        assert_eq!(
            clean_query("какой сервак у юзер"),
            "сервак юзер сервер server пользователь user"
        );
    }
}
