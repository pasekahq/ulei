"""The helper's vectors against the reference pack, byte for byte (ADR-0087 E7k and E12a).

``python3 ci/encoder_vectors.py HELPER MODEL_DIR TEXTS_JSONL REF_F32`` starts ``HELPER --model-dir
MODEL_DIR``, sends every line of TEXTS_JSONL (a JSON string beginning ``query: `` or ``passage: ``;
the prefix becomes the request's kind, the helper adds it back) as one request in the single-call
exchange, writes the vectors in order and compares them with REF_F32 (little-endian f32). Exact
equality is the claim (every kernel bit-identical to portable, the same bits on every CPU); one
``::notice`` line on success, one ``::error`` naming the first differing text otherwise.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


def main(argv: list[str]) -> int:
    if len(argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    helper, model_dir, texts, ref = argv[0], argv[1], Path(argv[2]), Path(argv[3]).read_bytes()
    lines = [json.loads(l) for l in texts.read_text(encoding="utf-8").splitlines() if l.strip()]
    p = subprocess.Popen([helper, "--model-dir", model_dir], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    ready = json.loads(p.stdout.readline())
    dim = ready["dim"]
    out = bytearray()
    for text in lines:
        kind, _, body = text.partition(": ")
        p.stdin.write((json.dumps({"kind": kind, "texts": [body]}) + "\n").encode())
        p.stdin.flush()
        head = json.loads(p.stdout.readline())
        if "error" in head:
            print(f"::error title=encoder vectors::the helper refused {text[:60]!r}: {head['error']}")
            return 1
        out += p.stdout.read(head["count"] * head["dim"] * 4)
    p.stdin.close()
    p.wait(60)
    row = dim * 4
    if bytes(out) == ref:
        print(f"::notice title=encoder vectors::{len(lines)} vectors of {dim} identical to the reference "
              f"({ready.get('model_sha256', '')[:12]})")
        return 0
    for i in range(min(len(out), len(ref)) // row):
        if out[i * row:(i + 1) * row] != ref[i * row:(i + 1) * row]:
            print(f"::error title=encoder vectors::vector {i} ({lines[i][:60]!r}) differs from the reference")
            return 1
    print(f"::error title=encoder vectors::{len(out)} bytes of vectors, the reference has {len(ref)}")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
