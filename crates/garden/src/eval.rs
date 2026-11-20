//! Retrieval evaluation: a Markdown corpus is ingested as `document` items with one fact per
//! bounded passage (`source_ref` = relative path), gold questions are searched, and recall@k
//! and MRR are measured at file level together with warm query latency.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

use crate::embed::Embedder;
use crate::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind,
    TtlKind, UnixTime,
};
use crate::search::{SearchError, SearchParams, Searcher, rrf};
use crate::store::{Store, StoreError};

/// Failure of an evaluation run.
#[derive(Debug)]
pub enum EvalError {
    /// A gold or corpus file is unreadable or malformed.
    Input(String),
    /// The store failed.
    Store(StoreError),
    /// The search failed.
    Search(SearchError),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(m) => write!(f, "input error: {m}"),
            Self::Store(e) => write!(f, "{e}"),
            Self::Search(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EvalError {}

impl From<StoreError> for EvalError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<SearchError> for EvalError {
    fn from(e: SearchError) -> Self {
        Self::Search(e)
    }
}

/// One gold question (the fields the evaluation reads).
#[derive(Clone, Debug, PartialEq)]
pub struct GoldRow {
    /// Identifier.
    pub id: String,
    /// The question as asked.
    pub query: String,
    /// Category for the per-category breakdown.
    pub category: String,
    /// Where the answer is expected to come from; only `garden` rows are scored.
    pub expected_source: String,
    /// Corpus-relative paths of files holding the answer.
    pub gold_paths: Vec<String>,
}

impl GoldRow {
    /// Whether the row is scored: answered by the garden and labelled with files.
    pub fn scored(&self) -> bool {
        self.expected_source == "garden" && !self.gold_paths.is_empty()
    }
}

/// Parses gold JSONL; blank lines are skipped, unknown fields ignored.
///
/// # Errors
///
/// [`EvalError::Input`] naming the line of a malformed row.
pub fn parse_gold(text: &str) -> Result<Vec<GoldRow>, EvalError> {
    let mut rows = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = |m: &str| EvalError::Input(format!("gold line {}: {m}", n + 1));
        let v: Value = serde_json::from_str(line).map_err(|e| bad(&e.to_string()))?;
        let field = |k: &str| -> Result<String, EvalError> {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| bad(&format!("missing text field {k:?}")))
        };
        let gold_paths = match v.get("gold_paths") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|p| p.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| bad("gold_paths must hold strings"))?,
            Some(_) => return Err(bad("gold_paths must be a list")),
        };
        rows.push(GoldRow {
            id: field("id")?,
            query: field("query")?,
            category: field("category")?,
            expected_source: field("expected_source")?,
            gold_paths,
        });
    }
    Ok(rows)
}

/// Descriptions of documents listed in an `INDEX.md` (`- path.md — description`).
pub fn index_descriptions(index: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in index.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- ") else {
            continue;
        };
        let rest = rest.trim_start();
        let (path, tail) = match rest.strip_prefix('`') {
            Some(r) => match r.split_once('`') {
                Some((p, t)) => (p, t),
                None => continue,
            },
            None => match rest.split_once(char::is_whitespace) {
                Some((p, t)) => (p, t),
                None => continue,
            },
        };
        if !is_markdown(path) || path.contains(char::is_whitespace) {
            continue;
        }
        let tail = tail.trim_start();
        let Some(desc) = ["—", "–", "-"]
            .iter()
            .find_map(|d| tail.strip_prefix(d))
            .and_then(|d| d.strip_prefix(char::is_whitespace))
        else {
            continue;
        };
        out.insert(path.to_owned(), desc.trim().trim_matches('"').to_owned());
    }
    out
}

/// Front matter fields a passage repeats as context.
const META_KEYS: [&str; 4] = ["name:", "description:", "aliases:", "tags:"];

/// Splits a Markdown document into passages of at most `max_chars` body characters (at least
/// 200). Each passage repeats a context of the path, selected front matter and `description`
/// (at most 600 characters) and its heading trail; no content is dropped.
pub fn passages(path: &str, text: &str, description: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(200);
    let text = text.replace("\r\n", "\n");
    let (meta, body) = split_front_matter(&text);
    let metadata: Vec<&str> = meta
        .lines()
        .filter(|l| META_KEYS.iter().any(|k| l.starts_with(k)))
        .collect();
    let mut context_parts = vec![path];
    context_parts.extend(&metadata);
    context_parts.push(description);
    let context = truncate_chars(context_parts.join("\n").trim(), 600);

    let mut out: Vec<String> = Vec::new();
    let mut headings: Vec<String> = Vec::new();
    let mut pending = String::new();
    let mut pending_heading = String::new();
    let flush = |out: &mut Vec<String>, pending: &mut String, heading: &str| {
        if !pending.trim().is_empty() {
            let parts: Vec<&str> = [context.as_str(), heading, pending.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect();
            out.push(parts.join("\n\n"));
        }
        pending.clear();
    };
    for block in blocks(body) {
        let mut block = block.trim();
        if block.is_empty() {
            continue;
        }
        if let Some((depth, title, rest)) = heading(block) {
            flush(&mut out, &mut pending, &pending_heading);
            headings.truncate(depth - 1);
            headings.push(title.to_owned());
            block = rest.trim();
        }
        let current = truncate_chars(&headings.join(" > "), 300);
        let chars: Vec<char> = block.chars().collect();
        for piece in chars.chunks(max_chars) {
            let fragment: String = piece.iter().collect();
            let len = |s: &str| s.chars().count();
            if !pending.is_empty() && len(&pending) + len(&fragment) + 2 > max_chars {
                flush(&mut out, &mut pending, &pending_heading);
            }
            pending_heading.clone_from(&current);
            if !pending.is_empty() {
                pending.push_str("\n\n");
            }
            pending.push_str(&fragment);
        }
    }
    flush(&mut out, &mut pending, &pending_heading);
    if out.is_empty() && (!metadata.is_empty() || !description.is_empty() || !headings.is_empty()) {
        out.push(format!("{context}\n{}", headings.join(" > ")));
    }
    out
}

/// Whether `path` names a Markdown file.
pub fn is_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Front matter (between leading `---` lines) and the rest.
fn split_front_matter(text: &str) -> (&str, &str) {
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        let body = &rest[end + 4..];
        return (&rest[..end], body.trim_start_matches('\n'));
    }
    ("", text)
}

/// Blocks split at blank lines and before every heading line.
fn blocks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in body.lines() {
        if line.trim().is_empty() || heading(line).is_some() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if line.trim().is_empty() {
                continue;
            }
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `(depth, title, rest)` of a block starting with an ATX heading.
fn heading(block: &str) -> Option<(usize, &str, &str)> {
    let depth = block.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&depth) {
        return None;
    }
    let after = &block[depth..];
    if !after.starts_with([' ', '\t']) {
        return None;
    }
    let (line, rest) = after.split_once('\n').unwrap_or((after, ""));
    let title = line.trim();
    (!title.is_empty()).then_some((depth, title, rest))
}

/// Name and aliases of a document: front matter `name:` / `aliases:`, else the first heading,
/// else the file stem.
fn document_names(path: &str, text: &str) -> (String, Vec<String>) {
    let (meta, body) = split_front_matter(text);
    let mut name = None;
    let mut aliases = Vec::new();
    let mut in_aliases = false;
    for line in meta.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().trim_matches(['"', '\'']).to_owned());
            in_aliases = false;
        } else if let Some(v) = line.strip_prefix("aliases:") {
            let v = v.trim().trim_start_matches('[').trim_end_matches(']');
            aliases.extend(
                v.split(',')
                    .map(|a| a.trim().trim_matches(['"', '\'']).to_owned())
                    .filter(|a| !a.is_empty()),
            );
            in_aliases = true;
        } else if in_aliases && let Some(a) = line.trim_start().strip_prefix("- ") {
            aliases.push(a.trim().trim_matches(['"', '\'']).to_owned());
        } else {
            in_aliases = false;
        }
    }
    let name = name
        .filter(|n| !n.is_empty())
        .or_else(|| {
            body.lines()
                .find_map(|l| heading(l).map(|(_, t, _)| t.to_owned()))
        })
        .unwrap_or_else(|| {
            Path::new(path)
                .file_stem()
                .map_or_else(|| path.to_owned(), |s| s.to_string_lossy().into_owned())
        });
    (name, aliases)
}

/// What was ingested.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Corpus {
    /// Fact id to the corpus-relative path it came from.
    pub fact_paths: HashMap<FactId, String>,
    /// Fact id to the ordinal of its passage within its document (from 0).
    pub ordinals: HashMap<FactId, usize>,
    /// Documents ingested.
    pub files: usize,
    /// Passages the store refused (for example because they look like they carry a secret).
    pub rejected: usize,
}

/// Ingests `files` (relative to `root`) as `document` items, one fact per passage of at most
/// `max_chars` body characters. An `INDEX.md` at the root supplies descriptions and is not
/// ingested itself. Facts are valid and recorded from `at` and marked public; only the
/// store's secret heuristic stands between a passage and the index, so a store on disk holds
/// the corpus in clear. Passages the store refuses as invalid are skipped and counted.
///
/// # Errors
///
/// [`EvalError::Input`] for an unreadable file, [`EvalError::Store`] when the store fails.
pub fn ingest(
    store: &mut Store,
    root: &Path,
    files: &[String],
    max_chars: usize,
    at: UnixTime,
) -> Result<Corpus, EvalError> {
    let read = |rel: &str| {
        std::fs::read_to_string(root.join(rel)).map_err(|e| EvalError::Input(format!("{rel}: {e}")))
    };
    let descriptions = if root.join("INDEX.md").is_file() {
        index_descriptions(&read("INDEX.md")?)
    } else {
        HashMap::new()
    };
    let mut corpus = Corpus::default();
    for rel in files {
        if rel == "INDEX.md" {
            continue;
        }
        let text = read(rel)?;
        let (name, aliases) = document_names(rel, &text);
        // Paths can exceed the id limit of the store; the path itself is the `source_ref`.
        let doc = format!("doc:{:016x}", fnv1a64(rel.as_bytes()));
        let entity = EntityId(doc.clone());
        store.upsert_entity(&Entity {
            id: entity.clone(),
            kind: EntityKind::Item,
            name,
            aliases,
            domain: None,
            attrs: Attrs::None,
        })?;
        let desc = descriptions.get(rel).map_or("", String::as_str);
        for (i, passage) in passages(rel, &text, desc, max_chars)
            .into_iter()
            .enumerate()
        {
            let id = FactId(format!("{doc}#{i}"));
            let inserted = store.insert_fact(&Fact {
                id: id.clone(),
                entity: entity.clone(),
                text: passage,
                domain: None,
                attribute: None,
                ttl: TtlKind::General,
                valid_from: at,
                valid_to: None,
                recorded_at: at,
                superseded_at: None,
                superseded_by: None,
                provenance: Provenance {
                    kind: SourceKind::Raw,
                    source_ref: rel.clone(),
                    observed_at: at,
                    quote: None,
                    proof_count: 1,
                },
                expires_at: None,
                sensitivity: Sensitivity::Public,
                confidence: 1.0,
            });
            match inserted {
                Ok(()) => {
                    corpus.ordinals.insert(id.clone(), i);
                    corpus.fact_paths.insert(id, rel.clone());
                }
                Err(StoreError::Invalid(_)) => corpus.rejected += 1,
                Err(e) => return Err(e.into()),
            }
        }
        corpus.files += 1;
    }
    Ok(corpus)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Corpus-relative paths of every `*.md` file under `root`, sorted.
///
/// # Errors
///
/// [`EvalError::Input`] when a directory cannot be read.
pub fn markdown_files(root: &Path) -> Result<Vec<String>, EvalError> {
    let mut out = Vec::new();
    let mut dirs = vec![PathBuf::new()];
    while let Some(rel) = dirs.pop() {
        let dir = root.join(&rel);
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| EvalError::Input(format!("{}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| EvalError::Input(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = rel.join(&name);
            let ty = entry
                .file_type()
                .map_err(|e| EvalError::Input(e.to_string()))?;
            if ty.is_dir() && !name.starts_with('.') {
                dirs.push(child);
            } else if ty.is_file() && is_markdown(&name) {
                out.push(child.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// File-level ranking: each source's facts are mapped to their paths and the path rankings
/// fused by RRF, as the reference prototype fuses files.
fn file_ranking(
    corpus: &Corpus,
    dense: &[FactId],
    lexical: &[FactId],
    p: &SearchParams,
) -> Result<Vec<String>, SearchError> {
    let paths = |ids: &[FactId]| -> Vec<String> {
        ids.iter()
            .filter_map(|id| corpus.fact_paths.get(id).cloned())
            .collect()
    };
    let (d, l) = (paths(dense), paths(lexical));
    let dw = if d.is_empty() { 0.0 } else { p.dense_weight };
    let lw = if l.is_empty() { 0.0 } else { p.lexical_weight };
    if dw == 0.0 && lw == 0.0 {
        return Ok(Vec::new());
    }
    Ok(rrf(&[(dw, &d), (lw, &l)], p.rrf_k)?
        .into_iter()
        .map(|(path, _)| path)
        .collect())
}

/// Metrics of one backend over the scored rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metrics {
    /// Scored rows.
    pub n: usize,
    /// Mean share of gold files among the first `k` files.
    pub recall: f64,
    /// Mean reciprocal rank of the first gold file (0 when absent).
    pub mrr: f64,
    /// Per category: rows, recall, MRR.
    pub per_category: BTreeMap<String, (usize, f64, f64)>,
    /// Warm query latencies in milliseconds, in row order.
    pub latencies_ms: Vec<f64>,
}

impl Metrics {
    /// Nearest-rank percentile `q` in `(0, 1]` of the latencies; 0 without rows.
    pub fn latency_percentile(&self, q: f64) -> f64 {
        let mut v = self.latencies_ms.clone();
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(f64::total_cmp);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let i = ((q * v.len() as f64).ceil() as usize).clamp(1, v.len());
        v[i - 1]
    }

    fn add(&mut self, category: &str, recall: f64, rr: f64, ms: f64) {
        self.n += 1;
        self.recall += recall;
        self.mrr += rr;
        let c = self
            .per_category
            .entry(category.to_owned())
            .or_insert((0, 0.0, 0.0));
        c.0 += 1;
        c.1 += recall;
        c.2 += rr;
        self.latencies_ms.push(ms);
    }

    #[allow(clippy::cast_precision_loss)]
    fn finish(&mut self) {
        if self.n > 0 {
            self.recall /= self.n as f64;
            self.mrr /= self.n as f64;
        }
        for c in self.per_category.values_mut() {
            c.1 /= c.0 as f64;
            c.2 /= c.0 as f64;
        }
    }

    /// The metrics as JSON.
    pub fn to_json(&self, k: usize) -> Value {
        let cats: serde_json::Map<String, Value> = self
            .per_category
            .iter()
            .map(|(c, (n, r, m))| {
                (
                    c.clone(),
                    json!({ "n": n, format!("recall@{k}"): r, "mrr": m }),
                )
            })
            .collect();
        json!({
            "n": self.n,
            format!("recall@{k}"): self.recall,
            "mrr": self.mrr,
            "latency_ms": { "p50": self.latency_percentile(0.5), "p95": self.latency_percentile(0.95) },
            "per_category": cats,
        })
    }
}

/// `(recall@k, reciprocal rank)` of one ranked file list against gold files.
pub fn score(files: &[String], gold: &[String], k: usize) -> (f64, f64) {
    if gold.is_empty() {
        return (0.0, 0.0);
    }
    let hits = gold
        .iter()
        .filter(|g| files.iter().take(k).any(|f| f == *g))
        .count();
    let rr = files
        .iter()
        .position(|f| gold.contains(f))
        .map_or(0.0, |i| {
            1.0 / f64::from(u32::try_from(i + 1).unwrap_or(u32::MAX))
        });
    #[allow(clippy::cast_precision_loss)]
    (hits as f64 / gold.len() as f64, rr)
}

/// Lexical-only and (with an embedder) hybrid metrics of the scored rows of `gold`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// Lexical candidates alone.
    pub lexical: Metrics,
    /// RRF of dense and lexical; `None` without an embedder.
    pub hybrid: Option<Metrics>,
}

/// Searches every scored row once after one warm-up query and scores the file rankings.
///
/// # Errors
///
/// [`EvalError::Search`] when a search fails.
pub fn evaluate(
    store: &Store,
    embedder: Option<&dyn Embedder>,
    corpus: &Corpus,
    gold: &[GoldRow],
    p: &SearchParams,
) -> Result<Outcome, EvalError> {
    let searcher = Searcher::new(store, embedder);
    let lexical_only = SearchParams {
        dense_weight: 0.0,
        ..p.clone()
    };
    if let Some(first) = gold.iter().find(|r| r.scored()) {
        searcher.candidates(&first.query, p)?;
    }
    let mut out = Outcome {
        lexical: Metrics::default(),
        hybrid: embedder.map(|_| Metrics::default()),
    };
    for row in gold.iter().filter(|r| r.scored()) {
        let t0 = Instant::now();
        let lex = searcher.candidates(&row.query, &lexical_only)?;
        let lex_ids: Vec<FactId> = lex.lexical.into_iter().map(|(id, _)| id).collect();
        let files = file_ranking(corpus, &[], &lex_ids, &lexical_only)?;
        let lex_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let (r, rr) = score(&files, &row.gold_paths, p.k);
        out.lexical.add(&row.category, r, rr, lex_ms);
        if let Some(h) = out.hybrid.as_mut() {
            let t1 = Instant::now();
            let c = searcher.candidates(&row.query, p)?;
            let ids = |v: Vec<(FactId, f32)>| v.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
            let files = file_ranking(corpus, &ids(c.dense), &ids(c.lexical), p)?;
            let ms = t1.elapsed().as_secs_f64() * 1000.0;
            let (r, rr) = score(&files, &row.gold_paths, p.k);
            h.add(&row.category, r, rr, ms);
        }
    }
    out.lexical.finish();
    if let Some(h) = out.hybrid.as_mut() {
        h.finish();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passages_repeat_context_and_split_long_blocks() {
        let doc = "---\nname: Cafe Moss\nowner: x\n---\n# Cafe\n\nSoup.\n\n## Visits\n\n"
            .to_owned()
            + &"a".repeat(450);
        let ps = passages("places/moss.md", &doc, "favourite cafe", 200);
        assert_eq!(ps.len(), 4, "{ps:#?}");
        assert!(
            ps[0].starts_with("places/moss.md\nname: Cafe Moss\nfavourite cafe\n\nCafe\n\nSoup.")
        );
        assert!(!ps[0].contains("owner"), "only selected front matter");
        assert!(ps[1].contains("Cafe > Visits"));
        let body: usize = ps[1..].iter().map(|p| p.matches('a').count()).sum();
        assert!(body >= 450, "nothing dropped");
    }

    #[test]
    fn passages_of_metadata_only_document() {
        let ps = passages("x.md", "---\nname: X\n---\n", "", 1200);
        assert_eq!(ps, vec!["x.md\nname: X\n".to_owned()]);
        assert_eq!(passages("y.md", "", "", 1200), Vec::<String>::new());
    }

    #[test]
    fn index_lines_give_descriptions() {
        let d = index_descriptions(
            "# Index\n- `a/b.md` — first\n- c.md - \"second\"\n- not a link\n- d.txt — no\n",
        );
        assert_eq!(d.len(), 2);
        assert_eq!(d["a/b.md"], "first");
        assert_eq!(d["c.md"], "second");
    }

    #[test]
    fn gold_rows_parse_and_reject_bad_lines() {
        let rows = parse_gold(
            "{\"id\":\"1\",\"query\":\"q\",\"category\":\"c\",\"expected_source\":\"garden\",\
             \"gold_paths\":[\"a.md\"],\"extra\":1}\n\n\
             {\"id\":\"2\",\"query\":\"q\",\"category\":\"c\",\"expected_source\":\"internet\"}\n",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].scored() && !rows[1].scored());
        let err = parse_gold("{\"id\":\"1\"}\n").unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(parse_gold("{\"id\":\"1\",\"query\":\"q\",\"category\":\"c\",\"expected_source\":\"g\",\"gold_paths\":\"a\"}").is_err());
    }

    #[test]
    fn scores_count_distinct_gold_files() {
        let files: Vec<String> = ["x", "a", "y", "b"].map(str::to_owned).into();
        let gold: Vec<String> = ["a", "b"].map(str::to_owned).into();
        assert_eq!(score(&files, &gold, 3), (0.5, 0.5));
        assert_eq!(score(&files, &gold, 4), (1.0, 0.5));
        assert_eq!(score(&[], &gold, 3), (0.0, 0.0));
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let m = Metrics {
            latencies_ms: (1..=20).map(f64::from).collect(),
            ..Metrics::default()
        };
        assert!((m.latency_percentile(0.5) - 10.0).abs() < f64::EPSILON);
        assert!((m.latency_percentile(0.95) - 19.0).abs() < f64::EPSILON);
        assert!(Metrics::default().latency_percentile(0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn names_come_from_front_matter_or_heading() {
        let (n, a) = document_names(
            "p/x.md",
            "---\nname: \"Ilsa\"\naliases:\n  - Il\n  - 'I.'\n---\n# H\n",
        );
        assert_eq!(
            (n.as_str(), a),
            ("Ilsa", vec!["Il".to_owned(), "I.".to_owned()])
        );
        let (n, a) = document_names("p/x.md", "intro\n## Heading two\n");
        assert_eq!((n.as_str(), a.len()), ("Heading two", 0));
        assert_eq!(document_names("p/x.md", "plain").0, "x");
    }
}
