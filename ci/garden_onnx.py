"""The ONNX path of the garden on a tiny model, the driver of the CI job ``garden-onnx``
(ADR-0047 section 8).

``python3 ci/garden_onnx.py model DIR`` writes the tiny model into DIR in the layout the helper
reads. ``onnx/model_quantized.onnx`` has the inputs and the output of the bge-m3 export:
``input_ids`` and ``attention_mask`` int64 ``[batch, sequence]``, ``last_hidden_state`` float
``[batch, sequence, dim]``, where dim is the vocabulary size. Its embedding table is the
identity, so position s of row b is ``mask[b,s] * onehot(ids[b,s]) + sum over t of mask[b,t] *
onehot(ids[b,t])``: the CLS vector is the row's token histogram plus the CLS token itself.
``tokenizer.json`` is a character-level Unigram tokenizer of the bge-m3 pipeline shape
(Metaspace, no byte fallback, ``<s> <pad> </s> <unk>`` as ids 0 to 3), without the precompiled
character map.

``python3 ci/garden_onnx.py serve ULEI_GARDEN HELPER MODEL_DIR ORT_LIB WORKDIR`` starts
``ULEI_GARDEN serve`` with ``garden.embedder.enabled`` on HELPER, MODEL_DIR and ORT_LIB, checks
that `health` answers `dense: ready`, proposes fictional facts and processes their seedbed
records through `seedbed_take` and `seedbed_done`, checks that `search` for a
query sharing no word or word prefix with any fact ranks first the fact its letters match and
is not lexical-only, stops the daemon and prints one ``::notice`` line.
"""

from __future__ import annotations

import json
import string
import struct
import subprocess
import sys
import time
from pathlib import Path

from garden_run import FakeInbox, alive, night, spawn, write_config
from inbox_load import Failure, call, stop

SPECIALS = ["<s>", "<pad>", "</s>", "<unk>"]
PIECES = ["▁", *string.ascii_lowercase, *string.ascii_uppercase, *string.digits, *".,:;?!-'"]
OPSET = 13
IR_VERSION = 8
FLOAT = 1
INT64 = 7

FACTS = [
    ("Ilsa Varn", "keeps a brass lantern in the attic."),
    ("Odo Quill", "buys fig jam on Mondays."),
    ("Wren Holt", "walks by the quiet pond."),
]
# The words of the first fact reversed: the same letters, no word of any fact.
QUERY = "ssarb nretnal"
QUIET_SECONDS = 3.0
DENSE_TIMEOUT = 30.0
SOURCE_REF = "synthetic onnx material"


def varint(n: int) -> bytes:
    out = bytearray()
    while True:
        low = n & 0x7F
        n >>= 7
        if n:
            out.append(low | 0x80)
        else:
            out.append(low)
            return bytes(out)


def scalar(field: int, n: int) -> bytes:
    return varint(field << 3) + varint(n)


def nested(field: int, payload: bytes) -> bytes:
    return varint(field << 3 | 2) + varint(len(payload)) + payload


def text(field: int, value: str) -> bytes:
    return nested(field, value.encode())


def tensor(name: str, dims: list[int], data_type: int, raw: bytes) -> bytes:
    """A TensorProto with ``raw`` as its little-endian raw data."""
    return (b"".join(scalar(1, d) for d in dims) + scalar(2, data_type) + text(8, name)
            + nested(9, raw))


def value_info(name: str, elem_type: int, dims: list[int | str]) -> bytes:
    """A ValueInfoProto of a tensor; a string dimension is symbolic."""
    shape = b"".join(
        nested(1, scalar(1, d) if isinstance(d, int) else text(2, d)) for d in dims
    )
    return text(1, name) + nested(2, nested(1, scalar(1, elem_type) + nested(2, shape)))


def node(op: str, inputs: list[str], outputs: list[str], **ints: int) -> bytes:
    """A NodeProto of the default domain with integer attributes."""
    attributes = b"".join(
        nested(5, text(1, key) + scalar(20, 2) + scalar(3, value)) for key, value in ints.items()
    )
    return (b"".join(text(1, i) for i in inputs) + b"".join(text(2, o) for o in outputs)
            + text(3, outputs[0]) + text(4, op) + attributes)


def model(dim: int) -> bytes:
    """The serialized ModelProto of the tiny graph over a vocabulary of ``dim`` pieces."""
    identity = b"".join(
        struct.pack("<f", 1.0 if row == col else 0.0) for row in range(dim) for col in range(dim)
    )
    nodes = [
        node("Gather", ["table", "input_ids"], ["embedded"]),
        node("Cast", ["attention_mask"], ["mask"], to=FLOAT),
        node("Unsqueeze", ["mask", "last_axis"], ["mask3"]),
        node("Mul", ["embedded", "mask3"], ["tokens"]),
        node("ReduceSum", ["tokens", "sequence_axis"], ["bag"]),
        node("Add", ["tokens", "bag"], ["last_hidden_state"]),
    ]
    initializers = [
        tensor("table", [dim, dim], FLOAT, identity),
        tensor("last_axis", [1], INT64, struct.pack("<q", 2)),
        tensor("sequence_axis", [1], INT64, struct.pack("<q", 1)),
    ]
    graph = (
        b"".join(nested(1, n) for n in nodes)
        + text(2, "tiny")
        + b"".join(nested(5, t) for t in initializers)
        + nested(11, value_info("input_ids", INT64, ["batch", "sequence"]))
        + nested(11, value_info("attention_mask", INT64, ["batch", "sequence"]))
        + nested(12, value_info("last_hidden_state", FLOAT, ["batch", "sequence", dim]))
    )
    return (scalar(1, IR_VERSION) + text(2, "ulei-ci") + nested(7, graph)
            + nested(8, scalar(2, OPSET)))


def tokenizer() -> dict:
    """The ``tokenizer.json`` document of the tiny model."""
    vocab = [[p, 0.0] for p in SPECIALS] + [[p, -2.0 if p == "▁" else -3.0] for p in PIECES]
    metaspace = {"type": "Metaspace", "replacement": "▁", "prepend_scheme": "always",
                 "split": True}
    return {
        "version": "1.0",
        "truncation": None,
        "padding": None,
        "added_tokens": [
            {"id": i, "content": p, "single_word": False, "lstrip": False, "rstrip": False,
             "normalized": False, "special": True}
            for i, p in enumerate(SPECIALS)
        ],
        "normalizer": {"type": "Sequence", "normalizers": [
            {"type": "Replace", "pattern": {"Regex": " {2,}"}, "content": " "},
        ]},
        "pre_tokenizer": metaspace,
        "post_processor": {
            "type": "TemplateProcessing",
            "single": [{"SpecialToken": {"id": "<s>", "type_id": 0}},
                       {"Sequence": {"id": "A", "type_id": 0}},
                       {"SpecialToken": {"id": "</s>", "type_id": 0}}],
            "pair": [],
            "special_tokens": {p: {"id": p, "ids": [SPECIALS.index(p)], "tokens": [p]}
                               for p in ("<s>", "</s>")},
        },
        "decoder": metaspace,
        "model": {"type": "Unigram", "unk_id": SPECIALS.index("<unk>"), "vocab": vocab,
                  "byte_fallback": False},
    }


def write_model(directory: Path) -> int:
    """Writes the tiny model and its tokenizer under ``directory``; returns the dimension."""
    dim = len(SPECIALS) + len(PIECES)
    (directory / "onnx").mkdir(parents=True, exist_ok=True)
    (directory / "onnx" / "model_quantized.onnx").write_bytes(model(dim))
    (directory / "tokenizer.json").write_text(
        json.dumps(tokenizer(), ensure_ascii=False, indent=1), encoding="utf-8"
    )
    return dim


def words(value: str) -> list[str]:
    return ["".join(c for c in w if c.isalnum()).lower() for w in value.split()]


def check_query() -> None:
    """Fails unless no word of ``QUERY`` is a prefix of a word of a fact, so the lexical side,
    which matches word prefixes, cannot find any fact."""
    fact_words = {w for entity, fact in FACTS for w in words(f"{entity} {fact}")}
    for q in words(QUERY):
        if any(w.startswith(q) for w in fact_words):
            raise Failure(f"query word {q!r} is a word prefix of a fact")


def propose(path: str, entity: str, fact: str) -> None:
    answer, _ = call(path, "propose", {
        "fact": fact,
        "entity": entity,
        "kind": "person",
        "source_kind": "owner",
        "source_ref": SOURCE_REF,
    })
    if answer.get("status") != "seeded":
        raise Failure(f"propose about {entity} answered {answer}")


def dense_hit(path: str, daemon: subprocess.Popen) -> dict:
    """The first `search` answer for ``QUERY`` that ranks the first fact first, as a processed
    fact rather than a seedbed record, and is not lexical-only; it waits ``QUIET_SECONDS``
    before each, so the worker can embed the facts."""
    deadline = time.monotonic() + DENSE_TIMEOUT
    while True:
        time.sleep(QUIET_SECONDS)
        alive(daemon, "while the facts are embedded")
        answer, _ = call(path, "search", {"q": QUERY})
        facts = answer["facts"]
        processed = bool(facts) and not facts[0].get("unprocessed")
        if processed and FACTS[0][1] in facts[0]["fact"] and not answer.get("lexical_only"):
            return answer
        if time.monotonic() > deadline:
            raise Failure(f"search {QUERY!r} has no dense hit after {DENSE_TIMEOUT} s: {answer}")


def serve(garden: str, helper: str, model_dir: Path, ort_lib: Path, work: Path) -> str:
    """Runs the daemon with the real helper; returns the summary of the ``::notice`` line."""
    check_query()
    root = work / "garden"
    root.mkdir(mode=0o700)
    path = str(root / "run" / "garden.sock")
    inbox = FakeInbox(str(root / "c1.sock"))
    try:
        config = write_config(
            root, path, str(root / "c1.sock"),
            embedder=(f'enabled = true\nhelper = "{helper}"\nmodel_dir = "{model_dir}"\n'
                      f'ort_library = "{ort_lib}"\nthreads = 1\n'),
        )
        daemon, ready = spawn(garden, config, path)
        try:
            health, _ = call(path, "health", {})
            if health["dense"] != "ready":
                raise Failure(f"health answered {health}")
            for entity, fact in FACTS:
                propose(path, entity, fact)
            if night(path)[0] != len(FACTS):
                raise Failure(f"the night did not process the {len(FACTS)} records")
            answer = dense_hit(path, daemon)
        finally:
            stop(daemon)
        inbox.check()
    finally:
        inbox.close()
    return (f"dense ready {ready * 1000:.0f} ms after spawn; search {QUERY!r} ranked first "
            f"{answer['facts'][0]['fact']!r} of {len(answer['facts'])} facts with no lexical match")


def main(argv: list[str]) -> int:
    try:
        if len(argv) == 2 and argv[0] == "model":
            dim = write_model(Path(argv[1]))
            print(f"garden_onnx: tiny model of dimension {dim} in {argv[1]}")
            return 0
        if len(argv) == 6 and argv[0] == "serve":
            summary = serve(argv[1], argv[2], Path(argv[3]), Path(argv[4]), Path(argv[5]))
            print(f"::notice title=garden-onnx::{summary}")
            return 0
    except (Failure, OSError, KeyError, ValueError) as e:
        print(f"garden_onnx: {e}", file=sys.stderr)
        return 1
    print("usage: garden_onnx.py model DIR | serve ULEI_GARDEN HELPER MODEL_DIR ORT_LIB WORKDIR",
          file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
