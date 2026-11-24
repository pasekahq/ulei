//! The model configuration: the columns of ADR-0087's model table, read from a checkpoint's
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
    /// [`Error::Config`] on malformed JSON, a missing or inconsistent key, or an unsupported
    /// model type, activation or position embedding type.
    pub fn from_json(text: &str, pooling: Pooling) -> Result<Self> {
        let v: Value =
            serde_json::from_str(text).map_err(|e| Error::Config(format!("config.json: {e}")))?;
        let model_type = str_key(&v, "model_type")?;
        let position_offset = match model_type {
            "bert" => 0,
            "xlm-roberta" => usize_key(&v, "pad_token_id")? + 1,
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
        if cfg.num_heads == 0 || !cfg.hidden_size.is_multiple_of(cfg.num_heads) {
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
        if cfg.type_vocab_size == 0 {
            return Err(Error::Config("type_vocab_size is 0".into()));
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
}
