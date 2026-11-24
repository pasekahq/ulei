"""Housekeeping safety: exact matches, bounded suggestions, live revalidation."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import time

import pytest
from test_hive import env  # noqa: F401
from hive import api, inbox_library, steward


def card(conn, title='task', goal='goal', **kw):
    return api.create(conn, title=title, goal=goal, acceptance='true', step=1,
                      source='system:test', created_by='secretary', **kw)[0]


def test_exact_duplicates_keep_oldest_and_do_not_merge_series_instances(env):
    cfg, conn, _ = env
    old = card(conn, goal='Do X')
    young = card(conn, title='different title', goal=' do   x ')
    other = card(conn, goal='Different goal', body='different context')
    series = card(conn, kind='recurring', every=60, max_runs=3)
    instance = card(conn, goal='Do X')
    conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'series',?,?)",
                 (instance, series, time.time()))
    result = steward.plan(conn, cfg)
    assert [x['id'] for x in result['cancel']] == [young]
    assert result['cancel'][0]['reason'].endswith(old)
    assert other not in [x['id'] for x in result['cancel']]


def test_recheck_limits_only_and_final_blocked_untouched(env):
    cfg, conn, _ = env
    expired = card(conn, goal='expired', recheck_every=60, recheck_until=time.time()+120)
    exhausted = card(conn, goal='exhausted', recheck_every=60, recheck_max=2)
    future = card(conn, goal='future', recheck_every=60, recheck_max=3)
    final = card(conn, goal='already closed', recheck_every=60, recheck_max=1)
    conn.execute('UPDATE cards SET recheck_until=1 WHERE id=?', (expired,))
    conn.execute('UPDATE cards SET recheck_attempts=2 WHERE id=?', (exhausted,))
    conn.execute("UPDATE cards SET state='blocked',recheck_attempts=1 WHERE id=?", (final,))
    result = steward.plan(conn, cfg)
    assert {x['id'] for x in result['cancel']} == {expired, exhausted}
    assert future not in {x['id'] for x in result['cancel']}


def test_context_and_digest_only_owner_decisions(env):
    cfg, conn, _ = env
    cid = card(conn)
    before = conn.total_changes
    context = steward.board(conn, cfg)
    assert context['cards'][0]['events'][-1]['kind'] == 'created'
    result = steward.model_plan(cfg, context, {'cancel': [], 'decisions': []})
    assert steward.digest(result) == ''
    result = steward.model_plan(cfg, context, {'cancel': [], 'decisions': [
        {'id': cid, 'action': 'оставить', 'reason': 'Нужен ли платный сервис?'}]})
    assert cid in steward.digest(result)
    assert conn.total_changes == before


def test_cancel_revalidates_plan_inside_transaction(env):
    cfg, conn, d = env
    old = card(conn)
    young = card(conn)
    item = steward.plan(conn, cfg)['cancel'][0]
    conn.execute('UPDATE cards SET version=version+1 WHERE id=?', (young,))
    with pytest.raises(api.CardError, match='changed'):
        d.cancel(young, item['reason'], expected_version=item['version'], steward_check=True)
    conn.execute("UPDATE cards SET state='cancelled' WHERE id=?", (old,))
    with pytest.raises(api.CardError, match='no longer valid'):
        d.cancel(young, item['reason'], steward_check=True)


def test_valid_cancel_journals_reason_and_is_idempotent(env):
    cfg, conn, d = env
    card(conn)
    young = card(conn)
    item = steward.plan(conn, cfg)['cancel'][0]
    assert d.cancel(young, item['reason'], expected_version=item['version'], steward_check=True)
    assert not d.cancel(young, item['reason'], expected_version=item['version'], steward_check=True)
    assert conn.execute("SELECT count(*) FROM events WHERE card_id=? AND kind='cancelled'", (young,)).fetchone()[0] == 1
    assert conn.execute('SELECT result FROM cards WHERE id=?', (young,)).fetchone()[0] == item['reason']


def test_cli_dry_run_digest_and_apply_guard(env, monkeypatch, tmp_path):
    cfg, conn, _ = env
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location('hive_steward', root/'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    card(conn)
    card(conn)
    monkeypatch.setattr(steward, 'query_model', lambda *a: {'cancel': [], 'decisions': []})
    assert module.main([]) == 0
    assert module.main(['--digest', '--output', str(tmp_path)]) == 0
    class Night:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 8, 2, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Night)
    output = tmp_path/'steward'
    assert module.main(['--apply', '--output', str(output)]) == 0
    assert not output.exists()


def test_scheduled_apply_uses_cli_and_repeated_run_does_not_recancel(env, monkeypatch, tmp_path):
    _, conn, _ = env
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location('hive_steward_apply', root/'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    card(conn)
    young = card(conn)
    monkeypatch.setattr(steward, 'query_model', lambda *a: {
        'cancel': [{'id': young, 'reason': 'Один и тот же результат', 'keep_id': None}], 'decisions': []})
    class Morning:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 8, 8, 5, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Morning)
    output = tmp_path/'steward'
    assert module.main(['--apply', '--output', str(output)]) == 0
    assert conn.execute('SELECT state FROM cards WHERE id=?', (young,)).fetchone()[0] == 'cancelled'
    journal = (output/'journal.jsonl').read_text()
    assert 'Один и тот же результат' in journal and '"phase": "result"' in journal
    assert module.main(['--apply', '--output', str(output)]) == 0
    assert (output/'journal.jsonl').read_text() == journal


@pytest.mark.parametrize('state', ['running', 'review', 'check'])
def test_model_cannot_cancel_active_cards_even_without_version_change(env, state):
    cfg, conn, d = env
    cid = card(conn)
    context = steward.board(conn, cfg)
    result = steward.model_plan(cfg, context, {'cancel': [{'id': cid, 'reason': 'obsolete'}], 'decisions': []})
    conn.execute('UPDATE cards SET state=? WHERE id=?', (state, cid))
    with pytest.raises(api.CardError, match='protected'):
        d.cancel(cid, 'obsolete', actor='steward', expected_version=result['cancel'][0]['version'])
    with pytest.raises(ValueError, match='protected'):
        steward.model_plan(cfg, steward.board(conn, cfg), {'cancel': [{'id': cid, 'reason': 'obsolete'}], 'decisions': []})


def test_recent_owner_card_protected_at_plan_and_cancel(env):
    cfg, conn, d = env
    cid = card(conn)
    conn.execute("UPDATE cards SET created_by='owner' WHERE id=?", (cid,))
    assert not steward.plan(conn, cfg)['cancel']
    with pytest.raises(ValueError, match='protected'):
        steward.model_plan(cfg, steward.board(conn, cfg), {'cancel': [{'id': cid, 'reason': 'obsolete'}], 'decisions': []})
    with pytest.raises(api.CardError, match='protected'):
        d.cancel(cid, 'obsolete', actor='steward', expected_version=0)
    conn.execute('UPDATE cards SET created_at=? WHERE id=?', (time.time()-steward.DAY-1, cid))
    assert d.cancel(cid, 'obsolete', actor='steward', expected_version=0)
    assert conn.execute("SELECT actor FROM events WHERE card_id=? AND kind='cancelled'", (cid,)).fetchone()[0] == 'steward'
    assert not conn.execute('SELECT * FROM outbox WHERE card_id=?', (cid,)).fetchall()


@pytest.mark.parametrize('answer', [None, {}, {'cancel': 'oops', 'decisions': []},
    {'cancel': [{'id': 'missing', 'reason': 'oops'}], 'decisions': []}])
def test_invalid_response_fails_closed(env, answer):
    cfg, conn, _ = env
    card(conn)
    with pytest.raises(ValueError):
        steward.model_plan(cfg, steward.board(conn, cfg), answer)


def test_semantic_duplicate_with_mocked_model(env, monkeypatch):
    cfg, conn, _ = env
    old = card(conn, title='push: замерить задержку', goal='Замерить задержку первого входящего сообщения')
    new = card(conn, title='push: повтор с перепроверкой', goal='Сравнить время первого входящего сообщения с owner-inbox')
    assert not steward.plan(conn, cfg)['cancel']  # Different wording needs model judgment.
    def fake_query(cfg, context, vendor):
        assert 'events' in context['cards'][0]
        assert old in steward.prompt(cfg, context) and new in steward.prompt(cfg, context)
        return {'cancel': [{'id': new, 'keep_id': old, 'reason': 'Тот же замер первого входящего'}], 'decisions': []}
    monkeypatch.setattr(steward, 'query_model', fake_query)
    context = steward.board(conn, cfg)
    result = steward.model_plan(cfg, context, steward.query_model(cfg, context, 'claude'))
    assert result['cancel'][0]['id'] == new


def test_keeper_cannot_be_cancelled_in_same_response(env):
    cfg, conn, _ = env
    a, b = card(conn), card(conn)
    with pytest.raises(ValueError, match='keeper also'):
        steward.model_plan(cfg, steward.board(conn, cfg), {'cancel': [
            {'id': a, 'keep_id': b, 'reason': 'duplicate'}, {'id': b, 'reason': 'obsolete'}], 'decisions': []})


def test_model_transport_mocked_and_disabled_vendor(env, monkeypatch):
    cfg, conn, _ = env
    monkeypatch.setattr(steward.heads, 'resolve_binary', lambda *a, **kw: ['claude'])
    # Serialize the transport envelope without relying on string escaping.
    def run(cmd, **kw):
        assert cmd[:2] == ['claude', '-p']
        assert cmd[cmd.index('--tools')+1] == ''
        assert 'OPENAI_API_KEY' not in kw['env']
        return subprocess.CompletedProcess(cmd, 0, json.dumps({'result': json.dumps({'cancel': [], 'decisions': []})}), '')
    monkeypatch.setattr(steward.subprocess, 'run', run)
    assert steward.query_model(cfg, steward.board(conn, cfg)) == {'cancel': [], 'decisions': []}
    cfg['vendors']['claude']['disabled'] = True
    with pytest.raises(ValueError, match='disabled'):
        steward.query_model(cfg, steward.board(conn, cfg))


def test_keeper_race_and_failed_blocked_cancellation(env):
    _, conn, d = env
    keeper, duplicate = card(conn), card(conn)
    conn.execute("UPDATE cards SET state='blocked' WHERE id=?", (duplicate,))
    with pytest.raises(api.CardError, match='keeper changed'):
        d.cancel(duplicate, 'duplicate', actor='steward', expected_version=0, keep_card=keeper, keep_version=9)
    assert d.cancel(duplicate, 'duplicate', actor='steward', expected_version=0, keep_card=keeper, keep_version=0)
    assert not conn.execute('SELECT * FROM outbox WHERE card_id=?', (duplicate,)).fetchall()


def test_owner_decision_limit_and_reason_validation(env):
    cfg, conn, _ = env
    ids = [card(conn, goal=f'g{i}') for i in range(6)]
    context = steward.board(conn, cfg)
    with pytest.raises(ValueError, match='more than five'):
        steward.model_plan(cfg, context, {'cancel': [], 'decisions': [
            {'id': cid, 'action': 'оставить', 'reason': 'Нужно решение о деньгах'} for cid in ids]})
    for reason in ('', 'two\nlines', 42):
        with pytest.raises(ValueError, match='reason must'):
            steward.model_plan(cfg, context, {'cancel': [{'id': ids[0], 'reason': reason}], 'decisions': []})


@pytest.mark.parametrize('vendor', ['claude', 'codex'])
def test_failed_apply_reuses_model_plan_and_digest_ignores_legacy(env, monkeypatch, tmp_path, capsys, vendor):
    _, conn, _ = env
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location('steward_retry', root/'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    cid = card(conn)
    class Morning:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 8, 7, 30, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Morning)
    calls = []
    def model(*args):
        calls.append(True)
        return {'cancel': [{'id': cid, 'reason': 'obsolete'}], 'decisions': []}
    monkeypatch.setattr(steward, 'query_model', model)
    monkeypatch.setattr(module.subprocess, 'run', lambda *a, **kw: subprocess.CompletedProcess(a, 2, '', 'changed'))
    output = tmp_path/'steward'
    output.mkdir()
    (output/'2026-11-08.json').write_text('{"propose":[{"id":"old","reason":"age"}]}')
    assert module.main(['--digest', '--output', str(output)]) == 0
    assert '[SILENT]' in capsys.readouterr().out
    assert module.main(['--apply', '--vendor', vendor, '--output', str(output)]) == 1
    assert module.main(['--apply', '--vendor', vendor, '--output', str(output)]) == 1
    assert len(calls) == 1
    assert not (output/'completed-date').exists()
    assert conn.execute('SELECT state FROM cards WHERE id=?', (cid,)).fetchone()[0] == 'created'


ANSWER = {'cancel': [], 'decisions': []}


@pytest.mark.parametrize('text', [
    '```json\n{"cancel": [], "decisions": []}\n```',
    '```\n{"cancel": [], "decisions": []}\n```',
    'Вот результат разбора:\n{"cancel": [], "decisions": []}\nГотово, вопросов нет.',
    'Ответ:\n```json\n{"cancel": [], "decisions": []}\n```\nПояснение {не JSON}.',
    '  {"cancel": [], "decisions": []}  ',
])
def test_parse_answer_strips_fence_and_surrounding_text(text):
    assert steward.parse_answer(text) == ANSWER


@pytest.mark.parametrize('text', ['', '   \n', None, 'Нечего отменять.', '```json\n```', '[1, 2]'])
def test_parse_answer_rejects_empty_or_objectless(text):
    with pytest.raises(steward.ModelResponseError):
        steward.parse_answer(text)


def test_parse_answer_prefers_plan_over_example_object():
    assert steward.parse_answer('Пример {"a": 1}, ответ: {"cancel": [], "decisions": []}') == ANSWER


def test_redact_hides_secrets_and_bounds_length():
    raw = ('token=abcabc123123 sk-ant-0123456789abcdef Authorization: Bearer qwertyuiop123 '
           'token zxcvbnm98765 ' + 'x' * 5000)
    out = steward.redact(raw)
    for secret in ('abcabc123123', 'sk-ant-0123', 'qwertyuiop123', 'zxcvbnm98765'):
        assert secret not in out
    assert len(out) < 2100


def _transport(monkeypatch, answers, vendor='claude'):
    monkeypatch.setattr(steward.heads, 'resolve_binary', lambda *a, **kw: [vendor])
    prompts = []
    def run(cmd, input, **kw):
        assert cmd[0] == vendor
        prompts.append(input)
        answer = answers[len(prompts) - 1]
        if vendor == 'codex':
            Path(cmd[cmd.index('-o') + 1]).write_text(answer)
        return subprocess.CompletedProcess(cmd, 0, json.dumps({'result': answer}), '')
    monkeypatch.setattr(steward.subprocess, 'run', run)
    return prompts


def test_query_model_accepts_fenced_answer_without_retry(env, monkeypatch):
    cfg, conn, _ = env
    prompts = _transport(monkeypatch, ['```json\n{"cancel": [], "decisions": []}\n```'])
    assert steward.query_model(cfg, steward.board(conn, cfg)) == ANSWER
    assert len(prompts) == 1


def test_query_model_retries_once_with_json_only_reminder_and_logs_raw(env, monkeypatch):
    cfg, conn, _ = env
    prompts = _transport(monkeypatch, ['', '{"cancel": [], "decisions": []}'])
    logged = []
    assert steward.query_model(cfg, steward.board(conn, cfg), log=logged.append) == ANSWER
    assert len(prompts) == 2 and steward.JSON_ONLY in prompts[1] and steward.JSON_ONLY not in prompts[0]
    assert len(logged) == 1 and 'raw answer' in logged[0]


def test_query_model_gives_up_after_one_retry(env, monkeypatch):
    cfg, conn, _ = env
    prompts = _transport(monkeypatch, ['Извините, не могу.', 'password: hunter2secret'])
    logged = []
    with pytest.raises(steward.ModelResponseError, match='after a JSON-only retry') as e:
        steward.query_model(cfg, steward.board(conn, cfg), log=logged.append)
    assert len(prompts) == 2 and len(logged) == 2
    assert 'hunter2secret' not in ''.join(logged)
    assert e.value.raw == 'password: hunter2secret'


def test_unparseable_answer_alerts_owner_inbox_instead_of_crashing(env, monkeypatch, tmp_path):
    cfg, conn, _ = env
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location('steward_alert', root/'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    card(conn)
    class Morning:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 10, 7, 30, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Morning)
    def broken(*a):
        raise steward.ModelResponseError('empty model answer after a JSON-only retry', '')
    monkeypatch.setattr(steward, 'query_model', broken)
    output = tmp_path/'steward'
    assert module.main(['--apply', '--output', str(output)]) == 0
    assert not (output/'completed-date').exists() and not (output/'2026-11-10.json').exists()
    assert 'empty model answer' in (output/'last-error.json').read_text()
    kept = inbox_library.open_inbox(cfg).packets
    ids = list(kept)
    assert len(ids) == 1 and ids[0].startswith('hive-steward-error:2026-11-10:')
    payload = kept[ids[0]]['payload']
    assert 'do not forward to the owner' in payload and 'Nothing was cancelled' in payload
    # Repeated failure of the same answer does not duplicate the alert.
    assert module.main(['--apply', '--output', str(output)]) == 0
    assert len(inbox_library.open_inbox(cfg).packets) == 1


@pytest.fixture
def selection_board(env):
    cfg, conn, _ = env
    for i in range(12):
        card(conn, title=f'ordinary {i}', goal=f'result {i}', body='Контекст "\n' * 10000)
    protected = []
    for state in ('running', 'check', 'review'):
        cid = card(conn, title=state, goal=f'protected {state}')
        conn.execute('UPDATE cards SET state=? WHERE id=?', (state, cid))
        protected.append(cid)
    owner = card(conn, title='owner', goal='recent owner request')
    conn.execute("UPDATE cards SET created_by='owner' WHERE id=?", (owner,))
    protected.append(owner)
    series = card(conn, title='series', goal='periodic result', kind='recurring', every=60, max_runs=3)
    periods = [card(conn, title='period', goal='same result') for _ in range(2)]
    for period, cid in enumerate(periods, 1):
        conn.execute('UPDATE cards SET series_period=? WHERE id=?', (period, cid))
        conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'series',?,?)",
                     (cid, series, time.time()))
    conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'blocked_by',?,?)",
                 (periods[0], protected[0], time.time()))
    epic = card(conn, title='epic', goal='combined result', kind='epic')
    conn.execute("INSERT INTO links(card_id,kind,target_id,optional,created_at) VALUES(?,'parent',?,1,?)",
                 (periods[1], epic, time.time()))
    failed = card(conn, title='failed', goal='failed result')
    conn.execute("UPDATE cards SET state='blocked',block_reason=? WHERE id=?", ('failed acceptance', failed))
    conn.execute('UPDATE cards SET version=7 WHERE id=?', (protected[0],))
    context = steward.board(conn, cfg)
    return context, protected, [series, *periods, epic, failed]


@pytest.mark.parametrize('vendor', ['claude', 'codex'])
def test_large_board_bounds_both_requests_and_preserves_safety_context(env, selection_board, monkeypatch, vendor):
    cfg, _, _ = env
    context, protected, related = selection_board
    assert len(steward.prompt(cfg, context)) > 1_080_820
    original = json.dumps(context)
    prompts = _transport(monkeypatch, ['', json.dumps(ANSWER)], vendor)
    answer = steward.query_model(cfg, context, vendor)
    assert len(prompts) == 2 and all(len(p) <= 1_048_576 for p in prompts)
    assert prompts[1] == prompts[0] + steward.JSON_ONLY
    submitted = json.loads(prompts[0].split(cfg['texts']['steward_prompt'], 1)[1])
    shown = {c['id']: c for c in submitted['cards']}
    source = {c['id']: c for c in context['cards']}
    assert set(protected + related) <= shown.keys()
    assert all(shown[cid] == source[cid] for cid in shown)
    assert all(not shown[cid]['can_cancel'] for cid in protected)
    assert shown[related[-1]]['can_cancel']
    assert [shown[cid]['series_period'] for cid in related[1:3]] == [1, 2]
    assert any(link['kind'] == 'blocked_by' for link in shown[related[1]]['links'])
    assert any(link['kind'] == 'parent' and link['optional'] == 1 for link in shown[related[2]]['links'])
    assert any(c['can_cancel'] and c['title'].startswith('ordinary') for c in shown.values())
    coverage = submitted['selection']
    assert coverage['partial'] and coverage['omitted_cards'] > 0
    assert coverage['selected_cards'] + coverage['omitted_cards'] == coverage['total_cards'] == len(source)
    assert coverage['eligible_cards'] == sum(c['can_cancel'] for c in shown.values())
    assert steward.model_plan(cfg, context, answer)['selection'] == coverage
    assert json.dumps(context) == original
    repeated = _transport(monkeypatch, [json.dumps(ANSWER)], vendor)
    steward.query_model(cfg, dict(context, cards=list(reversed(context['cards']))), vendor)
    assert repeated == prompts[:1]


def test_small_board_transport_preserves_cards_hints_and_original_versions(env, monkeypatch):
    cfg, conn, _ = env
    keeper, candidate = card(conn), card(conn)
    conn.execute('UPDATE cards SET version=9 WHERE id=?', (keeper,))
    conn.execute('UPDATE cards SET version=4 WHERE id=?', (candidate,))
    context = steward.board(conn, cfg)
    response = {'cancel': [{'id': candidate, 'keep_id': keeper, 'reason': 'duplicate',
                            'version': 999, 'keep_version': 999}], 'decisions': []}
    prompts = _transport(monkeypatch, [json.dumps(response)])
    answer = steward.query_model(cfg, context)
    submitted = json.loads(prompts[0].split(cfg['texts']['steward_prompt'], 1)[1])
    assert submitted['cards'] == context['cards'] and submitted['exact_hints'] == context['exact_hints']
    result = steward.model_plan(cfg, context, answer)
    assert not result['selection']['partial'] and result['selection']['omitted_cards'] == 0
    assert result['cancel'] == [dict(id=candidate, version=4, reason='duplicate', keep_id=keeper, keep_version=9)]


@pytest.mark.parametrize('vendor', ['claude', 'codex'])
def test_successful_partial_day_records_and_prints_selection_coverage(
        selection_board, monkeypatch, tmp_path, capsys, vendor):
    context, _, _ = selection_board
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location('steward_partial_day', root/'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    class Morning:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 8, 7, 30, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Morning)
    monkeypatch.setattr(module, 'snapshot', lambda db: context)
    prompts = _transport(monkeypatch, [json.dumps(ANSWER)], vendor)
    output = tmp_path/'steward'
    assert module.main(['--apply', '--vendor', vendor, '--output', str(output)]) == 0
    saved = json.loads((output/'2026-11-08.json').read_text())
    printed = json.loads(capsys.readouterr().out)
    assert saved['selection']['partial'] and saved['selection']['omitted_cards'] > 0
    assert printed['selection'] == saved['selection']
    assert saved['model_review'] and saved['vendor'] == vendor
    assert (output/'completed-date').read_text().strip() == '2026-11-08'
    assert module.main(['--apply', '--vendor', vendor, '--output', str(output)]) == 0
    assert len(prompts) == 1


@pytest.mark.parametrize('attack', ['running', 'check', 'review', 'owner', 'omitted'])
@pytest.mark.parametrize('vendor', ['claude', 'codex'])
def test_selected_plan_rejects_protected_or_omitted_before_any_cancellation(
        env, selection_board, monkeypatch, tmp_path, attack, vendor):
    cfg, conn, _ = env
    context, protected, _ = selection_board
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location('steward_selection_refusal', root/'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    selected = steward.select_context(cfg, context)
    shown = {c['id'] for c in selected['cards']}
    valid = next(c['id'] for c in selected['cards'] if c['can_cancel'])
    target = (next(c['id'] for c in context['cards'] if c['id'] not in shown) if attack == 'omitted'
              else protected[['running', 'check', 'review', 'owner'].index(attack)])
    response = {'cancel': [{'id': valid, 'reason': 'obsolete'}, {'id': target, 'reason': 'obsolete'}], 'decisions': []}
    prompts = _transport(monkeypatch, [json.dumps(response)], vendor)
    class Morning:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 8, 7, 30, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Morning)
    monkeypatch.setattr(module, 'snapshot', lambda db: context)
    monkeypatch.setattr(module, 'alert', lambda *args: True)
    output = tmp_path/'steward'
    assert module.main(['--apply', '--vendor', vendor, '--output', str(output)]) == 0
    assert len(prompts) == 1
    assert not (output/'completed-date').exists() and not (output/'2026-11-08.json').exists()
    assert not (output/'journal.jsonl').exists()
    assert ('omitted' if attack == 'omitted' else 'protected') in (output/'last-error.json').read_text()
    assert conn.execute('SELECT state FROM cards WHERE id=?', (valid,)).fetchone()[0] == 'created'


@pytest.mark.parametrize('keeper', ['omitted', 'unknown', 'self', 'cancelled', 'invalid_type'])
def test_selected_plan_rejects_invalid_keepers(env, selection_board, monkeypatch, keeper):
    cfg, _, _ = env
    context, _, _ = selection_board
    selected = steward.select_context(cfg, context)
    shown = {c['id'] for c in selected['cards']}
    valid = next(c['id'] for c in selected['cards'] if c['can_cancel'])
    omitted = next(c['id'] for c in context['cards'] if c['id'] not in shown)
    keep = {'omitted': omitted, 'unknown': 'h_missing', 'self': valid,
            'cancelled': selected['cards'][-1]['id'], 'invalid_type': []}[keeper]
    response = {'cancel': [{'id': valid, 'keep_id': keep, 'reason': 'duplicate'}], 'decisions': []}
    if keeper == 'cancelled':
        response['cancel'].append({'id': keep, 'reason': 'obsolete'})
    _transport(monkeypatch, [json.dumps(response)])
    answer = steward.query_model(cfg, context)
    with pytest.raises(ValueError, match='keeper'):
        steward.model_plan(cfg, context, answer)


@pytest.mark.parametrize('overflow', ['protected_card', 'protected_dependency'])
@pytest.mark.parametrize('vendor', ['claude', 'codex'])
def test_mandatory_context_overflow_calls_no_model_or_cancel_and_keeps_day_open(
        env, monkeypatch, tmp_path, overflow, vendor):
    _, conn, _ = env
    protected = card(conn, goal='protected result')
    conn.execute("UPDATE cards SET state='running' WHERE id=?", (protected,))
    large = protected
    if overflow == 'protected_dependency':
        large = card(conn, goal='required dependency')
        conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'blocked_by',?,?)",
                     (large, protected, time.time()))
    conn.execute('UPDATE cards SET body=? WHERE id=?', ('safety context ' * 90000, large))
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location('steward_input_overflow', root/'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    class Morning:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 8, 7, 30, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Morning)
    prompts = _transport(monkeypatch, [], vendor)
    output = tmp_path/'steward'
    assert module.main(['--apply', '--vendor', vendor, '--output', str(output)]) == 0
    assert not prompts
    assert not (output/'completed-date').exists() and not (output/'2026-11-08.json').exists()
    assert not (output/'journal.jsonl').exists()
    assert 'mandatory safety context' in (output/'last-error.json').read_text()
    assert conn.execute('SELECT state FROM cards WHERE id=?', (protected,)).fetchone()[0] == 'running'


@pytest.fixture
def scheduled_steward(monkeypatch, tmp_path):
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location('scheduled_steward', root/'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    class Morning:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 8, 7, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Morning)
    return module, tmp_path/'steward'


@pytest.mark.parametrize('retry', [False, True])
def test_codex_apply_transport_cas_journal_and_completed_day(
        env, scheduled_steward, monkeypatch, capsys, retry):
    cfg, conn, _ = env
    module, output = scheduled_steward
    keeper, candidate = card(conn), card(conn)
    conn.execute('UPDATE cards SET version=9 WHERE id=?', (keeper,))
    conn.execute('UPDATE cards SET version=4 WHERE id=?', (candidate,))
    cfg['vendors']['codex']['binary'] = ['selected-codex']
    cfg['vendors']['claude']['disabled'] = True
    monkeypatch.setattr(module.store, 'load_config', lambda path: cfg)
    def resolve(binary, **kw):
        assert binary == ['selected-codex']
        return binary
    monkeypatch.setattr(steward.heads, 'resolve_binary', resolve)
    original_run = subprocess.run
    prompts, cancellations = [], []
    response = {'cancel': [{'id': candidate, 'keep_id': keeper, 'reason': 'duplicate',
                            'version': 999, 'keep_version': 999}], 'decisions': []}
    def run(cmd, **kw):
        if cmd[0] == 'selected-codex':
            assert cmd[1:6] == ['--ask-for-approval', 'never', 'exec', '--sandbox', 'read-only']
            assert cmd[cmd.index('-m') + 1] == cfg['ladder'][3]['codex']['model']
            assert kw['timeout'] == 900
            prompts.append(kw['input'])
            Path(cmd[cmd.index('-o') + 1]).write_text('' if len(prompts) == 1 else json.dumps(response))
            return subprocess.CompletedProcess(cmd, 0, '', '')
        assert cmd == [sys.executable, str(module.ROOT/'scripts/hive.py'), '--config',
                       str(Path(cfg['db']).with_name('config.toml')), 'cancel', candidate,
                       '--reason', 'duplicate', '--by', 'steward', '--if-version', '4',
                       '--keep-card', keeper, '--keep-version', '9']
        assert kw == {'capture_output': True, 'text': True, 'timeout': 180}
        cancellations.append(cmd)
        if retry and len(cancellations) == 1:
            return subprocess.CompletedProcess(cmd, 2, '', 'synthetic cancellation failure')
        return original_run(cmd, **kw)
    monkeypatch.setattr(module.subprocess, 'run', run)
    args = ['--apply', '--vendor', 'codex', '--config', str(Path(cfg['db']).with_name('config.toml')),
            '--output', str(output)]
    assert module.main(args) == int(retry)
    saved = json.loads((output/'2026-11-08.json').read_text())
    submitted = json.loads(prompts[0].split(cfg['texts']['steward_prompt'], 1)[1])
    assert saved['model_review'] is True and saved['vendor'] == 'codex'
    assert saved['selection'] == submitted['selection']
    assert len(prompts) == 2 and prompts[1] == prompts[0] + steward.JSON_ONLY
    if retry:
        assert saved['results_failed'][0]['exit_code'] == 2
        assert not (output/'completed-date').exists()
        assert conn.execute('SELECT state FROM cards WHERE id=?', (candidate,)).fetchone()[0] == 'created'
        assert module.main(args) == 0
        completed = json.loads((output/'2026-11-08.json').read_text())
        assert {k: v for k, v in completed.items() if k != 'results_failed'} == {
            k: v for k, v in saved.items() if k != 'results_failed'}
        assert completed['results_failed'] == []
    assert len(prompts) == 2
    assert conn.execute('SELECT state FROM cards WHERE id=?', (candidate,)).fetchone()[0] == 'cancelled'
    assert conn.execute('SELECT state FROM cards WHERE id=?', (keeper,)).fetchone()[0] == 'created'
    assert conn.execute("SELECT actor FROM events WHERE card_id=? AND kind='cancelled'", (candidate,)).fetchone()[0] == 'steward'
    assert not conn.execute('SELECT * FROM outbox WHERE card_id=?', (candidate,)).fetchall()
    journal = [json.loads(line) for line in (output/'journal.jsonl').read_text().splitlines()]
    assert [r['phase'] for r in journal] == ['intent', 'result'] * (2 if retry else 1)
    assert [r['exit_code'] for r in journal if r['phase'] == 'result'] == ([2, 0] if retry else [0])
    assert all({k: r[k] for k in saved['cancel'][0]} == saved['cancel'][0] for r in journal)
    assert (output/'completed-date').read_text() == '2026-11-08\n'
    before = {p.name: p.read_bytes() for p in output.iterdir()}
    capsys.readouterr()
    assert module.main(args) == 0
    assert capsys.readouterr().out == ''
    assert len(prompts) == 2 and len(cancellations) == (2 if retry else 1)
    assert {p.name: p.read_bytes() for p in output.iterdir()} == before


@pytest.mark.parametrize('hour,minute,second', [(6, 59, 59), (7, 0, 0), (11, 59, 59), (12, 0, 0)])
@pytest.mark.parametrize('existing', [False, True])
def test_codex_apply_window_boundaries_and_outside_zero_calls_or_writes(
        env, scheduled_steward, monkeypatch, hour, minute, second, existing):
    cfg, _, _ = env
    module, output = scheduled_steward
    class Clock:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            assert tz.key == cfg['owner_tz']
            return datetime(2026, 11, 8, hour, minute, second, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Clock)
    if existing:
        output.mkdir()
        (output/'2026-11-08.json').write_text('{"propose": []}')
        (output/'completed-date').write_text('2026-11-07\n')
    before = {p.name: p.read_bytes() for p in output.iterdir()} if existing else {}
    calls = []
    def snapshot(db):
        assert 7 <= hour < 12
        calls.append('snapshot')
        return {'at': time.time(), 'cards': [], 'exact_hints': []}
    def model(cfg, context, vendor):
        assert 7 <= hour < 12 and vendor == 'codex'
        calls.append('model')
        return ANSWER
    def forbidden(*a, **kw):
        pytest.fail('unexpected cancellation or output mutation outside window')
    monkeypatch.setattr(module, 'snapshot', snapshot)
    monkeypatch.setattr(steward, 'query_model', model)
    monkeypatch.setattr(module.subprocess, 'run', forbidden)
    if not 7 <= hour < 12:
        monkeypatch.setattr(module, 'save', forbidden)
        monkeypatch.setattr(module, 'record', forbidden)
    assert module.main(['--apply', '--vendor', 'codex', '--output', str(output)]) == 0
    if 7 <= hour < 12:
        assert calls == ['snapshot', 'model']
        assert (output/'completed-date').read_text() == '2026-11-08\n'
    else:
        assert calls == [] and output.exists() == existing
        assert ({p.name: p.read_bytes() for p in output.iterdir()} if existing else {}) == before


@pytest.mark.parametrize('vendor', ['claude', 'codex'])
@pytest.mark.parametrize('provenance', ['missing', 'other'])
def test_reviewed_cache_vendor_refused_without_model_or_mutation(
        env, scheduled_steward, monkeypatch, capsys, vendor, provenance):
    cfg, conn, _ = env
    module, output = scheduled_steward
    cid = card(conn)
    result = dict(steward.model_plan(cfg, steward.board(conn, cfg), {
        'cancel': [{'id': cid, 'reason': 'obsolete'}], 'decisions': []}), model_review=True)
    if provenance == 'other':
        result['vendor'] = 'claude' if vendor == 'codex' else 'codex'
    output.mkdir()
    (output/'2026-11-08.json').write_text(json.dumps(result))
    (output/'journal.jsonl').write_text('{"phase": "result", "exit_code": 2}\n')
    (output/'latest.md').write_text('cached digest\n')
    (output/'completed-date').write_text('2026-11-07\n')
    (output/'steward.lock').touch()
    before = {p.name: p.read_bytes() for p in output.iterdir()}
    def forbidden(*a, **kw):
        pytest.fail('incompatible reviewed plan must stop before snapshot, model, cancellation or save')
    for target, name in ((module, 'snapshot'), (steward, 'query_model'),
                         (module.subprocess, 'run'), (module, 'save'), (module, 'record')):
        monkeypatch.setattr(target, name, forbidden)
    assert module.main(['--apply', '--vendor', vendor, '--output', str(output)]) == 1
    diagnostic = capsys.readouterr()
    assert 'vendor provenance mismatch' in diagnostic.err and f'selected {vendor!r}' in diagnostic.err
    assert f"cached {result.get('vendor')!r}" in diagnostic.err and diagnostic.out == ''
    assert {p.name: p.read_bytes() for p in output.iterdir()} == before
    assert conn.execute('SELECT state FROM cards WHERE id=?', (cid,)).fetchone()[0] == 'created'


def test_completed_claude_day_is_not_relabelled_as_codex(
        env, scheduled_steward, monkeypatch, capsys):
    module, output = scheduled_steward
    output.mkdir()
    (output/'2026-11-08.json').write_text(json.dumps(dict(ANSWER, model_review=True, vendor='claude')))
    (output/'completed-date').write_text('2026-11-08\n')
    (output/'steward.lock').touch()
    before = {p.name: p.read_bytes() for p in output.iterdir()}
    def forbidden(*a, **kw):
        pytest.fail('completed day must not query or cancel')
    monkeypatch.setattr(module, 'snapshot', forbidden)
    monkeypatch.setattr(steward, 'query_model', forbidden)
    monkeypatch.setattr(module.subprocess, 'run', forbidden)
    assert module.main(['--apply', '--vendor', 'codex', '--output', str(output)]) == 0
    assert capsys.readouterr().out == ''
    assert {p.name: p.read_bytes() for p in output.iterdir()} == before


@pytest.mark.parametrize('failure', ['disabled', 'unparseable', 'invalid_decision', 'invalid_keeper'])
def test_codex_apply_invalid_response_never_cancels_or_completes(
        env, scheduled_steward, monkeypatch, failure):
    cfg, conn, _ = env
    module, output = scheduled_steward
    cid, decision = card(conn), card(conn, goal='owner decision')
    response = {'cancel': [{'id': cid, 'reason': 'obsolete'}], 'decisions': []}
    if failure == 'disabled':
        cfg['vendors']['codex']['disabled'] = True
    elif failure == 'invalid_decision':
        response['decisions'] = [{'id': decision, 'action': 'unsupported', 'reason': 'question'}]
    elif failure == 'invalid_keeper':
        response['cancel'][0]['keep_id'] = 'h_missing'
    monkeypatch.setattr(module.store, 'load_config', lambda path: cfg)
    prompts = _transport(monkeypatch, ['unusable', 'still unusable'] if failure == 'unparseable'
                         else [json.dumps(response)], 'codex')
    monkeypatch.setattr(module, 'alert', lambda *a: True)
    assert module.main(['--apply', '--vendor', 'codex', '--output', str(output)]) == 0
    assert len(prompts) == (0 if failure == 'disabled' else 2 if failure == 'unparseable' else 1)
    assert not (output/'2026-11-08.json').exists()
    assert not (output/'completed-date').exists() and not (output/'journal.jsonl').exists()
    assert (output/'last-error.json').exists()
    assert conn.execute('SELECT state FROM cards WHERE id=?', (cid,)).fetchone()[0] == 'created'
