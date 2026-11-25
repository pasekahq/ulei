"""The encoder engine's path of the garden on a tiny model, the driver of the CI job
``garden-encoder`` (ADR-0086 E6).

``python3 ci/garden_encoder.py model DIR HELPER`` writes a tiny fp32 BERT checkpoint into
``DIR/checkpoint`` (``config.json``, ``model.safetensors``, the character-level ``tokenizer.json``
of ``ci/garden_onnx.py`` without its normaliser and the identity row order) and converts it with ``HELPER convert`` to
``DIR/model/model.ulei-infer``, the model directory the helper reads. The checkpoint's width is
its vocabulary size: the token table is the identity, every other embedding zero, and its one
layer has all weights and biases zero, so each layer adds nothing and its LayerNorm keeps the
normalised one-hot of the token. Mean pooling then gives the text's character histogram (minus a
constant), as the ONNX job's tiny model does.

``python3 ci/garden_encoder.py serve ULEI_GARDEN HELPER MODEL_DIR WORKDIR`` runs the check of
``ci/garden_onnx.py serve`` on that model directory: the daemon starts the helper as for ONNX
Runtime (with an ``ort_library`` that does not exist, which the engine ignores), the helper picks
the engine by the weight file, and `search` must find the fact by its letters alone.
"""

from __future__ import annotations

import json
import struct
import subprocess
import sys
from pathlib import Path

from garden_onnx import PIECES, SPECIALS, serve, tokenizer
from inbox_load import Failure

MAX_TOKENS = 64


def safetensors(tensors: dict[str, tuple[list[int], list[float]]]) -> bytes:
    """The ``safetensors`` file of ``tensors`` (name to shape and row-major f32 values)."""
    header, blobs, offset = {}, [], 0
    for name, (shape, values) in tensors.items():
        blob = struct.pack(f"<{len(values)}f", *values)
        header[name] = {"dtype": "F32", "shape": shape, "data_offsets": [offset, offset + len(blob)]}
        blobs.append(blob)
        offset += len(blob)
    head = json.dumps(header).encode()
    head += b" " * (-len(head) % 8)
    return struct.pack("<Q", len(head)) + head + b"".join(blobs)


def write_checkpoint(directory: Path) -> int:
    """Writes the tiny checkpoint under ``directory``; returns its width."""
    vocab = len(SPECIALS) + len(PIECES)
    h, ffn, positions = vocab, 4, MAX_TOKENS + 2
    config = {"model_type": "bert", "hidden_act": "gelu", "hidden_size": h,
              "intermediate_size": ffn, "layer_norm_eps": 1e-12,
              "max_position_embeddings": positions, "num_attention_heads": 1,
              "num_hidden_layers": 1, "pad_token_id": SPECIALS.index("<pad>"),
              "type_vocab_size": 1, "vocab_size": vocab}
    zeros = lambda n: [0.0] * n  # noqa: E731
    ones = lambda n: [1.0] * n  # noqa: E731
    t = {
        "embeddings.word_embeddings.weight":
            ([vocab, h], [1.0 if i == j else 0.0 for i in range(vocab) for j in range(h)]),
        "embeddings.position_embeddings.weight": ([positions, h], zeros(positions * h)),
        "embeddings.token_type_embeddings.weight": ([1, h], zeros(h)),
        "embeddings.LayerNorm.weight": ([h], ones(h)),
        "embeddings.LayerNorm.bias": ([h], zeros(h)),
    }
    for name, out, inp in (("attention.self.query", h, h), ("attention.self.key", h, h),
                           ("attention.self.value", h, h), ("attention.output.dense", h, h),
                           ("intermediate.dense", ffn, h), ("output.dense", h, ffn)):
        t[f"encoder.layer.0.{name}.weight"] = ([out, inp], zeros(out * inp))
        t[f"encoder.layer.0.{name}.bias"] = ([out], zeros(out))
    for name in ("attention.output.LayerNorm", "output.LayerNorm"):
        t[f"encoder.layer.0.{name}.weight"] = ([h], ones(h))
        t[f"encoder.layer.0.{name}.bias"] = ([h], zeros(h))
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "config.json").write_text(json.dumps(config), encoding="utf-8")
    (directory / "model.safetensors").write_bytes(safetensors(t))
    # The engine's tokenizer reads a precompiled character map or none; the ONNX job's
    # normaliser only collapses runs of spaces, which the engine's pipeline does not do.
    document = tokenizer()
    document["normalizer"] = None
    (directory / "tokenizer.json").write_text(
        json.dumps(document, ensure_ascii=False, indent=1), encoding="utf-8")
    (directory / "order.u32").write_bytes(struct.pack(f"<{vocab}I", *range(vocab)))
    return h


def write_model(directory: Path, helper: str) -> str:
    """Writes the checkpoint and converts it; returns the converter's JSON line."""
    checkpoint = directory / "checkpoint"
    width = write_checkpoint(checkpoint)
    model = directory / "model"
    model.mkdir(parents=True, exist_ok=True)
    out = subprocess.run(
        [helper, "convert", "--checkpoint", str(checkpoint),
         "--token-order", str(checkpoint / "order.u32"), "--pooling", "mean",
         "--query-prefix", "", "--passage-prefix", "", "--max-tokens", str(MAX_TOKENS),
         "--out", str(model / "model.ulei-infer")],
        capture_output=True, text=True, check=False)
    if out.returncode != 0:
        raise Failure(f"convert failed: {out.stderr.strip()}")
    return f"width {width}, weight file {out.stdout.strip()}"


def main(argv: list[str]) -> int:
    try:
        if len(argv) == 3 and argv[0] == "model":
            summary = write_model(Path(argv[1]), argv[2])
            print(f"garden_encoder: tiny model, {summary}, in {argv[1]}/model")
            return 0
        if len(argv) == 5 and argv[0] == "serve":
            work = Path(argv[4])
            summary = serve(argv[1], argv[2], Path(argv[3]), work / "no-onnxruntime.so", work)
            print(f"::notice title=garden-encoder::{summary}")
            return 0
    except (Failure, OSError, KeyError, ValueError) as e:
        print(f"garden_encoder: {e}", file=sys.stderr)
        return 1
    print("usage: garden_encoder.py model DIR HELPER | serve ULEI_GARDEN HELPER MODEL_DIR WORKDIR",
          file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
