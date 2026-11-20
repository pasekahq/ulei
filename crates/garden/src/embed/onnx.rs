//! bge-m3 int8 (ONNX) on the CPU: the tokenizer and model are read from a directory given at
//! run time, the ONNX Runtime shared library from a path given at run time; nothing downloads.
//! Raw text without prefixes, at most 512 tokens, CLS pooling, L2 normalisation.

use std::path::Path;
use std::sync::Mutex;

use ort::session::Session;
use ort::value::Tensor;

use super::tokenizer::Tokenizer;
use super::{EmbedError, Embedder, l2_normalize};

/// Model file inside the model directory.
pub const ONNX_FILE: &str = "onnx/model_quantized.onnx";
/// Tokenizer file inside the model directory.
pub const TOKENIZER_FILE: &str = "tokenizer.json";
/// Token limit of one input; longer texts are truncated.
pub const MAX_TOKENS: usize = 512;
/// Id of the padding token of the XLM-R vocabulary.
const PAD_ID: i64 = 1;
/// Texts per session call.
const BATCH: usize = 8;
const MODEL_ID: &str = "bge-m3-int8-onnx-cls-512";

/// bge-m3 behind [`Embedder`]; one session, calls serialised.
pub struct OnnxEmbedder {
    tokenizer: Tokenizer,
    session: Mutex<Session>,
    feeds_token_types: bool,
    dim: usize,
}

impl OnnxEmbedder {
    /// Loads ONNX Runtime from `ort_lib` (once per process) and bge-m3 from `model_dir`, running
    /// on `threads` intra-op threads and one inter-op thread.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Load`] when the library, the tokenizer or the model cannot be loaded.
    pub fn load(model_dir: &Path, ort_lib: &Path, threads: usize) -> Result<Self, EmbedError> {
        let load = |e: &dyn std::fmt::Display| EmbedError::Load(e.to_string());
        ort::init_from(ort_lib)
            .map_err(|e| load(&e))?
            .with_name("ulei-garden")
            .commit();
        let json = std::fs::read_to_string(model_dir.join(TOKENIZER_FILE))
            .map_err(|e| load(&format!("{TOKENIZER_FILE}: {e}")))?;
        let tokenizer = Tokenizer::from_json(&json)?;
        let session = Session::builder()
            .map_err(|e| load(&e))?
            .with_intra_threads(threads.max(1))
            .map_err(|e| load(&e))?
            .with_inter_threads(1)
            .map_err(|e| load(&e))?
            .commit_from_file(model_dir.join(ONNX_FILE))
            .map_err(|e| load(&format!("{ONNX_FILE}: {e}")))?;
        let feeds_token_types = session
            .inputs()
            .iter()
            .any(|i| i.name() == "token_type_ids");
        let mut me = Self {
            tokenizer,
            session: Mutex::new(session),
            feeds_token_types,
            dim: 0,
        };
        me.dim = me.run(&["dim"])?.first().map_or(0, Vec::len);
        Ok(me)
    }

    /// CLS vectors of one batch, normalised, in input order.
    fn run(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let err = |e: &dyn std::fmt::Display| EmbedError::Run(e.to_string());
        let enc: Vec<Vec<u32>> = texts
            .iter()
            .map(|t| self.tokenizer.encode(t, MAX_TOKENS))
            .collect();
        let width = enc.iter().map(Vec::len).max().unwrap_or(0);
        if width == 0 {
            return Ok(vec![Vec::new(); texts.len()]);
        }
        let mut ids = vec![PAD_ID; enc.len() * width];
        let mut mask = vec![0_i64; enc.len() * width];
        for (row, e) in enc.iter().enumerate() {
            for (col, id) in e.iter().enumerate() {
                ids[row * width + col] = i64::from(*id);
                mask[row * width + col] = 1;
            }
        }
        let shape = [enc.len(), width];
        let mut inputs = ort::inputs![
            "input_ids" => Tensor::from_array((shape, ids)).map_err(|e| err(&e))?,
            "attention_mask" => Tensor::from_array((shape, mask)).map_err(|e| err(&e))?,
        ];
        if self.feeds_token_types {
            let zeros = vec![0_i64; enc.len() * width];
            inputs.push((
                "token_type_ids".into(),
                Tensor::from_array((shape, zeros))
                    .map_err(|e| err(&e))?
                    .into(),
            ));
        }
        let mut session = self
            .session
            .lock()
            .map_err(|_| EmbedError::Run("session lock poisoned".into()))?;
        let outputs = session.run(inputs).map_err(|e| err(&e))?;
        let (shape, hidden) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| err(&e))?;
        // last_hidden_state is [batch, seq, dim]; CLS pooling takes token 0 of each row.
        let dims: Vec<usize> = shape
            .iter()
            .map(|d| usize::try_from(*d).unwrap_or(0))
            .collect();
        let [batch, seq, dim] = dims[..] else {
            return Err(EmbedError::Run(format!("unexpected output shape {shape}")));
        };
        if batch != enc.len() || hidden.len() != batch * seq * dim {
            return Err(EmbedError::Run(format!("unexpected output shape {shape}")));
        }
        Ok((0..batch)
            .map(|b| {
                let start = b * seq * dim;
                let mut v = hidden[start..start + dim].to_vec();
                l2_normalize(&mut v);
                v
            })
            .collect())
    }
}

impl Embedder for OnnxEmbedder {
    fn model_id(&self) -> &str {
        MODEL_ID
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.run(&[text])?
            .pop()
            .ok_or_else(|| EmbedError::Run("no output".into()))
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        // Batches of similar length waste less padding; results go back to input order.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let mut out = vec![Vec::new(); texts.len()];
        for chunk in order.chunks(BATCH) {
            let batch: Vec<&str> = chunk.iter().map(|&i| texts[i]).collect();
            for (i, v) in chunk.iter().zip(self.run(&batch)?) {
                out[*i] = v;
            }
        }
        Ok(out)
    }
}
