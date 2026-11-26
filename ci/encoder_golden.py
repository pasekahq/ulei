"""PyTorch fp32 reference data of the encoder engine's golden and parity tests (ADR-0086 E0, E2).

``PYTHON ci/encoder_golden.py CHECKPOINT CASES_JSON TEXTS_JSONL OUT MODEL_NAME POOLING`` runs with the
interpreter of a venv holding ``ci/requirements-encoder-ref.txt`` (torch CPU, transformers) and
writes, from the checkpoint directory (``config.json``, ``model.safetensors``, ``tokenizer.json``):

- ``OUT/golden/<model>/``: for every case of CASES_JSON the hidden states of the
  embeddings and of every layer and the pooled, L2-normalised vector as little-endian f32 files,
  and ``manifest.json`` in the format ``crates/infer/tests/golden_int8.rs`` reads (the script of
  ADR-0086 E0, fp32, one thread, batch of one). The tokenizer's ids must equal the ones recorded
  in CASES_JSON: the reference tokenizer has not moved under the pins.
- ``OUT/parity/<model>-cases.jsonl``: ``{"text", "ids"}`` of ``transformers``' tokenizer for the
  checkpoint at truncation 512 (E2's variant B) for the synthetic texts of
  TEXTS_JSONL and a fixed set of edge cases (controls, zero-width and bidi
  marks, combining sequences, digits and symbols, long runs), the cases ``tokenizer_parity.rs``
  checks the engine's tokenizer against.
"""

from __future__ import annotations

import json
import os
import sys

import numpy as np
import torch
import transformers
from transformers import AutoModel, AutoTokenizer


def edge_texts() -> list[str]:
    base = ["Привет, мир", "hello world", "Москва — столица", "e-mail: a@b.c", "2026-11-25 14:30", "ёлка",
            "naïve café"]
    out = []
    for cp in list(range(0, 0x20)) + list(range(0x7F, 0xA0)):
        c = chr(cp)
        out += [c, f"a{c}b", f"{c}hello", f"hello{c}", f"слово{c}{c}слово"]
    for c in ["​", "‌", "‍", "‎", "‏", "‪", "‮", "⁠", "﻿", "­",
              "⁢", "　", " ", " ", " "]:
        out += [f"{b}{c}{b}" for b in base] + [c * 3, f"{c} x {c}"]
    out += ["é", "й", "й", "ﬁ", "Ⅻ", "①②", "ｆｕｌｌｗｉｄｔｈ", "½ ™ ※ ǅ ß", "漢字かなカナ", "😄" * 40,
            "x" * 3000, "слово " * 600, "a\tb\nc\r\nd", "  leading", "trailing  ", "", " "]
    return out


def main(argv: list[str]) -> int:
    if len(argv) != 6:
        print(__doc__, file=sys.stderr)
        return 2
    ck, cases_json, texts_jsonl, out, name, pooling = argv
    torch.set_num_threads(1)
    tok = AutoTokenizer.from_pretrained(ck)
    model = AutoModel.from_pretrained(ck, torch_dtype=torch.float32).eval()
    spec = json.load(open(cases_json, encoding="utf-8"))
    gdir = os.path.join(out, "golden", name)
    os.makedirs(gdir, exist_ok=True)
    cases = []
    for c in spec["cases"]:
        enc = tok(c["prefixed_text"], truncation=True, max_length=512, return_tensors="pt")
        ids = enc["input_ids"][0].tolist()
        if ids != c["input_ids"]:
            raise SystemExit(f"{c['id']}: the reference tokenizer's ids changed under the pins")
        with torch.no_grad():
            o = model(**enc, output_hidden_states=True)
        hs = [h[0].numpy().astype("<f4") for h in o.hidden_states]
        last = hs[-1]
        v = last.mean(0) if pooling == "mean" else last[0]
        pooled = (v / np.linalg.norm(v)).astype("<f4")
        os.makedirs(os.path.join(gdir, c["id"]), exist_ok=True)
        tens = {}
        names = ["emb"] + [f"layer_{i:02d}" for i in range(len(hs) - 1)]
        for n, a in list(zip(names, hs)) + [("pooled", pooled)]:
            np.ascontiguousarray(a).tofile(os.path.join(gdir, c["id"], f"{n}.f32"))
            tens[n] = {"file": f"{c['id']}/{n}.f32", "shape": list(a.shape)}
        cases.append({**c, "tensors": tens})
    manifest = {"format": 1, "model": name, "checkpoint_sha256": spec.get("checkpoint_sha256"),
                "config": json.load(open(os.path.join(ck, "config.json"))), "pooling": pooling,
                "torch": torch.__version__, "transformers": transformers.__version__, "cases": cases}
    with open(os.path.join(gdir, "manifest.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=1)
    texts = [json.loads(l) for l in open(texts_jsonl, encoding="utf-8")]
    if pooling == "cls":  # bge-m3 has no prefixes: its parity cases are the texts as they are
        texts = [t.split(": ", 1)[1] if t.startswith(("query: ", "passage: ")) else t for t in texts]
    prefixes = ("query: ", "passage: ") if pooling == "mean" else ("",)
    texts += [p + t for p in prefixes for t in edge_texts()]
    os.makedirs(os.path.join(out, "parity"), exist_ok=True)
    ids = []
    for i in range(0, len(texts), 500):
        ids += tok(texts[i:i + 500], truncation=True, max_length=512)["input_ids"]
    with open(os.path.join(out, "parity", f"{name}-cases.jsonl"), "w", encoding="utf-8") as f:
        for t, x in zip(texts, ids):
            f.write(json.dumps({"text": t, "ids": x}, ensure_ascii=False) + "\n")
    print(f"golden {len(cases)} cases, parity {len(texts)} cases, torch {torch.__version__}, "
          f"transformers {transformers.__version__}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
