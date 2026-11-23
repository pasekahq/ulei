"""Model-free jobs; the ordinary runner owns claiming, heartbeat and process-group timeout."""
import os
import sys
from pathlib import Path


def command(path):
    path = Path(path)
    if os.access(path, os.X_OK):
        return [str(path)]
    if path.suffix == '.py':
        return [sys.executable, str(path)]
    if path.suffix in ('.sh', '.bash'):
        return ['bash', str(path)]
    raise ValueError('script must be executable or have a .py/.sh/.bash extension')


def environment(cfg, card_id):
    """This process's environment with config `script_env`, HIVE_CARD and the literal assignments of the
    `script_env_file` when it exists."""
    # Credentials are loaded by the child at runtime, never serialized in job.json.
    env = {**os.environ, **cfg['script_env'], 'HIVE_CARD': card_id}
    env_file = Path(cfg['script_env_file'])
    if env_file.is_file():
        env.update(read_env(env_file))
    return env


def read_env(path):
    """Read literal dotenv assignments without evaluating shell commands or interpolation."""
    import shlex
    values = {}
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if line.startswith('export '):
            line = line[7:].lstrip()
        key, sep, value = line.partition('=')
        if not sep or not key.strip().isidentifier():
            continue
        try:
            parts = shlex.split(value, comments=True)
        except ValueError:
            continue
        if len(parts) <= 1:
            values[key.strip()] = parts[0] if parts else ''
    return values
