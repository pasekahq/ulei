//! The system prompt: the whole render of a synthetic brain, skills directory and repository
//! against its golden file, the core tier with its overrides and its token ceiling, the skills
//! index switched on and without a directory, the memory blocks over their limits, the cut of a
//! long SOUL.md, the zone of the `Conversation started` line and the reuse of a stored prompt.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::path::{Path, PathBuf};

use fake_gateway::{Scratch, copy_tree};
use ulei_engine::compress::estimate_tokens_rough;
use ulei_engine::config::Config;
use ulei_engine::prompt::core::{Core, Kind};
use ulei_engine::prompt::{self, Brain, CONTEXT_FILE_MAX_CHARS, Error, Host};
use ulei_engine::skills::Skills;
use ulei_engine::zone::{self, Zone};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/prompt");
const SKILLS_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/skills");
/// Thursday, 2026-01-01 12:00:00 UTC.
const WINTER: i64 = 1_767_268_800;
/// Wednesday, 2026-07-01 12:00:00 UTC.
const SUMMER: i64 = 1_782_907_200;

/// A `TZif` file of zone `name` under `dir` without transitions: one local time type of
/// `offset` and `abbrev`, then the POSIX rule `footer`.
fn zone_file(dir: &Path, name: &str, offset: i32, abbrev: &str, footer: &str) {
    let header = |out: &mut Vec<u8>| {
        out.extend_from_slice(b"TZif2");
        out.extend_from_slice(&[0; 15]);
        let chars = u32::try_from(abbrev.len() + 1).unwrap();
        for count in [0u32, 0, 0, 0, 1, chars] {
            out.extend_from_slice(&count.to_be_bytes());
        }
    };
    let data = |out: &mut Vec<u8>| {
        out.extend_from_slice(&offset.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(abbrev.as_bytes());
        out.push(0);
    };
    let mut bytes = Vec::new();
    header(&mut bytes);
    data(&mut bytes);
    header(&mut bytes);
    data(&mut bytes);
    bytes.extend_from_slice(format!("\n{footer}\n").as_bytes());
    let path = dir.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// UTC+1, UTC+2 from the last Sunday of March to the last of October.
fn central_european_zone(dir: &Path, name: &str) {
    zone_file(dir, name, 3600, "CET", "CET-1CEST,M3.5.0,M10.5.0/3");
}

/// The fixture brain, skills and repository copied into a scratch root with a git repository
/// at `repo`, the skill `pruning` disabled, the skills index off, the shipped core without a task
/// board, and the zone `Fixture/Capital`.
struct Setup {
    root: Scratch,
    brain: Brain,
    host: Host,
    zone: Zone,
}

fn setup() -> Setup {
    let root = Scratch::new();
    copy_tree(
        Path::new(FIXTURE).join("brain").as_path(),
        &root.path("brain"),
    );
    copy_tree(
        Path::new(FIXTURE).join("repo").as_path(),
        &root.path("repo"),
    );
    fs::create_dir(root.path("repo").join(".git")).unwrap();
    copy_tree(Path::new(SKILLS_FIXTURE), &root.path("skills"));
    let zoneinfo = root.path("zoneinfo");
    central_european_zone(&zoneinfo, "Fixture/Capital");
    let brain = Brain {
        soul: root.path("brain").join("SOUL.md"),
        index: root.path("brain").join("INDEX.md"),
        memory: root.path("brain").join("memories").join("MEMORY.md"),
        user: root.path("brain").join("memories").join("USER.md"),
        memory_char_limit: prompt::DEFAULT_MEMORY_CHAR_LIMIT,
        user_char_limit: prompt::DEFAULT_USER_CHAR_LIMIT,
        skills: Skills {
            dir: root.path("skills"),
            disabled: vec!["pruning".to_owned()],
        },
        skills_index: false,
        core: Core::defaults(None),
    };
    let host = Host {
        system: "Linux (6.1.0-fixture)".to_owned(),
        home: "/var/lib/fixture".to_owned(),
        cwd: root.path("repo").join("work"),
    };
    let zone = Zone::named("Fixture/Capital", &[zoneinfo.as_path()]).unwrap();
    Setup {
        root,
        brain,
        host,
        zone,
    }
}

#[test]
fn the_prompt_of_the_fixture_brain_matches_its_golden_file() {
    let s = setup();
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    let golden = fs::read_to_string(Path::new(FIXTURE).join("golden.txt")).unwrap();
    let want = golden.replace("<ROOT>", &s.root.0.display().to_string());
    assert_eq!(built, want.strip_suffix('\n').unwrap());
    assert_eq!(
        prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap(),
        built
    );
}

#[test]
fn a_store_over_its_limit_renders_in_full_at_100_percent() {
    let mut s = setup();
    s.brain.memory_char_limit = 40;
    s.brain.user_char_limit = 0;
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    let memory = "The household calendar lives in calendar/2026.md.\n§\nGarden notes are kept per \
                  season.\n§\nBackups run on the first day of each month.";
    assert!(built.contains(&format!(
        "MEMORY (your personal notes) [100% — 131/40 chars]\n{}\n{memory}\n\n",
        "═".repeat(46)
    )));
    assert!(built.contains(
        "USER PROFILE (who the user is) [0% — 69/0 chars]\n══════════════════════════════════════════════\nThe user is called Robin.\n§\nRobin prefers answers without small talk.\n\nConversation started:"
    ));
}

#[test]
fn absent_or_blank_memory_files_leave_their_blocks_out() {
    let s = setup();
    fs::remove_file(&s.brain.memory).unwrap();
    fs::write(&s.brain.user, "\n§\n  \n").unwrap();
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    assert!(!built.contains("MEMORY (your personal notes)"));
    assert!(!built.contains("USER PROFILE"));
    assert!(built.contains("Keep drafts in drafts/.\n\nConversation started:"));
}

#[test]
fn a_missing_blank_or_unreadable_brain_file_is_an_error_naming_it() {
    let s = setup();
    fs::write(&s.brain.memory, [0xff, 0xfe, b'x']).unwrap();
    let err = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap_err();
    assert!(
        matches!(&err, Error::Read(path, _) if *path == s.brain.memory),
        "{err}"
    );

    fs::write(&s.brain.soul, " \n\t\n").unwrap();
    let err = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("SOUL.md {} is missing or blank", s.brain.soul.display())
    );
    fs::remove_file(&s.brain.soul).unwrap();
    assert!(matches!(
        prompt::build(&s.brain, &s.host, &s.zone, WINTER),
        Err(Error::NoSoul(_))
    ));
}

#[test]
fn a_long_soul_keeps_its_head_and_tail_around_a_marker_naming_the_file() {
    let s = setup();
    let soul: String = (0..CONTEXT_FILE_MAX_CHARS + 5_000)
        .map(|i| char::from(b'a' + u8::try_from(i % 26).unwrap()))
        .collect();
    fs::write(&s.brain.soul, &soul).unwrap();
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    let head: String = soul.chars().take(14_000).collect();
    let tail: String = soul.chars().skip(soul.len() - 4_000).collect();
    let want = format!(
        "{head}\n\n[...truncated SOUL.md: kept 14000+4000 of 25000 chars. The middle is omitted — \
         if you need the full instructions, read the complete file with the read_file tool: \
         {}]\n\n{tail}\n\n# Safeguards\n",
        s.brain.soul.display()
    );
    assert!(built.starts_with(&want));
}

#[test]
fn the_start_line_follows_the_zone_rule_and_an_unnamed_zone_shows_no_name() {
    let s = setup();
    let summer = prompt::build(&s.brain, &s.host, &s.zone, SUMMER).unwrap();
    assert!(summer.ends_with(
        "Conversation started: Wednesday, July 01, 2026 (Fixture/Capital, CEST, UTC+02:00)"
    ));
    let late = prompt::build(&s.brain, &s.host, &s.zone, WINTER + 11 * 3600 + 30 * 60).unwrap();
    assert!(late.ends_with(
        "Conversation started: Friday, January 02, 2026 (Fixture/Capital, CET, UTC+01:00)"
    ));
    let utc = prompt::build(&s.brain, &s.host, &Zone::utc(), WINTER).unwrap();
    assert!(utc.ends_with("Conversation started: Thursday, January 01, 2026 (UTC, UTC+00:00)"));
}

#[test]
fn a_zone_name_must_be_relative_and_present() {
    let dir = Scratch::new();
    central_european_zone(&dir.0, "Fixture/Capital");
    for bad in [
        "",
        "/Fixture/Capital",
        "Fixture/../Capital",
        "Fixture//Capital",
    ] {
        assert!(
            matches!(
                Zone::named(bad, &[dir.0.as_path()]),
                Err(zone::Error::BadName(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        Zone::named("Fixture/Nowhere", &[dir.0.as_path()]),
        Err(zone::Error::NotFound(_))
    ));
    let empty = Scratch::new();
    let found = Zone::named("Fixture/Capital", &[empty.0.as_path(), dir.0.as_path()]).unwrap();
    assert_eq!(found.name(), Some("Fixture/Capital"));
}

#[test]
fn a_zone_switches_at_its_rule_boundaries() {
    let dir = Scratch::new();
    zone_file(
        &dir.0,
        "Fixture/East",
        7200,
        "EET",
        "EET-2EEST,M3.5.0/3,M10.5.0/4",
    );
    zone_file(&dir.0, "Fixture/South", -10_800, "-03", "<-03>3");
    let east = Zone::named("Fixture/East", &[dir.0.as_path()]).unwrap();
    // 2026-03-29 01:00 and 2026-10-25 01:00 UTC.
    let (start, end) = (1_774_746_000, 1_792_890_000);
    let at = |t: i64| {
        let local = east.at(t);
        (local.offset, local.abbrev)
    };
    assert_eq!(at(start - 1), (7200, "EET".to_owned()));
    assert_eq!(at(start), (10_800, "EEST".to_owned()));
    assert_eq!(at(end - 1), (10_800, "EEST".to_owned()));
    assert_eq!(at(end), (7200, "EET".to_owned()));
    let south = Zone::named("Fixture/South", &[dir.0.as_path()]).unwrap();
    assert_eq!(south.at(SUMMER).offset, -10_800);
    assert_eq!(south.at(SUMMER).abbrev, "-03");
    fs::write(dir.0.join("Fixture/Broken"), "not a zone file").unwrap();
    assert!(matches!(
        Zone::named("Fixture/Broken", &[dir.0.as_path()]),
        Err(zone::Error::Malformed(_))
    ));
}

#[test]
fn a_stored_prompt_is_reused_while_it_has_history_and_names_this_working_directory() {
    let s = setup();
    let stored = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    let cwd = s.host.cwd.as_path();
    assert_eq!(
        prompt::restore(Some(&stored), true, cwd),
        Some(stored.as_str())
    );
    assert_eq!(prompt::restore(Some(&stored), false, cwd), None);
    assert_eq!(prompt::restore(Some(""), true, cwd), None);
    assert_eq!(prompt::restore(None, true, cwd), None);
    assert_eq!(
        prompt::restore(Some(&stored), true, &PathBuf::from("/srv/elsewhere")),
        None
    );
    let without_host = "Identity only.";
    assert_eq!(
        prompt::restore(Some(without_host), true, Path::new("/srv/elsewhere")),
        Some(without_host)
    );
}

#[test]
fn a_missing_skills_directory_or_one_without_skills_leaves_the_index_out() {
    let mut s = setup();
    s.brain.skills_index = true;
    s.brain.skills.dir = s.root.path("no-skills");
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    assert!(!built.contains("## Skills (mandatory)"));
    assert!(built.contains("Keep drafts in drafts/.\n\n══"));
    fs::create_dir_all(s.root.path("no-skills").join("empty")).unwrap();
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    assert!(!built.contains("<available_skills>"));
}

#[test]
fn the_working_directory_is_read_past_a_home_line_without_one_after_it() {
    let s = setup();
    let soul = fs::read_to_string(&s.brain.soul).unwrap();
    fs::write(
        &s.brain.soul,
        format!("{soul}\nUser home directory: noted in a letter\n\nSigned, the household.\n"),
    )
    .unwrap();
    let stored = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    assert_eq!(
        prompt::restore(Some(&stored), true, &s.host.cwd),
        Some(stored.as_str())
    );
    assert_eq!(
        prompt::restore(Some(&stored), true, Path::new("/srv/elsewhere")),
        None
    );
}

/// Tokens the shipped core may take by the engine's rough estimate.
const CORE_TOKEN_CEILING: usize = 384;

/// The core under the config file `text` in a scratch directory, with `env` for its variables.
fn core_of(
    dir: &Scratch,
    text: &str,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Core, String> {
    let path = dir.path("ulei.toml");
    fs::write(&path, text).unwrap();
    let config = Config::read(Some(&path), env).map_err(|e| e.to_string())?;
    Core::from_settings(&config, env)
        .map_err(|e| e.to_string().replace(&dir.0.display().to_string(), "<DIR>"))
}

#[test]
fn the_skills_index_switched_on_sits_between_the_context_and_the_memory_block() {
    let mut s = setup();
    s.brain.skills_index = true;
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    let golden = fs::read_to_string(Path::new(FIXTURE).join("golden.txt")).unwrap();
    let index = fs::read_to_string(Path::new(FIXTURE).join("skills-index.txt")).unwrap();
    let want = golden
        .replace("<ROOT>", &s.root.0.display().to_string())
        .replace(
            "Keep drafts in drafts/.\n\n══",
            &format!("Keep drafts in drafts/.\n\n{index}\n══"),
        );
    assert_eq!(built, want.strip_suffix('\n').unwrap());
}

#[test]
fn the_shipped_core_holds_three_safeguards_and_three_meta_skills_by_id() {
    let dir = Scratch::new();
    let core = core_of(&dir, "", &|_| None).unwrap();
    assert_eq!(core, Core::defaults(None));
    let ids = |kind: Kind| -> Vec<&str> {
        core.items
            .iter()
            .filter(|i| i.kind == kind)
            .map(|i| i.id)
            .collect()
    };
    assert_eq!(
        ids(Kind::Safeguard),
        [
            "no_unasked_messages",
            "no_unfiled_promises",
            "no_irreversible_deletion"
        ]
    );
    assert_eq!(
        ids(Kind::MetaSkill),
        ["search", "terminal_and_files", "task_delegation"]
    );
}

#[test]
fn a_core_item_is_replaced_from_its_file_or_disabled_by_its_id_and_the_task_board_is_named() {
    let dir = Scratch::new();
    let promise = dir.path("promise.md");
    fs::write(&promise, "\n  Promise only what the board holds.\n\n").unwrap();
    let text = format!(
        "[engine]\ntask_board_file = \"/srv/fixture/board.md\"\n\n\
         [engine.core.no_unfiled_promises]\nfile = \"{}\"\n\n\
         [engine.core.search]\nenabled = false\n",
        promise.display()
    );
    let core = core_of(&dir, &text, &|_| None).unwrap();
    let rendered = core.render();
    let safeguards = Core::defaults(None).items;
    assert_eq!(
        rendered,
        format!(
            "# Safeguards\n{}\nPromise only what the board holds.\n{}\n\n# Meta-skills\n{}\n\
             Task delegation: to file a task or hand one out, read /srv/fixture/board.md with \
             read_file. It gives the address of the task board and the commands to use.",
            safeguards[0].text, safeguards[2].text, safeguards[4].text
        )
    );

    let mut s = setup();
    s.brain.core = core;
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    let soul = fs::read_to_string(&s.brain.soul).unwrap();
    assert!(built.starts_with(&format!(
        "{}\n\n{rendered}\n\nKnowledge base index: ",
        soul.trim()
    )));

    let off =
        |name: &str| (name == "ULEI_ENGINE_CORE_TERMINAL_AND_FILES_ENABLED").then(|| "0".into());
    let core = core_of(&dir, &text, &off).unwrap();
    let ids: Vec<&str> = core.items.iter().map(|i| i.id).collect();
    assert_eq!(
        ids,
        [
            "no_unasked_messages",
            "no_unfiled_promises",
            "no_irreversible_deletion",
            "task_delegation"
        ]
    );
}

#[test]
fn a_kind_with_every_item_disabled_leaves_its_heading_out() {
    let dir = Scratch::new();
    let text = "[engine.core.search]\nenabled = false\n[engine.core.terminal_and_files]\n\
                enabled = false\n[engine.core.task_delegation]\nenabled = \"no\"\n";
    let core = core_of(&dir, text, &|_| None).unwrap();
    assert!(core.render().starts_with("# Safeguards\n"));
    assert_eq!(core.render().matches('#').count(), 1);
    let mut s = setup();
    s.brain.core = Core { items: Vec::new() };
    let built = prompt::build(&s.brain, &s.host, &s.zone, WINTER).unwrap();
    let soul = fs::read_to_string(&s.brain.soul).unwrap();
    assert!(built.starts_with(&format!("{}\n\nKnowledge base index: ", soul.trim())));
}

#[test]
fn an_unknown_core_id_or_a_bad_override_file_is_an_error_naming_its_key() {
    let dir = Scratch::new();
    let known = "known: no_unasked_messages, no_unfiled_promises, no_irreversible_deletion, \
                 search, terminal_and_files, task_delegation";
    assert_eq!(
        core_of(
            &dir,
            "[engine.core.search]\nenabled = true\n[engine.core.no_money]\nenabled = false\n",
            &|_| None
        )
        .unwrap_err(),
        format!("config <DIR>/ulei.toml line 3: engine.core.no_money names an unknown id; {known}")
    );
    assert_eq!(
        core_of(&dir, "[engine.core.no-unasked-messages]\n", &|_| None).unwrap_err(),
        format!(
            "config <DIR>/ulei.toml line 1: engine.core.no-unasked-messages names an unknown id; {known}"
        )
    );
    assert_eq!(
        core_of(
            &dir,
            "[engine.core.search]\nfile = \"<DIR>/absent.md\"\n"
                .replace("<DIR>", &dir.0.display().to_string())
                .as_str(),
            &|_| None
        )
        .unwrap_err(),
        "engine.core.search.file (ULEI_ENGINE_CORE_SEARCH_FILE) <DIR>/absent.md: No such file or directory (os error 2)"
    );
    fs::write(dir.path("blank.md"), " \n\t\n").unwrap();
    let blank = |name: &str| {
        (name == "ULEI_ENGINE_CORE_SEARCH_FILE").then(|| dir.path("blank.md").into_os_string())
    };
    assert_eq!(
        core_of(&dir, "", &blank).unwrap_err(),
        "engine.core.search.file (ULEI_ENGINE_CORE_SEARCH_FILE) names a blank file"
    );
}

#[test]
fn the_core_tier_fits_its_token_ceiling() {
    let shipped = estimate_tokens_rough(&Core::defaults(None).render());
    let soul = fs::read_to_string(Path::new(FIXTURE).join("brain/SOUL.md")).unwrap();
    let full = format!(
        "{}\n\n{}",
        soul.trim(),
        Core::defaults(Some(Path::new("/srv/fixture/brain/task-board.md"))).render()
    );
    let full = estimate_tokens_rough(&full);
    println!(
        "::notice title=core tier budget::shipped core {shipped} tokens (ceiling \
         {CORE_TOKEN_CEILING}); synthetic SOUL.md with the shipped core and a task board {full} \
         tokens"
    );
    assert!(
        shipped <= CORE_TOKEN_CEILING,
        "{shipped} > {CORE_TOKEN_CEILING}"
    );
}
