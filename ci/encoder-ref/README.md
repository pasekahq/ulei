# Reference files of the encoder engine's CI (ADR-0087 section 10, E12a)

Synthetic texts, public-model outputs and token counts of a public dataset only; no weights.

| File | What |
|---|---|
| `miracl-ru-en-docs3.u32.gz` | Row order of e5-small's token table (ADR-0087 section 2): token ids by descending count over the MIRACL ru and en docs-3 sets, ties by id; `ci/encoder_model.py` pins the SHA-256 of the unpacked file |
| `miracl-ru-en-docs3-bge-m3.u32.gz` | The same for bge-m3's vocabulary |
| `golden-cases.json`, `golden-cases-bge.json` | The 16 synthetic golden cases of ADR-0087 E0 and section 4.1: texts and the reference tokenizer's ids; `ci/encoder_golden.py` dumps PyTorch fp32 for them |
| `e5-probe.json`, `e5-116.f32` | Token ids of the 116 probe inputs of E4 and the engine's pooled vectors for them (E7k), identical on every CPU measured |
| `bge-probe.json`, `bge-76.f32` | The same for bge-m3 (76 inputs) |
| `synthetic-664.jsonl`, `synthetic-664.f32` | The 664 synthetic texts (prefixed) and the static helper's vectors for them |
