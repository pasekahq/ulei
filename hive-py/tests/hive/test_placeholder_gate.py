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
    'RESULT: provisional numbers',
    'RESULT: waiting for the suite',
    'RESULT: running in the background',
    'RESULT: **in progress**',
    'RESULT: жду ответа',
    'RESULT: placeholder, overwritten on finish',
    'Жду окончания замера.',
]
FINAL = [
    'PLACEHOLDER: blocked no access to the PR, owner: выдать токен',
    'PLACEHOLDER: fixed PR #151, merged',
    'HYBRID: done 14 of 14 cases, report in out/hybrid.md',
    'RESULT: done no action needed',
    'RESULT: needs-owner owner: подтвердить доступ.',
    # A RESULT line is judged by its status: what follows describes the result, not its state.
    'RESULT: partial waiting for the suite',
    'RESULT: done suite running',
    'RESULT: partial ждём CI',
]
# The first lines the gate refused with exit 97 in 01–26.11, names and numbers replaced by synthetic ones.
# 14 begin with a RESULT status and are finished results; the 2 without a RESULT label are real stubs.
JOURNAL_FINAL = [
    'RESULT: done действий не требуется: gate отклоняет заглушки, PR #1 влит и задеплоен.',
    'RESULT: done [NEWS] — реши, переводить ли чат на qb (скажи «да» или «ждём сброса»), и оплати серверы.',
    'RESULT: done — no action needed; sprint:1 is still open (waiting), so no release/stop was done.',
    'RESULT: needs-owner. ulei is deployed and running on the test bot and all automated checks passed; '
    'the owner needs to send the test messages.',
    'RESULT: done — действий не нужно: sprint:2 запущен, наблюдатель в состоянии running, первая карточка нарезана.',
    'RESULT: done — ничего делать не нужно: sprint:3 запущен, observer работает.',
    'RESULT: done — действий не требуется: restic включён; осталось только сохранить пароль, ещё не сохранён.',
    'RESULT: done — batch is still running normally; no action needed.',
    'RESULT: done — итого еда за день: 1890 ккал; ничего делать не нужно, ждём фото упаковок.',
    'RESULT: need-owner: pay overdue invoices A-1 and B-2; the restic retry is still running, a watcher records it.',
    'RESULT: done — PR #2 (merge 0000000) is merged and running on the test install; nothing to do.',
    'RESULT: done — Example Domain (no action needed; the browser is left running)',
    'RESULT: done — Example Domain (no action needed; the browser is left running)',
    'RESULT: done — решить 2 пункта из hive-waiting-triage.md; было 20 / закрыто 11 / осталось 9.',
]
JOURNAL_STUBS = [
    'Жду окончания замера свежести (~08:51).',
    '2026-01-01 08:54:02 chats-run: active; chunk 1 (100/209) done, chunk 2 batch in progress; spend 1.07 USD of 20',
]


@pytest.mark.parametrize('line', STUBS + JOURNAL_STUBS)
def test_marker_found(line):
    assert dispatcher.Dispatcher.placeholder_in(line)


@pytest.mark.parametrize('line', FINAL + JOURNAL_FINAL)
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
