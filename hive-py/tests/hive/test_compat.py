"""Live formats the source hive at SOURCE wrote and read: the schema of a fresh store and the config key set."""
import json
import sqlite3
import tomllib
from pathlib import Path

import pytest
from hive import store

HERE = Path(__file__).resolve().parent
EXAMPLE = HERE.parents[1] / 'config.example.toml'
TOML_TYPES = {str: 'string', int: 'integer', float: 'float', bool: 'boolean', list: 'array'}


def flat(table, prefix=''):
    """{dotted key: TOML type} of every value in a parsed TOML table."""
    out = {}
    for key, value in table.items():
        if isinstance(value, dict):
            out.update(flat(value, f'{prefix}{key}.'))
        else:
            out[f'{prefix}{key}'] = TOML_TYPES[type(value)]
    return out


def test_fresh_store_has_the_schema_the_source_store_creates(tmp_path):
    store.connect(tmp_path / 'hive.db').close()
    conn = sqlite3.connect(tmp_path / 'hive.db')
    schema = [list(r) for r in conn.execute('SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name')]
    conn.close()
    assert schema == json.loads((HERE / 'source_schema.json').read_text())


def test_config_with_every_source_key_and_the_new_keys_loads():
    source = json.loads((HERE / 'source_config_keys.json').read_text())
    with EXAMPLE.open('rb') as f:
        keys = flat(tomllib.load(f))
    assert {k: keys.get(k) for k in source} == source
    cfg = store.load_config(EXAMPLE)
    assert set(store.REQUIRED_KEYS) <= set(cfg) and set(cfg['texts']) == set(store.TEXTS)


def without(text, key):
    """Config text without the top-level `key` or the table `[key]`."""
    out, skipping = [], False
    for line in text.splitlines(keepends=True):
        if line.startswith('['):
            skipping = line.strip() == f'[{key}]'
        if not skipping and not line.startswith(f'{key} ='):
            out.append(line)
    return ''.join(out)


@pytest.mark.parametrize('key', store.REQUIRED_KEYS)
def test_config_without_a_deployment_key_is_refused_naming_it(tmp_path, key):
    config = tmp_path / 'config.toml'
    config.write_text(without(EXAMPLE.read_text(), key))
    with pytest.raises(ValueError, match=f'config: missing key {key}$'):
        store.load_config(config)


@pytest.mark.parametrize('table,error', [
    ('max_tokens = 1\ntimeout_s = 1\n', 'missing key steward.route$'),
    ('route = "Not An Alias"\nmax_tokens = 1\ntimeout_s = 1\n', 'steward.route must be a C4G route alias'),
    ('route = "steward"\ntimeout_s = 1\n', 'missing key steward.max_tokens$'),
    ('route = "steward"\nmax_tokens = 1.5\ntimeout_s = 1\n', 'steward.max_tokens must be a positive number'),
    ('route = "steward"\nmax_tokens = 1\ntimeout_s = 0\n', 'steward.timeout_s must be a positive number'),
])
def test_steward_table_is_checked_at_load(tmp_path, table, error):
    config = tmp_path / 'config.toml'
    config.write_text(without(EXAMPLE.read_text(), 'steward') + '\n[steward]\n' + table)
    with pytest.raises(ValueError, match=error):
        store.load_config(config)


def test_config_is_required(monkeypatch):
    monkeypatch.delenv('HIVE_CONFIG', raising=False)
    with pytest.raises(ValueError, match='HIVE_CONFIG is not set'):
        store.load_config()
