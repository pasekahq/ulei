//! The model configuration: the columns of ADR-0086's model table, read from a checkpoint's
//! `config.json` (Hugging Face `BertModel` / `XLMRobertaModel` keys) plus the pooling, which
//! the checkpoint's `config.json` does not carry.

use serde_json::Value;

use crate::{Error, Result};

/// How the token states become one vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// The mean over the tokens of the attention mask (the e5 models).
    Mean,
    /// The state of the first token (bge-m3).
    Cls,
}

impl Pooling {
    /// Parses `mean` or `cls`.
    ///
    /// # Errors
    /// [`Error::Config`] for any other name.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "mean" => Ok(Self::Mean),
            "cls" => Ok(Self::Cls),
            other => Err(Error::Config(format!("unknown pooling `{other}`"))),
        }
    }

    /// The name [`Pooling::parse`] reads.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mean => "mean",
            Self::Cls => "cls",
        }
    }
}

/// The text a model wants before a query and before a passage (e5: `query: ` / `passage: `;
/// empty for a model without prefixes, bge-m3). Not in `config.json`: the converter writes
/// them into the weight file's header (ADR-0086 open question 11), and the caller puts the one
/// of its input's kind before the text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefixes {
    /// Before a search query.
    pub query: String,
    /// Before a stored passage.
    pub passage: String,
}

/// The shape and constants of one encoder model.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Rows of the token embedding table.
    pub vocab_size: usize,
    /// Width of every token state.
    pub hidden_size: usize,
    /// Number of encoder layers.
    pub num_layers: usize,
    /// Number of attention heads; divides `hidden_size`.
    pub num_heads: usize,
    /// Width of the feed-forward block.
    pub intermediate_size: usize,
    /// Epsilon of every `LayerNorm`.
    pub layer_norm_eps: f32,
    /// Rows of the position embedding table.
    pub max_positions: usize,
    /// Position id of the first token: 0 for BERT, `pad_token_id + 1` for XLM-RoBERTa.
    pub position_offset: usize,
    /// Rows of the token type table; type 0 is used for every token.
    pub type_vocab_size: usize,
    /// Pooling of the token states.
    pub pooling: Pooling,
}

impl Config {
    /// Reads the checkpoint's `config.json` text; `pooling` comes from the model's card
    /// (sentence-transformers' pooling config), not from this file.
    ///
    /// Supported: `model_type` `bert` (positions from 0) and `xlm-roberta` (positions from
    /// `pad_token_id + 1`), `hidden_act` `gelu` (the exact erf form), absolute positions.
    ///
    /// # Errors
    /// [`Error::Config`] on malformed JSON, a missing or inconsistent key, a size of 0, a
    /// `layer_norm_eps` that is not finite and positive as `f32`, or an unsupported model
    /// type, activation or position embedding type.
    pub fn from_json(text: &str, pooling: Pooling) -> Result<Self> {
        let v: Value =
            serde_json::from_str(text).map_err(|e| Error::Config(format!("config.json: {e}")))?;
        let model_type = str_key(&v, "model_type")?;
        let position_offset = match model_type {
            "bert" => 0,
            "xlm-roberta" => usize_key(&v, "pad_token_id")?
                .checked_add(1)
                .ok_or_else(|| Error::Config("pad_token_id + 1 overflows".into()))?,
            other => return Err(Error::Config(format!("unsupported model_type `{other}`"))),
        };
        let act = str_key(&v, "hidden_act")?;
        if act != "gelu" {
            return Err(Error::Config(format!("unsupported hidden_act `{act}`")));
        }
        if let Some(kind) = v.get("position_embedding_type").and_then(Value::as_str)
            && kind != "absolute"
        {
            return Err(Error::Config(format!(
                "unsupported position_embedding_type `{kind}`"
            )));
        }
        let eps = v
            .get("layer_norm_eps")
            .and_then(Value::as_f64)
            .ok_or_else(|| Error::Config("missing number `layer_norm_eps`".into()))?;
        #[allow(clippy::cast_possible_truncation)]
        let layer_norm_eps = eps as f32;
        let cfg = Self {
            vocab_size: usize_key(&v, "vocab_size")?,
            hidden_size: usize_key(&v, "hidden_size")?,
            num_layers: usize_key(&v, "num_hidden_layers")?,
            num_heads: usize_key(&v, "num_attention_heads")?,
            intermediate_size: usize_key(&v, "intermediate_size")?,
            layer_norm_eps,
            max_positions: usize_key(&v, "max_position_embeddings")?,
            position_offset,
            type_vocab_size: usize_key(&v, "type_vocab_size")?,
            pooling,
        };
        // A zero size passes every shape check of the weights (a tensor with a 0 in its shape
        // is empty) and then panics in the forward pass, so each is refused here.
        for (key, size) in [
            ("vocab_size", cfg.vocab_size),
            ("hidden_size", cfg.hidden_size),
            ("num_hidden_layers", cfg.num_layers),
            ("num_attention_heads", cfg.num_heads),
            ("intermediate_size", cfg.intermediate_size),
            ("max_position_embeddings", cfg.max_positions),
            ("type_vocab_size", cfg.type_vocab_size),
        ] {
            if size == 0 {
                return Err(Error::Config(format!("{key} is 0")));
            }
        }
        if !(cfg.layer_norm_eps.is_finite() && cfg.layer_norm_eps > 0.0) {
            return Err(Error::Config(format!(
                "layer_norm_eps {eps} is not a finite positive f32"
            )));
        }
        if !cfg.hidden_size.is_multiple_of(cfg.num_heads) {
            return Err(Error::Config(format!(
                "hidden_size {} is not a multiple of num_attention_heads {}",
                cfg.hidden_size, cfg.num_heads
            )));
        }
        if cfg.max_positions <= cfg.position_offset {
            return Err(Error::Config(format!(
                "max_position_embeddings {} leaves no position after offset {}",
                cfg.max_positions, cfg.position_offset
            )));
        }
        Ok(cfg)
    }

    /// The longest input in tokens, special tokens included.
    pub fn max_tokens(&self) -> usize {
        self.max_positions - self.position_offset
    }

    /// Width of one attention head.
    pub fn head_size(&self) -> usize {
        self.hidden_size / self.num_heads
    }
}

fn str_key<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Config(format!("missing string `{key}`")))
}

fn usize_key(v: &Value, key: &str) -> Result<usize> {
    v.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| Error::Config(format!("missing unsigned integer `{key}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const E5_SMALL: &str = r#"{"architectures":["BertModel"],"hidden_act":"gelu",
        "hidden_size":384,"intermediate_size":1536,"layer_norm_eps":1e-12,
        "max_position_embeddings":512,"model_type":"bert","num_attention_heads":12,
        "num_hidden_layers":12,"pad_token_id":0,"position_embedding_type":"absolute",
        "type_vocab_size":2,"vocab_size":250037}"#;

    const BGE_M3: &str = r#"{"hidden_act":"gelu","hidden_size":1024,"intermediate_size":4096,
        "layer_norm_eps":1e-05,"max_position_embeddings":8194,"model_type":"xlm-roberta",
        "num_attention_heads":16,"num_hidden_layers":24,"pad_token_id":1,
        "position_embedding_type":"absolute","type_vocab_size":1,"vocab_size":250002}"#;

    #[test]
    fn bert_positions_start_at_zero() {
        let c = Config::from_json(E5_SMALL, Pooling::Mean).unwrap();
        assert_eq!(c.position_offset, 0);
        assert_eq!(c.max_tokens(), 512);
        assert_eq!(c.head_size(), 32);
        assert_eq!(c.num_layers, 12);
        assert!((c.layer_norm_eps - 1e-12).abs() < 1e-18);
    }

    #[test]
    fn xlm_roberta_positions_start_after_pad() {
        let c = Config::from_json(BGE_M3, Pooling::Cls).unwrap();
        assert_eq!(c.position_offset, 2);
        assert_eq!(c.max_tokens(), 8192);
        assert_eq!(c.head_size(), 64);
        assert_eq!(c.pooling, Pooling::Cls);
    }

    #[test]
    fn unsupported_values_are_refused() {
        let relu = E5_SMALL.replace(r#""gelu""#, r#""relu""#);
        assert!(Config::from_json(&relu, Pooling::Mean).is_err());
        let rel = E5_SMALL.replace(r#""absolute""#, r#""relative_key""#);
        assert!(Config::from_json(&rel, Pooling::Mean).is_err());
        let heads = E5_SMALL.replace(r#""num_attention_heads":12"#, r#""num_attention_heads":7"#);
        assert!(Config::from_json(&heads, Pooling::Mean).is_err());
        assert!(Config::from_json("{}", Pooling::Mean).is_err());
        assert!(Pooling::parse("max").is_err());
        assert_eq!(Pooling::parse("cls").unwrap(), Pooling::Cls);
    }

    /// `E5_SMALL` with `key` set to the JSON `value`.
    fn with(key: &str, value: &str) -> String {
        let mut v: Value = serde_json::from_str(E5_SMALL).unwrap();
        v[key] = serde_json::from_str(value).unwrap();
        v.to_string()
    }

    fn refusal(json: &str) -> String {
        match Config::from_json(json, Pooling::Mean) {
            Err(Error::Config(m)) => m,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn zero_sizes_are_refused() {
        // A zero width passes every shape check of the weights and panics in the forward pass
        // (E13r F1), so each size is refused by name.
        for key in [
            "vocab_size",
            "hidden_size",
            "num_hidden_layers",
            "num_attention_heads",
            "intermediate_size",
            "max_position_embeddings",
        ] {
            let m = refusal(&with(key, "0"));
            assert!(m.contains(key) && m.contains("is 0"), "{key}: {m}");
        }
        assert_eq!(
            refusal(&with("type_vocab_size", "0")),
            "type_vocab_size is 0"
        );
    }

    #[test]
    fn layer_norm_eps_must_be_finite_and_positive() {
        // 1e39 and -1e39 are finite in JSON and infinite as f32; 1e-50 is 0 as f32.
        for eps in ["0", "-1e-12", "1e39", "-1e39", "1e-50"] {
            let m = refusal(&with("layer_norm_eps", eps));
            assert!(m.contains("layer_norm_eps"), "{eps}: {m}");
        }
        assert!(Config::from_json(&with("layer_norm_eps", "1e-30"), Pooling::Mean).is_ok());
    }

    #[test]
    fn pad_token_id_at_the_top_of_the_range_is_refused() {
        // `pad_token_id + 1` overflowed (E13r F2).
        let max = BGE_M3.replace(
            r#""pad_token_id":1"#,
            r#""pad_token_id":18446744073709551615"#,
        );
        assert!(refusal(&max).contains("pad_token_id"));
        let big = BGE_M3.replace(r#""pad_token_id":1"#, r#""pad_token_id":8193"#);
        assert!(refusal(&big).contains("leaves no position"));
    }
}
