//! Memory and start-up probe of the int8 encoder (ADR-0086 E4); a diagnostic, not part of the
//! library.
//!
//! ```text
//! mem_probe WEIGHT_FILE INPUTS_JSON VECTORS_OUT
//! ```
//!
//! `INPUTS_JSON` is `{"golden": [input, ...], "texts": [input, ...]}`, each input a list of token
//! ids or a string (then the file's tokenizer makes the ids, ADR-0086 E2). The probe opens the
//! weight file, binds the encoder, embeds the first golden input, the rest of the golden inputs,
//! then the texts, and after each step prints one JSON line on stdout with the process's memory
//! from `/proc/self/smaps_rollup` and `/proc/self/status`, the weight file's own mapping from
//! `/proc/self/smaps`, the resident pages of the main tensor groups from `/proc/self/pagemap`,
//! and the peak anonymous and dirty memory sampled every 5 ms while the step ran (unless
//! `MEM_PROBE_NO_SAMPLER` is set, for timing runs). All sizes in
//! KiB. The pooled vectors are written to `VECTORS_OUT` as little-endian `f32`, in input order.
//! Linux only.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ulei_infer::bert::Encoder;
use ulei_infer::text;
use ulei_infer::weights::file::WeightFile;

const PAGE: usize = 4096;

/// `key: value kB` lines of a `/proc` file, for the keys in `keys`.
fn kib_fields(text: &str, keys: &[&str]) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if keys.contains(&k) {
            let n = v.split_whitespace().next().and_then(|n| n.parse().ok());
            if let Some(n) = n {
                out.insert(k.to_string(), n);
            }
        }
    }
    out
}

fn rollup() -> BTreeMap<String, u64> {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    kib_fields(
        &text,
        &[
            "Rss",
            "Pss",
            "Pss_Anon",
            "Pss_File",
            "Shared_Clean",
            "Shared_Dirty",
            "Private_Clean",
            "Private_Dirty",
            "Anonymous",
            "Swap",
        ],
    )
}

fn status() -> BTreeMap<String, u64> {
    let text = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    kib_fields(
        &text,
        &["VmRSS", "RssAnon", "RssFile", "VmHWM", "VmData", "VmSize"],
    )
}

/// The `/proc/self/smaps` entries backed by `path`, summed.
fn mapping_of(path: &Path) -> BTreeMap<String, u64> {
    let text = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
    let name = path.to_string_lossy();
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    let mut inside = false;
    let mut vmas = 0;
    for line in text.lines() {
        let first = line.split_whitespace().next().unwrap_or("");
        if first.contains('-') && first.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            inside = line.ends_with(name.as_ref());
            vmas += u64::from(inside);
            continue;
        }
        if inside {
            for (k, v) in kib_fields(
                line,
                &[
                    "Size",
                    "Rss",
                    "Pss",
                    "Private_Clean",
                    "Private_Dirty",
                    "Shared_Clean",
                ],
            ) {
                *out.entry(k).or_default() += v;
            }
        }
    }
    out.insert("vmas".into(), vmas);
    out
}

/// KiB of `bytes` at `ptr` present in this process's page tables.
fn resident(pagemap: &mut File, ptr: *const u8, bytes: usize) -> u64 {
    let first = ptr as usize / PAGE;
    let last = (ptr as usize + bytes).div_ceil(PAGE);
    let mut buf = vec![0u8; (last - first) * 8];
    if pagemap
        .seek(SeekFrom::Start((first * 8) as u64))
        .and_then(|_| pagemap.read_exact(&mut buf))
        .is_err()
    {
        return 0;
    }
    let pages = buf
        .as_chunks::<8>()
        .0
        .iter()
        .filter(|e| u64::from_le_bytes(**e) >> 63 == 1)
        .count();
    (pages * PAGE / 1024) as u64
}

/// Peak `Anonymous` and `Private_Dirty` of the process while a step runs.
struct Sampler {
    stop: Arc<AtomicBool>,
    anon: Arc<AtomicU64>,
    dirty: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    fn start() -> Self {
        let off = std::env::var_os("MEM_PROBE_NO_SAMPLER").is_some();
        let stop = Arc::new(AtomicBool::new(false));
        let anon = Arc::new(AtomicU64::new(0));
        let dirty = Arc::new(AtomicU64::new(0));
        let (s, a, d) = (stop.clone(), anon.clone(), dirty.clone());
        let thread = (!off).then(|| {
            std::thread::spawn(move || {
                loop {
                    let r = rollup();
                    a.fetch_max(r.get("Anonymous").copied().unwrap_or(0), Ordering::Relaxed);
                    d.fetch_max(
                        r.get("Private_Dirty").copied().unwrap_or(0),
                        Ordering::Relaxed,
                    );
                    if s.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        });
        Self {
            stop,
            anon,
            dirty,
            thread,
        }
    }

    fn finish(mut self) -> (u64, u64) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        (
            self.anon.load(Ordering::Relaxed),
            self.dirty.load(Ordering::Relaxed),
        )
    }
}

struct Probe {
    /// Pages of the token table's rows and of their scales that the inputs so far need.
    needed_rows: BTreeSet<usize>,
    needed_scales: BTreeSet<usize>,
    path: PathBuf,
    pagemap: File,
    started: Instant,
    ranges: Vec<(&'static str, *const u8, usize)>,
}

impl Probe {
    fn report(&mut self, phase: &str, inputs: usize, tokens: usize, peak: Option<(u64, u64)>) {
        let groups: BTreeMap<&str, Value> = self
            .ranges
            .iter()
            .map(|&(name, ptr, bytes)| {
                let r = resident(&mut self.pagemap, ptr, bytes);
                (name, json!({"kib": bytes / 1024, "resident_kib": r}))
            })
            .collect();
        let line = json!({
            "phase": phase,
            "ms": self.started.elapsed().as_secs_f64() * 1e3,
            "inputs": inputs,
            "tokens": tokens,
            "rollup": rollup(),
            "status": status(),
            "weight_mapping": mapping_of(&self.path),
            "tensor_groups": groups,
            "table_needed_kib": {
                "rows": self.needed_rows.len() * PAGE / 1024,
                "scales": self.needed_scales.len() * PAGE / 1024,
            },
            "peak_during_step": peak.map(|(a, d)| json!({"Anonymous": a, "Private_Dirty": d})),
        });
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

/// Records the pages of the table rows and scales that `ids` read.
fn need(probe: &mut Probe, ids: &[u32], table: (&[i8], &[f32], &[u32]), width: usize) {
    let (rows, scales, map) = table;
    for &id in ids {
        let r = map[id as usize] as usize;
        let row = rows.as_ptr() as usize + r * width;
        probe.needed_rows.insert(row / PAGE);
        probe.needed_rows.insert((row + width - 1) / PAGE);
        probe
            .needed_scales
            .insert((scales.as_ptr() as usize + r * 4) / PAGE);
    }
}

/// An input: token ids, or a text the file's tokenizer encodes.
enum Input {
    Ids(Vec<u32>),
    Text(String),
}

fn inputs_of(v: &Value) -> Vec<Input> {
    v.as_array()
        .expect("an array of inputs")
        .iter()
        .map(|input| match input {
            Value::String(t) => Input::Text(t.clone()),
            ids => Input::Ids(
                ids.as_array()
                    .expect("an array of ids or a string")
                    .iter()
                    .map(|i| u32::try_from(i.as_u64().expect("an id")).expect("a u32 id"))
                    .collect(),
            ),
        })
        .collect()
}

/// The ids of `input`.
fn ids(encoder: &Encoder<'_>, input: &Input) -> Vec<u32> {
    match input {
        Input::Ids(ids) => ids.clone(),
        Input::Text(t) => encoder
            .tokenizer()
            .expect("a tokenizer")
            .encode(t)
            .expect("encode"),
    }
}

/// Start and length of the tokenizer section: the file's last tensors, contiguous but for
/// their 64-byte alignment.
fn tokenizer_range(weights: &WeightFile) -> (*const u8, usize) {
    let trie = weights
        .u32(text::CHARSMAP_TRIE)
        .expect("tokenizer section")
        .data;
    let scores = weights
        .f64(text::PIECE_SCORES)
        .expect("tokenizer section")
        .data;
    let start = trie.as_ptr().cast::<u8>();
    let end = scores.as_ptr_range().end.cast::<u8>();
    (start, end as usize - start as usize)
}

fn main() {
    let started = Instant::now();
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [weights_path, inputs_path, vectors_path] = args.as_slice() else {
        panic!("usage: mem_probe WEIGHT_FILE INPUTS_JSON VECTORS_OUT");
    };
    let path = std::fs::canonicalize(weights_path).expect("weight file");
    let mut probe = Probe {
        needed_rows: BTreeSet::new(),
        needed_scales: BTreeSet::new(),
        path: path.clone(),
        pagemap: File::open("/proc/self/pagemap").expect("pagemap"),
        started,
        ranges: Vec::new(),
    };
    probe.report("start", 0, 0, None);

    let weights = WeightFile::open(&path).expect("open the weight file");
    let table = "embeddings.word_embeddings";
    let rows = weights.i8(&format!("{table}.weight")).expect("table").data;
    let scales = weights.f32(&format!("{table}.scale")).expect("scales").data;
    let map = weights
        .u32(&format!("{table}.row_of_id"))
        .expect("map")
        .data;
    probe
        .ranges
        .push(("table_rows", rows.as_ptr().cast(), rows.len()));
    probe
        .ranges
        .push(("table_scales", scales.as_ptr().cast(), scales.len() * 4));
    probe
        .ranges
        .push(("row_of_id", map.as_ptr().cast(), map.len() * 4));
    let mut linear = 0usize;
    for name in weights.names() {
        if name.starts_with("encoder.")
            && name.ends_with(".weight")
            && let Ok(w) = weights.i8(name)
        {
            linear += w.data.len();
        }
    }
    // The linear weights are contiguous in the file: from the first layer's first weight on.
    let first = weights
        .i8("encoder.layer.0.attention.self.query.weight")
        .expect("layer 0")
        .data;
    probe
        .ranges
        .push(("layers_from_first_linear", first.as_ptr().cast(), linear));
    let (start, bytes) = tokenizer_range(&weights);
    probe.ranges.push(("tokenizer", start, bytes));
    probe.report("open", 0, 0, None);

    let encoder = Encoder::from_file(&weights).expect("bind the encoder");
    probe.report("bind", 0, 0, None);

    let inputs: Value =
        serde_json::from_str(&std::fs::read_to_string(inputs_path).expect("inputs file"))
            .expect("inputs are JSON");
    let golden = inputs_of(&inputs["golden"]);
    let texts = inputs_of(&inputs["texts"]);
    let mut vectors = Vec::new();
    let mut tokens = 0;
    let mut done = 0;

    let sampler = Sampler::start();
    let width = encoder.config().hidden_size;
    let first_ids = ids(&encoder, &golden[0]);
    need(&mut probe, &first_ids, (rows, scales, map), width);
    let v = encoder.embed(&first_ids).expect("embed");
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "{}",
            json!({"phase": "first_vector", "ms": started.elapsed().as_secs_f64() * 1e3})
        );
        let _ = out.flush();
    }
    vectors.extend_from_slice(&v);
    tokens += first_ids.len();
    done += 1;
    probe.report("after_1", done, tokens, Some(sampler.finish()));

    for (label, set) in [("after_16_golden", &golden[1..]), ("after_texts", &texts)] {
        let sampler = Sampler::start();
        for input in set {
            let ids = ids(&encoder, input);
            need(&mut probe, &ids, (rows, scales, map), width);
            vectors.extend_from_slice(&encoder.embed(&ids).expect("embed"));
            tokens += ids.len();
            done += 1;
        }
        probe.report(label, done, tokens, Some(sampler.finish()));
    }

    let bytes: Vec<u8> = vectors.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(vectors_path, bytes).expect("write the vectors");
}
