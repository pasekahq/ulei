"""Seed corpora of the fuzz targets (ADR-0086 E12a; the `assurance` workflow runs it before cargo fuzz).

usage: make_seeds.py --repo ROOT --e5 E5_FILE [--bge BGE_FILE] --texts SYNTHETIC_JSONL
                     --helper ULEI_GARDEN_EMBED --out DIR

- the tiny BERT checkpoint of the garden-encoder CI job (ci/garden_encoder.py), converted by the
  real helper: a whole, sealed, valid weight file of a few hundred KB;
- the real e5-small and bge-m3 files' preambles and JSON headers (no blobs);
- tokenizer-only.uiw: the e5 file's tokenizer tensors and meta.tokenizer re-packed into a small
  valid weight file (format of crates/infer/src/weights/file.rs), sealed;
- texts of the kit (synthetic) and edge cases for the tokenizer;
- request and answer streams of the pipe, both protocols;
- config and blob bytes of the crafted small model of encoder_crafted.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import struct
import subprocess
import sys
from pathlib import Path

MAGIC = b"ULEIINFW"


def preamble_header(path: Path) -> tuple[bytes, dict, int]:
    with open(path, "rb") as f:
        pre = f.read(64)
        n = struct.unpack("<Q", pre[16:24])[0]
        header = f.read(n)
    return pre, json.loads(header), n


def pack(meta: dict, tensors: list[tuple[str, str, list[int], bytes]]) -> bytes:
    """A version-3 weight file of ``tensors`` (name, dtype, shape, raw bytes), sealed."""
    table, blobs, off = [], [], 0
    for name, dtype, shape, raw in tensors:
        off = (off + 63) // 64 * 64
        table.append({"dtype": dtype, "name": name, "offset": off, "shape": shape})
        blobs.append((off, raw))
        off += len(raw)
    header = json.dumps({"meta": meta, "tensors": table}, separators=(",", ":")).encode()
    data_start = (64 + len(header) + 63) // 64 * 64
    total = data_start + off
    out = bytearray(total)
    out[0:8] = MAGIC
    out[8:12] = struct.pack("<I", 3)
    out[16:24] = struct.pack("<Q", len(header))
    out[24:32] = struct.pack("<Q", total)
    out[64:64 + len(header)] = header
    for o, raw in blobs:
        out[data_start + o:data_start + o + len(raw)] = raw
    out[32:64] = hashlib.sha256(bytes(out)).digest()  # digest bytes were zero while hashing
    return bytes(out)


def tokenizer_only(e5: Path) -> bytes:
    _, header, n = preamble_header(e5)
    data_start = (64 + n + 63) // 64 * 64
    size = {"u8": 1, "u32": 4, "f64": 8, "f32": 4, "i8": 1}
    tensors = []
    with open(e5, "rb") as f:
        for t in header["tensors"]:
            if not t["name"].startswith("tokenizer."):
                continue
            count = 1
            for d in t["shape"]:
                count *= d
            f.seek(data_start + t["offset"])
            tensors.append((t["name"], t["dtype"], t["shape"], f.read(count * size[t["dtype"]])))
    meta = {"tokenizer": header["meta"]["tokenizer"], "format": "ulei-fuzz/tokenizer-only"}
    return pack(meta, tensors)


def structured(header: bytes, data: bytes, flags: int) -> bytes:
    return bytes([flags]) + struct.pack(">H", min(len(header), 0xFFFF)) + header[:0xFFFF] + data


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", type=Path, required=True)
    ap.add_argument("--e5", type=Path, required=True)
    ap.add_argument("--bge", type=Path)
    ap.add_argument("--texts", type=Path, required=True)
    ap.add_argument("--helper", required=True)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    # garden_encoder.py runs with ci/ as its directory: every path absolute.
    a.repo, a.e5, a.texts, a.out = a.repo.resolve(), a.e5.resolve(), a.texts.resolve(), a.out.resolve()
    a.bge = a.bge.resolve() if a.bge else None
    a.helper = str(Path(a.helper).resolve())
    out = a.out
    for t in ("weight_file_raw", "weight_file_structured", "verify_file", "tokenizer_encode",
              "tokenizer_section", "canonical_seal", "pipe_helper_side", "pipe_daemon_side",
              "encoder_crafted"):
        (out / t).mkdir(parents=True, exist_ok=True)

    # The tiny model of the garden-encoder CI job, converted and verified by the real helper.
    tiny_dir = out / "tiny"
    subprocess.run([sys.executable, str(a.repo / "ci" / "garden_encoder.py"), "model",
                    str(tiny_dir), a.helper], check=True, cwd=a.repo / "ci")
    tiny = (tiny_dir / "model" / "model.ulei-infer").read_bytes()
    (out / "weight_file_raw" / "tiny").write_bytes(tiny)
    n = struct.unpack("<Q", tiny[16:24])[0]
    t_header = tiny[64:64 + n]
    t_data = tiny[(64 + n + 63) // 64 * 64:]
    for flags in (2, 6):  # version 3, unsealed and sealed
        (out / "weight_file_structured" / f"tiny-{flags}").write_bytes(structured(t_header, t_data, flags))
        (out / "verify_file" / f"tiny-{flags & 4 and 1}").write_bytes(structured(t_header, t_data, 1 if flags & 4 else 0))

    for name, path in (("e5", a.e5), ("bge", a.bge)):
        if path is None:
            continue
        pre, header, n = preamble_header(path)
        raw_header = json.dumps(header, separators=(",", ":")).encode()
        (out / "weight_file_raw" / f"{name}.head").write_bytes(pre + raw_header)
        (out / "weight_file_structured" / f"{name}.head").write_bytes(structured(raw_header, b"", 2))

    tok = tokenizer_only(a.e5)
    (out / "tokenizer-only.uiw").write_bytes(tok)
    for i, patch in enumerate([b"", struct.pack("<IB", 0, 0xFF), struct.pack("<IB", 177_000, 0)]):
        (out / "tokenizer_section" / f"p{i}").write_bytes(patch + "passage: Привет".encode())

    texts = [json.loads(l) for l in open(a.texts, encoding="utf-8")]
    for i, t in enumerate(texts[::16]):
        (out / "tokenizer_encode" / f"syn-{i}").write_bytes(t.encode())
    edge = ["", " ", "\u0000", "á", "ﬁ", "Ⅻ", "﻿", "x" * 5000, "ё" * 3000, "😄" * 600,
            "​‌‍", "퟿", "query: \t\n"]
    for i, t in enumerate(edge):
        (out / "tokenizer_encode" / f"edge-{i}").write_bytes(t.encode("utf-8", "surrogatepass"))
    (out / "tokenizer_encode" / "invalid-utf8").write_bytes(b"\xff\xfe\xc3(\xe2\x82")

    # encoder_crafted: 12 config bytes (fuzz/src/lib.rs, crafted_model), then the blob bytes.
    # Width 8, 2 heads, FFN 16, one layer, 14 ids, 10 positions, one token type, eps 1e-12,
    # block 4; blobs of small finite floats (0x3c233c23 ~ 0.01) and int8 35 and 60.
    for name, flags, eps, fill in (("mean", 0, 0, b"\x23\x3c"), ("cls-xlmr", 3, 1, b"\x3c\x23"),
                                   ("zero-ffn", 0, 0, b"\x23\x3c"), ("nan", 0, 0, b"\xff\xff\xc0\x7f")):
        ffn = 0 if name == "zero-ffn" else 16
        cfg = bytes([flags, 8, 2, ffn, 1, 2, 10, 1, 1, eps, 4, 0])
        (out / "encoder_crafted" / name).write_bytes(cfg + fill * 32)

    (out / "canonical_seal" / "a").write_bytes(bytes([7]) + bytes(range(7)) + tiny[:200])

    q = json.dumps({"kind": "query", "texts": ["кофе"]})
    p = json.dumps({"kind": "passage", "texts": ["a", "b"]})
    ql = json.dumps({"kind": "query", "texts": ["x"], "id": 1})
    pl = json.dumps({"kind": "passage", "texts": ["y"], "id": 2})
    (out / "pipe_helper_side" / "old").write_bytes(f"{q}\n{p}\n".encode())
    (out / "pipe_helper_side" / "lanes").write_bytes(f"{pl}\n{ql}\n{{bad\n".encode())

    ready = json.dumps({"name": "fuzz", "model_sha256": "a" * 64, "tokenizer_sha256": "a" * 64,
                        "dim": 2})
    vec = struct.pack("<2f", 0.6, 0.8)
    # Lanes: the daemon numbers its calls from 1 (query first here, then the passage).
    lanes = (json.dumps(json.loads(ready) | {"lanes": 2}) + "\n").encode()
    lanes += (json.dumps({"count": 1, "dim": 2, "id": 1}) + "\n").encode() + vec
    lanes += (json.dumps({"count": 2, "dim": 2, "id": 2}) + "\n").encode() + vec + vec
    (out / "pipe_daemon_side" / "lanes").write_bytes(lanes)
    old = (ready + "\n").encode() + (json.dumps({"count": 1, "dim": 2}) + "\n").encode() + vec
    old += (json.dumps({"count": 2, "dim": 2}) + "\n").encode() + vec + vec
    (out / "pipe_daemon_side" / "old").write_bytes(old)
    (out / "pipe_daemon_side" / "refuse").write_bytes(
        (json.dumps({"error": "model file corrupted: x", "class": "corrupted"}) + "\n").encode())
    print("seeds in", out, {d: len(os.listdir(out / d)) for d in os.listdir(out) if (out / d).is_dir()})
    return 0


if __name__ == "__main__":
    sys.exit(main())
