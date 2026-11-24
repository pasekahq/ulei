"""One head run, executed in its own process (systemd transient unit by default).

`python3 -m hive.runner --db DB --run-id N [--config PATH]` reads <run_dir>/job.json, takes the run
with a compare-and-swap starting->running (a duplicate launcher loses and exits),
heartbeats into the runs row, enforces the timeout on the whole process group and
records exit/tokens/report. After a worker head of a card without a panel exits on its own, the runner stays
the main process of its systemd unit until the processes the head left in the unit's cgroup end.
A report file left in the workspace by an earlier run is moved into
<run_dir> before the head starts, so a `run_report` event with source 'file' means this head wrote it.
It never moves the card: that is the dispatcher's job.
"""
import argparse
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

if __package__ in (None, ''):
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    __package__ = 'hive'

from hive import heads, liveness, store  # noqa: E402

HEARTBEAT_S = 15
REPORT_LIMIT = 60000
REPORT_TAIL_LIMIT = 2500
# Heads run pytest/sqlite with many small fsyncs; on a single virtual disk those stall the gateway.
# A per-run tmpfs TMPDIR keeps that scratch IO in RAM. HIVE_HEAD_TMPFS='' turns it off.
TMPFS_ROOT = os.environ.get('HIVE_HEAD_TMPFS', '/dev/shm')
TMPFS_MIN_FREE_MB = 1024
TMP_VARS = ('TMPDIR', 'TMP', 'TEMP', 'SQLITE_TMPDIR')
TMPFS_STALE_S = 48 * 3600  # leftovers of a SIGKILLed runner; no head lives this long


def head_tmpdir(run_id, root=None, min_free_mb=TMPFS_MIN_FREE_MB):
    """Create a private per-run scratch dir on tmpfs; None (keep the default /tmp) when tmpfs is
    disabled, missing or nearly full. Pages written there are charged to the run's memory cgroup."""
    root = TMPFS_ROOT if root is None else root
    if not root:
        return None
    try:
        for old in Path(root).glob('hive-run-*'):
            if time.time() - old.stat().st_mtime > TMPFS_STALE_S:
                shutil.rmtree(old, ignore_errors=True)
        fs = os.statvfs(root)
        if fs.f_bavail * fs.f_frsize < min_free_mb * 1024 ** 2:
            return None
        return Path(tempfile.mkdtemp(prefix=f'hive-run-{run_id}-', dir=root))
    except OSError:
        return None


def with_tmpdir(env, tmpdir):
    if tmpdir is None:
        return env
    return dict(env, **{k: str(tmpdir) for k in TMP_VARS})


def outlive_leftovers(unit, stopping):
    """Stay the main process of the run's unit while its cgroup holds processes the head left running, until they
    end or a stop arrives: systemd kills the unit's cgroup only when its main process exits or the unit is stopped."""
    me = {'unit': unit, 'pid': os.getpid()}
    while liveness.leftovers(me, liveness.CGROUP_ROOT, liveness.PROC_ROOT) and not stopping.wait(HEARTBEAT_S):
        pass


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument('--db', required=True)
    ap.add_argument('--run-id', type=int, required=True)
    ap.add_argument('--config', help='hive config of a script run (default: HIVE_CONFIG)')
    a = ap.parse_args(argv)
    conn = store.connect(a.db)
    row = conn.execute('SELECT * FROM runs WHERE id=?', (a.run_id,)).fetchone()
    if row is None:
        return 2
    run_dir = Path(row['run_dir'])
    job = json.loads((run_dir / 'job.json').read_text())
    script_cfg = store.load_config(a.config) if job.get('script', False) else None
    proc, stopping = None, threading.Event()
    session = None

    def term(*_):
        stopping.set()
        if proc is not None:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    now = time.time()
    with store.tx(conn):
        cur = conn.execute("UPDATE runs SET status='running', pid=?, heartbeat_at=? WHERE id=? AND status='starting'",
                           (os.getpid(), now, a.run_id))
        if cur.rowcount != 1:
            return 3  # someone else owns this run (duplicate launch) or it was reaped
    # Installed before the head starts: a stop that lands first still reaches the head, and the run row is still written.
    signal.signal(signal.SIGTERM, term)
    is_script = job.get('script', False)
    log = (run_dir / ('stdout.log' if is_script else 'head.log')).open('wb')
    stderr_log = (run_dir / 'stderr.log').open('wb') if is_script else None
    stop = threading.Event()

    def beat():
        c = store.connect(a.db)
        while not stop.wait(HEARTBEAT_S):
            try:
                with store.writer_io_priority():  # the autocommit UPDATE holds the write lock too
                    c.execute("UPDATE runs SET heartbeat_at=? WHERE id=? AND status='running'", (time.time(), a.run_id))
            except Exception:
                pass
    threading.Thread(target=beat, daemon=True).start()

    report_file = Path(job['cwd']) / job['report_file']
    inherited = None
    if not is_script and report_file.exists():
        inherited = run_dir / f"inherited-{job['report_file']}"
        report_file.rename(inherited)
    started = time.time()
    status, code, err = 'exited', None, None
    tmpdir = head_tmpdir(a.run_id)
    keeps_leftovers = job['role'] == 'worker' and not is_script and row['member'] is None and row['unit'] is not None
    try:
        try:
            if is_script:
                from .script import environment
                env = environment(script_cfg, row['card_id'])
            else:
                env = job['env']
            env = with_tmpdir(env, tmpdir)
            proc = subprocess.Popen(job['argv'], cwd=job['cwd'], env=env, stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE if job.get('live_input') else log,
                                    stderr=stderr_log if is_script else subprocess.STDOUT, start_new_session=True)
        except OSError as e:
            status, err = 'failed_launch', f'{type(e).__name__}: {e}'
        else:
            if stopping.is_set():
                term()
            try:
                if job.get('live_input'):
                    from .live import Session
                    session = Session(proc, conn, row, job, log, stopping)
                    code = session.run()
                    proc.stdin.close()
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGTERM)
                        proc.wait(timeout=10)
                else:
                    try:
                        proc.stdin.write(job['prompt'].encode())
                        proc.stdin.close()
                    except BrokenPipeError:
                        pass
                    code = proc.wait(timeout=job['timeout'])
                if stopping.is_set():
                    err = 'stopped by SIGTERM'
            except subprocess.TimeoutExpired:
                status, err = 'timeout', f"timeout after {job['timeout']}s"
                term()
                try:
                    code = proc.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    code = proc.wait()
        finally:
            stop.set()
            if session:
                session.close()
            log.close()
            if stderr_log:
                stderr_log.close()
        ended = time.time()
        text = (run_dir / ('stdout.log' if is_script else 'head.log')).read_text(errors='replace')
        tin, tout, detail, last = (None, None, None, text) if is_script else heads.parse_usage(job['vendor'], text, run_dir)
        provider_failure = (heads.provider_failure(status, code, text, tout, stopping.is_set(),
                                                   bool(detail and detail.get('is_error')))
                            if job['role'] == 'worker' and not is_script else None)
        if is_script:
            source, report = 'stdout', text
            report_file.write_text(text)
        elif report_file.is_file():
            source, report = 'file', report_file.read_text(errors='replace')
        else:
            source, report = ('fallback' if last else 'none'), last
        report = report or ''
        report_sha256 = hashlib.sha256(report.encode('utf-8')).hexdigest()
        verdict = heads.parse_verdict(report) if job['role'] == 'reviewer' else None
        outcome = 'ok' if status == 'exited' and code == 0 else status if status != 'exited' else f'exit {code}'
        with store.tx(conn):
            cur = conn.execute("""UPDATE runs SET status=?, exit_code=?, ended_at=?, duration_s=?, outcome=?, verdict=?,
                                  tokens_in=?, tokens_out=?, tokens_json=?, report=?, report_sha256=?, report_tail=?, error=?, provider_failure=?, heartbeat_at=?
                                  WHERE id=? AND status='running'""",
                               (status, code, ended, round(ended - started, 1), outcome, verdict, tin, tout,
                                json.dumps(detail) if detail is not None else None,
                                report[:REPORT_LIMIT], report_sha256, report[-REPORT_TAIL_LIMIT:], err, provider_failure, ended, a.run_id))
            recorded = cur.rowcount == 1
            if recorded:
                store.event(conn, row['card_id'], 'run_report', {'run': a.run_id, 'file': job['report_file'], 'source': source,
                                                                 'inherited': str(inherited) if inherited else None}, actor='runner')
        if keeps_leftovers and recorded and status == 'exited' and not stopping.is_set():
            outlive_leftovers(row['unit'], stopping)
    finally:
        if tmpdir is not None:  # leftovers may still use it
            shutil.rmtree(tmpdir, ignore_errors=True)
    return 0


if __name__ == '__main__':
    sys.exit(main())
