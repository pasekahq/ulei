"""The gate refuses an unfinished placeholder in the first line of a result, whatever the acceptance says.

An acceptance regexp that accepts «blocked <anything>» passes a stub such as «blocked ... запущен, результат ещё не
записан» or «provisional» that a head wrote before the work was done.
"""
import pytest

from test_hive import env, mk, run_until, kinds  # noqa: F401
from hive import api, dispatcher, heads

STUBS = [
    'PLACEHOLDER: blocked round 2 in progress, placeholder, overwritten on finish',
    'HYBRID: blocked прогон запущен, результат ещё не записан',
    'RESULT: partial provisional numbers',
    'RESULT: partial waiting for the suite',
    'RESULT: done suite running',
    'RESULT: partial жду ответа',
    'RESULT: partial ждём CI',
    'RESULT: partial will be overwritten at the end',
]
FINAL = [
    'PLACEHOLDER: blocked no access to the PR, owner: выдать токен',
    'PLACEHOLDER: fixed PR #151, merged',
    'HYBRID: done 14 of 14 cases, report in out/hybrid.md',
    'RESULT: done no action needed',
    'RESULT: needs-owner owner: подтвердить доступ.',
]


@pytest.mark.parametrize('line', STUBS)
def test_marker_found(line):
    assert dispatcher.Dispatcher.placeholder_in(line)


@pytest.mark.parametrize('line', FINAL)
def test_final_lines_pass(line):
    assert dispatcher.Dispatcher.placeholder_in(line) is None


def test_placeholder_report_fails_gate_and_next_round_passes(env):
    _, conn, d = env
    cid = mk(conn, '[ok][placeholder-r1] stub then final')
    assert run_until(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert c['round'] == 2 and [g['passed'] for g in c['gates']] == [0, 1]
    assert c['gates'][0]['exit_code'] == dispatcher.PLACEHOLDER_EXIT
    assert 'unfinished placeholder' in c['gates'][0]['output']
    assert 'Gate refused' in c['runs'][1]['prompt'] and 'retry' in kinds(conn, cid)


def test_placeholder_in_every_round_blocks(env):
    _, conn, d = env
    cid = mk(conn, '[ok][placeholder-r1][placeholder-r2] never final')
    assert run_until(d, conn, cid) == 'blocked'
    assert [g['passed'] for g in api.get(conn, cid)['gates']] == [0, 0]


def test_result_file_first_line_checked_even_when_acceptance_passes(env, tmp_path):
    _, conn, d = env
    out = tmp_path / 'result.md'
    out.write_text('PULSE: blocked round 2 in progress, placeholder\nbody\n')
    cid, _ = api.create(conn, title='file stub', goal='g', acceptance=f'head -1 {out} | grep -Eq "^PULSE: (fixed|blocked) "',
                        step=1, source='system:test', created_by='secretary')
    conn.execute("UPDATE cards SET state='check', version=version+1 WHERE id=?", (cid,))
    d.gate(api.card_row(conn, cid))
    g = api.get(conn, cid)['gates'][0]
    assert g['exit_code'] == dispatcher.PLACEHOLDER_EXIT and not g['passed']
    assert 'first line of' in g['output']
    out.write_text('PULSE: fixed PR #9, merged\n')
    cid2, _ = api.create(conn, title='file final', goal='g', acceptance=f'head -1 {out} | grep -Eq "^PULSE: (fixed|blocked) "',
                         step=1, source='system:test', created_by='secretary')
    conn.execute("UPDATE cards SET state='check', version=version+1 WHERE id=?", (cid2,))
    assert d.placeholder_gate(api.card_row(conn, cid2)) is None


def test_head_prompt_forbids_stub_result_line(env):
    cfg, _, _ = env
    context = heads.workspace_context(cfg, {'deny_tools': []})
    assert 'Never write a result line' in context and 'detached' in context
