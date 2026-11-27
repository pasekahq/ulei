"""Cheap host admission samples; no process walks, disk scans or blocking waits."""
import math
import os
from pathlib import Path


def io_avg10(path):
    """avg10 of each line ('some', 'full') of a PSI file."""
    psi = {}
    for line in path.read_text().splitlines():
        kind, *fields = line.split()
        psi[kind] = float(dict(field.split('=') for field in fields)['avg10'])
    if not all(math.isfinite(v) and 0 <= v <= 100 for v in psi.values()):
        raise ValueError(f'invalid PSI in {path}')
    return psi


def sample(disk, cgroups, root=Path('/proc'), cgroup_root=Path('/sys/fs/cgroup')):
    """Host memory, IO PSI, CPU and the free space of `disk`, plus IO PSI some of each cgroup path under cgroup_root.
    A cgroup whose io.pressure is missing or unreadable is listed in missing_cgroups instead."""
    mem = dict(line.split(':', 1) for line in (root / 'meminfo').read_text().splitlines())
    psi = io_avg10(root / 'pressure/io')
    cpu = [int(v) for v in (root / 'stat').read_text().splitlines()[0].split()[1:9]]
    if len(cpu) != 8:
        raise ValueError('invalid host sample')
    protected, missing = {}, []
    for cgroup in cgroups:
        try:
            protected[cgroup] = io_avg10(cgroup_root / cgroup / 'io.pressure')['some']
        except OSError:
            missing.append(cgroup)
    fs = os.statvfs(disk)
    return dict(available_mb=int(mem['MemAvailable'].split()[0]) / 1024,
                some=psi['some'], full=psi['full'], cpu=cpu, cgroups=protected, missing_cgroups=missing,
                free_mb=fs.f_bavail * fs.f_frsize / 1024**2)


def evaluate(current, previous, cfg, now, urgent_floor_mb):
    """Return extra memory slots, the urgent launches the RAM and disk floors allow whether or not the gate is closed,
    and durable cooldown state (survives dispatcher recreation)."""
    state = dict(previous or {})
    reasons = []
    old_cpu = state.get('cpu')
    state['cpu'] = list(current['cpu'])
    iowait = None
    if old_cpu:
        total = sum(current['cpu']) - sum(old_cpu)
        wait = current['cpu'][4] - old_cpu[4]
        if total > 0 and wait >= 0:
            iowait = 100 * wait / total
    reasons += [f'protected cgroup io PSI some: {cgroup}' for cgroup, some in current['cgroups'].items()
                if some >= cfg.get('protected_io_some_pct', 10)]
    if current['some'] >= cfg.get('io_some_pct', 20):
        reasons.append('io PSI some')
    if current['full'] >= cfg.get('io_full_pct', 10):
        reasons.append('io PSI full')
    if iowait is not None and iowait >= cfg.get('iowait_pct', 15):
        reasons.append('CPU iowait')
    slots = max(0, int((current['available_mb'] - cfg.get('reserve_mb', 1536)) /
                       cfg.get('per_run_mb', 768)))
    if not slots:
        reasons.append('RAM reserve')
    if current['free_mb'] < cfg.get('min_free_mb', 5120):
        reasons.append('disk headroom')
    if reasons:
        state['blocked_until'] = now + cfg.get('cooldown_s', 60)
    state['reason'] = ', '.join(reasons) if reasons else (
        'cooldown' if now < state.get('blocked_until', 0) else
        'waiting for second CPU sample' if iowait is None else None)
    state['sample'] = {**current, 'iowait_pct': iowait}
    urgent = 0
    if current['free_mb'] >= cfg.get('min_free_mb', 5120) and current['available_mb'] >= urgent_floor_mb:
        # The k-th urgent start of a tick sees k more starting runs at per_run_mb each.
        urgent = int((current['available_mb'] - urgent_floor_mb) // cfg.get('per_run_mb', 768)) + 1
    return (0 if state['reason'] else slots), urgent, state


def background(argv):
    """Children inherit both scheduling classes, including detached tool processes."""
    return ['/usr/bin/ionice', '-c3', '/usr/bin/nice', '-n', '10', *argv]
