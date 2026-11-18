//! `skills_list` and `skill_view` through the registry on the synthetic fixture skills
//! directory: the listing and its cuts, the skill view with its frontmatter fields and linked
//! files, linked files, the refusals of paths outside a skill, missing, disabled and foreign
//! skills, the repeat-view dedup and files that are not text.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use fake_gateway::{Scratch, copy_tree};
use serde_json::{Map, Value, json};
use ulei_engine::skills::Skills;
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{
    Context, Interrupt, MAX_DESCRIPTION_LENGTH, MAX_NAME_LENGTH, Output, Registry,
    VIEW_DEDUP_MESSAGE,
};

const SKILLS_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/skills");

/// The fixture skills copied into a scratch root, `pruning` disabled, behind one registry.
struct Fixture {
    root: Scratch,
    registry: Registry,
}

impl Fixture {
    fn new() -> Self {
        let root = Scratch::new();
        copy_tree(Path::new(SKILLS_FIXTURE), &root.path("skills"));
        let skills = Skills {
            dir: root.path("skills"),
            disabled: vec!["pruning".to_owned()],
        };
        let registry = Registry::builtin(
            &root.path("spill"),
            Redactor { enabled: true },
            None,
            &skills,
            None,
        );
        Self { root, registry }
    }

    fn call(&self, tool: &str, value: Value) -> Output {
        let Value::Object(input) = value else {
            panic!("not an object: {value}");
        };
        let interrupt = Interrupt::new();
        self.registry.execute(
            tool,
            &input,
            Context {
                cwd: &self.root.0,
                interrupt: &interrupt,
            },
        )
    }

    fn view(&self, value: Value) -> Output {
        self.call("skill_view", value)
    }

    /// `rel` below the skills directory, written as the answers show it.
    fn at(&self, rel: &str) -> String {
        self.root.path("skills").join(rel).display().to_string()
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.root.path("skills").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

fn ok(output: &Output) -> &str {
    assert!(!output.is_error, "{}", output.content);
    &output.content
}

fn failed(output: &Output) -> &str {
    assert!(output.is_error, "{}", output.content);
    &output.content
}

fn field(output: &Output, key: &str) -> Value {
    serde_json::from_str::<Value>(&output.content).unwrap()[key].clone()
}

const LISTED: &str = r#"{"name": "standalone", "description": "A skill outside any category.", "category": null}, {"name": "compost-heap", "description": "Turn the compost heap every two weeks.", "category": "garden"}, {"name": "grocery-list", "description": "Keep the shared grocery list sorted by aisle.", "category": "household"}, {"name": "laundry", "description": "Wash the towels on Saturdays.", "category": "household"}, {"name": "meal-plan", "description": "Plan the meals of the coming week from what is in the pantry, then hand the missing items to the grocery list.", "category": "household"}, {"name": "diary", "description": "Add one line to the household diary each evening.", "category": "notes"}"#;
const LIST_HINT: &str = "Use skill_view(name) to see full content, tags, and linked files";

#[test]
fn skills_list_lists_the_shown_skills_by_category_and_name() {
    let f = Fixture::new();
    let all = f.call("skills_list", json!({}));
    assert_eq!(
        ok(&all),
        format!(
            r#"{{"success": true, "skills": [{LISTED}], "categories": ["garden", "household", "notes"], "count": 6, "hint": "{LIST_HINT}"}}"#
        )
    );
}

#[test]
fn skills_list_filters_by_category() {
    let f = Fixture::new();
    let household = f.call("skills_list", json!({"category": "household"}));
    assert_eq!(field(&household, "count"), json!(3));
    assert_eq!(field(&household, "categories"), json!(["household"]));
    let names: Vec<Value> = field(&household, "skills")
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].clone())
        .collect();
    assert_eq!(
        names,
        [json!("grocery-list"), json!("laundry"), json!("meal-plan")]
    );
    let none = f.call("skills_list", json!({"category": "nope"}));
    assert_eq!(
        ok(&none),
        format!(
            r#"{{"success": true, "skills": [], "categories": [], "count": 0, "hint": "{LIST_HINT}"}}"#
        )
    );
    let all = f.call("skills_list", json!({"category": ""}));
    assert_eq!(field(&all, "count"), json!(6));
}

#[test]
fn skills_list_creates_a_missing_directory_and_reports_no_skills() {
    let root = Scratch::new();
    let skills = Skills {
        dir: root.path("skills"),
        disabled: Vec::new(),
    };
    let registry = Registry::builtin(
        &root.path("spill"),
        Redactor { enabled: true },
        None,
        &skills,
        None,
    );
    let interrupt = Interrupt::new();
    let ctx = Context {
        cwd: &root.0,
        interrupt: &interrupt,
    };
    let out = registry.execute("skills_list", &Map::new(), ctx);
    assert_eq!(
        ok(&out),
        r#"{"success": true, "skills": [], "categories": [], "message": "No skills found in skills/ directory."}"#
    );
    assert!(root.path("skills").is_dir());
}

#[test]
fn skills_list_cuts_long_names_and_descriptions_and_skips_excluded_directories() {
    let f = Fixture::new();
    let long_name = "n".repeat(70);
    let long_description = "d".repeat(1100);
    f.write(
        "household/long/SKILL.md",
        &format!("---\nname: {long_name}\ndescription: {long_description}\n---\n"),
    );
    f.write(
        ".git/hooks/stray/SKILL.md",
        "---\nname: stray\ndescription: Never indexed.\n---\n",
    );
    let skills = field(&f.call("skills_list", json!({})), "skills");
    let long = skills
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["category"] == "household" && s["name"].as_str().unwrap().starts_with('n'))
        .unwrap();
    assert_eq!(long["name"], json!("n".repeat(MAX_NAME_LENGTH)));
    assert_eq!(
        long["description"],
        json!(format!("{}...", "d".repeat(MAX_DESCRIPTION_LENGTH - 3)))
    );
    assert!(!skills.to_string().contains("stray"));
}

#[test]
fn a_frontmatter_outside_the_subset_is_read_line_by_line() {
    let f = Fixture::new();
    f.write(
        "household/tabbed/SKILL.md",
        "---\nname: tabbed\ndescription: \"Quoted: with a colon.\"\nmetadata:\n\towner: x\n---\n",
    );
    let skills = field(
        &f.call("skills_list", json!({"category": "household"})),
        "skills",
    );
    assert_eq!(
        skills[3],
        json!({"name": "tabbed", "description": "\"Quoted: with a colon.\"", "category": "household"})
    );
    let view = f.view(json!({"name": "tabbed"}));
    assert_eq!(
        field(&view, "description"),
        json!("\"Quoted: with a colon.\"")
    );
    assert_eq!(field(&view, "metadata"), Value::Null);
}

#[test]
fn skill_view_returns_the_skill_with_its_frontmatter_fields_and_linked_files() {
    let f = Fixture::new();
    let view = f.view(json!({"name": "grocery-list"}));
    let content = fs::read_to_string(f.at("household/grocery-list/SKILL.md")).unwrap();
    assert_eq!(
        ok(&view),
        format!(
            r#"{{"success": true, "name": "grocery-list", "description": "Keep the shared grocery list sorted by aisle.", "tags": ["shopping", "lists"], "related_skills": ["meal-plan"], "content": {}, "path": "household/grocery-list/SKILL.md", "skill_dir": "{}", "org_provenance": null, "linked_files": {{"references": ["references/aisles.md", "references/stores.md"], "templates": ["templates/week.md", "templates/print/label.json"], "assets": ["assets/badge.txt"], "scripts": ["scripts/sort.py"]}}, "usage_hint": "To view linked files, call skill_view(name, file_path) where file_path is e.g. 'references/api.md' or 'assets/config.yaml'", "required_environment_variables": [], "required_commands": [], "missing_required_environment_variables": [], "missing_credential_files": [], "missing_required_commands": [], "setup_needed": false, "setup_skipped": false, "readiness_status": "available", "_source_path": "{}", "metadata": {{"fixture": {{"owner": "kitchen", "reviewed": "2026-01-01"}}}}}}"#,
            Value::from(content),
            f.at("household/grocery-list"),
            f.at("household/grocery-list/SKILL.md"),
        )
    );
}

#[test]
fn skill_view_reads_block_scalars_and_skills_without_linked_files() {
    let f = Fixture::new();
    let meal = f.view(json!({"name": "meal-plan"}));
    assert_eq!(
        field(&meal, "description"),
        json!(
            "Plan the meals of the coming week from what is in the pantry, then hand the \
             missing items to the grocery list."
        )
    );
    assert_eq!(field(&meal, "linked_files"), Value::Null);
    assert_eq!(field(&meal, "usage_hint"), Value::Null);
    f.write(
        "household/checklist/SKILL.md",
        "---\nname: checklist\ndescription: |\n  First line.\n  Second line.\n\ntags: lists, \"paper\"\n---\n",
    );
    let checklist = f.view(json!({"name": "checklist"}));
    assert_eq!(
        field(&checklist, "description"),
        json!("First line.\nSecond line.\n")
    );
    assert_eq!(field(&checklist, "tags"), json!(["lists", "paper"]));
}

#[test]
fn skill_view_finds_a_skill_by_path_by_frontmatter_name_and_by_category_prefix() {
    let f = Fixture::new();
    let by_path = f.view(json!({"name": "household/laundry"}));
    assert_eq!(field(&by_path, "name"), json!("laundry"));
    assert_eq!(field(&by_path, "description"), json!(""));
    assert_eq!(field(&by_path, "path"), json!("household/laundry/SKILL.md"));
    let by_name = f.view(json!({"name": "compost-heap"}));
    assert_eq!(field(&by_name, "skill_dir"), json!(f.at("garden/compost")));
    let fresh = Fixture::new();
    let by_prefix = fresh.view(json!({"name": "household:laundry"}));
    assert_eq!(
        field(&by_prefix, "skill_dir"),
        json!(fresh.at("household/laundry"))
    );
    let coalesced = f.view(json!({"name": "household:laundry"}));
    assert_eq!(field(&coalesced, "status"), json!("unchanged"));
}

#[test]
fn skill_view_serves_a_linked_file() {
    let f = Fixture::new();
    let file =
        f.view(json!({"name": "household/grocery-list", "file_path": "references/aisles.md"}));
    assert_eq!(
        ok(&file),
        format!(
            r##"{{"success": true, "name": "household/grocery-list", "file": "references/aisles.md", "content": "# Aisles\n\nProduce comes first.\n", "file_type": ".md", "_source_path": "{}"}}"##,
            f.at("household/grocery-list/references/aisles.md")
        )
    );
}

#[test]
fn a_repeat_view_of_an_unchanged_file_gets_the_dedup_answer_until_the_file_changes() {
    let f = Fixture::new();
    ok(&f.view(json!({"name": "grocery-list"})));
    let again = f.view(json!({"name": "grocery-list"}));
    assert_eq!(
        ok(&again),
        format!(
            r#"{{"success": true, "status": "unchanged", "name": "grocery-list", "file": "SKILL.md", "dedup": true, "content_returned": false, "message": "{VIEW_DEDUP_MESSAGE}"}}"#
        )
    );
    ok(&f.view(json!({"name": "household/grocery-list", "file_path": "references/aisles.md"})));
    let other_form = f.view(json!({"name": "grocery-list", "file_path": "references/aisles.md"}));
    assert_eq!(field(&other_form, "status"), json!("unchanged"));
    assert_eq!(field(&other_form, "name"), json!("household/grocery-list"));
    f.write(
        "household/grocery-list/references/aisles.md",
        "# Aisles\n\nBread comes first now.\n",
    );
    let changed = f.view(json!({"name": "grocery-list", "file_path": "references/aisles.md"}));
    assert_eq!(
        field(&changed, "content"),
        json!("# Aisles\n\nBread comes first now.\n")
    );
}

#[test]
fn a_linked_file_outside_the_skill_directory_is_refused() {
    let f = Fixture::new();
    let hint = "Use a relative path within the skill directory";
    let traversal = f.view(json!({"name": "grocery-list", "file_path": "../meal-plan/SKILL.md"}));
    assert_eq!(
        failed(&traversal),
        format!(
            r#"{{"success": false, "error": "Path traversal ('..') is not allowed.", "hint": "{hint}"}}"#
        )
    );
    let outside = f.root.path("outside.md");
    fs::write(&outside, "secret\n").unwrap();
    symlink(
        &outside,
        f.root
            .path("skills/household/grocery-list/references/leak.md"),
    )
    .unwrap();
    let escape = format!(
        r#"{{"success": false, "error": "Path escapes allowed directory: '{}' is not in the subpath of '{}' OR one path is relative and the other is absolute.", "hint": "{hint}"}}"#,
        outside.display(),
        f.at("household/grocery-list")
    );
    let linked = f.view(json!({"name": "grocery-list", "file_path": "references/leak.md"}));
    assert_eq!(failed(&linked), escape);
    let absolute =
        f.view(json!({"name": "grocery-list", "file_path": outside.display().to_string()}));
    assert_eq!(failed(&absolute), escape);
}

#[test]
fn a_missing_linked_file_lists_the_files_of_the_skill() {
    let f = Fixture::new();
    let missing = f.view(json!({"name": "grocery-list", "file_path": "references/missing.md"}));
    assert_eq!(
        failed(&missing),
        r#"{"success": false, "error": "File 'references/missing.md' not found in skill 'grocery-list'.", "available_files": {"references": ["references/aisles.md", "references/stores.md", "references/archive/old.md"], "templates": ["templates/week.md", "templates/print/label.json"], "assets": ["assets/badge.txt"], "scripts": ["scripts/notes.txt", "scripts/sort.py"], "other": ["notes.md"]}, "hint": "Use one of the available file paths listed above"}"#
    );
}

#[test]
fn a_binary_linked_file_comes_back_decoded_with_replacement_characters() {
    let f = Fixture::new();
    fs::write(
        f.root.path("skills/household/grocery-list/assets/icon.png"),
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\xff\xfe",
    )
    .unwrap();
    let binary = f.view(json!({"name": "grocery-list", "file_path": "assets/icon.png"}));
    let r = char::REPLACEMENT_CHARACTER;
    assert_eq!(
        ok(&binary),
        format!(
            r#"{{"success": true, "name": "grocery-list", "file": "assets/icon.png", "content": "{r}PNG\n\u001a\n\u0000\u0000\u0000\nIHDR{r}{r}", "file_type": ".png", "_source_path": "{}"}}"#,
            f.at("household/grocery-list/assets/icon.png")
        )
    );
    let directory = f.view(json!({"name": "grocery-list", "file_path": "templates"}));
    assert_eq!(
        failed(&directory),
        format!(
            r#"{{"error": "[Errno 21] Is a directory: '{}'", "success": false}}"#,
            f.at("household/grocery-list/templates")
        )
    );
}

#[test]
fn a_missing_skill_names_the_listed_ones() {
    let f = Fixture::new();
    let missing = f.view(json!({"name": "nothing-here"}));
    assert_eq!(
        failed(&missing),
        r#"{"success": false, "error": "Skill 'nothing-here' not found.", "available_skills": ["standalone", "compost-heap", "grocery-list", "laundry", "meal-plan", "diary"], "hint": "Use skills_list to see all available skills"}"#
    );
}

#[test]
fn names_that_leave_the_skills_directory_or_have_a_bad_namespace_are_refused() {
    let f = Fixture::new();
    let hint = "Use a skill name or relative path within the skills directory.";
    for (name, error) in [
        (
            "../x",
            "Skill name cannot contain '..' path traversal components.",
        ),
        (
            "/abs",
            "Skill name must be a relative path within the skills directory.",
        ),
        (
            "C:skills",
            "Skill name must be a relative path within the skills directory.",
        ),
        (
            "ns:../x",
            "Skill name cannot contain '..' path traversal components.",
        ),
    ] {
        assert_eq!(
            failed(&f.view(json!({"name": name}))),
            format!(r#"{{"success": false, "error": "{error}", "hint": "{hint}"}}"#),
            "{name}"
        );
    }
    assert_eq!(
        failed(&f.view(json!({"name": "bad ns:x"}))),
        r#"{"success": false, "error": "Invalid namespace 'bad ns' in 'bad ns:x'. Namespaces must match [a-zA-Z0-9_-]+."}"#
    );
}

#[test]
fn disabled_and_foreign_platform_skills_are_refused() {
    let f = Fixture::new();
    assert_eq!(
        failed(&f.view(json!({"name": "pruning"}))),
        r#"{"success": false, "error": "Skill 'pruning' is disabled. Enable it by removing it from the engine.disabled_skills setting or inspect the files directly on disk."}"#
    );
    assert_eq!(
        failed(&f.view(json!({"name": "watering"}))),
        r#"{"success": false, "error": "Skill 'watering' is not supported on this platform.", "readiness_status": "unsupported"}"#
    );
}

#[test]
fn a_name_matching_two_files_is_ambiguous() {
    let f = Fixture::new();
    f.write("notes/standalone.md", "# Standalone notes\n");
    assert_eq!(
        failed(&f.view(json!({"name": "standalone"}))),
        format!(
            r#"{{"success": false, "error": "Ambiguous skill name 'standalone': 2 skills match across your local skills dir and external_dirs. Refusing to guess — load one explicitly by its categorized path.", "matches": ["{}", "{}"], "hint": "Pass the full relative path instead of the bare name (e.g., 'category/skill-name'), or rename one of the colliding skills so each name is unique."}}"#,
            f.at("standalone/SKILL.md"),
            f.at("notes/standalone.md")
        )
    );
}

#[test]
fn a_missing_skills_directory_is_reported_by_skill_view() {
    let root = Scratch::new();
    let skills = Skills {
        dir: root.path("skills"),
        disabled: Vec::new(),
    };
    let registry = Registry::builtin(
        &root.path("spill"),
        Redactor { enabled: true },
        None,
        &skills,
        None,
    );
    let interrupt = Interrupt::new();
    let ctx = Context {
        cwd: &root.0,
        interrupt: &interrupt,
    };
    let mut input = Map::new();
    input.insert("name".into(), json!("any"));
    assert_eq!(
        failed(&registry.execute("skill_view", &input, ctx)),
        r#"{"success": false, "error": "Skills directory does not exist yet. It will be created on first install."}"#
    );
}
