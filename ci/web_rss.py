"""RSS driver of the `ulei-web` budget in ``ci/budgets.sh`` (ADR-0015 section 2).

``python3 ci/web_rss.py ULEI_WEB WORKDIR`` runs one ``web_extract`` of five URLs through
``ULEI_WEB``, a build with the ``web-loopback-http`` test switch, against a fake Exa on loopback
plain http whose ``/contents`` reply sits just under the helper's 16 MiB cap, and prints one
``<name> <value>`` line per measurement.
"""

from __future__ import annotations

import json
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

from inbox_load import Failure

REPLY_CAP = 16 * 1024 * 1024
# The reply size of the ADR-0015 measurement: five ASCII pages under the cap.
REPLY_BYTES = 16_700_000
PAGES = 5
CALL_TIMEOUT_MS = 120_000
RUN_TIMEOUT = 180
# A public address literal: the SSRF check passes it without a DNS lookup.
PUBLIC_IP = "93.184.215.14"


def reply_body() -> bytes:
    """A `/contents` reply of five equal pages, padded to ``REPLY_BYTES``."""
    empty = {"requestId": "budget", "results": []}
    frame = len(json.dumps(empty))
    per_page = (REPLY_BYTES - frame) // PAGES
    results = []
    for n in range(PAGES):
        url = f"https://{PUBLIC_IP}/page-{n}"
        shell = {"url": url, "title": f"Budget page {n}", "text": ""}
        size = per_page - len(json.dumps(shell)) - 2
        line = "Budget page text of a fixed size for the helper RSS measurement.\n"
        text = (line * (size // len(line) + 1))[:size]
        # Each line break takes two bytes in the JSON string.
        while len(json.dumps(text)) - 2 > size:
            text = text[: size - text.count("\n")]
        results.append({**shell, "text": text})
    body = json.dumps({"requestId": "budget", "results": results}).encode()
    if not REPLY_BYTES - 65536 < len(body) <= REPLY_CAP:
        raise Failure(f"reply of {len(body)} bytes is not near {REPLY_BYTES} within the cap")
    return body


def serve(body: bytes) -> HTTPServer:
    class Exa(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            self.rfile.read(int(self.headers["Content-Length"]))
            if self.path != "/contents":
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args: object) -> None:
            pass

    server = HTTPServer(("127.0.0.1", 0), Exa)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def main(helper: str, work: Path) -> None:
    body = reply_body()
    server = serve(body)
    env_file = work / "exa.env"
    env_file.write_text("EXA_API_KEY=synthetic-budget-key\n", encoding="utf-8")
    request = {
        "op": "extract",
        "base_url": f"http://127.0.0.1:{server.server_port}",
        "env_file": str(env_file),
        "store_dir": str(work / "web-store"),
        "timeout_ms": CALL_TIMEOUT_MS,
        "urls": [f"https://{PUBLIC_IP}/page-{n}" for n in range(PAGES)],
        "char_limit": 15000,
    }
    try:
        # GNU time forks a small process to exec the helper, so the peak is the helper's own;
        # this driver's memory would count in a direct child's maximum.
        run = subprocess.run(
            ["/usr/bin/time", "-f", "%M", "-o", str(work / "web-rss-kb"), helper],
            input=json.dumps(request).encode(),
            capture_output=True,
            env={},
            timeout=RUN_TIMEOUT,
            check=False,
        )
    finally:
        server.shutdown()
    if run.returncode != 0:
        raise Failure(f"ulei-web exited with {run.returncode}: {run.stdout[:300]!r}")
    peak_kb = (work / "web-rss-kb").read_text(encoding="utf-8").split()[-1]
    pages = json.loads(run.stdout)["results"]
    cut = [p for p in pages if not p["error"] and "[TRUNCATED]" in p["content"]]
    if len(cut) != PAGES:
        raise Failure(f"expected {PAGES} truncated pages, got {json.dumps(pages)[:300]}")
    print(f"reply_bytes {len(body)}")
    print(f"peak_rss_kb {peak_kb}")


if __name__ == "__main__":
    try:
        main(sys.argv[1], Path(sys.argv[2]))
    except Failure as e:
        print(f"web_rss: {e}", file=sys.stderr)
        sys.exit(1)
