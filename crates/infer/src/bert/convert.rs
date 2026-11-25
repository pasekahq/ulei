//! The converter from an fp32 `safetensors` checkpoint to the engine's int8 weight file
//! (ADR-0086 section 2). Pure computation: the caller reads the files and writes the result.
//!
//! What the file holds, under the checkpoint's tensor names without their prefix:
//!
//! - every linear weight of the encoder layers (Q, K, V, attention output, FFN in and out) as
//!   int8 `name.weight` `[out, in]` with one fp32 scale per output channel `name.scale` `[out]`
//!   (symmetric, [`kernels::quantize`] per row); biases fp32;
//! - the token table as int8 `embeddings.word_embeddings.weight` `[vocab, hidden]` whose row
//!   `r` is token id `row_order[r]`, its per-row scales `embeddings.word_embeddings.scale`
//!   `[vocab]` in the same row order, and the map `embeddings.word_embeddings.row_of_id`
//!   `[vocab]` (`u32`) from token id to row;
//! - position and token type embeddings and every `LayerNorm` fp32, as in the checkpoint;
//! - the tokenizer section of [`crate::text::convert`] (tensors `tokenizer.*`), built from the
//!   checkpoint's `tokenizer.json` (ADR-0086 E2).
//!
//! The header's `meta` holds [`FORMAT`], the checkpoint's `config.json` as an object, the
//! pooling, the activation block size B of ADR-0086 section 3, the tokenizer's object
//! (`meta.tokenizer`) and the caller's provenance object (the SHA-256 of the checkpoint, of the
//! row order and of `tokenizer.json`). The output is a function of its inputs only, so its
//! SHA-256 identifies the model and its tokenizer.

use std::io::Write;

use serde_json::{Value, json};

use super::params::{TABLE, Tensors};
use super::{Config, Pooling};
use crate::kernels;
use crate::text::{self, Pipeline};
use crate::weights::Checkpoint;
use crate::weights::file::{self, Data, Entry};
use crate::{Error, Result};

/// The value of `meta.format` in a file this module writes. `/2` (ADR-0086 E2) carries the
/// tokenizer section; `/1` files are refused.
pub const FORMAT: &str = "ulei-infer/bert-int8/2";
/// Key of the format name in `meta`.
pub const META_FORMAT: &str = "format";
/// Key of the checkpoint's `config.json` (an object) in `meta`.
pub const META_CONFIG: &str = "config";
/// Key of the pooling name in `meta`.
pub const META_POOLING: &str = "pooling";
/// Key of the activation block size in `meta`.
pub const META_BLOCK: &str = "act_block";
/// Key of the caller's provenance object in `meta`.
pub const META_SOURCE: &str = "source";

/// The activation block size of ADR-0086 section 3 (B = 32 for every model, fixed after the
/// bge-m3 simulation) unless the caller picks another; the file header carries the one used.
pub const DEFAULT_BLOCK: usize = 32;

/// The choices of one conversion.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    /// Pooling of the model (not in `config.json`).
    pub pooling: Pooling,
    /// Activation block size B, at least 1.
    pub act_block: usize,
    /// Row order of the token table: row `r` holds token id `row_order[r]`; a permutation of
    /// `0..vocab_size`.
    pub row_order: &'a [u32],
    /// Provenance recorded as `meta.source` (checkpoint, row order and tokenizer SHA-256).
    pub source: Value,
    /// The text of the checkpoint's `tokenizer.json`.
    pub tokenizer_json: &'a str,
    /// Which reference pipeline the tokenizer reproduces.
    pub pipeline: Pipeline,
    /// Longest input in tokens, `<s>` and `</s>` included; at most the model's positions.
    pub max_tokens: usize,
}

/// The truncation length of every model today (ADR-0086 section 1: at most 512 tokens).
pub const DEFAULT_MAX_TOKENS: usize = 512;

/// One tensor of the output, owned until it is written.
enum Out {
    F32(Vec<f32>),
    I8(Vec<i8>),
    U32(Vec<u32>),
    Text(text::convert::Owned),
}

/// Quantises the rows of `m` (`rows x width`) symmetric int8, one scale per row; `order`
/// picks the source row of each output row.
fn quantize_rows_of(
    m: &[f32],
    width: usize,
    order: impl Iterator<Item = usize>,
) -> (Vec<i8>, Vec<f32>) {
    let mut q = vec![0i8; m.len()];
    let mut scales = Vec::with_capacity(m.len() / width);
    for (dst, src) in q.chunks_exact_mut(width).zip(order) {
        scales.push(kernels::quantize(&m[src * width..(src + 1) * width], dst));
    }
    (q, scales)
}

/// Converts `checkpoint` with its `config.json` text `config_json` and writes the weight file
/// to `out`; returns the bytes written.
///
/// # Errors
/// [`Error::Config`] for a bad config, block size or row order, [`Error::Weights`] for a
/// missing tensor or a shape that disagrees with the config, [`Error::Io`] if writing fails.
pub fn convert<W: Write>(
    checkpoint: &Checkpoint,
    config_json: &str,
    opts: &Options<'_>,
    w: W,
) -> Result<u64> {
    let c = Config::from_json(config_json, opts.pooling)?;
    let config: Value = serde_json::from_str(config_json)
        .map_err(|e| Error::Config(format!("config.json: {e}")))?;
    if opts.act_block == 0 {
        return Err(Error::Config("activation block size 0".into()));
    }
    let (vocab, h, ffn) = (c.vocab_size, c.hidden_size, c.intermediate_size);
    if opts.row_order.len() != vocab {
        return Err(Error::Config(format!(
            "row order has {} ids, the vocabulary {vocab}",
            opts.row_order.len()
        )));
    }
    let row_of_id = row_map(opts.row_order, vocab)?;
    if opts.max_tokens > c.max_tokens() {
        return Err(Error::Config(format!(
            "max_tokens {} exceeds the model's {} positions",
            opts.max_tokens,
            c.max_tokens()
        )));
    }
    let tokenizer = text::convert::section(opts.tokenizer_json, opts.pipeline, opts.max_tokens)?;
    let token_vocab = tokenizer.meta["vocab_size"].as_u64().unwrap_or(u64::MAX);
    if token_vocab > vocab as u64 {
        return Err(Error::Config(format!(
            "the tokenizer's {token_vocab} ids exceed the model's vocabulary of {vocab}"
        )));
    }
    let mut out = Collect {
        t: Tensors::checkpoint(checkpoint)?,
        tensors: Vec::new(),
    };
    let word = out.t.get(&format!("{TABLE}.weight"), &[vocab, h])?;
    let (rows, scales) = quantize_rows_of(word, h, opts.row_order.iter().map(|&id| id as usize));
    out.push(format!("{TABLE}.weight"), vec![vocab, h], Out::I8(rows));
    out.push(format!("{TABLE}.scale"), vec![vocab], Out::F32(scales));
    out.push(
        format!("{TABLE}.row_of_id"),
        vec![vocab],
        Out::U32(row_of_id),
    );
    out.copy(
        "embeddings.position_embeddings.weight",
        &[c.max_positions, h],
    )?;
    out.copy(
        "embeddings.token_type_embeddings.weight",
        &[c.type_vocab_size, h],
    )?;
    out.copy("embeddings.LayerNorm.weight", &[h])?;
    out.copy("embeddings.LayerNorm.bias", &[h])?;
    for i in 0..c.num_layers {
        let p = format!("encoder.layer.{i}");
        out.linear(&format!("{p}.attention.self.query"), h, h)?;
        out.linear(&format!("{p}.attention.self.key"), h, h)?;
        out.linear(&format!("{p}.attention.self.value"), h, h)?;
        out.linear(&format!("{p}.attention.output.dense"), h, h)?;
        out.copy(&format!("{p}.attention.output.LayerNorm.weight"), &[h])?;
        out.copy(&format!("{p}.attention.output.LayerNorm.bias"), &[h])?;
        out.linear(&format!("{p}.intermediate.dense"), ffn, h)?;
        out.linear(&format!("{p}.output.dense"), h, ffn)?;
        out.copy(&format!("{p}.output.LayerNorm.weight"), &[h])?;
        out.copy(&format!("{p}.output.LayerNorm.bias"), &[h])?;
    }
    for (name, shape, data) in tokenizer.tensors {
        out.push(name, shape, Out::Text(data));
    }
    let tensors = out.tensors;

    let meta = json!({
        META_FORMAT: FORMAT,
        META_CONFIG: config,
        META_POOLING: opts.pooling.name(),
        META_BLOCK: opts.act_block,
        text::META_TOKENIZER: tokenizer.meta,
        META_SOURCE: opts.source,
    });
    let entries: Vec<Entry<'_>> = tensors
        .iter()
        .map(|(name, shape, data)| Entry {
            name: name.clone(),
            shape: shape.clone(),
            data: match data {
                Out::F32(d) => Data::F32(d),
                Out::I8(d) => Data::I8(d),
                Out::U32(d) | Out::Text(text::convert::Owned::U32(d)) => Data::U32(d),
                Out::Text(text::convert::Owned::U8(d)) => Data::U8(d),
                Out::Text(text::convert::Owned::F64(d)) => Data::F64(d),
            },
        })
        .collect();
    file::write(w, &meta, &entries)
}

/// The id -> row map of `order`, checked to be a permutation of `0..vocab`.
fn row_map(order: &[u32], vocab: usize) -> Result<Vec<u32>> {
    if order.len() != vocab {
        return Err(Error::Config(format!(
            "row order has {} ids, the vocabulary {vocab}",
            order.len()
        )));
    }
    let mut row_of_id = vec![u32::MAX; vocab];
    for (row, &id) in order.iter().enumerate() {
        let slot = usize::try_from(id)
            .ok()
            .and_then(|i| row_of_id.get_mut(i))
            .filter(|s| **s == u32::MAX)
            .ok_or_else(|| Error::Config(format!("row order: id {id} out of range or repeated")))?;
        *slot = u32::try_from(row).map_err(|_| Error::Config("vocabulary too large".into()))?;
    }
    Ok(row_of_id)
}

/// The output tensors, collected in file order from the checkpoint's tensors.
struct Collect<'w> {
    t: Tensors<'w>,
    tensors: Vec<(String, Vec<usize>, Out)>,
}

impl Collect<'_> {
    fn push(&mut self, name: String, shape: Vec<usize>, data: Out) {
        self.tensors.push((name, shape, data));
    }

    /// Copies the fp32 tensor `name` of `shape`.
    fn copy(&mut self, name: &str, shape: &[usize]) -> Result<()> {
        let data = self.t.get(name, shape)?.to_vec();
        self.push(name.to_string(), shape.to_vec(), Out::F32(data));
        Ok(())
    }

    /// The linear layer `name`: int8 weight per output channel, its scales, the fp32 bias.
    fn linear(&mut self, name: &str, out: usize, inputs: usize) -> Result<()> {
        let w = self.t.get(&format!("{name}.weight"), &[out, inputs])?;
        let (q, s) = quantize_rows_of(w, inputs, 0..out);
        self.push(format!("{name}.weight"), vec![out, inputs], Out::I8(q));
        self.push(format!("{name}.scale"), vec![out], Out::F32(s));
        self.copy(&format!("{name}.bias"), &[out])
    }
}
