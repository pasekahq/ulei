"""The encoder engine's real models and reference data for CI, from public sources only
(ADR-0087 section "E12a").

``python3 ci/encoder_model.py e5 OUT HELPER PYTHON`` downloads multilingual-e5-small's fp32
checkpoint from Hugging Face at the pinned revision (every file checked by SHA-256), converts it
with ``HELPER convert`` exactly as every ADR-0087 measurement's file was made (E6c) and requires
the content digest ``E5_DIGEST``: checkpoint, token order and converter reproduce that file byte
for byte. ``PYTHON`` is the interpreter of a venv with ``ci/requirements-encoder-ref.txt``: it
runs ``ci/encoder_golden.py`` for the PyTorch fp32 golden dumps and the tokenizer parity cases.
Layout::

    OUT/model/model.ulei-infer               the converted weight file (a model directory)
    OUT/ref/golden/e5/                       fp32 dumps of the 16 golden cases (E0), manifest.json
    OUT/ref/parity/e5-cases.jsonl            {"text", "ids"} of transformers' tokenizer (E2 variant B)
    OUT/ref/inputs/e5-probe.json             token ids of the 116 probe inputs
    OUT/ref/ref/e5-116.f32                   their pooled vectors (E7k; every CPU measured so far)
    OUT/ref/texts/synthetic-664.{jsonl,f32}  synthetic texts and the helper's vectors for them

The small files come from ``ci/encoder-ref/`` (committed: the token orders, the probe inputs and
reference vectors, the synthetic texts and their vectors, the golden cases' texts and ids).

``python3 ci/encoder_model.py bge OUT HELPER PYTHON`` does the same for bge-m3 (manual job only):
BAAI/bge-m3 publishes no safetensors, so ``PYTHON`` (torch) first turns ``pytorch_model.bin``
into ``model.safetensors`` (``weights_only`` load, no pickled code). Layout as above with
``golden/bge``, ``parity/bge-cases.jsonl``, ``inputs/bge-probe.json`` and ``ref/bge-76.f32``.

Stdlib only; a download that fails its SHA-256 is refused.
"""

from __future__ import annotations

import gzip
import hashlib
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REF = HERE / "encoder-ref"
HF = "https://huggingface.co"

E5 = {
    "repo": "intfloat/multilingual-e5-small",
    "revision": "614241f622f53c4eeff9890bdc4f31cfecc418b3",
    "files": {
        "config.json": "69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959",
        "model.safetensors": "1a55775f53449dac10a2bcbc312469fac40b96d53198c407081a831f81c98477",
        # (the digests on their own lines keep gitleaks' generic-api-key rule off a file name)
        "tokenizer.json":
            "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    },
    "order": ("miracl-ru-en-docs3.u32.gz", "4d730c58285b2297ff6d5472584e1f29b26dd16e7aa3818f6ae76e2add8659fc"),
    "digest": "a06d503fdcb8b1e5062128bd72cf6ed198c314536b7a25c4108ff700bc25d923",
    "convert": ["--pooling", "mean", "--query-prefix", "query: ", "--passage-prefix", "passage: ",
                "--act-block", "32", "--tokenizer-pipeline", "whitespace-split"],
    "name": "e5", "pooling": "mean", "cases": "golden-cases.json", "probe": ("e5-probe.json", "e5-116.f32"),
}
BGE = {
    "repo": "BAAI/bge-m3",
    "revision": "5617a9f61b028005a4858fdac845db406aefb181",
    "files": {
        "config.json": "26159e7ad065073448460117eb24b7a4572f6f4e78eadff65dc0a11c052449fa",
        "pytorch_model.bin": "b5e0ce3470abf5ef3831aa1bd5553b486803e83251590ab7ff35a117cf6aad38",
        "tokenizer.json":
            "21106b6d7dab2952c1d496fb21d5dc9db75c28ed361a05f5020bbba27810dd08",
    },
    # The safetensors file of ADR-0087 section 4.1 (torch 2.15.0, safetensors 0.8.0); its SHA-256
    # enters the weight file's meta, so the digest below holds only if the writer is the same.
    "safetensors": "993b2248881724788dcab8c644a91dfd63584b6e5604ff2037cb5541e1e38e7e",
    "order": ("miracl-ru-en-docs3-bge-m3.u32.gz", "ee3384390349fcbe7650f9418a641e67602819d30a1fb69ecb086ada2d886961"),
    "digest": "606910506167edbea1c03fec1767d97fa909e41d6158f9eca49ca89d37349270",
    "convert": ["--pooling", "cls", "--query-prefix", "", "--passage-prefix", "",
                "--act-block", "32", "--tokenizer-pipeline", "whitespace-split"],
    "name": "bge", "pooling": "cls", "cases": "golden-cases-bge.json", "probe": ("bge-probe.json", "bge-76.f32"),
}

TO_SAFETENSORS = (
    "import sys, torch\n"
    "from safetensors.torch import save_file, load_file\n"
    "sd = torch.load(sys.argv[1], map_location='cpu', weights_only=True, mmap=True)\n"
    "sd = {k: v.contiguous() for k, v in sd.items()}\n"
    "save_file(sd, sys.argv[2])\n"
    "back = load_file(sys.argv[2])\n"
    "assert back.keys() == sd.keys() and all(torch.equal(back[k], sd[k]) for k in sd)\n"
)


class Failure(Exception):
    pass


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def fetch(url: str, dest: Path, want: str) -> None:
    if dest.exists() and sha256(dest) == want:
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_name(dest.name + ".part")
    for attempt in range(5):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "ulei-ci"})
            with urllib.request.urlopen(req, timeout=120) as r, open(tmp, "wb") as f:
                shutil.copyfileobj(r, f, 1 << 20)
            break
        except OSError as e:
            if attempt == 4:
                raise Failure(f"{url}: {e}") from e
            time.sleep(5 * (attempt + 1))
    got = sha256(tmp)
    if got != want:
        tmp.unlink()
        raise Failure(f"{url}: SHA-256 {got}, pinned {want}")
    tmp.replace(dest)


def model(m: dict, out: Path, helper: str, python: str) -> str:
    ck = out / "checkpoint"
    for name, want in m["files"].items():
        fetch(f"{HF}/{m['repo']}/resolve/{m['revision']}/{name}", ck / name, want)
    if "safetensors" in m:
        st = ck / "model.safetensors"
        if not st.exists():
            subprocess.run([python, "-I", "-c", TO_SAFETENSORS, str(ck / "pytorch_model.bin"), str(st)], check=True)
            (ck / "pytorch_model.bin").unlink()
        if sha256(st) != m["safetensors"]:
            print(f"::warning title=encoder model::{st.name} differs from the one of ADR-0087 4.1; "
                  "the weight file's digest will differ (the vectors are compared below anyway)")
    order = out / "order.u32"
    order.write_bytes(gzip.decompress((REF / m["order"][0]).read_bytes()))
    if sha256(order) != m["order"][1]:
        raise Failure(f"{m['order'][0]}: not the pinned token order")
    wf = out / "model" / "model.ulei-infer"
    wf.parent.mkdir(parents=True, exist_ok=True)
    run = subprocess.run([helper, "convert", "--checkpoint", str(ck), "--token-order", str(order), *m["convert"],
                          "--tokenizer", str(ck / "tokenizer.json"), "--out", str(wf)],
                         capture_output=True, text=True, check=False)
    if run.returncode != 0:
        raise Failure(f"convert: {run.stderr.strip()}")
    ver = subprocess.run([helper, "verify", str(wf)], capture_output=True, text=True, check=False)
    if ver.returncode != 0:
        raise Failure(f"verify: exit {ver.returncode}: {ver.stderr.strip()}")
    if f"content sha256 {m['digest']}" not in ver.stdout:
        msg = f"content digest is not the pinned {m['digest']}: {ver.stdout.strip()}"
        if "safetensors" not in m:
            raise Failure(msg)
        print(f"::warning title=encoder model::{msg}")
    ref = out / "ref"
    for sub, files in (("inputs", [m["probe"][0]]), ("ref", [m["probe"][1]]),
                       ("texts", ["synthetic-664.jsonl", "synthetic-664.f32"])):
        (ref / sub).mkdir(parents=True, exist_ok=True)
        for f in files:
            shutil.copyfile(REF / f, ref / sub / f)
    subprocess.run([python, "-I", str(HERE / "encoder_golden.py"), str(ck), str(REF / m["cases"]),
                    str(REF / "synthetic-664.jsonl"), str(ref), m["name"], m["pooling"]], check=True)
    return f"{m['repo']} at {m['revision'][:12]} converted, {ver.stdout.strip()}"


def main(argv: list[str]) -> int:
    try:
        if len(argv) == 4 and argv[0] in ("e5", "bge"):
            line = model(E5 if argv[0] == "e5" else BGE, Path(argv[1]), argv[2], argv[3])
            print(f"::notice title=encoder model::{line}")
            return 0
    except (Failure, OSError, subprocess.CalledProcessError) as e:
        print(f"::error title=encoder model::{e}")
        return 1
    print("usage: encoder_model.py e5|bge OUT HELPER PYTHON", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
