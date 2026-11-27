"""Live formats the source hive at SOURCE wrote and read: the schema of a fresh store and the config key set."""
import json
import re
import sqlite3
import tomllib
from pathlib import Path

import pytest
from hive import delivery, store

HERE = Path(__file__).resolve().parent
EXAMPLE = HERE.parents[1] / 'config.example.toml'
TOML_TYPES = {str: 'string', int: 'integer', float: 'float', bool: 'boolean', list: 'array'}
# Nullable columns the reform adds to the source cards table (ADR-0068 section 5), appended by the add-column path.
REFORM_CARD_COLUMNS = ', tag TEXT'
# Indexes the reform adds (ADR-0068 section 5: additive, through CREATE INDEX IF NOT EXISTS): the board's (store.BOARD_INDEXES).
REFORM_INDEXES = [['index', 'cards_board', 'cards', 'CREATE INDEX cards_board ON cards(card_class, kind, id)'],
                  ['index', 'links_followup', 'links', "CREATE INDEX links_followup ON links(card_id) WHERE kind='followup'"]]


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
    """The schema the source store creates, its cards table extended by REFORM_CARD_COLUMNS and REFORM_INDEXES only."""
    return sorted([[kind, name, table, sql[:-1] + REFORM_CARD_COLUMNS + ')' if (kind, name) == ('table', 'cards') else sql]
                   for kind, name, table, sql in json.loads((HERE / 'source_schema.json').read_text())] + REFORM_INDEXES,
                  key=lambda r: (r[0], r[1]))


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


def test_example_config_ships_daily_runs_off(tmp_path):
    assert store.load_config(EXAMPLE)['vendors']['claude']['daily_runs'] == 0
    config = tmp_path / 'config.toml'
    config.write_text(''.join(l for l in EXAMPLE.read_text().splitlines(keepends=True) if not l.startswith('daily_runs'))
                      .replace('"texts/', f'"{EXAMPLE.parent}/texts/'))
    assert 'daily_runs' not in store.load_config(config)['vendors']['claude']


@pytest.mark.parametrize('value', ['-1', 'true', '1.5', '"140"'])
def test_daily_runs_must_be_a_non_negative_integer(tmp_path, value):
    config = tmp_path / 'config.toml'
    config.write_text(EXAMPLE.read_text().replace('daily_runs = 0 ', f'daily_runs = {value} ', 1)
                      .replace('"texts/', f'"{EXAMPLE.parent}/texts/'))
    with pytest.raises(ValueError, match='config: vendors.claude.daily_runs must be a non-negative integer'):
        store.load_config(config)


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
    ('"inbox"', "config: delivery must be one of drop, ulei, got 'inbox'$"),
])
def test_delivery_without_an_adapter_is_refused_at_load(tmp_path, value, error):
    config = tmp_path / 'config.toml'
    config.write_text(EXAMPLE.read_text().replace('delivery = "drop"', f'delivery = {value}'))
    with pytest.raises(ValueError, match=error):
        store.load_config(config)


def example_with(tmp_path, adapter, without_keys=(), key=bytes(32), mode=0o600):
    """The example config selecting `adapter`, with its texts, the fake inbox library, an inbox and a producer key file
    holding `key` at `mode` (none for None) under tmp_path, without the top-level `without_keys`."""
    key_file = tmp_path / 'producer.key'
    if key is not None:
        key_file.write_bytes(key)
        key_file.chmod(mode)
    text = (EXAMPLE.read_text().replace('delivery = "drop"', f'delivery = "{adapter}"')
            .replace('"/srv/hive/keys/hive-producer.key"', f'"{key_file}"').replace('"texts/', f'"{EXAMPLE.parent}/texts/')
            .replace('"/srv/hive/inbox-library"', f'"{HERE / "fake_inbox_library"}"')
            .replace('"/srv/hive/data/owner-inbox"', f'"{tmp_path / "inbox"}"'))
    for key_name in without_keys:
        text = without(text, key_name)
    config = tmp_path / 'config.toml'
    config.write_text(text)
    return config


@pytest.mark.parametrize('adapter,unused', [('drop', ('inbox_socket', 'inbox_producer_key_file')),
                                            ('ulei', ('inbox', 'inbox_library'))])
def test_config_needs_only_the_keys_of_the_selected_adapter(tmp_path, adapter, unused):
    cfg = store.load_config(example_with(tmp_path, adapter, unused))
    assert type(delivery.open_adapter(cfg)) is delivery.ADAPTERS[adapter]


@pytest.mark.parametrize('adapter,key', [('drop', 'inbox'), ('drop', 'inbox_library'),
                                         ('ulei', 'inbox_socket'), ('ulei', 'inbox_producer_key_file')])
def test_config_without_a_key_of_the_selected_adapter_is_refused_naming_it(tmp_path, adapter, key):
    with pytest.raises(ValueError, match=f'config: missing key {key}$'):
        store.load_config(example_with(tmp_path, adapter, [key]))


@pytest.mark.parametrize('key,mode,error', [
    (None, 0o600, 'No such file or directory'),
    (bytes(31), 0o600, 'must hold 32 bytes, got 31'),
    (bytes(33), 0o600, 'must hold 32 bytes, got 33'),
    (bytes(32), 0o640, r'open to group or others \(mode 640\)'),
    (bytes(32), 0o604, r'open to group or others \(mode 604\)'),
])
def test_bad_producer_key_file_is_refused_at_load_naming_the_key(tmp_path, key, mode, error):
    with pytest.raises(ValueError, match=f'^config: inbox_producer_key_file {re.escape(str(tmp_path))}/producer.key: {error}$'):
        store.load_config(example_with(tmp_path, 'ulei', key=key, mode=mode))


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
