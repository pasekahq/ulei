"""Whether a run's process lives, read from the cgroup files of the user manager or from /proc."""
import os
from pathlib import Path

CGROUP_ROOT = Path('/sys/fs/cgroup')
PROC_ROOT = Path('/proc')
CMDLINE_CHARS = 500


def unit_cgroup(cgroup_root, unit):
    """The ControlGroup of a transient `systemd-run --user` service, which the user manager puts under app.slice."""
    uid = os.getuid()
    return cgroup_root / 'user.slice' / f'user-{uid}.slice' / f'user@{uid}.service' / 'app.slice' / f'{unit}.service'


def read_unit_file(cgroup_root, unit, name):
    """The text of one file of the unit's cgroup, None when the unit cgroup is gone; a missing user manager cgroup
    raises FileNotFoundError."""
    cgroup = unit_cgroup(cgroup_root, unit)
    try:
        return (cgroup / name).read_text()
    except FileNotFoundError:
        manager = cgroup.parents[1]
        if not manager.is_dir():
            raise FileNotFoundError(f'no user manager cgroup at {manager}') from None
        return None


def alive(run, cgroup_root):
    """True while the run's unit cgroup is populated (systemd launcher) or its runner pid is a live process (popen).
    A missing unit cgroup is a gone process."""
    if run['unit']:
        events = read_unit_file(cgroup_root, run['unit'], 'cgroup.events')
        return events is not None and 'populated 1' in events.splitlines()
    if run['pid']:
        return pid_alive(run['pid'])
    return False


def pid_alive(pid):
    try:
        os.kill(pid, 0)
        stat = Path(f'/proc/{pid}/stat').read_text()
    except (FileNotFoundError, ProcessLookupError):
        return False
    except PermissionError:
        return True
    return stat.rsplit(')', 1)[1].split()[0] != 'Z'  # a zombie is dead


def leftovers(run, cgroup_root, proc_root):
    """[{pid, cmdline}] of the processes in the run's unit cgroup other than its runner (`run['pid']`), each
    command line cut to CMDLINE_CHARS; empty for a run without a unit or whose unit cgroup is gone."""
    if not run['unit']:
        return []
    procs = read_unit_file(cgroup_root, run['unit'], 'cgroup.procs')
    if procs is None:
        return []
    found = []
    for pid in sorted({int(line) for line in procs.split()} - {run['pid']}):
        try:
            raw = (proc_root / str(pid) / 'cmdline').read_bytes()
        except (FileNotFoundError, ProcessLookupError):
            continue  # it ended between the two reads
        if not raw:
            continue  # a zombie: it has ended
        cmdline = raw.rstrip(b'\0').replace(b'\0', b' ').decode(errors='replace')
        found.append({'pid': pid, 'cmdline': cmdline[:CMDLINE_CHARS]})
    return found
