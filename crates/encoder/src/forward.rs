//! The encoder forward pass: embeddings, post-LayerNorm encoder layers, pooling and L2
//! normalisation, for one sequence without padding (the attention mask is all ones).
//!
//! Tensor names follow Hugging Face `BertModel` / `XLMRobertaModel`
//! (`embeddings.word_embeddings.weight`, `encoder.layer.N.attention.self.query.weight`, ...),
//! under whatever prefix the checkpoint puts before `embeddings.` (`bert.`, `roberta.`, none).

use crate::config::{Config, Pooling};
use crate::ops;
use crate::weights::{Checkpoint, View};
use crate::{Error, Result};

/// A linear layer: `weight` of shape `[out, in]` and `bias` of `out` values.
#[derive(Debug, Clone, Copy)]
struct Linear<'w> {
    weight: &'w [f32],
    bias: &'w [f32],
    inputs: usize,
}

impl Linear<'_> {
    fn apply(&self, x: &[f32]) -> Vec<f32> {
        ops::linear(x, self.weight, self.bias, self.inputs)
    }
}

/// `LayerNorm` parameters.
#[derive(Debug, Clone, Copy)]
struct Norm<'w> {
    gamma: &'w [f32],
    beta: &'w [f32],
}

#[derive(Debug, Clone, Copy)]
struct Layer<'w> {
    query: Linear<'w>,
    key: Linear<'w>,
    value: Linear<'w>,
    attn_out: Linear<'w>,
    attn_norm: Norm<'w>,
    ffn_in: Linear<'w>,
    ffn_out: Linear<'w>,
    ffn_norm: Norm<'w>,
}

/// One model ready to run: its config and weights borrowed from a [`Checkpoint`].
#[derive(Debug)]
pub struct Encoder<'w> {
    config: Config,
    word: &'w [f32],
    position: &'w [f32],
    token_type: &'w [f32],
    emb_norm: Norm<'w>,
    layers: Vec<Layer<'w>>,
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
        let prefix = checkpoint
            .names()
            .into_iter()
            .find_map(|n| n.strip_suffix("embeddings.word_embeddings.weight"))
            .ok_or_else(|| Error::Weights("no `embeddings.word_embeddings.weight`".into()))?
            .to_string();
        let get = |name: &str, shape: &[usize]| -> Result<&'w [f32]> {
            let full = format!("{prefix}{name}");
            let View { shape: s, data } = checkpoint.get(&full)?;
            if s == shape {
                Ok(data)
            } else {
                Err(Error::Weights(format!(
                    "`{full}` has shape {s:?}, the config expects {shape:?}"
                )))
            }
        };
        let h = config.hidden_size;
        let ffn = config.intermediate_size;
        let linear = |name: &str, out: usize, inputs: usize| -> Result<Linear<'w>> {
            Ok(Linear {
                weight: get(&format!("{name}.weight"), &[out, inputs])?,
                bias: get(&format!("{name}.bias"), &[out])?,
                inputs,
            })
        };
        let norm = |name: &str| -> Result<Norm<'w>> {
            Ok(Norm {
                gamma: get(&format!("{name}.weight"), &[h])?,
                beta: get(&format!("{name}.bias"), &[h])?,
            })
        };
        let mut layers = Vec::with_capacity(config.num_layers);
        for i in 0..config.num_layers {
            let p = format!("encoder.layer.{i}");
            layers.push(Layer {
                query: linear(&format!("{p}.attention.self.query"), h, h)?,
                key: linear(&format!("{p}.attention.self.key"), h, h)?,
                value: linear(&format!("{p}.attention.self.value"), h, h)?,
                attn_out: linear(&format!("{p}.attention.output.dense"), h, h)?,
                attn_norm: norm(&format!("{p}.attention.output.LayerNorm"))?,
                ffn_in: linear(&format!("{p}.intermediate.dense"), ffn, h)?,
                ffn_out: linear(&format!("{p}.output.dense"), h, ffn)?,
                ffn_norm: norm(&format!("{p}.output.LayerNorm"))?,
            });
        }
        Ok(Self {
            word: get("embeddings.word_embeddings.weight", &[config.vocab_size, h])?,
            position: get(
                "embeddings.position_embeddings.weight",
                &[config.max_positions, h],
            )?,
            token_type: get(
                "embeddings.token_type_embeddings.weight",
                &[config.type_vocab_size, h],
            )?,
            emb_norm: norm("embeddings.LayerNorm")?,
            layers,
            config,
        })
    }

    /// The model's config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Runs the encoder on one sequence of token ids (special tokens included).
    ///
    /// # Errors
    /// [`Error::Input`] if `ids` is empty, longer than [`Config::max_tokens`] or holds an id
    /// outside the vocabulary.
    pub fn forward(&self, ids: &[u32], keep_intermediates: bool) -> Result<Forward> {
        let c = &self.config;
        let h = c.hidden_size;
        if ids.is_empty() || ids.len() > c.max_tokens() {
            return Err(Error::Input(format!(
                "{} tokens, expected 1 to {}",
                ids.len(),
                c.max_tokens()
            )));
        }
        let mut x = Vec::with_capacity(ids.len() * h);
        for (pos, &id) in ids.iter().enumerate() {
            let id = usize::try_from(id)
                .ok()
                .filter(|&i| i < c.vocab_size)
                .ok_or_else(|| Error::Input(format!("token id {id} outside the vocabulary")))?;
            let word = &self.word[id * h..(id + 1) * h];
            let p = pos + c.position_offset;
            let position = &self.position[p * h..(p + 1) * h];
            let token_type = &self.token_type[..h];
            x.extend(
                word.iter()
                    .zip(position)
                    .zip(token_type)
                    .map(|((w, p), t)| w + p + t),
            );
        }
        ops::layer_norm(
            &mut x,
            self.emb_norm.gamma,
            self.emb_norm.beta,
            c.layer_norm_eps,
        );
        let mut intermediates = Vec::new();
        if keep_intermediates {
            intermediates.push(x.clone());
        }
        for layer in &self.layers {
            x = self.layer(layer, &x, ids.len());
            if keep_intermediates {
                intermediates.push(x.clone());
            }
        }
        Ok(Forward {
            tokens: ids.len(),
            hidden: x,
            intermediates,
        })
    }

    fn layer(&self, layer: &Layer<'_>, input: &[f32], tokens: usize) -> Vec<f32> {
        let eps = self.config.layer_norm_eps;
        let context = self.attention(layer, input, tokens);
        let mut attended = layer.attn_out.apply(&context);
        for (value, residual) in attended.iter_mut().zip(input) {
            *value += residual;
        }
        let norm = layer.attn_norm;
        ops::layer_norm(&mut attended, norm.gamma, norm.beta, eps);
        let mut inner = layer.ffn_in.apply(&attended);
        for value in &mut inner {
            *value = ops::gelu(*value);
        }
        let mut out = layer.ffn_out.apply(&inner);
        for (value, residual) in out.iter_mut().zip(&attended) {
            *value += residual;
        }
        let norm = layer.ffn_norm;
        ops::layer_norm(&mut out, norm.gamma, norm.beta, eps);
        out
    }

    /// Multi-head scaled dot-product self-attention; returns the context, `tokens x hidden`.
    #[allow(clippy::cast_precision_loss)]
    fn attention(&self, layer: &Layer<'_>, input: &[f32], tokens: usize) -> Vec<f32> {
        let hidden = self.config.hidden_size;
        let width = self.config.head_size();
        let query = layer.query.apply(input);
        let key = layer.key.apply(input);
        let value = layer.value.apply(input);
        let scale = 1.0 / (width as f32).sqrt();
        // The slice of token `t` in head `head` of a `tokens x hidden` matrix.
        let span =
            |t: usize, head: usize| t * hidden + head * width..t * hidden + (head + 1) * width;
        let mut context = vec![0.0f32; tokens * hidden];
        let mut scores = vec![0.0f32; tokens];
        for head in 0..self.config.num_heads {
            for i in 0..tokens {
                let q = &query[span(i, head)];
                for (j, score) in scores.iter_mut().enumerate() {
                    *score = ops::dot(q, &key[span(j, head)]) * scale;
                }
                ops::softmax(&mut scores);
                let out = &mut context[span(i, head)];
                for (j, &p) in scores.iter().enumerate() {
                    for (o, v) in out.iter_mut().zip(&value[span(j, head)]) {
                        *o += p * v;
                    }
                }
            }
        }
        context
    }

    /// Pools the token states of `f` by the config's pooling and L2-normalises the result.
    #[allow(clippy::cast_precision_loss)]
    pub fn pool(&self, f: &Forward) -> Vec<f32> {
        let h = self.config.hidden_size;
        let mut out = match self.config.pooling {
            Pooling::Cls => f.hidden[..h].to_vec(),
            Pooling::Mean => {
                let mut sum = vec![0.0f64; h];
                for row in f.hidden.chunks_exact(h) {
                    for (s, v) in sum.iter_mut().zip(row) {
                        *s += f64::from(*v);
                    }
                }
                #[allow(clippy::cast_possible_truncation)]
                sum.iter().map(|s| (s / f.tokens as f64) as f32).collect()
            }
        };
        ops::l2_normalize(&mut out);
        out
    }

    /// Embeds one sequence: forward pass, pooling, L2 normalisation.
    ///
    /// # Errors
    /// As [`Encoder::forward`].
    pub fn embed(&self, ids: &[u32]) -> Result<Vec<f32>> {
        Ok(self.pool(&self.forward(ids, false)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weights::encode;

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
        assert_eq!(enc.attention(l, &x, 1), l.value.apply(&x));
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
}
