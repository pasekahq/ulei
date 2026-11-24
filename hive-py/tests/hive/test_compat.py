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
# Nullable columns the reform adds to the source cards table (ADR-0068 section 5), appended by the add-column path.
REFORM_CARD_COLUMNS = ', tag TEXT'


def flat(table, prefix=''):
    """{dotted key: TOML type} of every value in a parsed TOML table."""
    out = {}
    for key, value in table.items():
        if isinstance(value, dict):
            out.update(flat(value, f'{prefix}{key}.'))
        else:
            out[f'{prefix}{key}'] = TOML_TYPES[type(value)]
    return out


def schema(path):
    conn = sqlite3.connect(path)
    rows = [list(r) for r in conn.execute('SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name')]
    conn.close()
    return rows


def source_schema_with_reform_columns():
    """The schema the source store creates, its cards table extended by REFORM_CARD_COLUMNS only."""
    return [[kind, name, table, sql[:-1] + REFORM_CARD_COLUMNS + ')' if (kind, name) == ('table', 'cards') else sql]
            for kind, name, table, sql in json.loads((HERE / 'source_schema.json').read_text())]


def test_fresh_store_has_the_source_schema_with_the_reform_columns_only(tmp_path):
    store.connect(tmp_path / 'hive.db').close()
    assert schema(tmp_path / 'hive.db') == source_schema_with_reform_columns()


def test_source_store_gains_the_reform_columns_on_connect(tmp_path):
    conn = sqlite3.connect(tmp_path / 'hive.db')
    rows = json.loads((HERE / 'source_schema.json').read_text())
    for kind, name, _, sql in sorted(rows, key=lambda r: r[0] != 'table'):
        if sql is not None and name != 'sqlite_sequence':
            conn.execute(sql)
    conn.close()
    store.connect(tmp_path / 'hive.db', owner_actor='owner').close()
    assert schema(tmp_path / 'hive.db') == source_schema_with_reform_columns()


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


@pytest.mark.parametrize('value,error', [
    ('"ulei"', "config: delivery 'ulei' has no adapter yet; adapters: drop$"),
    ('"inbox"', "config: delivery must be one of drop, got 'inbox'$"),
])
def test_delivery_without_an_adapter_is_refused_at_load(tmp_path, value, error):
    config = tmp_path / 'config.toml'
    config.write_text(EXAMPLE.read_text().replace('delivery = "drop"', f'delivery = {value}'))
    with pytest.raises(ValueError, match=error):
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


@pytest.mark.parametrize('old,new,error', [
    ('tag_launcher = false\n', '', 'missing key vendors.codex.tag_launcher$'),
    ('tag_launcher = true\n', 'tag_launcher = "yes"\n', "vendors.claude.tag_launcher must be true or false, got 'yes'"),
])
def test_vendor_tag_launcher_is_checked_at_load(tmp_path, old, new, error):
    config = tmp_path / 'config.toml'
    config.write_text(EXAMPLE.read_text().replace(old, new))
    with pytest.raises(ValueError, match=error):
        store.load_config(config)


def test_config_is_required(monkeypatch):
    monkeypatch.delenv('HIVE_CONFIG', raising=False)
    with pytest.raises(ValueError, match='HIVE_CONFIG is not set'):
        store.load_config()
