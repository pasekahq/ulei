"""Whether a run's process lives, read from the cgroup files of the user manager or from /proc."""
import os
from pathlib import Path

CGROUP_ROOT = Path('/sys/fs/cgroup')


def unit_cgroup(cgroup_root, unit):
    """The ControlGroup of a transient `systemd-run --user` service, which the user manager puts under app.slice."""
    uid = os.getuid()
    return cgroup_root / 'user.slice' / f'user-{uid}.slice' / f'user@{uid}.service' / 'app.slice' / f'{unit}.service'


def alive(run, cgroup_root):
    """True while the run's unit cgroup is populated (systemd launcher) or its runner pid is a live process (popen).
    A missing unit cgroup is a gone process; a missing user manager cgroup raises FileNotFoundError."""
    if run['unit']:
        cgroup = unit_cgroup(cgroup_root, run['unit'])
        try:
            events = (cgroup / 'cgroup.events').read_text()
        except FileNotFoundError:
            manager = cgroup.parents[1]
            if not manager.is_dir():
                raise FileNotFoundError(f'no user manager cgroup at {manager}') from None
            return False
        return 'populated 1' in events.splitlines()
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
