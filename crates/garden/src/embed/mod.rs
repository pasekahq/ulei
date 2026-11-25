//! Text embedders behind one trait: a deterministic hashing fake for tests and CI, the encoder
//! engine of `ulei-infer` behind the `native` feature (ADR-0086 E6), and bge-m3 int8 through
//! ONNX Runtime behind the `onnx` feature. The models run only in the helper
//! `ulei-garden-embed` behind [`pipe`]. Every vector an embedder returns has unit length (or is
//! all zeros for a text without features), so a dot product is the cosine.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub mod digest;
#[cfg(feature = "native")]
pub mod native;
#[cfg(feature = "onnx")]
pub mod onnx;
pub mod pipe;
pub mod supervisor;
#[cfg(feature = "onnx")]
pub mod tokenizer;

/// Model file inside the model directory.
pub const ONNX_FILE: &str = "onnx/model_quantized.onnx";
/// Tokenizer file inside the model directory.
pub const TOKENIZER_FILE: &str = "tokenizer.json";
/// The encoder engine's weight file inside the model directory (ADR-0086 E6), written by
/// `ulei-garden-embed convert`; it holds the tokenizer too.
pub const ENGINE_FILE: &str = "model.ulei-infer";

/// A text embedder. Queries and passages may be encoded differently (prefixes, pooling).
pub trait Embedder {
    /// Identity of the model and of every setting that changes the vectors; stored vectors of
    /// another identity are stale.
    fn model_id(&self) -> &str;
    /// Length of every returned vector.
    fn dim(&self) -> usize;
    /// Unit-length vector of a search query.
    ///
    /// # Errors
    ///
    /// [`EmbedError`] when the model fails.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
    /// Unit-length vectors of passages, in input order.
    ///
    /// # Errors
    ///
    /// [`EmbedError`] when the model fails.
    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

impl<E: Embedder + ?Sized> Embedder for Arc<E> {
    fn model_id(&self) -> &str {
        (**self).model_id()
    }

    fn dim(&self) -> usize {
        (**self).dim()
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        (**self).embed_query(text)
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        (**self).embed_passages(texts)
    }
}

/// Failure of an embedder.
#[derive(Debug)]
pub enum EmbedError {
    /// A model artifact or the runtime library could not be read or loaded.
    Load(String),
    /// Tokenization or inference failed.
    Run(String),
    /// The helper was down before the call; nothing reached it.
    Down(String),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(m) => write!(f, "cannot load the embedding model: {m}"),
            Self::Run(m) => write!(f, "embedding failed: {m}"),
            Self::Down(m) => write!(f, "the embedder is down: {m}"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// The guard of `mutex`, also after a thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Scales `v` to unit length in place; an all-zero vector stays zero.
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 && norm.is_finite() {
        for x in v {
            *x /= norm;
        }
    }
}

/// Dot product; the cosine for unit-length vectors. Extra elements of the longer slice are ignored.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Deterministic fake: a hashed bag of character n-grams of the casefolded words, L2-normalised.
/// No model, no I/O; similar spellings land close, so tests can exercise the dense path.
#[derive(Clone, Debug)]
pub struct HashEmbedder {
    dim: usize,
    id: String,
}

impl HashEmbedder {
    /// Fake embedder with `dim` buckets (at least 1).
    pub fn new(dim: usize) -> Self {
        let dim = dim.max(1);
        Self {
            dim,
            id: format!("hash-ngram-3-{dim}"),
        }
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0_f32; self.dim];
        for word in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let padded: Vec<char> = std::iter::once('^')
                .chain(word.chars().flat_map(char::to_lowercase))
                .chain(std::iter::once('$'))
                .collect();
            for gram in padded.windows(3.min(padded.len())) {
                let h = fnv1a(gram);
                // Index from the low bits, sign from a high bit: a cheap signed feature hash.
                let slot = usize::try_from(h % self.dim as u64).unwrap_or(0);
                v[slot] += if h >> 63 == 0 { 1.0 } else { -1.0 };
            }
        }
        l2_normalize(&mut v);
        v
    }
}

fn fnv1a(chars: &[char]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in chars {
        for b in u32::from(*c).to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self.embed(text))
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| self.embed(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: &[f32]) -> f32 {
        dot(v, v).sqrt()
    }

    #[test]
    fn fake_is_deterministic_and_unit_length() {
        let e = HashEmbedder::new(128);
        let a = e.embed_query("Dentist appointment").unwrap();
        assert_eq!(a, e.embed_passages(&["Dentist appointment"]).unwrap()[0]);
        assert_eq!(a.len(), 128);
        assert!((norm(&a) - 1.0).abs() < 1e-5);
        assert_eq!(e.model_id(), "hash-ngram-3-128");
    }

    #[test]
    fn fake_puts_similar_texts_closer() {
        let e = HashEmbedder::new(256);
        let q = e.embed_query("dentists").unwrap();
        let near = e.embed_query("dentist on Maple street").unwrap();
        let far = e.embed_query("train timetable").unwrap();
        assert!(dot(&q, &near) > dot(&q, &far));
    }

    #[test]
    fn empty_text_is_zero_vector() {
        let e = HashEmbedder::new(8);
        assert!(e.embed_query(" ,. ").unwrap().iter().all(|x| *x == 0.0));
    }

    #[test]
    fn normalize_keeps_zero() {
        let mut z = [0.0_f32; 3];
        l2_normalize(&mut z);
        assert_eq!(z, [0.0; 3]);
        let mut v = [3.0_f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
    }
}
