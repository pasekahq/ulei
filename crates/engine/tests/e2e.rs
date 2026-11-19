//! One `ulei-engine serve` driven over C6 through two interleaved sessions, every party a fake:
//! the file and terminal tools, a skill, an MCP tool, `web_extract`, a compaction, a route
//! switch and an interrupt that drops the queued turn.

#![allow(missing_docs, clippy::unwrap_used)]

mod c6_wire;
mod fake_exa;
mod fake_gateway;
mod fake_mcp;

use std::fs;
use std::path::Path;

use c6_wire::{
    OWNER, config, ended, hold_inbox, interrupt, is_idle, prompt, report, set_route, start, status,
};
use fake_exa::{FakeExa, pages};
use fake_gateway::{
    Answer, FakeGateway, Scratch, call, dead, pid_file, request, route, stop, text, text_block,
    text_step, tool_step, turns, unframed, usage_of,
};
use fake_mcp::{FakeMcp, Reply, Spec};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::{BlockType, Message, Role, StopReason, TurnRequest};
use ulei_contracts::generated::c6::{TurnOutcome, TurnState};
use ulei_engine::compress::text::SUMMARY_PREFIX;
use ulei_engine::journal::Journal;

const HELPER: &str = env!("CARGO_BIN_EXE_ulei-web");
const A: &str = OWNER;
const B: &str = "side";
const PAGE_URL: &str = "https://93.184.215.14/plan";
const PAGE_TEXT: &str = "Synthetic plan page: ship the garden shed on Friday.";
const SKILL: &str = "---\nname: meeting-notes\ndescription: Turn a raw meeting log into action items.\n---\n# Meeting notes\n\n1. List every decision.\n2. Give each action item an owner.\n";
const SUMMARY: &str = "## Goal\nKeep the shed notes current.\n\n## Completed Actions\n1. Wrote, read, patched and searched notes.txt.";
/// Prompt tokens the last step of session A reports: over the threshold of route `main`.
const FULL_PROMPT: u64 = 150_000;

/// A step that answers `reply` and reports a prompt of `input` tokens.
fn text_step_of(reply: &str, input: u64) -> Answer {
    Answer::Events(vec![
        route(),
        text(reply),
        usage_of(input, 5),
        stop(StopReason::End),
    ])
}

fn one_call(id: &str, tool: &str, input: Value) -> Answer {
    tool_step(vec![call(id, tool, input)])
}

/// The content and error flag of the result of call `id` in `sent`.
fn result(sent: &TurnRequest, id: &str) -> (String, bool) {
    let block = sent
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find(|b| b.r#type == BlockType::ToolResult && b.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no result of {id}"));
    (block.content.clone().unwrap(), block.is_error.unwrap())
}

/// The content of the result of call `id` in `sent`, which must not be an error.
fn result_of(sent: &TurnRequest, id: &str) -> String {
    let (content, is_error) = result(sent, id);
    assert!(!is_error, "{id}: {content}");
    content
}

/// The JSON of the result of call `id` in `sent`.
fn json_result(sent: &TurnRequest, id: &str) -> Value {
    serde_json::from_str(&result_of(sent, id)).unwrap()
}

/// The texts of the user prompts of `sent`, tool results left out.
fn prompts(sent: &TurnRequest) -> Vec<String> {
    sent.messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.content[0].text.clone())
        .collect()
}

/// The text of the one user message of the summary request `sent`.
fn summary_prompt(sent: &TurnRequest) -> String {
    assert_eq!(sent.messages.len(), 1);
    ulei_engine::agent::text(&sent.messages[0])
}

/// The fake gateway's turn requests from index `from` on.
fn sent_since(fake: &FakeGateway, from: usize) -> Vec<TurnRequest> {
    turns(fake)[from..].iter().map(|b| request(b)).collect()
}

/// `(request_id, route)` of each request.
fn ids(sent: &[TurnRequest]) -> Vec<(String, String)> {
    sent.iter()
        .map(|r| (r.request_id.clone(), r.route.clone()))
        .collect()
}

/// `(request_id, route)` of steps `0..n` of turn `turn` of session `session`.
fn steps(session: u64, turn: u64, n: usize, route: &str) -> Vec<(String, String)> {
    (0..n)
        .map(|step| (format!("s{session}-t{turn}-{step}"), route.to_owned()))
        .collect()
}

/// The settings of the web helper, the MCP server `notes` and the window of route `main`.
fn extra_settings(dir: &Scratch, exa: &FakeExa, mcp: &FakeMcp) -> String {
    let env_file = dir.path("exa.env");
    fs::write(&env_file, "EXA_API_KEY=synthetic-exa-key-for-tests\n").unwrap();
    format!(
        "web_helper = \"{HELPER}\"\nweb_base_url = \"{}\"\nweb_env_file = \"{}\"\nweb_store = \"{}\"\nmcp_servers = \"notes\"\n\
         [engine.compression]\nthreshold_tokens = 100000\n\
         [engine.routes.main]\ncontext_length = 200000\n\
         [engine.mcp.notes]\nurl = \"{}\"\n",
        exa.base_url(),
        env_file.display(),
        dir.path("web-store").display(),
        mcp.url(),
    )
}

fn write_skill(dir: &Path) {
    let skill = dir.join("brain/skills/notes/meeting-notes");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), SKILL).unwrap();
}

/// `(turn, active, compacted)` of every stored message of `session`.
fn flags(journal: &Journal, session: u64) -> Vec<(u64, bool, bool)> {
    journal
        .records(session)
        .unwrap()
        .into_iter()
        .map(|r| (r.turn, r.active.unwrap(), r.compacted.unwrap()))
        .collect()
}

#[test]
#[allow(clippy::too_many_lines, reason = "one scenario, step by step")]
fn two_sessions_run_every_ported_feature_through_one_serve_on_fakes() {
    let dir = Scratch::new();
    let notes = dir.path("notes.txt");
    let file = notes.display().to_string();
    let pid = dir.path("side.pid");
    let fake = FakeGateway::start(vec![
        // A, turn 1: the file and terminal tools, one per step.
        one_call(
            "w1",
            "write_file",
            json!({"path": file, "content": "shed: paint\nshed: roof\n"}),
        ),
        one_call("r1", "read_file", json!({"path": file})),
        one_call(
            "p1",
            "patch",
            json!({"path": file, "old_string": "shed: roof", "new_string": "shed: roof done"}),
        ),
        one_call(
            "s1",
            "search_files",
            json!({"pattern": "done", "path": dir.0, "target": "content"}),
        ),
        one_call(
            "t1",
            "terminal",
            json!({"command": "grep -c shed notes.txt > count.txt && cat count.txt"}),
        ),
        text_step("A1 done."),
        // B, turn 1: web_extract.
        one_call("x1", "web_extract", json!({"urls": [PAGE_URL]})),
        text_step("B1 done."),
        // A, turn 2: a skill and the MCP tool; the last step reports a full prompt.
        one_call("k1", "skills_list", json!({})),
        one_call("k2", "skill_view", json!({"name": "meeting-notes"})),
        one_call("m1", "mcp__notes__lookup", json!({"topic": "shed"})),
        text_step_of("A2 done.", FULL_PROMPT),
        // A, turn 3: the summary turn of the compaction, then the step.
        text_step(SUMMARY),
        text_step("A3 done."),
        // B, turn 2: a terminal command the interrupt kills.
        one_call(
            "t2",
            "terminal",
            json!({"command": format!("echo $$ > {}; exec sleep 30", pid.display())}),
        ),
        // B, the turn after the interrupt.
        text_step("B4 done."),
        // A, the turn after set_route.
        text_step("A4 done."),
    ]);
    let mcp = FakeMcp::start(
        Spec {
            capabilities: json!({"tools": {}}),
            ..Spec::new(vec![json!({
                "name": "lookup",
                "description": "Looks up a topic.",
                "inputSchema": {"type": "object", "properties": {"topic": {"type": "string"}}}
            })])
        }
        .reply(
            "lookup",
            Reply::Json(json!({"content": [{"type": "text", "text": "shed: two open items"}]})),
        ),
    );
    let exa = FakeExa::start(vec![fake_exa::Answer::Json(
        200,
        pages(&[(PAGE_URL, PAGE_TEXT)]),
    )]);
    let path = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let settings = fs::read_to_string(&path).unwrap() + &extra_settings(&dir, &exa, &mcp);
    fs::write(&path, settings).unwrap();
    write_skill(&dir.0);
    let engine = start(&dir.0, &path);
    let s = &engine.socket;

    // A, turn 1: each tool's effect on disk and its result in the next request.
    let a1 = prompt(s, A, "Keep the shed notes.");
    assert_eq!(ended(s, A, &a1).outcome, Some(TurnOutcome::Completed));
    let sent = sent_since(&fake, 0);
    assert_eq!(ids(&sent), steps(1, 1, 6, "main"));
    let offered: Vec<&str> = sent[0]
        .tools
        .as_ref()
        .unwrap()
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(
        offered,
        [
            "terminal",
            "read_file",
            "write_file",
            "patch",
            "search_files",
            "skills_list",
            "skill_view",
            "web_search",
            "web_extract",
            "mcp__notes__lookup",
        ]
    );
    assert_eq!(
        fs::read_to_string(&notes).unwrap(),
        "shed: paint\nshed: roof done\n"
    );
    assert_eq!(fs::read_to_string(dir.path("count.txt")).unwrap(), "2\n");
    let diff = format!(
        "--- a/{file}\n+++ b/{file}\n@@ -1,2 +1,2 @@\n shed: paint\n-shed: roof\n+shed: roof done\n"
    );
    let expected = [
        (
            "w1",
            json!({"bytes_written": 23, "dirs_created": true, "verified": true, "resolved_path": file, "files_modified": [file]}),
        ),
        (
            "r1",
            json!({"content": "1|shed: paint\n2|shed: roof\n3|", "total_lines": 2, "file_size": 23, "truncated": false, "is_binary": false, "is_image": false}),
        ),
        (
            "p1",
            json!({"success": true, "diff": diff, "files_modified": [file], "resolved_path": file}),
        ),
        (
            "s1",
            json!({"total_count": 1, "matches": [{"path": file, "line": 2, "content": "shed: roof done"}]}),
        ),
        ("t1", json!({"output": "2", "exit_code": 0, "error": null})),
    ];
    for ((id, want), next) in expected.iter().zip(&sent[1..]) {
        assert_eq!(&json_result(next, id), want, "{id}");
    }

    // B, turn 1, between the turns of A.
    let b1 = prompt(s, B, "What does the plan page say?");
    assert_eq!(ended(s, B, &b1).outcome, Some(TurnOutcome::Completed));
    let sent = sent_since(&fake, 6);
    assert_eq!(ids(&sent), steps(2, 1, 2, "main"));
    let requests = exa.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target(), "POST /contents");
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({"urls": [PAGE_URL], "text": true})
    );
    let extracted = unframed("web_extract", &result_of(&sent[1], "x1")).to_owned();
    assert_eq!(
        serde_json::from_str::<Value>(&extracted).unwrap(),
        json!({"results": [{"url": PAGE_URL, "title": format!("T {PAGE_URL}"), "content": PAGE_TEXT, "error": null}]})
    );

    // A, turn 2: the skill tools and the MCP tool.
    let a2 = prompt(s, A, "Which skill fits, and what is open?");
    assert_eq!(ended(s, A, &a2).outcome, Some(TurnOutcome::Completed));
    let sent = sent_since(&fake, 8);
    assert_eq!(ids(&sent), steps(1, 2, 4, "main"));
    assert_eq!(
        json_result(&sent[1], "k1"),
        json!({
            "success": true,
            "skills": [{"name": "meeting-notes", "description": "Turn a raw meeting log into action items.", "category": "notes"}],
            "categories": ["notes"],
            "count": 1,
            "hint": "Use skill_view(name) to see full content, tags, and linked files"
        })
    );
    let viewed = json_result(&sent[2], "k2");
    assert_eq!(
        [
            &viewed["success"],
            &viewed["name"],
            &viewed["content"],
            &viewed["path"]
        ],
        [
            &json!(true),
            &json!("meeting-notes"),
            &json!(SKILL),
            &json!("notes/meeting-notes/SKILL.md")
        ]
    );
    let calls: Vec<Value> = mcp
        .seen()
        .into_iter()
        .filter(|seen| seen.method() == "tools/call")
        .map(|seen| seen.body["params"].clone())
        .collect();
    assert_eq!(
        calls,
        [json!({"name": "lookup", "arguments": {"topic": "shed"}})]
    );
    assert_eq!(
        unframed("mcp__notes__lookup", &result_of(&sent[3], "m1")),
        r#"{"result": "shed: two open items"}"#
    );

    // A, turn 3: the prompt the last step reported crossed the threshold, so the turn compacts
    // first with a summary request on the compression route.
    let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    let before = journal.messages(1).unwrap();
    let stored = before.len();
    let a3 = prompt(s, A, "Summarise the shed work.");
    assert_eq!(ended(s, A, &a3).outcome, Some(TurnOutcome::Completed));
    let sent = sent_since(&fake, 12);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].route, "compact");
    assert!(sent[0].request_id.starts_with("compact-"));
    assert_eq!(ids(&sent[1..]), steps(1, 3, 1, "main"));
    let live = sent[1].messages.clone();
    assert!(live.len() <= stored, "{} of {stored}", live.len());
    assert_eq!(live[..3], before[..3]);
    let summary = ulei_engine::agent::text(&live[3]);
    assert!(summary.contains(SUMMARY_PREFIX), "{summary}");
    assert_eq!(live[3].content[1].id.as_deref(), Some("k1"));
    assert!(
        summary_prompt(&sent[0]).contains("Which skill fits, and what is open?"),
        "the turns between the head and the tail are summarised"
    );
    assert!(summary.contains("## Goal\nKeep the shed notes current."));
    assert_eq!(
        prompts(&sent[1]).last().unwrap(),
        "Summarise the shed work."
    );
    let mut after = live;
    after.push(Message {
        role: Role::Assistant,
        content: vec![text_block("A3 done.")],
    });
    assert_eq!(journal.messages(1).unwrap(), after);
    let rows = flags(&journal, 1);
    assert_eq!(rows.len(), stored + 1 + after.len());
    assert!(rows[..stored].iter().all(|r| !r.1 && r.2), "{rows:?}");
    assert_eq!(
        rows[stored],
        (3, false, true),
        "the prompt before the compaction"
    );
    assert!(
        rows[stored + 1..].iter().all(|r| r.0 == 3 && r.1 && !r.2),
        "{rows:?}"
    );

    // set_route on A; B stays on the default.
    set_route(s, A, "deep");
    assert_eq!(status(s, A, None).route, "deep");
    assert_eq!(status(s, B, None).route, "main");

    // B, turn 2 is interrupted in its terminal command; turn 3, queued, is dropped.
    let b2 = prompt(s, B, "Wait for the long job.");
    let shell = pid_file(&pid);
    let b3 = prompt(s, B, "Never runs.");
    let answer = interrupt(s, B);
    assert!(answer.interrupted);
    assert_eq!(answer.turn.as_deref(), Some(b2.as_str()));
    assert_eq!(answer.dropped.as_deref(), Some(b3.as_str()));
    assert_eq!(answer.queue_depth, 0);
    assert_eq!(ended(s, B, &b2).outcome, Some(TurnOutcome::Interrupted));
    assert!(dead(&shell), "the shell {shell}");
    assert_eq!(report(s, B, &b3).state, TurnState::Dropped);
    let b4 = prompt(s, B, "Is the job done?");
    assert_eq!(ended(s, B, &b4).outcome, Some(TurnOutcome::Completed));
    let sent = sent_since(&fake, 14);
    assert_eq!(
        ids(&sent),
        [steps(2, 2, 1, "main"), steps(2, 3, 1, "main")].concat()
    );
    assert_eq!(
        prompts(&sent[1]),
        [
            "What does the plan page say?",
            "Wait for the long job.",
            "Is the job done?"
        ]
    );
    assert_eq!(
        result(&sent[1], "t2"),
        (
            r#"{"output": "[Command interrupted]", "exit_code": 130, "error": null}"#.to_owned(),
            true
        )
    );

    // A, turn 4, on the stored route.
    let a4 = prompt(s, A, "Anything left?");
    assert_eq!(ended(s, A, &a4).outcome, Some(TurnOutcome::Completed));
    assert_eq!(ids(&sent_since(&fake, 16)), steps(1, 4, 1, "deep"));

    for (session, turn) in [(A, a4), (B, b4)] {
        let now = status(s, session, None);
        assert!(!now.running, "{session}");
        let last = now.last_turn.unwrap();
        assert_eq!(
            (last.turn, last.outcome),
            (turn, Some(TurnOutcome::Completed))
        );
    }
    let _hold = hold_inbox(s, A);
    assert!(is_idle(s).idle);
    assert_eq!(turns(&fake).len(), 17);
}
