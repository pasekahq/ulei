"""Measurement of the IO classes on the CI runner (ADR-0103), with the io controller delegated to
the user manager.

``python3 ci/io_classes_run.py RELEASE_DIR WORKDIR`` writes a 1 GiB data file in WORKDIR on the
runner's disk, installs the class slices with ``deploy/install.sh`` from a synthetic ``[host.io]``
naming the physical disk under it with 300 read and 300 write IOPS, and puts them into the user manager. Under an
uncapped random 4 KiB O_DIRECT read load of 8 threads at the idle IO class, it runs two probe
readers at once, one 4 KiB O_DIRECT read every 50 ms each: one in ``hive-urgent.slice`` at
``CPUWeight=1000`` and the best-effort IO class at level 0, one in ``background.slice`` at the idle
class. It reports their p50 and p90, the load's IOPS and the disk's scheduler in a notice, and
says so when the urgent probe is not faster. Then it runs the same load in
``hive-heads.slice`` and compares the slice's ``io.stat`` read delta per second with its
``IOReadIOPSMax``. Only a rate above the cap by more than TOLERANCE fails the run.

``load FILE THREADS SECONDS`` and ``probe FILE COUNT INTERVAL`` are the readers the units run:
the load prints its reads and IOPS as JSON at the end or on SIGTERM, the probe one latency in
milliseconds per line.
"""

from __future__ import annotations

import json
import mmap
import os
import random
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

from annotations import annotate

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = Path(__file__).resolve()
BLOCK = 4096
FILE_BYTES = 1 << 30
CAPACITY = 300
LOAD_THREADS = 8
PROBE_COUNT = 200
PROBE_INTERVAL = 0.05
CAPPED_SECONDS = 12
SAMPLE_SECONDS = 10
# A capped slice may read this much above its IOReadIOPSMax before the run fails.
TOLERANCE = 0.15
UNIT_TIMEOUT = 180


class Failure(Exception):
    pass


def reader(path: str) -> tuple[int, mmap.mmap, int]:
    """An O_DIRECT descriptor of ``path``, an aligned buffer and the file's 4 KiB blocks."""
    fd = os.open(path, os.O_RDONLY | os.O_DIRECT)
    return fd, mmap.mmap(-1, BLOCK), os.fstat(fd).st_size // BLOCK


def load(path: str, threads: int, seconds: float) -> None:
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    counts = [0] * threads
    start = time.monotonic()

    def run(i: int) -> None:
        fd, buf, blocks = reader(path)
        while not stop.is_set() and time.monotonic() - start < seconds:
            os.preadv(fd, [buf], random.randrange(blocks) * BLOCK)
            counts[i] += 1

    workers = [threading.Thread(target=run, args=(i,)) for i in range(threads)]
    for w in workers:
        w.start()
    for w in workers:
        w.join()
    elapsed = time.monotonic() - start
    print(json.dumps({"reads": sum(counts), "seconds": round(elapsed, 2),
                      "iops": round(sum(counts) / elapsed, 1)}), flush=True)


def probe(path: str, count: int, interval: float) -> None:
    fd, buf, blocks = reader(path)
    for _ in range(count):
        began = time.perf_counter()
        os.preadv(fd, [buf], random.randrange(blocks) * BLOCK)
        took = time.perf_counter() - began
        print(f"{took * 1000:.3f}", flush=True)
        time.sleep(max(0.0, interval - took))


def disk_of(path: Path) -> tuple[str, str, str]:
    """The physical disk under ``path``'s filesystem, below its partition and any device-mapper
    layers (the first of their ``slaves``): its name, ``MAJ:MIN`` and active scheduler."""
    dev = os.stat(path).st_dev
    block = Path(f"/sys/dev/block/{os.major(dev)}:{os.minor(dev)}").resolve()
    while True:
        if (block / "partition").exists():
            block = block.parent
        slaves = sorted((block / "slaves").iterdir()) if (block / "slaves").is_dir() else []
        if not slaves:
            break
        block = slaves[0].resolve()
    scheduler_file = block / "queue/scheduler"
    if not scheduler_file.exists():
        raise Failure(f"{block.name} under {path} has no queue/scheduler")
    scheduler = scheduler_file.read_text(encoding="ascii").split()
    active = next((s.strip("[]") for s in scheduler if s.startswith("[")), " ".join(scheduler))
    return block.name, (block / "dev").read_text(encoding="ascii").strip(), active


def manager_cgroup() -> Path:
    uid = os.getuid()
    return Path(f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service")


def systemctl(*args: str) -> str:
    done = subprocess.run(["systemctl", "--user", *args], capture_output=True, text=True,
                          timeout=60, check=False)
    if done.returncode != 0:
        raise Failure(f"systemctl --user {' '.join(args)}: {done.stderr.strip()}")
    return done.stdout


def unit(name: str, slice_: str, props: list[str], argv: list[str]) -> subprocess.Popen:
    """``argv`` in a transient unit ``name`` of ``slice_``, its stdout piped back."""
    command = ["systemd-run", "--user", "--wait", "--pipe", "--quiet", "--collect", f"--unit={name}",
               f"--slice={slice_}", "-p", f"RuntimeMaxSec={UNIT_TIMEOUT}",
               *(f"--property={p}" for p in props), "--", sys.executable, str(SCRIPT), *argv]
    return subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def finish(proc: subprocess.Popen, partial: bool = False) -> str:
    """The unit's stdout; with ``partial`` also of a unit RuntimeMaxSec stopped."""
    out, err = proc.communicate(timeout=UNIT_TIMEOUT + 30)
    if proc.returncode != 0 and not partial:
        raise Failure(f"{' '.join(proc.args[:8])}: exit {proc.returncode}: {err.strip()}")
    return out


def data_file(work: Path) -> Path:
    path = work / "data.bin"
    chunk = os.urandom(1 << 20)
    with path.open("wb") as f:
        for _ in range(FILE_BYTES // len(chunk)):
            f.write(chunk)
        f.flush()
        os.fsync(f.fileno())
    return path


def install_slices(release: Path, work: Path, disk: str) -> dict[str, dict[str, str]]:
    """The class slices install.sh writes for ``[host.io]`` of ``/dev/<disk>`` at CAPACITY IOPS,
    put into this user's unit directory and loaded; their ``[Slice]`` keys by name."""
    home = work / "home"
    config = home / ".config/ulei/ulei.toml"
    config.parent.mkdir(parents=True)
    config.write_text(f'[host.io]\ndevice = "/dev/{disk}"\nread_iops = {CAPACITY}\n'
                      f"write_iops = {CAPACITY}\n", encoding="utf-8")
    env = {k: v for k, v in os.environ.items() if k != "XDG_CONFIG_HOME"}
    env["HOME"] = str(home)
    done = subprocess.run(["sh", str(ROOT / "deploy/install.sh"), str(release)], env=env,
                          capture_output=True, text=True, timeout=120, check=False)
    if done.returncode != 0:
        raise Failure(f"install.sh: exit {done.returncode}: {done.stderr.strip()}")
    units = Path(os.environ["HOME"]) / ".config/systemd/user"
    units.mkdir(parents=True, exist_ok=True)
    slices = {}
    for name in ("hive-heads.slice", "hive-urgent.slice", "background.slice"):
        made = home / ".config/systemd/user" / name
        shutil.copyfile(made, units / name)
        lines = made.read_text(encoding="utf-8").split("[Slice]", 1)[1].splitlines()
        slices[name] = dict(line.split("=", 1) for line in lines if "=" in line)
    systemctl("daemon-reload")
    return slices


def percentiles(values: list[float]) -> str:
    if len(values) < 2:
        return f"{len(values)} reads"
    cuts = statistics.quantiles(values, n=10)
    return f"p50 {statistics.median(values):.2f} ms, p90 {cuts[8]:.2f} ms over {len(values)} reads"


def classes(data: Path, scheduler: str) -> str:
    """The urgent and background probes under the uncapped idle-class load."""
    background_load = unit("ulei-io-load", "app.slice", ["IOSchedulingClass=idle"],
                           ["load", str(data), str(LOAD_THREADS), str(UNIT_TIMEOUT - 10)])
    time.sleep(2)
    probe_args = ["probe", str(data), str(PROBE_COUNT), str(PROBE_INTERVAL)]
    urgent = unit("ulei-io-probe-urgent", "hive-urgent.slice",
                  ["CPUWeight=1000", "IOSchedulingClass=best-effort", "IOSchedulingPriority=0"],
                  probe_args)
    background = unit("ulei-io-probe-background", "background.slice",
                      ["IOSchedulingClass=idle", "Nice=10"], probe_args)
    try:
        fast = [float(x) for x in finish(urgent, partial=True).split()]
        slow = [float(x) for x in finish(background, partial=True).split()]
    finally:
        subprocess.run(["systemctl", "--user", "stop", "ulei-io-load.service"], check=False,
                       capture_output=True, timeout=60)
    loaded = json.loads(finish(background_load))
    verdict = ("the urgent probe is faster" if len(fast) > 1 and len(slow) > 1
               and statistics.median(slow) > 1.2 * statistics.median(fast) else
               f"ioprio shows no effect under {scheduler}: the IOPS caps are the mechanism")
    return (f"scheduler {scheduler}; load {LOAD_THREADS} threads at the idle class in app.slice, "
            f"{loaded['iops']} IOPS over {loaded['seconds']} s; urgent probe "
            f"(hive-urgent.slice, best-effort 0, CPUWeight=1000): {percentiles(fast)}; "
            f"background probe (background.slice, idle): {percentiles(slow)}; {verdict}")


def capped(data: Path, majmin: str, slices: dict[str, dict[str, str]]) -> str:
    """The read rate of hive-heads.slice under the same load against its IOReadIOPSMax."""
    cap = int(slices["hive-heads.slice"]["IOReadIOPSMax"].split()[1])
    heads = unit("ulei-io-capped", "hive-heads.slice", ["IOSchedulingClass=idle"],
                 ["load", str(data), str(LOAD_THREADS), str(CAPPED_SECONDS)])
    time.sleep(1)
    group = manager_cgroup() / "hive.slice/hive-heads.slice"

    def reads() -> int:
        for line in (group / "io.stat").read_text(encoding="ascii").splitlines():
            if line.startswith(majmin + " "):
                return int(dict(f.split("=") for f in line.split()[1:])["rios"])
        return 0

    io_max = (group / "io.max").read_text(encoding="ascii").strip()
    first, began = reads(), time.monotonic()
    time.sleep(SAMPLE_SECONDS)
    rate = (reads() - first) / (time.monotonic() - began)
    loaded = json.loads(finish(heads))
    message = (f"hive-heads.slice io.max {io_max!r}; io.stat rios {rate:.1f}/s over "
               f"{SAMPLE_SECONDS} s against IOReadIOPSMax {cap} (tolerance {TOLERANCE:.0%}); "
               f"the load counted {loaded['iops']} IOPS")
    if rate > cap * (1 + TOLERANCE):
        raise Failure(message)
    return message


def run(release: Path, work: Path) -> bool:
    work.mkdir(parents=True, exist_ok=True)
    controllers = (manager_cgroup() / "cgroup.controllers").read_text(encoding="ascii").split()
    if "io" not in controllers:
        annotate("error", "io classes", f"the user manager has no io controller: {controllers}")
        return False
    try:
        disk, majmin, scheduler = disk_of(work)
        data = data_file(work)
        slices = install_slices(release, work, disk)
        annotate("notice", "io classes: urgent against background", classes(data, scheduler))
    except (Failure, OSError, subprocess.SubprocessError, ValueError, KeyError) as e:
        annotate("error", "io classes: urgent against background", str(e))
        return False
    try:
        annotate("notice", "io classes: capped slice", capped(data, majmin, slices))
    except (Failure, OSError, subprocess.SubprocessError, ValueError, KeyError) as e:
        annotate("error", "io classes: capped slice", str(e))
        return False
    return True


def main(argv: list[str]) -> int:
    if argv[:1] == ["load"] and len(argv) == 4:
        load(argv[1], int(argv[2]), float(argv[3]))
        return 0
    if argv[:1] == ["probe"] and len(argv) == 4:
        probe(argv[1], int(argv[2]), float(argv[3]))
        return 0
    if len(argv) != 2:
        print("usage: io_classes_run.py RELEASE_DIR WORKDIR | load FILE THREADS SECONDS | "
              "probe FILE COUNT INTERVAL", file=sys.stderr)
        return 2
    return 0 if run(Path(argv[0]), Path(argv[1])) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
