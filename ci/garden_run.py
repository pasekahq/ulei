"""Driver of the `ulei-garden` budgets in ``ci/budgets.sh`` (ADR-0047 section 7).

``python3 ci/garden_run.py ULEI_GARDEN WORKDIR`` writes a synthetic ``ulei.toml`` with the
embedder off, a producer key and a fresh store under WORKDIR, serves a fake C1 inbox there,
fills the store with fictional facts through C7 `propose` into the seedbed and the night path
`seedbed_take` and `seedbed_done`, restarts ``serve`` on the filled store, drives it with
`pointers`, `search` and `propose`, whose records stay unprocessed and answer the later
`pointers` and `search` beside the facts (ADR-0055 sections 3 and 8), and measures its idle.
On a copy of the filled store it then runs the night path over ``NIGHT_RECORDS`` records spread
over every person and over as many records naming one person. On a fresh store of a daemon of
its own it writes ``PLACES_COUNT`` synthetic places through C7 `places_write`, `places_profile`
and `places_verdict` and times ``PLACES_SEARCHES`` `places_search` calls. It prints one
``<name> <value>`` line per measurement. Any non-200 answer or unexpected body is a failure.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

from inbox_load import (
    Failure,
    call,
    proc_field,
    read_message,
    stop,
    thread_switches,
)

FILL_FACTS = 2_000
NIGHT_BATCH = 100
NIGHT_RECORDS = 500
REQUESTS = 10_000
IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
READY_RESTARTS = 21
READY_TIMEOUT = 5.0
PUBLISH_TIMEOUT = 30.0
INBOX_TIMEOUT = 5.0
SOURCE_REF = "synthetic budget material"

# Transliterations of the full names are pairwise at least three edits apart, so a one-letter
# typo of the Latin spelling names exactly one person.
FIRST_NAMES = [
    ("Ведмира", "Vedmira"), ("Ольгерд", "Olgerd"), ("Миролюб", "Miroljub"),
    ("Златина", "Zlatina"), ("Борислав", "Borislav"), ("Радмила", "Radmila"),
    ("Всеволод", "Vsevolod"), ("Любава", "Ljubava"), ("Святомир", "Svjatomir"),
    ("Дарина", "Darina"),
]
SURNAMES = [
    ("Ольхастов", "Olhastov"), ("Брезняков", "Breznjakov"), ("Тумановец", "Tumanovec"),
    ("Кречетин", "Krecetin"), ("Сабуровский", "Saburovskij"), ("Ракитянский", "Rakitjanskij"),
    ("Жемчугов", "Zemcugov"), ("Велесов", "Velesov"), ("Гордеевич", "Gordeevic"),
    ("Ладожин", "Ladozin"), ("Мятлев", "Mjatlev"), ("Черноус", "Cernous"),
    ("Шиповник", "Sipovnik"), ("Ясколов", "Jaskolov"), ("Дубравин", "Dubravin"),
    ("Колыванов", "Kolyvanov"), ("Ферзиков", "Ferzikov"), ("Хмелёв", "Hmelev"),
    ("Цветаев", "Cvetaev"), ("Юрганов", "Jurganov"),
]
PEOPLE = [(f"{f} {s}", f"{fl} {sl}") for f, fl in FIRST_NAMES for s, sl in SURNAMES]

# (phrase, query word) pairs; a fact is one person with one of each list.
ACTIVITIES = [
    ("keeps a blue bicycle", "bicycle"), ("plays the cello", "cello"),
    ("buys sourdough bread", "sourdough"), ("walks a grey terrier", "terrier"),
    ("repairs old lanterns", "lanterns"), ("collects river pebbles", "pebbles"),
    ("sketches heron nests", "heron"), ("rents a rowing boat", "rowing"),
    ("sings in the choir", "choir"), ("grows tomato seedlings", "seedlings"),
    ("trades chess puzzles", "chess"), ("bakes plum tarts", "tarts"),
    ("borrows atlas volumes", "atlas"), ("feeds stray pigeons", "pigeons"),
    ("weaves wool scarves", "scarves"), ("tunes harpsichords", "harpsichords"),
    ("paints ceramic tiles", "ceramic"), ("brews juniper tea", "juniper"),
    ("binds leather journals", "journals"), ("photographs lighthouses", "lighthouses"),
]
PLACES = [
    ("near the Larkwell ferry", "Larkwell"), ("at the Brindlemoor market", "Brindlemoor"),
    ("in Ostvale gardens", "Ostvale"), ("by Quillhaven pier", "Quillhaven"),
    ("behind the Marrowfen library", "Marrowfen"), ("at Thistlecombe station", "Thistlecombe"),
    ("on Duskmere hill", "Duskmere"), ("inside the Fernholt arcade", "Fernholt"),
    ("along Cobblereach canal", "Cobblereach"), ("at the Wrenford bakery", "Wrenford"),
    ("near Velmora fountain", "Velmora"), ("in Tarnwick square", "Tarnwick"),
    ("by the Oskholm mill", "Oskholm"), ("at Pellbrook clinic", "Pellbrook"),
    ("under Hollowmere bridge", "Hollowmere"), ("in the Saltreach yard", "Saltreach"),
    ("next to Gorsefield school", "Gorsefield"), ("at the Ambervale theatre", "Ambervale"),
    ("around Kestrelby harbour", "Kestrelby"), ("beside Mirelock orchard", "Mirelock"),
]
TIMES = [
    "on rainy evenings", "every spring", "after the harvest fair", "before breakfast",
    "on long weekends", "during the winter thaw", "after choir practice", "in late autumn",
    "on market days", "at dusk", "when the tide is low", "before the first frost",
    "on quiet mornings", "during school holidays", "after the night shift",
    "in the dry season", "on feast days", "when the ferry is late", "after the storms",
    "at the turn of the year",
]
# Every tenth search asks one of these; no fact or record holds their words, so each publishes
# one miss, its repeats coalesce.
MISS_EVERY = 10
MISSES = [
    f"{a} {b}"
    for a in ("zorbulent", "quaxifer", "drimbleth", "vorpaxine", "skelluvet")
    for b in ("glimtrove", "pondraxel", "frunsible", "wexhollow")
]

C7_SCHEMA = Path(__file__).resolve().parent.parent / "contracts" / "c7-garden.schema.json"
PLACES_COUNT = 1_000
PLACES_BATCH = 100
PLACES_SEARCHES = 100
# Percent of the searches that must answer at least one place.
PLACES_HIT_SHARE = 90
DAY = 86_400
HOME = (12.25, -31.5)
# Grid step in degrees, about 165 m, past the 75 m within which the bed pairs two places of
# different names as possible duplicates.
GRID_STEP = 0.0015
GRID_SIDE = 32
PLACE_ADJECTIVES = [
    "Amber", "Brass", "Cedar", "Copper", "Dune", "Ember", "Fable", "Gilded", "Harbour", "Indigo",
    "Juniper", "Kestrel", "Lantern", "Marble", "Nettle", "Opal", "Pebble", "Quill", "Rowan",
    "Saffron", "Thistle", "Umber", "Velvet", "Willow", "Yarrow",
]
PLACE_NOUNS = [
    "Bowl", "Table", "Kitchen", "Oven", "Ladle", "Hearth", "Spoon", "Corner", "Pantry", "Grill",
    "Kettle", "Cellar", "Larder", "Terrace", "Courtyard", "Skillet", "Platter", "Barrel", "Wok",
    "Crust", "Griddle", "Tavern", "Canteen", "Bistro", "Diner", "Saucepan", "Teapot", "Mortar",
    "Basket", "Crock", "Pitcher", "Trencher", "Tureen", "Galley", "Buffet", "Ramekin", "Sieve",
    "Cauldron", "Brazier", "Cupboard",
]
CUISINES = [
    "noodles", "dumplings", "grill", "seafood", "pastry", "curry", "pizza", "salads", "soups",
    "tapas", "bakery",
]
BANNED_CUISINE = "offal"
VIBES = [
    "quiet", "cosy", "lively", "terrace", "candlelit", "rooftop", "garden", "riverside", "family",
    "jazz",
]
HOURS = [
    "Mo-Fr 10:00-22:00; Sa,Su 12:00-23:00", "24/7", "Mo-Su 08:00-15:00",
    "Tu-Su 18:00-02:00; Mo off", "Mo,We,Fr 11:00-14:00,17:00-21:00",
]
LOCAL_TIMES = [
    "2031-03-17T13:00", "2031-03-22T20:00", "2031-03-16T09:30", "2031-03-19T01:30",
    "2031-03-21T18:30", "2031-03-18T23:00",
]
STATUS_SETS = [["active"], ["candidate"], ["candidate", "active", "gone"]]
PLACES_SOURCE = "synthetic-maps"


class FakeInbox:
    """A C1 inbox on a Unix socket that answers each `POST /v1/publish` of a `garden_miss`
    with `200` and the packet's id, and counts them."""

    def __init__(self, path: str) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(path)
        self.listener.listen(8)
        self.published = 0
        self.errors: list[str] = []
        self.lock = threading.Lock()
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self) -> None:
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            with conn:
                conn.settimeout(INBOX_TIMEOUT)
                try:
                    self.answer(conn)
                except (Failure, OSError, ValueError, KeyError) as e:
                    with self.lock:
                        self.errors.append(f"fake inbox: {e!r}")

    def answer(self, conn: socket.socket) -> None:
        head, body = read_message(conn, "publish")
        line = head.split(b"\r\n", 1)[0]
        if not line.startswith(b"POST /v1/publish "):
            raise Failure(f"fake inbox got {line.decode(errors='replace')}")
        packet = json.loads(body)["packet"]
        if packet["kind"] != "garden_miss":
            raise Failure(f"fake inbox got a packet of kind {packet['kind']}")
        reply = json.dumps({"id": packet["id"]}).encode()
        conn.sendall(
            f"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
            f"Content-Length: {len(reply)}\r\nConnection: close\r\n\r\n".encode() + reply
        )
        with self.lock:
            self.published += 1

    def check(self) -> None:
        with self.lock:
            if self.errors:
                raise Failure("; ".join(self.errors))

    def close(self) -> None:
        self.listener.close()


def write_config(
    root: Path, socket_path: str, inbox_path: str, embedder: str = "enabled = false\n"
) -> Path:
    """Writes the C9 document of the run under ``root`` with an owner-only synthetic producer
    key and the ``[garden.embedder]`` body ``embedder``, and returns it."""
    key = root / "producer.key"
    fd = os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(fd, bytes(range(32)))
    finally:
        os.close(fd)
    config = root / "ulei.toml"
    config.write_text(
        f'[garden]\nstore = "{root / "store" / "garden.sqlite3"}"\nsocket = "{socket_path}"\n'
        f'producer_key_file = "{key}"\n\n[garden.embedder]\n{embedder}\n'
        f'[inbox]\nsocket = "{inbox_path}"\n',
        encoding="utf-8",
    )
    return config


def spawn(garden: str, config: Path, path: str) -> tuple[subprocess.Popen, float]:
    """A running ``serve`` of ``config`` on the socket ``path`` and the seconds from spawn to
    the first 200 of `health`."""
    spawned = time.perf_counter()
    daemon = subprocess.Popen(
        [garden, "serve", "--config", str(config)], stdout=subprocess.DEVNULL
    )
    try:
        while True:
            if daemon.poll() is not None:
                raise Failure(f"daemon exited with status {daemon.returncode} before ready")
            try:
                call(path, "health", {})
            except (FileNotFoundError, ConnectionRefusedError):
                if time.perf_counter() - spawned > READY_TIMEOUT:
                    raise Failure(f"no health answer within {READY_TIMEOUT} s") from None
                time.sleep(0.0002)
                continue
            return daemon, time.perf_counter() - spawned
    except BaseException:
        stop(daemon)
        raise


def alive(daemon: subprocess.Popen, during: str) -> None:
    if daemon.poll() is not None:
        raise Failure(f"daemon exited with status {daemon.returncode} {during}")


def fact(index: int) -> str:
    """The text of fact ``index``; distinct for every index below 8,000."""
    activity = ACTIVITIES[index % len(ACTIVITIES)][0]
    place = PLACES[index // len(ACTIVITIES) % len(PLACES)][0]
    when = TIMES[index // (len(ACTIVITIES) * len(PLACES)) % len(TIMES)]
    return f"{activity} {place} {when}"


def propose(path: str, entity: str, index: int) -> float:
    """Proposes fact ``index`` about the person ``entity``; returns the round trip of its
    write into the seedbed."""
    answer, elapsed = call(path, "propose", {
        "fact": f"{entity} {fact(index)}.",
        "entity": entity,
        "kind": "person",
        "source_kind": "owner",
        "source_ref": SOURCE_REF,
    })
    if answer.get("status") != "seeded":
        raise Failure(f"propose of fact {index} about {entity} answered {answer}")
    return elapsed


def night(path: str) -> tuple[int, list[float]]:
    """Hands every unprocessed seedbed record to the edge through `seedbed_take` and
    `seedbed_done`, ``NIGHT_BATCH`` at a time; returns the records processed, each of which
    must be accepted as a fact, and the round trip of each `seedbed_done`."""
    processed = 0
    rounds = []
    while True:
        taken, _ = call(path, "seedbed_take", {"limit": NIGHT_BATCH})
        refs = [record["ref"] for record in taken["records"]]
        if not refs:
            return processed, rounds
        done, elapsed = call(path, "seedbed_done", {"items": [{"ref": ref} for ref in refs]})
        for item in done["items"]:
            if [o["status"] for o in item["outcomes"]] != ["accepted"]:
                raise Failure(f"seedbed_done of {item['ref']} answered {item['outcomes']}")
        processed += len(refs)
        rounds.append(elapsed)


def fill(garden: str, config: Path, path: str) -> tuple[int, int]:
    """Fills the fresh store with ``FILL_FACTS`` facts in Cyrillic spelling through `propose`
    and the night path; returns the daemon's wchar per seedbed write and per processed
    record."""
    daemon, _ = spawn(garden, config, path)
    try:
        start = proc_field(daemon.pid, "io", "wchar")
        for i in range(FILL_FACTS):
            propose(path, PEOPLE[i % len(PEOPLE)][0], i)
        seeded = proc_field(daemon.pid, "io", "wchar")
        processed, _ = night(path)
        end = proc_field(daemon.pid, "io", "wchar")
        alive(daemon, "during the fill")
    finally:
        stop(daemon)
    if processed != FILL_FACTS:
        raise Failure(f"the night processed {processed} records, expected {FILL_FACTS}")
    return (seeded - start) // FILL_FACTS, (end - seeded) // FILL_FACTS


def crowded_names() -> list[str]:
    """``NIGHT_RECORDS`` names of the first person: three in five Cyrillic, then its Latin
    spelling and that spelling with a typo, which the edge resolves by transliteration and by
    one edit."""
    cyrillic, latin = PEOPLE[0]
    spellings = [cyrillic, cyrillic, cyrillic, latin, typo(latin)]
    return [spellings[i % len(spellings)] for i in range(NIGHT_RECORDS)]


def night_phase(
    path: str, daemon: subprocess.Popen, names: list[str], first: int
) -> tuple[int, int]:
    """Proposes facts ``first`` onwards, one about each person of ``names``, and hands them to
    the night path; returns the daemon's wchar per processed record and the p50 of
    `seedbed_done` per batch."""
    for i, entity in enumerate(names):
        propose(path, entity, first + i)
    start = proc_field(daemon.pid, "io", "wchar")
    processed, rounds = night(path)
    end = proc_field(daemon.pid, "io", "wchar")
    if processed != len(names):
        raise Failure(f"the night processed {processed} records, expected {len(names)}")
    return (end - start) // processed, p50(rounds)


def night_phases(garden: str, root: Path, inbox_path: str) -> dict[str, int]:
    """On the copy of the filled store under ``root``: the night path over ``NIGHT_RECORDS``
    new facts spread over every person, the control, then over as many about one person."""
    path = str(root / "run" / "garden.sock")
    config = write_config(root, path, inbox_path)
    daemon, _ = spawn(garden, config, path)
    try:
        spread = [PEOPLE[i % len(PEOPLE)][0] for i in range(NIGHT_RECORDS)]
        control_bytes, control_done = night_phase(path, daemon, spread, FILL_FACTS)
        crowded_bytes, crowded_done = night_phase(
            path, daemon, crowded_names(), FILL_FACTS + NIGHT_RECORDS
        )
        alive(daemon, "during the night phases")
    finally:
        stop(daemon)
    return {
        "night_records": NIGHT_RECORDS,
        "night_batch": NIGHT_BATCH,
        "control_entities": len(set(spread)),
        "control_write_bytes_per_processed": control_bytes,
        "control_done_p50_us": control_done,
        "crowded_write_bytes_per_processed": crowded_bytes,
        "crowded_done_p50_us": crowded_done,
    }


def place(i: int, now: int) -> dict:
    """The `places_write` entry of synthetic place ``i`` on the grid around ``HOME``, checked
    at ``now``: hours on nine in ten, stale ratings and counts on one in seven, stale hours on
    one in six, the banned cuisine on one in fifty and in the warnings of one in forty."""
    fresh, stale = now - 2 * DAY, now - 40 * DAY
    lat = HOME[0] + (i // GRID_SIDE - GRID_SIDE // 2) * GRID_STEP
    lon = HOME[1] + (i % GRID_SIDE - GRID_SIDE // 2) * GRID_STEP
    cuisine = [CUISINES[i % len(CUISINES)]]
    if i % 3 == 0 and CUISINES[i // 3 % len(CUISINES)] not in cuisine:
        cuisine.append(CUISINES[i // 3 % len(CUISINES)])
    if i % 50 == 7:
        cuisine.append(BANNED_CUISINE)
    rated = stale if i % 7 == 3 else fresh
    observed = [
        ("location", f"{lat:.6f}, {lon:.6f}", fresh),
        ("cuisine", ", ".join(cuisine), fresh),
        ("vibe", f"{VIBES[i % len(VIBES)]}, {VIBES[i // len(VIBES) % len(VIBES)]}", fresh),
        ("review_count", str(i * 53 % 900 + 5), rated),
    ]
    if i % 13 != 5:
        observed.append(("rating", f"{3 + i * 37 % 21 / 10:.1f}", rated))
    if i % 9 != 4:
        low = 15 + i * 7 % 12 * 5
        observed.append(("price_for_two", f"{low}-{low + 20}", fresh))
    if i % 10 != 9:
        observed.append(("hours", HOURS[i % len(HOURS)], now - 10 * DAY if i % 6 == 1 else fresh))
    if i % 25 == 11:
        observed.append(("operating_status", "temporarily_closed", fresh))
    elif i % 7 == 0:
        observed.append(("operating_status", "open", fresh))
    if i % 40 == 13:
        observed.append(("warnings", BANNED_CUISINE, fresh))
    status = "gone" if i % 60 == 17 else "candidate" if i % 3 == 0 else "active"
    return {
        "name": f"{PLACE_ADJECTIVES[i % len(PLACE_ADJECTIVES)]} "
                f"{PLACE_NOUNS[i // len(PLACE_ADJECTIVES)]}",
        "status": status,
        "source": PLACES_SOURCE,
        "attrs": [
            {"key": k, "value": v, "checked_at": at, "source": PLACES_SOURCE}
            for k, v, at in observed
        ],
    }


def fill_places(path: str, now: int) -> list[str]:
    """Writes ``PLACES_COUNT`` new places in batches of ``PLACES_BATCH``, the profile's `home`,
    `min_rating` and a ban, and a verdict on one place in 45; returns the place ids."""
    ids = []
    for first in range(0, PLACES_COUNT, PLACES_BATCH):
        batch = [place(i, now) for i in range(first, first + PLACES_BATCH)]
        answer, _ = call(path, "places_write", {"places": batch})
        for i, written in enumerate(answer["places"], first):
            refused = [a for a in written["attrs"] if a["outcome"] == "refused"]
            if written["outcome"] != "created" or refused:
                raise Failure(f"places_write of place {i} answered {written}")
            ids.append(written["id"])
    for key, value in (
        ("home", f"{HOME[0]}, {HOME[1]}"), ("min_rating", "3.5"), ("ban", BANNED_CUISINE),
    ):
        answer, _ = call(path, "places_profile", {
            "key": key, "value": value, "source_ref": "synthetic:profile", "set_at": now - DAY,
        })
        if answer["outcome"] != "written":
            raise Failure(f"places_profile {key} answered {answer}")
    for i in range(0, PLACES_COUNT, 45):
        verdict = (1, 0, -1)[i // 45 % 3]
        body = {"place": ids[i], "verdict": verdict, "said_at": now - 3 * DAY,
                "source_ref": f"synthetic:verdict-{i}"}
        if verdict == 1:
            body["note"] = "quiet terrace, kind staff"
        answer, _ = call(path, "places_verdict", body)
        if answer["outcome"] != "written":
            raise Failure(f"places_verdict of place {i} answered {answer}")
    return ids


def places_query(n: int) -> dict:
    """`places_search` ``n``: cuisine, distance, rating, price and status filters, `near`,
    `open_now` with `local_time`, `vibe` and `limit`, each on its own period over ``n``."""
    filters: dict = {}
    if n % 2 == 0:
        filters["cuisine"] = [CUISINES[n % len(CUISINES)]]
        if n % 4 == 0 and CUISINES[n // 4 % len(CUISINES)] not in filters["cuisine"]:
            filters["cuisine"].append(CUISINES[n // 4 % len(CUISINES)])
    if n % 3 != 2:
        filters["max_distance_m"] = 800 + n % 5 * 600
    if n % 4 == 1:
        filters["min_rating"] = 3.5 + n % 3 * 0.5
    if n % 5 == 3:
        filters["max_price_for_two"] = 40 + n % 4 * 10
    if n % 7 == 4:
        filters["status"] = STATUS_SETS[n % len(STATUS_SETS)]
    req: dict = {"filters": filters} if filters else {}
    if n % 6 == 5:
        req["near"] = {"lat": HOME[0] + n % 17 * GRID_STEP, "lon": HOME[1] - n % 13 * GRID_STEP}
    if n % 2 == 1:
        req["open_now"] = True
        req["local_time"] = LOCAL_TIMES[n // 2 % len(LOCAL_TIMES)]
    if n % 3 == 1:
        req["vibe"] = f"{VIBES[n % len(VIBES)]} {VIBES[n // 3 % len(VIBES)]}"
    if n % 5 != 4:
        req["limit"] = (5, 10, 20, 50)[n % 5]
    return req


SCHEMA_KEYWORDS = {
    "type", "description", "enum", "minimum", "maximum", "minLength", "maxLength", "pattern",
    "items", "minItems", "maxItems", "properties", "required", "additionalProperties", "$ref",
}
JSON_TYPES = {
    "object": dict, "array": list, "string": str, "boolean": bool, "integer": int,
    "number": (int, float),
}


def conform(value: object, schema: dict, defs: dict, where: str) -> None:
    """Raises Failure where ``value`` breaks ``schema``; a keyword outside ``SCHEMA_KEYWORDS``
    fails too, so no part of the schema goes unchecked."""
    if "$ref" in schema:
        conform(value, defs[schema["$ref"].removeprefix("#/$defs/")], defs, where)
        return
    if unknown := schema.keys() - SCHEMA_KEYWORDS:
        raise Failure(f"{where}: the schema check does not know {sorted(unknown)}")
    kind = schema.get("type")
    if kind is not None and (
        not isinstance(value, JSON_TYPES[kind])
        or (isinstance(value, bool) and kind in ("integer", "number"))
    ):
        raise Failure(f"{where}: {value!r} is not of type {kind}")
    if "enum" in schema and value not in schema["enum"]:
        raise Failure(f"{where}: {value!r} is not one of {schema['enum']}")
    if isinstance(value, (int, float)) and not (
        schema.get("minimum", value) <= value <= schema.get("maximum", value)
    ):
        raise Failure(f"{where}: {value} is out of range")
    if isinstance(value, str) and not (
        schema.get("minLength", 0) <= len(value) <= schema.get("maxLength", len(value))
        and re.search(schema.get("pattern", ""), value)
    ):
        raise Failure(f"{where}: {value!r} breaks its length or pattern")
    if isinstance(value, list):
        if not schema.get("minItems", 0) <= len(value) <= schema.get("maxItems", len(value)):
            raise Failure(f"{where}: {len(value)} items")
        for i, item in enumerate(value):
            conform(item, schema.get("items", {}), defs, f"{where}[{i}]")
    if isinstance(value, dict):
        if missing := set(schema.get("required", [])) - value.keys():
            raise Failure(f"{where}: missing {sorted(missing)}")
        properties = schema.get("properties", {})
        if schema.get("additionalProperties") is False and (extra := value.keys() - properties):
            raise Failure(f"{where}: unexpected {sorted(extra)}")
        for key, item in value.items():
            if key in properties:
                conform(item, properties[key], defs, f"{where}.{key}")


def places_phase(garden: str, root: Path, inbox_path: str) -> dict[str, int]:
    """On a fresh store under ``root`` of a daemon of its own: the places fill, then
    ``PLACES_SEARCHES`` `places_search` calls, each answer checked against C7's
    `PlacesSearchResponse`; the p95 from connect to last byte and the calls with a hit."""
    defs = json.loads(C7_SCHEMA.read_text(encoding="utf-8"))["$defs"]
    path = str(root / "run" / "garden.sock")
    config = write_config(root, path, inbox_path)
    daemon, _ = spawn(garden, config, path)
    try:
        fill_places(path, int(time.time()))
        times = []
        hits = 0
        for n in range(PLACES_SEARCHES):
            answer, elapsed = call(path, "places_search", places_query(n))
            conform(answer, defs["PlacesSearchResponse"], defs, f"places_search {n}")
            hits += bool(answer["places"])
            times.append(elapsed)
        alive(daemon, "during the places search")
    finally:
        stop(daemon)
    if hits * 100 < PLACES_HIT_SHARE * PLACES_SEARCHES:
        raise Failure(f"{hits} of {PLACES_SEARCHES} places_search calls answered a place, "
                      f"expected at least {PLACES_HIT_SHARE} %")
    return {
        "places_count": PLACES_COUNT,
        "places_search_calls": PLACES_SEARCHES,
        "places_search_hits": hits,
        "places_search_p95_us": p95(times),
    }


def median_ready(garden: str, config: Path, path: str) -> int:
    """Median microseconds from spawn to ready over ``READY_RESTARTS`` starts."""
    samples = []
    for _ in range(READY_RESTARTS):
        daemon, ready = spawn(garden, config, path)
        stop(daemon)
        samples.append(ready)
    return p50(samples)


def percentile(samples: list[float], pct: int) -> int:
    """Microseconds of the sorted sample at index ``len(samples) * pct // 100``.
    Of 100 samples, p95 is the 96th smallest."""
    return int(sorted(samples)[len(samples) * pct // 100] * 1e6)


def p50(samples: list[float]) -> int:
    return percentile(samples, 50)


def p95(samples: list[float]) -> int:
    return percentile(samples, 95)


def query(n: int) -> str:
    """A search text that facts of the fill match: an activity word and a place word."""
    return f"{ACTIVITIES[n % len(ACTIVITIES)][1]} {PLACES[n // len(ACTIVITIES) % len(PLACES)][1]}"


def typo(latin: str) -> str:
    """``latin`` with its last letter replaced: one edit from its own transliteration."""
    return latin[:-1] + "x"


def load(path: str) -> dict[str, int]:
    """``REQUESTS`` requests in turn `pointers`, `search` and `propose`: p50 of each from
    connect to last byte, the pointers answered and the searches a record answered. A tenth of
    the searches ask a query of ``MISSES``; the proposes, left unprocessed in the seedbed, name
    a person by Latin spelling or with one typo, and their records match the other queries as
    the facts do."""
    times: dict[str, list[float]] = {"pointers": [], "search": [], "propose": []}
    pointed = 0
    with_records = 0
    for i in range(REQUESTS):
        n = i // 3
        if i % 3 == 0:
            answer, elapsed = call(path, "pointers", {"q": query(n)})
            pointed += bool(answer["pointers"])
            times["pointers"].append(elapsed)
        elif i % 3 == 1:
            miss = n % MISS_EVERY == 0
            q = MISSES[n // MISS_EVERY % len(MISSES)] if miss else query(n)
            answer, elapsed = call(path, "search", {"q": q})
            if bool(answer["facts"]) == miss:
                raise Failure(f"search {q!r} answered {len(answer['facts'])} facts")
            with_records += any(f.get("unprocessed") for f in answer["facts"])
            times["search"].append(elapsed)
        else:
            latin = PEOPLE[n % len(PEOPLE)][1]
            entity = latin if n % 2 == 0 else typo(latin)
            times["propose"].append(propose(path, entity, FILL_FACTS + n))
    if with_records == 0:
        raise Failure("no search was answered with an unprocessed record")
    return {
        "pointers_p50_us": p50(times["pointers"]),
        "pointers_answered": pointed,
        "pointers_requests": len(times["pointers"]),
        "search_p50_us": p50(times["search"]),
        "search_requests": len(times["search"]),
        "search_records_answered": with_records,
        "propose_p50_us": p50(times["propose"]),
    }


def published(path: str, inbox: FakeInbox) -> int:
    """Waits until `health` reports no unpublished miss; the publishes the fake inbox took."""
    deadline = time.perf_counter() + PUBLISH_TIMEOUT
    while True:
        health, _ = call(path, "health", {})
        if health["unpublished_misses"] == 0:
            break
        if time.perf_counter() > deadline:
            raise Failure(f"{health['unpublished_misses']} misses unpublished after "
                          f"{PUBLISH_TIMEOUT} s")
        time.sleep(0.05)
    inbox.check()
    if inbox.published != len(MISSES):
        raise Failure(f"fake inbox took {inbox.published} misses, expected {len(MISSES)}")
    return inbox.published


def measure(garden: str, work: Path) -> dict[str, int | str]:
    root = work / "garden"
    root.mkdir(mode=0o700)
    path = str(root / "run" / "garden.sock")
    inbox = FakeInbox(str(root / "c1.sock"))
    try:
        config = write_config(root, path, str(root / "c1.sock"))
        seed_bytes, processed_bytes = fill(garden, config, path)
        night_root = work / "garden-night"
        night_root.mkdir(mode=0o700)
        shutil.copytree(root / "store", night_root / "store")
        ready = median_ready(garden, config, path)
        daemon, _ = spawn(garden, config, path)
        try:
            latencies = load(path)
            alive(daemon, "during the load")
            misses = published(path, inbox)
            time.sleep(SETTLE_SECONDS)
            before = thread_switches(daemon.pid)
            time.sleep(IDLE_SECONDS)
            after = thread_switches(daemon.pid)
            alive(daemon, "while idle")
            hwm = proc_field(daemon.pid, "status", "VmHWM")
        finally:
            stop(daemon)
        phases = night_phases(garden, night_root, str(root / "c1.sock"))
        places_root = work / "garden-places"
        places_root.mkdir(mode=0o700)
        places = places_phase(garden, places_root, str(root / "c1.sock"))
    finally:
        inbox.close()
    if before.keys() != after.keys():
        raise Failure(f"threads changed while idle: {sorted(before)} -> {sorted(after)}")
    if len(after) != 2:
        raise Failure(f"serve runs {len(after)} threads, expected the server and the worker")
    return {
        "fill_facts": FILL_FACTS,
        "write_bytes_per_seed": seed_bytes,
        "write_bytes_per_processed": processed_bytes,
        "ready_us": ready,
        "ready_restarts": READY_RESTARTS,
        "requests": REQUESTS,
        "vmhwm_kb": hwm,
        **latencies,
        "published_misses": misses,
        "idle_wakeups": sum(after.values()) - sum(before.values()),
        "idle_threads": len(after),
        "idle_seconds": IDLE_SECONDS,
        **phases,
        **places,
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: garden_run.py ULEI_GARDEN WORKDIR", file=sys.stderr)
        return 2
    try:
        results = measure(argv[0], Path(argv[1]))
    except (Failure, OSError, KeyError, ValueError) as e:
        print(f"garden_run: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
