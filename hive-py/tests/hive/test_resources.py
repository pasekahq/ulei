"""Host pressure must close every local launch path, while allowing recovery."""
from functools import partial
from pathlib import Path

import pytest
from test_hive import env, mk, REAL_SCHEDULE_GATE  # noqa: F401
from hive import api, dispatcher, resources, store


def healthy(cpu=None, **kw):
    return dict(available_mb=4096, some=0, full=0, free_mb=8192, cgroups={}, missing_cgroups=[],
                cpu=cpu or [100, 0, 100, 800, 0, 0, 0, 0], **kw)


def test_iowait_and_cooldown_survive_new_dispatcher():
    s = healthy()
    slots, _, state = resources.evaluate(s, None, {}, 100, 768)
    assert slots == 0  # need the second non-blocking CPU sample
    s['cpu'][4] += 50
    s['cpu'][3] += 50
    slots, _, state = resources.evaluate(s, state, {}, 110, 768)
    assert slots == 0 and state['reason'] == 'CPU iowait'
    s['cpu'][3] += 100
    slots, _, state = resources.evaluate(s, state, {}, 120, 768)
    assert slots == 0 and state['reason'] == 'cooldown'
    s['cpu'][3] += 100
    slots, _, state = resources.evaluate(s, state, {}, 171, 768)
    assert slots == 3 and state['reason'] is None


def test_ram_psi_and_disk_close_admission():
    for change in ({'some': 20}, {'full': 10}, {'available_mb': 2303}, {'free_mb': 5119}):
        s = healthy()
        s.update(change)
        assert resources.evaluate(s, {'cpu': [0] * 8}, {}, 100, 768)[0] == 0


def enable(cfg, conn, monkeypatch, **sample):
    cfg['resources'] = dict(enabled=True, max_local_runs=2, disk=str(Path(cfg['db']).parent), psi_cgroups=[])
    monkeypatch.setattr(resources, 'sample', lambda **_: {**healthy(), **sample})
    store.meta_set(conn, 'resource_gate', {'cpu': [0] * 8})


def test_pressure_defers_heads_scripts_and_gates_without_claiming(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    enable(cfg, conn, monkeypatch, full=80)
    cid = mk(conn, '[ok] head')
    script = tmp_path / 'script.sh'
    script.write_text('true')
    api.create(conn, title='script', goal='test', step=1, script=str(script), deliver='local',
               source='system:test', created_by='secretary')
    gate = mk(conn, '[ok] gate')
    conn.execute("UPDATE cards SET state='check' WHERE id=?", (gate,))
    monkeypatch.setattr(dispatcher.Dispatcher, 'schedule_gate', REAL_SCHEDULE_GATE)
    monkeypatch.setattr(d, 'spawn', lambda *a: (_ for _ in ()).throw(AssertionError('spawned')))
    d.tick()
    assert api.get(conn, cid)['state'] == 'queued'
    assert conn.execute('SELECT count(*) FROM runs').fetchone()[0] == 0
    assert conn.execute('SELECT count(*) FROM gate_jobs').fetchone()[0] == 0
    # A fresh instance still honours the durable cooldown after PSI recovers.
    monkeypatch.setattr(resources, 'sample', lambda **_: healthy(cpu=[100, 0, 100, 900, 0, 0, 0, 0]))
    new = dispatcher.Dispatcher(cfg, conn)
    assert not new.resource_room()


def test_scripts_and_heads_share_memory_and_global_slots(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    enable(cfg, conn, monkeypatch)
    cfg['concurrency'] = 5
    script = tmp_path / 'script.sh'
    script.write_text('true')
    api.create(conn, title='script', goal='test', step=1, script=str(script), deliver='local',
               source='system:test', created_by='secretary')
    for i in range(5):
        mk(conn, f'[ok] head {i}')
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    d.tick()
    assert conn.execute('SELECT count(*) FROM runs').fetchone()[0] == 2
    assert conn.execute("SELECT count(*) FROM runs WHERE vendor='script'").fetchone()[0] == 0


def test_starting_rss_is_budgeted_before_next_launch(env, monkeypatch):
    cfg, conn, d = env
    enable(cfg, conn, monkeypatch, available_mb=2400)
    for i in range(3):
        mk(conn, f'[ok] head {i}')
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    d.tick()
    assert conn.execute('SELECT count(*) FROM runs').fetchone()[0] == 1
    d.tick()
    assert conn.execute('SELECT count(*) FROM runs').fetchone()[0] == 1


def test_missing_proc_fails_closed_and_expired_gates_do_not_deadlock(env, monkeypatch):
    cfg, conn, d = env
    enable(cfg, conn, monkeypatch)
    cid = mk(conn, '[ok] stale gate')
    conn.execute('INSERT INTO gate_jobs VALUES(?,?,?,?)', (cid, 1, 'gate', 0))
    def missing(**_):
        raise FileNotFoundError('PSI unavailable')
    monkeypatch.setattr(resources, 'sample', missing)
    assert not d.resource_room()
    assert conn.execute('SELECT count(*) FROM gate_jobs').fetchone()[0] == 0
    assert 'unavailable' in store.meta_get(conn, 'resource_gate')[0]['reason']


def test_detached_gates_consume_shared_pool(env, monkeypatch):
    cfg, conn, d = env
    enable(cfg, conn, monkeypatch)
    ids = [mk(conn, f'[ok] gate {i}') for i in range(3)]
    calls = []
    monkeypatch.setattr(dispatcher.subprocess, 'Popen', lambda argv, **kw: calls.append(argv))
    for i, cid in enumerate(ids):
        assert REAL_SCHEDULE_GATE(d, api.card_row(conn, cid), 'gate') == (1 if i < 2 else 0)
    assert len(calls) == 2
    assert calls[0][:5] == ['/usr/bin/ionice', '-c3', '/usr/bin/nice', '-n', '10']


def systemd_launches(cfg, conn, d, monkeypatch, cards):
    """The systemd-run argv of each head the dispatcher launches for `cards` (kwargs of mk) in one tick."""
    d.launcher = 'systemd'
    cfg['concurrency'] = 5
    calls = []
    real_run = dispatcher.subprocess.run
    def run(argv, **kw):
        if argv[0] == 'systemd-run':
            return calls.append(argv)
        return real_run(argv, **kw)
    monkeypatch.setattr(dispatcher.subprocess, 'run', run)
    for i, kw in enumerate(cards):
        mk(conn, f'[ok] head {i}', **kw)
    d.tick()
    assert len(calls) == len(cards)
    return calls


URGENT_OWNER = dict(card_class='owner_task', step=3, urgent=True)


def test_systemd_children_inherit_io_and_memory_bounds(env, monkeypatch):
    cfg, conn, d = env
    [argv] = systemd_launches(cfg, conn, d, monkeypatch, [{}])
    assert {'--slice=hive-heads.slice', 'IOSchedulingClass=idle', 'Nice=10', 'MemoryHigh=768M',
            'MemoryMax=1280M'} <= set(argv)
    assert not [a for a in argv if a.startswith(('IOWeight', 'CPUWeight'))]


def test_systemd_urgent_head_runs_in_the_urgent_slice_with_cpu_weight_and_high_io_class(env, monkeypatch):
    cfg, conn, d = env
    [argv] = systemd_launches(cfg, conn, d, monkeypatch, [URGENT_OWNER])
    assert {'--slice=hive-urgent.slice', 'CPUWeight=1000', 'IOSchedulingClass=best-effort', 'IOSchedulingPriority=0',
            'MemoryHigh=768M', 'MemoryMax=1280M'} <= set(argv)
    assert not [a for a in argv if a.startswith(('IOWeight', 'Nice=', '--slice=hive-heads'))]
    assert argv.index('CPUWeight=1000') < argv.index('--')


def test_systemd_heads_get_iops_caps_only_when_configured(env, monkeypatch):
    cfg, conn, d = env
    cfg.setdefault('resources', {}).update(head_io_device='/dev/vdz', head_io_read_iops=40, head_io_write_iops=20)
    capped = systemd_launches(cfg, conn, d, monkeypatch, [{}, URGENT_OWNER])
    for argv in capped:
        assert {'IOReadIOPSMax=/dev/vdz 40', 'IOWriteIOPSMax=/dev/vdz 20'} <= set(argv)
        assert argv.index('--') > argv.index('IOWriteIOPSMax=/dev/vdz 20')
    for key in ('head_io_device', 'head_io_read_iops', 'head_io_write_iops'):
        del cfg['resources'][key]
    [plain] = systemd_launches(cfg, conn, d, monkeypatch, [{}])
    assert not [a for a in plain if a.startswith(('IOReadIOPSMax', 'IOWriteIOPSMax'))]


@pytest.mark.parametrize('bad', [dict(head_io_device='vdz'), dict(head_io_read_iops='40'), dict(head_io_write_iops=0),
                                 dict(head_io_read_iops=True), dict(head_io_device=None)])
def test_head_io_caps_config_is_checked(env, bad):
    cfg, _, _ = env
    good = dict(head_io_device='/dev/vdz', head_io_read_iops=40, head_io_write_iops=20)
    store.check_deployment(dict(cfg, resources={**cfg.get('resources', {}), **good}))
    res = {k: v for k, v in {**cfg.get('resources', {}), **good, **bad}.items() if v is not None}
    with pytest.raises(ValueError, match='head_io_'):
        store.check_deployment(dict(cfg, resources=res))


@pytest.mark.parametrize('table, line', [('admission', 'urgent_reserve = 1'), ('admission', 'urgent_max_step = 2'),
                                         ('resources', 'head_io_read_max = "20M"'),
                                         ('resources', 'head_io_write_max = "10M"')])
def test_config_with_an_obsolete_key_loads_with_one_warning_naming_it(env, tmp_path, capsys, table, line):
    cfg, _, _ = env
    old = tmp_path / 'old.toml'
    text = Path(cfg['_path']).read_text()
    old.write_text(text.replace(f'[{table}]\n', f'[{table}]\n{line}\n') if f'[{table}]\n' in text
                   else f'{text}\n[{table}]\n{line}\n')
    capsys.readouterr()
    loaded = store.load_config(old)
    store.load_config(old)
    key = line.split()[0]
    assert key not in loaded[table]
    warnings = capsys.readouterr().err.splitlines()
    assert len(warnings) == 1 and f'{table}.{key}' in warnings[0] and 'obsolete' in warnings[0]


REAL_SAMPLE = resources.sample
GATEWAY, HIVE = 'user.slice/app.slice/gateway.service', 'user.slice/app.slice/hive-dispatcher.service'


def psi_line(some, full=0.0):
    return (f'some avg10={some:.2f} avg60=0.00 avg300=0.00 total=0\n'
            f'full avg10={full:.2f} avg60=0.00 avg300=0.00 total=0\n')


def fake_host(tmp_path, monkeypatch, cfg, conn, host_some, cgroups):
    """Dispatcher reads a fake /proc and /sys/fs/cgroup; a cgroup value of None leaves its io.pressure absent."""
    proc, sys_cgroup = tmp_path / 'proc', tmp_path / 'sys/fs/cgroup'
    (proc / 'pressure').mkdir(parents=True)
    (proc / 'meminfo').write_text('MemTotal: 8388608 kB\nMemAvailable: 4194304 kB\n')
    (proc / 'pressure/io').write_text(psi_line(host_some))
    (proc / 'stat').write_text('cpu  100 0 100 800 0 0 0 0 0 0\n')
    for path, some in cgroups.items():
        (sys_cgroup / path).mkdir(parents=True)
        if some is not None:
            (sys_cgroup / path / 'io.pressure').write_text(psi_line(some))
    cfg['resources'] = dict(enabled=True, max_local_runs=5, min_free_mb=0, reserve_mb=1024, per_run_mb=512,
                            disk=str(tmp_path), psi_cgroups=[GATEWAY, HIVE],
                            io_some_pct=70, io_full_pct=50, iowait_pct=45)
    cfg['concurrency'] = 5
    monkeypatch.setattr(resources, 'sample', partial(REAL_SAMPLE, root=proc, cgroup_root=sys_cgroup))
    store.meta_set(conn, 'resource_gate', {'cpu': [0] * 8})


@pytest.mark.parametrize('host_some, cgroups, admitted, reason, missing', [
    (60, {GATEWAY: 0, HIVE: 0}, True, None, []),
    (60, {GATEWAY: 0, HIVE: 10}, False, f'protected cgroup io PSI some: {HIVE}', []),
    (60, {GATEWAY: 0, HIVE: None}, True, None, [HIVE]),
    (70, {GATEWAY: 0, HIVE: 0}, False, 'io PSI some', []),
])
def test_protected_cgroup_pressure_closes_ordinary_admission(env, tmp_path, monkeypatch,
                                                             host_some, cgroups, admitted, reason, missing):
    cfg, conn, d = env
    fake_host(tmp_path, monkeypatch, cfg, conn, host_some, cgroups)
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    d.clock = lambda: 1000
    cid = mk(conn, '[ok] head')
    d.tick()
    gate = store.meta_get(conn, 'resource_gate')[0]
    assert (api.get(conn, cid)['state'] != 'queued') is admitted
    assert gate['reason'] == reason
    assert gate['sample']['missing_cgroups'] == missing
    assert gate.get('blocked_until') == (None if admitted else 1060)


def test_unreadable_cgroup_pressure_is_recorded_and_admits(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    fake_host(tmp_path, monkeypatch, cfg, conn, 0, {GATEWAY: 0, HIVE: None})
    (tmp_path / 'sys/fs/cgroup' / HIVE / 'io.pressure').mkdir()
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    cid = mk(conn, '[ok] head')
    d.tick()
    assert api.get(conn, cid)['state'] != 'queued'
    assert store.meta_get(conn, 'resource_gate')[0]['sample']['missing_cgroups'] == [HIVE]


def test_urgent_passes_a_gate_closed_by_protected_cgroup_pressure(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    fake_host(tmp_path, monkeypatch, cfg, conn, 0, {GATEWAY: 25, HIVE: 0})
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    background = mk(conn, 'background')
    owner = mk(conn, 'owner step 3', card_class='owner_task', step=3, urgent=True)
    d.tick()
    claimed = [r[0] for r in conn.execute('SELECT card_id FROM runs')]
    assert claimed == [owner]
    assert api.get(conn, background)['state'] == 'queued'
    last = store.meta_get(conn, 'resource_gate')[0]['last_urgent']
    assert last['bypass'] == 'pressure' and last['bypassed'] == f'protected cgroup io PSI some: {GATEWAY}'
