//! The model: the embedding block, the encoder layers and pooling bound to one checkpoint.

use super::embeddings::Embeddings;
use super::layer::Layer;
use super::params::Tensors;
use super::{Config, pooling};
use crate::weights::Checkpoint;
use crate::{Error, Result};

/// One model ready to run: its config and weights borrowed from a [`Checkpoint`].
#[derive(Debug)]
pub struct Encoder<'w> {
    config: Config,
    embeddings: Embeddings<'w>,
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
        let tensors = Tensors::new(checkpoint)?;
        let mut layers = Vec::with_capacity(config.num_layers);
        for i in 0..config.num_layers {
            layers.push(Layer::bind(&tensors, &config, i)?);
        }
        Ok(Self {
            embeddings: Embeddings::bind(&tensors, &config)?,
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
        if ids.is_empty() || ids.len() > c.max_tokens() {
            return Err(Error::Input(format!(
                "{} tokens, expected 1 to {}",
                ids.len(),
                c.max_tokens()
            )));
        }
        let mut x = self.embeddings.apply(ids, c)?;
        let mut intermediates = Vec::new();
        if keep_intermediates {
            intermediates.push(x.clone());
        }
        for layer in &self.layers {
            x = layer.apply(&x, ids.len(), c);
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

    /// Pools the token states of `f` by the config's pooling and L2-normalises the result.
    pub fn pool(&self, f: &Forward) -> Vec<f32> {
        pooling::pool(
            self.config.pooling,
            &f.hidden,
            f.tokens,
            self.config.hidden_size,
        )
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
    use crate::bert::Pooling;
    use crate::ops;
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
        assert_eq!(l.attention(&x, 1, enc.config()), l.value.apply(&x));
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
