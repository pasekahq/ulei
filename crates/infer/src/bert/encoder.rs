//! The model: the embedding block, the encoder layers and pooling bound to one checkpoint.

use super::embeddings::Embeddings;
use super::layer::Layer;
use super::params::Tensors;
use super::{Config, Pooling, Prefixes, Workspace, convert, pooling};
use crate::text::Tokenizer;
use crate::weights::Checkpoint;
use crate::weights::file::WeightFile;
use crate::{Error, Result};

/// One model ready to run: its config and weights borrowed from a [`Checkpoint`] (fp32) or a
/// [`WeightFile`] (int8, with its tokenizer).
#[derive(Debug)]
pub struct Encoder<'w> {
    config: Config,
    embeddings: Embeddings<'w>,
    layers: Vec<Layer<'w>>,
    tokenizer: Option<Tokenizer<'w>>,
    prefixes: Prefixes,
}

/// The result of one forward pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Forward {
    /// Number of tokens.
    pub tokens: usize,
    /// Final token states, `tokens x hidden_size` row-major.
    pub hidden: Vec<f32>,
    /// With intermediates kept: the embedding block's output, then each layer's output in
    /// order (the last equals `hidden`). Empty otherwise.
    pub intermediates: Vec<Vec<f32>>,
}

impl<'w> Encoder<'w> {
    /// Binds `config` to the tensors of `checkpoint`, checking every shape.
    ///
    /// # Errors
    /// [`Error::Weights`] if a tensor is missing or its shape disagrees with the config.
    pub fn new(config: Config, checkpoint: &'w Checkpoint) -> Result<Self> {
        Self::bind(config, &Tensors::checkpoint(checkpoint)?)
    }

    /// Binds the engine's int8 weight file (written by [`convert::convert`]): the config,
    /// pooling, input prefixes and activation block size come from its header, the weights and
    /// the tokenizer from its tensors.
    ///
    /// # Errors
    /// [`Error::Config`] if the header's format (a file of an older format included: convert the
    /// model again), config, pooling, prefixes or block size is missing or bad, [`Error::Weights`] if a tensor or the tokenizer
    /// section is missing, or a shape or the tokenizer's size disagrees with the config.
    pub fn from_file(file: &'w WeightFile) -> Result<Self> {
        let meta = file.meta();
        let format = meta.get(convert::META_FORMAT).and_then(|v| v.as_str());
        if format != Some(convert::FORMAT) {
            let family = convert::FORMAT.trim_end_matches(|c: char| c != '/');
            return Err(Error::Config(match format {
                Some(f) if f.starts_with(family) => format!(
                    "weight file format {f:?}, expected {:?}: convert the model again",
                    convert::FORMAT
                ),
                Some(f) => format!(
                    "weight file format {f:?} is not a BERT int8 weight file ({:?})",
                    convert::FORMAT
                ),
                None => "weight file names no format".to_owned(),
            }));
        }
        let prefix = |kind: &str| {
            meta.get(convert::META_PREFIXES)
                .and_then(|p| p.get(kind))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error::Config(format!("weight file has no {kind} prefix")))
        };
        let prefixes = Prefixes {
            query: prefix("query")?,
            passage: prefix("passage")?,
        };
        let pooling = meta
            .get(convert::META_POOLING)
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Config("weight file names no pooling".into()))
            .and_then(Pooling::parse)?;
        let config = meta
            .get(convert::META_CONFIG)
            .ok_or_else(|| Error::Config("weight file holds no config".into()))?;
        let config = Config::from_json(&config.to_string(), pooling)?;
        let block = meta
            .get(convert::META_BLOCK)
            .and_then(serde_json::Value::as_u64)
            .and_then(|b| usize::try_from(b).ok())
            .filter(|&b| b > 0)
            .ok_or_else(|| Error::Config("weight file has no activation block size".into()))?;
        let tokenizer = Tokenizer::from_file(file)?;
        if tokenizer.vocab_size() > config.vocab_size
            || tokenizer.max_tokens() > config.max_tokens()
        {
            return Err(Error::Weights(format!(
                "the tokenizer ({} ids, {} tokens) does not fit the model ({} ids, {} tokens)",
                tokenizer.vocab_size(),
                tokenizer.max_tokens(),
                config.vocab_size,
                config.max_tokens()
            )));
        }
        let mut me = Self::bind(config, &Tensors::File { file, block })?;
        me.tokenizer = Some(tokenizer);
        me.prefixes = prefixes;
        Ok(me)
    }

    fn bind(config: Config, tensors: &Tensors<'w>) -> Result<Self> {
        let mut layers = Vec::with_capacity(config.num_layers);
        for i in 0..config.num_layers {
            layers.push(Layer::bind(tensors, &config, i)?);
        }
        Ok(Self {
            embeddings: Embeddings::bind(tensors, &config)?,
            layers,
            config,
            tokenizer: None,
            prefixes: Prefixes::default(),
        })
    }

    /// The query and passage prefixes of a weight file; empty for a checkpoint (E1).
    pub fn prefixes(&self) -> &Prefixes {
        &self.prefixes
    }

    /// The tokenizer of a weight file; `None` for a checkpoint (E1), which takes ids only.
    pub fn tokenizer(&self) -> Option<&Tokenizer<'w>> {
        self.tokenizer.as_ref()
    }

    /// The model's config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Runs the encoder on one sequence of token ids (special tokens included), in a new
    /// [`Workspace`].
    ///
    /// # Errors
    /// [`Error::Input`] if `ids` is empty, longer than [`Config::max_tokens`] or holds an id
    /// outside the vocabulary.
    pub fn forward(&self, ids: &[u32], keep_intermediates: bool) -> Result<Forward> {
        let mut ws = Workspace::new();
        let mut intermediates = Vec::new();
        self.run(
            &mut ws,
            ids,
            keep_intermediates.then_some(&mut intermediates),
        )?;
        let n = ids.len() * self.config.hidden_size;
        Ok(Forward {
            tokens: ids.len(),
            hidden: ws.hidden[..n].to_vec(),
            intermediates,
        })
    }

    /// The forward pass on `ids` in `ws`, the final token states left in `ws.hidden`; with
    /// `keep`, the embedding block's output and each layer's output pushed to it.
    fn run(
        &self,
        ws: &mut Workspace,
        ids: &[u32],
        mut keep: Option<&mut Vec<Vec<f32>>>,
    ) -> Result<()> {
        let c = &self.config;
        if ids.is_empty() || ids.len() > c.max_tokens() {
            return Err(Error::Input(format!(
                "{} tokens, expected 1 to {}",
                ids.len(),
                c.max_tokens()
            )));
        }
        let n = ids.len() * c.hidden_size;
        self.embeddings
            .apply(ids, c, &mut ws.hidden, &mut ws.word)?;
        if let Some(keep) = keep.as_deref_mut() {
            keep.push(ws.hidden[..n].to_vec());
        }
        for layer in &self.layers {
            layer.apply(ws, ids.len(), c);
            if let Some(keep) = keep.as_deref_mut() {
                keep.push(ws.hidden[..n].to_vec());
            }
        }
        Ok(())
    }

    /// Runs encoder layer `index` alone on `input` (`tokens x hidden_size`), for layer-wise
    /// verification against a reference fed the same input.
    ///
    /// # Errors
    /// [`Error::Input`] if there is no such layer or `input` is not `tokens` rows.
    pub fn forward_layer(&self, index: usize, input: &[f32], tokens: usize) -> Result<Vec<f32>> {
        let layer = self
            .layers
            .get(index)
            .ok_or_else(|| Error::Input(format!("no layer {index}")))?;
        if tokens == 0 || input.len() != tokens * self.config.hidden_size {
            return Err(Error::Input(format!(
                "{} values are not {tokens} rows of {}",
                input.len(),
                self.config.hidden_size
            )));
        }
        let mut ws = Workspace::new();
        ws.hidden = input.to_vec();
        layer.apply(&mut ws, tokens, &self.config);
        Ok(ws.hidden)
    }

    /// Pools the token states of `f` by the config's pooling and L2-normalises the result.
    pub fn pool(&self, f: &Forward) -> Vec<f32> {
        pooling::pool(
            self.config.pooling,
            &f.hidden,
            f.tokens,
            self.config.hidden_size,
        )
    }

    /// Embeds one sequence in a new [`Workspace`]: as [`Encoder::embed_with`].
    ///
    /// # Errors
    /// As [`Encoder::forward`].
    pub fn embed(&self, ids: &[u32]) -> Result<Vec<f32>> {
        self.embed_with(&mut Workspace::new(), ids)
    }

    /// Embeds one sequence: forward pass, pooling, L2 normalisation, with the activation
    /// buffers of `ws`. A caller that embeds many inputs keeps one workspace for all of them:
    /// once it has grown to the longest input, the forward pass allocates nothing but the
    /// returned vector. The result does not depend on what `ws` held.
    ///
    /// # Errors
    /// As [`Encoder::forward`].
    pub fn embed_with(&self, ws: &mut Workspace, ids: &[u32]) -> Result<Vec<f32>> {
        self.run(ws, ids, None)?;
        let h = self.config.hidden_size;
        Ok(pooling::pool(
            self.config.pooling,
            &ws.hidden[..ids.len() * h],
            ids.len(),
            h,
        ))
    }

    /// Embeds one text in a new [`Workspace`]: as [`Encoder::embed_text_with`].
    ///
    /// # Errors
    /// As [`Encoder::embed_text_with`].
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_text_with(&mut Workspace::new(), text)
    }

    /// Embeds one text: its ids by the file's tokenizer (truncated to its maximum), then
    /// [`Encoder::embed_with`] in `ws`. The text is taken as it is: the caller puts the prefix
    /// of its kind ([`Encoder::prefixes`]) before it.
    ///
    /// # Errors
    /// [`Error::Input`] if the encoder has no tokenizer (bound to a checkpoint), else as
    /// [`crate::text::Tokenizer::encode`] and [`Encoder::forward`].
    pub fn embed_text_with(&self, ws: &mut Workspace, text: &str) -> Result<Vec<f32>> {
        let tokenizer = self
            .tokenizer
            .as_ref()
            .ok_or_else(|| Error::Input("no tokenizer: bound to a checkpoint".into()))?;
        self.embed_with(ws, &tokenizer.encode(text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bert::Pooling;
    use crate::weights::encode;
    use crate::{kernels, ops};

    /// A deterministic pseudo-random fill in [-0.5, 0.5).
    #[allow(clippy::cast_precision_loss)]
    fn fill(seed: u64, n: usize) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 40) as f32 / (1u64 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn tiny_config(model_type: &str, pooling: Pooling) -> Config {
        let json = format!(
            r#"{{"model_type":"{model_type}","hidden_act":"gelu","hidden_size":8,
            "intermediate_size":16,"layer_norm_eps":1e-12,"max_position_embeddings":10,
            "num_attention_heads":2,"num_hidden_layers":2,"pad_token_id":1,
            "type_vocab_size":2,"vocab_size":20}}"#
        );
        Config::from_json(&json, pooling).unwrap()
    }

    fn tiny_checkpoint(prefix: &str) -> Checkpoint {
        let (h, f) = (8, 16);
        let mut owned: Vec<(String, Vec<usize>, Vec<f32>)> = Vec::new();
        let mut seed = 1;
        let mut add = |name: String, shape: Vec<usize>| {
            seed += 1;
            let n = shape.iter().product();
            owned.push((format!("{prefix}{name}"), shape, fill(seed, n)));
        };
        add("embeddings.word_embeddings.weight".into(), vec![20, h]);
        add("embeddings.position_embeddings.weight".into(), vec![10, h]);
        add("embeddings.token_type_embeddings.weight".into(), vec![2, h]);
        add("embeddings.LayerNorm.weight".into(), vec![h]);
        add("embeddings.LayerNorm.bias".into(), vec![h]);
        for i in 0..2 {
            let p = format!("encoder.layer.{i}");
            for (name, out, inp) in [
                ("attention.self.query", h, h),
                ("attention.self.key", h, h),
                ("attention.self.value", h, h),
                ("attention.output.dense", h, h),
                ("intermediate.dense", f, h),
                ("output.dense", h, f),
            ] {
                add(format!("{p}.{name}.weight"), vec![out, inp]);
                add(format!("{p}.{name}.bias"), vec![out]);
            }
            for name in ["attention.output.LayerNorm", "output.LayerNorm"] {
                add(format!("{p}.{name}.weight"), vec![h]);
                add(format!("{p}.{name}.bias"), vec![h]);
            }
        }
        let refs: Vec<(&str, &[usize], &[f32])> = owned
            .iter()
            .map(|(n, s, d)| (n.as_str(), s.as_slice(), d.as_slice()))
            .collect();
        Checkpoint::read(encode(&refs).as_slice()).unwrap()
    }

    #[test]
    fn runs_with_any_prefix_and_keeps_intermediates() {
        let ckpt = tiny_checkpoint("bert.");
        let enc = Encoder::new(tiny_config("bert", Pooling::Mean), &ckpt).unwrap();
        let f = enc.forward(&[0, 5, 19, 2], true).unwrap();
        assert_eq!(f.tokens, 4);
        assert_eq!(f.hidden.len(), 4 * 8);
        assert_eq!(f.intermediates.len(), 3);
        assert_eq!(f.intermediates[2], f.hidden);
        assert!(f.hidden.iter().all(|v| v.is_finite()));
        let pooled = enc.pool(&f);
        let norm: f32 = pooled.iter().map(|v| v * v).sum();
        assert!((norm - 1.0).abs() < 1e-5);
        assert_eq!(enc.embed(&[0, 5, 19, 2]).unwrap(), pooled);
    }

    #[test]
    fn attention_of_one_token_is_its_value() {
        // With one token the softmax is 1, so the context equals the value projection.
        let ckpt = tiny_checkpoint("");
        let enc = Encoder::new(tiny_config("bert", Pooling::Cls), &ckpt).unwrap();
        let x = fill(99, 8);
        let l = &enc.layers[0];
        assert_eq!(l.attention_of(&x, 1, enc.config()), l.value.apply(&x));
    }

    #[test]
    fn cls_pooling_takes_the_first_token() {
        let ckpt = tiny_checkpoint("");
        let enc = Encoder::new(tiny_config("bert", Pooling::Cls), &ckpt).unwrap();
        let f = enc.forward(&[3, 4, 5], false).unwrap();
        assert_eq!(f.intermediates, Vec::<Vec<f32>>::new());
        let mut first = f.hidden[..8].to_vec();
        ops::l2_normalize(&mut first);
        assert_eq!(enc.pool(&f), first);
    }

    #[test]
    fn position_offset_shifts_positions() {
        // XLM-R positions start at pad + 1 = 2, so its first token uses row 2 of the table.
        let ckpt = tiny_checkpoint("roberta.");
        let bert = Encoder::new(tiny_config("bert", Pooling::Mean), &ckpt).unwrap();
        let xlmr = Encoder::new(tiny_config("xlm-roberta", Pooling::Mean), &ckpt).unwrap();
        assert_eq!(xlmr.config().max_tokens(), 8);
        let a = bert.forward(&[7, 8, 9], true).unwrap();
        let b = xlmr.forward(&[7, 8, 9], true).unwrap();
        assert_ne!(a.intermediates[0], b.intermediates[0]);
        assert!(xlmr.forward(&[1; 9], false).is_err());
        assert!(bert.forward(&[1; 10], false).is_ok());
    }

    #[test]
    fn bad_inputs_and_shapes_are_refused() {
        let ckpt = tiny_checkpoint("");
        let enc = Encoder::new(tiny_config("bert", Pooling::Mean), &ckpt).unwrap();
        assert!(matches!(enc.forward(&[], false), Err(Error::Input(_))));
        assert!(matches!(enc.forward(&[20], false), Err(Error::Input(_))));
        let mut wide = tiny_config("bert", Pooling::Mean);
        wide.intermediate_size = 32;
        assert!(matches!(Encoder::new(wide, &ckpt), Err(Error::Weights(_))));
        let empty = Checkpoint::default();
        assert!(Encoder::new(tiny_config("bert", Pooling::Mean), &empty).is_err());
    }

    fn convert_tiny(order: &[u32], block: usize) -> Vec<u8> {
        let ckpt = tiny_checkpoint("bert.");
        let config = r#"{"model_type":"bert","hidden_act":"gelu","hidden_size":8,
            "intermediate_size":16,"layer_norm_eps":1e-12,"max_position_embeddings":10,
            "num_attention_heads":2,"num_hidden_layers":2,"pad_token_id":1,
            "type_vocab_size":2,"vocab_size":20}"#;
        let tokenizer_json = crate::text::tests::tokenizer_json(false);
        let opts = convert::Options {
            pooling: Pooling::Mean,
            prefixes: Prefixes {
                query: "query: ".into(),
                passage: "passage: ".into(),
            },
            act_block: block,
            row_order: order,
            source: serde_json::json!({"test": true}),
            tokenizer_json: &tokenizer_json,
            pipeline: crate::text::Pipeline::WhitespaceSplit,
            max_tokens: 8,
        };
        let mut out = Vec::new();
        let n = convert::convert(&ckpt, config, &opts, &mut out).unwrap();
        assert_eq!(n, out.len() as u64);
        out
    }

    #[test]
    fn int8_file_round_trips_and_runs_close_to_fp32() {
        let order: Vec<u32> = (0..20).rev().collect();
        let bytes = convert_tiny(&order, 3);
        assert_eq!(
            convert_tiny(&order, 3),
            bytes,
            "conversion is deterministic"
        );
        let file = WeightFile::read(bytes.as_slice()).unwrap();
        assert_eq!(file.meta()["act_block"], 3);
        assert_eq!(file.meta()["source"]["test"], true);
        // The table rows follow the order and the map inverts it.
        let ckpt = tiny_checkpoint("bert.");
        let word = ckpt
            .get("bert.embeddings.word_embeddings.weight")
            .unwrap()
            .data;
        let rows = file.i8("embeddings.word_embeddings.weight").unwrap().data;
        let scales = file.f32("embeddings.word_embeddings.scale").unwrap().data;
        let map = file
            .u32("embeddings.word_embeddings.row_of_id")
            .unwrap()
            .data;
        for (id, &row) in map.iter().enumerate() {
            let row = row as usize;
            assert_eq!(order[row] as usize, id);
            let mut q = [0i8; 8];
            let s = kernels::quantize(&word[id * 8..(id + 1) * 8], &mut q);
            assert_eq!(&rows[row * 8..(row + 1) * 8], &q);
            assert_eq!(scales[row].to_bits(), s.to_bits());
        }
        // Every int8 tensor (linear weights and the token table) is in [-127, 127]: the
        // converter quantises them all with `kernels::quantize`.
        for name in file.names() {
            if let Ok(t) = file.i8(name) {
                assert!(!t.data.contains(&i8::MIN), "{name} holds -128");
            }
        }
        // Linear weights: int8 per output channel, biases and norms copied.
        let w = file.i8("encoder.layer.1.output.dense.weight").unwrap();
        assert_eq!(w.shape, &[8, 16]);
        assert_eq!(
            file.f32("encoder.layer.1.output.dense.bias").unwrap().data,
            ckpt.get("bert.encoder.layer.1.output.dense.bias")
                .unwrap()
                .data
        );
        // The int8 encoder runs and stays close to the fp32 one.
        let int8 = Encoder::from_file(&file).unwrap();
        assert_eq!(int8.config().pooling, Pooling::Mean);
        assert_eq!(int8.prefixes().query, "query: ");
        assert_eq!(int8.prefixes().passage, "passage: ");
        let fp32 = Encoder::new(tiny_config("bert", Pooling::Mean), &ckpt).unwrap();
        let ids = [0, 5, 19, 2, 7];
        let a = int8.embed(&ids).unwrap();
        let b = fp32.embed(&ids).unwrap();
        let cos: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!(cos > 0.99, "cosine {cos}");
        assert_ne!(a, b);
        assert!(matches!(int8.forward(&[20], false), Err(Error::Input(_))));
        // Text in: the file's tokenizer, truncated to its maximum, then the same forward pass.
        let t = int8.tokenizer().unwrap();
        assert_eq!(t.max_tokens(), 8);
        let ids = t.encode("ab c <mask>").unwrap();
        assert_eq!(ids, vec![0, 5, 9, 13, 2]);
        assert_eq!(
            int8.embed_text("ab c <mask>").unwrap(),
            int8.embed(&ids).unwrap()
        );
        assert_eq!(t.encode(&"ab ".repeat(100)).unwrap().len(), 8);
        assert!(fp32.tokenizer().is_none());
        assert_eq!(fp32.prefixes(), &Prefixes::default());
        assert!(matches!(fp32.embed_text("ab"), Err(Error::Input(_))));
    }

    #[test]
    fn a_reused_workspace_gives_the_same_bits_and_stops_growing() {
        let order: Vec<u32> = (0..20).collect();
        let file = WeightFile::read(convert_tiny(&order, 4).as_slice()).unwrap();
        let int8 = Encoder::from_file(&file).unwrap();
        let ckpt = tiny_checkpoint("");
        let fp32 = Encoder::new(tiny_config("bert", Pooling::Cls), &ckpt).unwrap();
        let bits = |v: Vec<f32>| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        let (long, short) = ([0, 5, 19, 2, 7, 3, 4, 1], [3, 4]);
        for enc in [&int8, &fp32] {
            let mut ws = Workspace::new();
            let a = bits(enc.embed_with(&mut ws, &long).unwrap());
            let held = ws.bytes();
            assert!(held > 0);
            // Shorter, then the long one again: the bits of a new workspace, no growth.
            assert_eq!(
                bits(enc.embed_with(&mut ws, &short).unwrap()),
                bits(enc.embed(&short).unwrap())
            );
            assert_eq!(bits(enc.embed_with(&mut ws, &long).unwrap()), a);
            assert_eq!(bits(enc.embed(&long).unwrap()), a);
            assert_eq!(ws.bytes(), held);
            // A refused input leaves the workspace usable.
            assert!(enc.embed_with(&mut ws, &[20]).is_err());
            assert_eq!(bits(enc.embed_with(&mut ws, &long).unwrap()), a);
            ws.release();
            assert_eq!(ws.bytes(), 0);
            assert_eq!(
                bits(enc.embed_with(&mut ws, &short).unwrap()),
                bits(enc.embed(&short).unwrap())
            );
        }
    }

    #[test]
    fn bad_conversions_and_files_are_refused() {
        let ckpt = tiny_checkpoint("");
        let config = r#"{"model_type":"bert","hidden_act":"gelu","hidden_size":8,
            "intermediate_size":16,"layer_norm_eps":1e-12,"max_position_embeddings":10,
            "num_attention_heads":2,"num_hidden_layers":2,"pad_token_id":1,
            "type_vocab_size":2,"vocab_size":20}"#;
        let mut repeated: Vec<u32> = (0..20).collect();
        repeated[3] = 4;
        let short: Vec<u32> = (0..19).collect();
        let good: Vec<u32> = (0..20).collect();
        let tokenizer_json = crate::text::tests::tokenizer_json(false);
        let mut big_vocab: serde_json::Value = serde_json::from_str(&tokenizer_json).unwrap();
        big_vocab["added_tokens"][4]["id"] = serde_json::json!(20);
        let big_vocab = big_vocab.to_string();
        for (order, block, json, max) in [
            (&repeated, 64, tokenizer_json.as_str(), 8),
            (&short, 64, tokenizer_json.as_str(), 8),
            (&good, 0, tokenizer_json.as_str(), 8),
            (&good, 64, tokenizer_json.as_str(), 11),
            (&good, 64, tokenizer_json.as_str(), 1),
            (&good, 64, "{}", 8),
            (&good, 64, big_vocab.as_str(), 8),
        ] {
            let opts = convert::Options {
                pooling: Pooling::Cls,
                prefixes: Prefixes::default(),
                act_block: block,
                row_order: order,
                source: serde_json::Value::Null,
                tokenizer_json: json,
                pipeline: crate::text::Pipeline::WhitespaceSplit,
                max_tokens: max,
            };
            assert!(convert::convert(&ckpt, config, &opts, Vec::new()).is_err());
        }
        // A file of a format before the tokenizer section or the prefixes is refused with its
        // reason, and so is a header without one of the prefixes.
        let good_file = WeightFile::read(convert_tiny(&good, 4).as_slice()).unwrap();
        for old in [
            "ulei-infer/bert-int8/1",
            "ulei-infer/bert-int8/2",
            "ulei-infer/bert-int8/3",
        ] {
            let mut meta = good_file.meta().clone();
            meta["format"] = serde_json::json!(old);
            let mut out = Vec::new();
            crate::weights::file::write(&mut out, &meta, &[]).unwrap();
            let old = WeightFile::read(out.as_slice()).unwrap();
            let e = Encoder::from_file(&old).unwrap_err();
            assert!(e.to_string().ends_with("convert the model again"), "{e}");
        }
        let mut meta = good_file.meta().clone();
        meta["prefixes"].as_object_mut().unwrap().remove("passage");
        let mut out = Vec::new();
        crate::weights::file::write(&mut out, &meta, &[]).unwrap();
        let file = WeightFile::read(out.as_slice()).unwrap();
        let e = Encoder::from_file(&file).unwrap_err();
        assert_eq!(e.to_string(), "config: weight file has no passage prefix");
        // A file of another format is refused.
        let mut out = Vec::new();
        crate::weights::file::write(&mut out, &serde_json::json!({"format": "x"}), &[]).unwrap();
        let file = WeightFile::read(out.as_slice()).unwrap();
        assert!(matches!(Encoder::from_file(&file), Err(Error::Config(_))));
    }

    /// `file` written again with `meta` and each tensor's shape as `shape` gives it (its data
    /// cut to the new shape's size, which must not be larger).
    fn rewrite(
        file: &WeightFile,
        meta: &serde_json::Value,
        shape: impl Fn(&str, &[usize]) -> Vec<usize>,
    ) -> Vec<u8> {
        use crate::weights::file::{Data, Entry};
        let entries: Vec<Entry<'_>> = file
            .names()
            .into_iter()
            .map(|name| {
                let (old, data) = if let Ok(v) = file.f32(name) {
                    (v.shape, Data::F32(v.data))
                } else if let Ok(v) = file.i8(name) {
                    (v.shape, Data::I8(v.data))
                } else if let Ok(v) = file.u32(name) {
                    (v.shape, Data::U32(v.data))
                } else if let Ok(v) = file.u8(name) {
                    (v.shape, Data::U8(v.data))
                } else {
                    let v = file.f64(name).unwrap();
                    (v.shape, Data::F64(v.data))
                };
                let shape = shape(name, old);
                let n: usize = shape.iter().product();
                let data = match data {
                    Data::F32(d) => Data::F32(&d[..n]),
                    Data::I8(d) => Data::I8(&d[..n]),
                    Data::U32(d) => Data::U32(&d[..n]),
                    Data::U8(d) => Data::U8(&d[..n]),
                    Data::F64(d) => Data::F64(&d[..n]),
                };
                Entry {
                    name: name.to_owned(),
                    shape,
                    data,
                }
            })
            .collect();
        let mut out = Vec::new();
        crate::weights::file::write(&mut out, meta, &entries).unwrap();
        out
    }

    #[test]
    fn a_zero_width_in_the_header_is_refused_at_load() {
        // E13r F1: `intermediate_size` 0 with every FFN shape 0 passed every check at load and
        // panicked on the first input. Now the config is refused.
        let good =
            WeightFile::read(convert_tiny(&(0..20).collect::<Vec<_>>(), 4).as_slice()).unwrap();
        let mut meta = good.meta().clone();
        meta["config"]["intermediate_size"] = serde_json::json!(0);
        let bytes = rewrite(&good, &meta, |name, shape| {
            let mut s = shape.to_vec();
            if name.contains("intermediate.dense.") {
                s[0] = 0;
            } else if name.contains(".output.dense.weight") && !name.contains("attention") {
                s[1] = 0;
            }
            s
        });
        let file = WeightFile::read(bytes.as_slice()).unwrap();
        let e = Encoder::from_file(&file)
            .map(|enc| enc.embed_text("ab c"))
            .unwrap_err();
        assert!(e.to_string().contains("intermediate_size is 0"), "{e}");
    }

    #[test]
    fn a_tokenizer_that_does_not_fit_the_model_is_refused() {
        // The converter never writes one, so only a rewritten header reaches this check.
        let good =
            WeightFile::read(convert_tiny(&(0..20).collect::<Vec<_>>(), 4).as_slice()).unwrap();
        assert!(Encoder::from_file(&good).is_ok());
        for (key, value) in [("vocab_size", 21), ("max_tokens", 11)] {
            let mut meta = good.meta().clone();
            meta["tokenizer"][key] = serde_json::json!(value);
            let file =
                WeightFile::read(rewrite(&good, &meta, |_, s| s.to_vec()).as_slice()).unwrap();
            let e = Encoder::from_file(&file).unwrap_err();
            assert!(
                matches!(&e, Error::Weights(m) if m.contains("does not fit the model")),
                "{key}: {e}"
            );
        }
    }

    #[test]
    fn a_non_finite_vector_is_an_error() {
        // E13r F3: one NaN in the last LayerNorm gave `Ok` with a vector of about 1e11.
        let ckpt = tiny_checkpoint("");
        let mut owned: Vec<(String, Vec<usize>, Vec<f32>)> = ckpt
            .names()
            .into_iter()
            .map(|n| {
                let v = ckpt.get(n).unwrap();
                (n.to_owned(), v.shape.to_vec(), v.data.to_vec())
            })
            .collect();
        for (name, _, data) in &mut owned {
            if name == "encoder.layer.1.output.LayerNorm.weight" {
                data[5] = f32::NAN;
            }
        }
        let refs: Vec<(&str, &[usize], &[f32])> = owned
            .iter()
            .map(|(n, s, d)| (n.as_str(), s.as_slice(), d.as_slice()))
            .collect();
        let broken = Checkpoint::read(encode(&refs).as_slice()).unwrap();
        for pooling in [Pooling::Mean, Pooling::Cls] {
            let enc = Encoder::new(tiny_config("bert", pooling), &broken).unwrap();
            let e = enc.embed(&[0, 5, 19, 2]).unwrap_err();
            assert!(e.to_string().contains("not finite"), "{e}");
            // The workspace stays usable for the next input of a sound model.
            let sound = Encoder::new(tiny_config("bert", pooling), &ckpt).unwrap();
            let mut ws = Workspace::new();
            assert!(enc.embed_with(&mut ws, &[0, 5]).is_err());
            assert_eq!(
                sound.embed_with(&mut ws, &[0, 5]).unwrap(),
                sound.embed(&[0, 5]).unwrap()
            );
        }
    }
}
