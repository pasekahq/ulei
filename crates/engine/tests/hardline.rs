//! The `hardline` policy: the reference tables of the unconditional floor, the detection
//! normalisation, the parser limit and the block result.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use ulei_engine::policy::{Chain, Hardline, MalformedGrep, Policy, Verdict};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Context, Interrupt, Registry};
use ulei_engine::wire::c4g::ToolCall;

// Commands under `/home` are split with `concat!` before the slash after it: the source lint
// reads that path prefix as a personal path.
const HOME: &str = "/srv/people/agent";
const ROOT: &str = "recursive delete of root filesystem";
const HOME_WIPE: &str = "recursive delete of home directory";
const SHUTDOWN: &str = "system shutdown/reboot";
const PARSER_LIMIT: &str = "command parser limit exceeded";
const MALFORMED: &str = "command parser limit or malformed executable payload";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ulei-hardline-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn terminal(command: &str) -> ToolCall {
    let Value::Object(input) = json!({"command": command}) else {
        unreachable!()
    };
    ToolCall {
        id: "call-1".into(),
        name: "terminal".into(),
        input,
    }
}

fn floor(home: Option<&str>, blocked: &Path) -> Hardline {
    Hardline::new(
        home.map(PathBuf::from),
        blocked.to_path_buf(),
        MalformedGrep::Warn,
    )
}

/// The description of the block of `command` under `policy`, `None` when it may run.
fn block_with(policy: &mut Hardline, command: &str) -> Option<String> {
    match policy.pre_tool(&terminal(command), Path::new("/")) {
        Verdict::Allow => None,
        Verdict::Synthetic(content) => {
            let result: Value = serde_json::from_str(&content).unwrap();
            assert_eq!(result["output"], "", "{command:?}");
            assert_eq!(result["exit_code"], -1, "{command:?}");
            assert_eq!(result["status"], "blocked", "{command:?}");
            let error = result["error"].as_str().unwrap();
            let rest = error.strip_prefix("BLOCKED (hardline): ").unwrap();
            Some(rest[..rest.find(". This command").unwrap()].to_owned())
        }
        Verdict::Deny(reason) => panic!("{command:?} denied as {reason}"),
    }
}

fn block(command: &str) -> Option<String> {
    let dir = std::env::temp_dir().join("ulei-hardline-unused");
    block_with(&mut floor(Some(HOME), &dir), command)
}

fn block_in_block_mode(command: &str) -> Option<String> {
    let dir = std::env::temp_dir().join("ulei-hardline-unused");
    let mut policy = Hardline::new(Some(HOME.into()), dir, MalformedGrep::Block);
    block_with(&mut policy, command)
}

fn assert_all_blocked(table: &[&str]) {
    let leaked: Vec<&&str> = table.iter().filter(|c| block(c).is_none()).collect();
    assert!(leaked.is_empty(), "not blocked: {leaked:?}");
}

fn assert_all_allowed(table: &[&str]) {
    let blocked: Vec<(&&str, String)> = table
        .iter()
        .filter_map(|c| block(c).map(|d| (c, d)))
        .collect();
    assert!(blocked.is_empty(), "blocked: {blocked:?}");
}

/// `_HARDLINE_BLOCK` cases the normalised command already blocks.
const HARDLINE_BLOCK: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "rm -rf //",
    "rm -rf /.",
    "rm -rf /./",
    "rm -rf /..",
    "rm -rf //*",
    "rm -fr /./",
    "rm -rf /home",
    concat!("rm -rf /home", "/*"),
    "rm -rf /etc",
    "rm -rf /usr",
    "rm -rf /var",
    "rm -rf /boot",
    "rm -rf /bin",
    "rm --recursive --force /",
    "rm -fr /",
    "sudo rm -rf /",
    "rm -rf ~",
    "rm -rf ~/",
    "rm -rf ~/*",
    "rm -rf $HOME",
    r#"rm -rf "/""#,
    "rm -rf '/'",
    r#"rm -rf "/*""#,
    r#"rm -rf "/etc""#,
    "rm -rf '/etc'",
    r#"rm -rf "/home""#,
    r#"rm -rf "/usr""#,
    r#"rm -rf "$HOME""#,
    "rm -rf '$HOME'",
    r#"rm -rf "$HOME/""#,
    r#"rm -rf "~""#,
    r#"sudo rm -rf "/""#,
    r#"rm -rf "/" && echo done"#,
    "rm -rf ${HOME}",
    r#"rm -rf "${HOME}""#,
    "rm -fr ${HOME}",
    "mkfs.ext4 /dev/sda1",
    "mkfs /dev/sdb",
    "mkfs.xfs /dev/nvme0n1",
    "dd if=/dev/zero of=/dev/sda bs=1M",
    "dd if=/dev/urandom of=/dev/nvme0n1",
    "dd if=anything of=/dev/hda",
    "echo bad > /dev/sda",
    "cat /dev/urandom > /dev/sdb",
    ":(){ :|:& };:",
    "kill -9 -1",
    "kill -1",
    "shutdown -h now",
    "shutdown -r now",
    "sudo shutdown now",
    "reboot",
    "sudo reboot",
    "halt",
    "poweroff",
    "init 0",
    "init 6",
    "telinit 0",
    "systemctl poweroff",
    "systemctl reboot",
    "systemctl halt",
    "$(reboot)",
    "`shutdown now`",
    "sudo -E shutdown now",
    "env FOO=1 reboot",
    "exec shutdown",
    "nohup reboot",
    "setsid poweroff",
];

/// `_HARDLINE_BLOCK` cases whose trigger sits after `;`, `&&`, `||`, `(` or `{`: only the shell
/// word detection variants put a command start there.
const HARDLINE_BLOCK_VARIANTS: &[&str] = &[
    "ls && rm -rf //",
    "ls; reboot",
    "echo done && shutdown -h now",
    "false || halt",
    "(reboot)",
    "( reboot )",
    "(shutdown -h now)",
    "(poweroff)",
    "(halt)",
    "(init 0)",
    "(systemctl reboot)",
    "(sudo reboot)",
    "{ reboot; }",
    "{ shutdown -h now; }",
    "{ poweroff; }",
    "true && (reboot)",
    "echo hi; { reboot; }",
];

/// `_HARDLINE_ALLOW`.
const HARDLINE_ALLOW: &[&str] = &[
    "rm -rf /tmp/foo",
    "rm -rf /tmp/*",
    "rm -rf ./build",
    "rm -rf node_modules",
    concat!("rm -rf /home", "/user/scratch"),
    "rm -rf ~/Downloads/old",
    "rm -rf $HOME/tmp",
    "rm foo.txt",
    "rm -rf some/path",
    "rm -rf /...",
    "rm -rf /....",
    "rm -rf /.foo",
    "rm -rf /.config/foo",
    r#"git commit -m "rm -rf /""#,
    r#"git commit -m "wipe with rm -rf /etc""#,
    "dd if=/dev/zero of=./image.bin",
    "dd if=./data of=./backup.bin",
    "echo done > /tmp/flag",
    "echo test > /dev/null",
    "ls /dev/sda",
    "cat /dev/urandom | head -c 10",
    "grep 'shutdown' logs.txt",
    "echo reboot",
    "echo '# init 0 in comment'",
    "cat rebooting.log",
    "echo 'halt and catch fire'",
    r#"python3 -c 'print("shutdown")'"#,
    "find . -name '*reboot*'",
    "mkfs_helper --version",
    "systemctl status nginx",
    "systemctl restart nginx",
    "systemctl stop nginx",
    "systemctl start nginx",
    "kill -9 12345",
    "kill -HUP 1234",
    "pkill python",
    "git status",
    "npm run build",
    "sudo apt update",
    "curl https://example.com | head",
];

/// `_QUOTED_BRACE_BYPASS`.
const QUOTED_BRACE_BYPASS: &[&str] = &[
    r#"rm -rf "/""#,
    "rm -rf '/'",
    r#"rm -rf "/etc""#,
    r#"rm -rf "/home""#,
    r#"rm -rf "$HOME""#,
    "rm -rf ${HOME}",
    r#"rm -rf "${HOME}""#,
];

/// `_QUOTED_NEWLINE_DATA_ALLOW` cases with no command start after a quoted newline.
const QUOTED_NEWLINE_DATA_ALLOW: &[&str] = &[
    "python3 - <<'EOF'\nmsg = 'run sudo reboot later'\nprint(msg)\nEOF",
    "cat > /tmp/notes.txt <<'EOF'\nremember: shutdown -h now\nEOF",
    "git commit -m \"docs:\nwarn about rm -rf / in the guide\"",
];

/// `_QUOTED_NEWLINE_DATA_ALLOW` cases and the full-chain case whose quoted newline is followed by
/// a trigger: only the quoted-newline mask of the detection variants keeps them runnable.
const QUOTED_NEWLINE_DATA_ALLOW_VARIANTS: &[&str] = &[
    "notify send -t chat -s \"box1\" \"console output:\nsudo reboot\ndone\"",
    "notify send -t chat \"line1\nshutdown -h now\nline3\"",
    "git commit -m 'ops notes:\nreboot the box after the deploy'",
    "git commit -m \"fix startup\nsystemctl reboot was flaky here\"",
    "notify send -t chat \"status:\nsudo reboot happened at 3am\"",
];

/// `_QUOTED_NEWLINE_THREATS_BLOCK` cases the normalised command already blocks.
const QUOTED_NEWLINE_THREATS_BLOCK: &[&str] = &[
    "echo hi\nsudo reboot",
    "echo \"a\"\nsudo reboot",
    "git commit -m \"safe message\"\nshutdown -h now",
    "notify send -t chat \"$(sudo reboot)\"",
    "echo \"`shutdown -h now`\"",
    "sudo reboot <<'EOF'\nignored\nEOF",
];

/// `_QUOTED_NEWLINE_THREATS_BLOCK` cases chained with `&&`.
const QUOTED_NEWLINE_THREATS_BLOCK_VARIANTS: &[&str] =
    &["notify send \"line1\nline2\" && sudo reboot"];

/// `_DATA_ARG_NOT_A_COMMAND` and the quoted prose of the subshell tests.
const DATA_ARG_NOT_A_COMMAND: &[&str] = &[
    r#"gh pr create --title "block rm -rf / spellings""#,
    r#"git commit -m "fixes rm -rf / bypass""#,
    r#"echo "run rm -rf / now""#,
    r#"echo "rm -rf /""#,
    r#"printf "%s" "rm -rf /""#,
    r#"gh issue comment 1 --body "the fix blocks rm -rf //""#,
    r#"gh pr create --title "block (reboot) spellings""#,
    r#"git commit -m "(rm -rf /) note""#,
    r#"echo "(reboot)""#,
    r#"echo "{ reboot; }""#,
    "echo '(poweroff)'",
    "echo '{ rm -rf /; }'",
    r#"find . -name "*(reboot)*""#,
];

/// `_COMMAND_POSITION_ROOT_WIPES` cases the normalised command already blocks.
const COMMAND_POSITION_ROOT_WIPES: &[&str] = &[
    "rm -rf /",
    "sudo rm -rf /",
    "env X=1 rm -rf /",
    "$(rm -rf /)",
    "`rm -rf /`",
    r#"echo "$(rm -rf /)""#,
];

/// `_COMMAND_POSITION_ROOT_WIPES` cases after a separator or a subshell or group opener.
const COMMAND_POSITION_ROOT_WIPES_VARIANTS: &[&str] = &[
    "ls && rm -rf /",
    "ls; rm -rf /",
    "echo x | rm -rf /",
    "(rm -rf /)",
    "{ rm -rf /; }",
    "(rm -rf ~)",
    "(sudo rm -rf /)",
];

/// The subshell and brace-group list of the yolo test; every case needs the variants.
const SUBSHELL_BRACE_GROUP_VARIANTS: &[&str] = &[
    "(reboot)",
    "( reboot )",
    "(shutdown -h now)",
    "(poweroff)",
    "(systemctl reboot)",
    "(init 0)",
    "(sudo reboot)",
    "{ reboot; }",
    "{ shutdown -h now; }",
    "{ poweroff; }",
    "(rm -rf /)",
    "{ rm -rf /; }",
    "(rm -rf ~)",
    "true && (reboot)",
    "echo hi; { reboot; }",
];

/// The commands of the yolo, root-collapse and line-continuation tests, which the floor blocks
/// with no setting able to pass them.
const NO_BYPASS: &[&str] = &[
    "rm -rf /",
    r#"rm -rf "/""#,
    r#"rm -rf "$HOME""#,
    "rm -rf ${HOME}",
    "shutdown -h now",
    "mkfs.ext4 /dev/sda",
    "reboot",
    "rm -rf //",
    "rm -rf /.",
    "rm -rf /./",
    "rm -rf /..",
    "rm -rf //*",
    "rm -rf \\\n/",
];

/// The real paths of the root-collapse test and the recoverable commands of the yolo test.
const NOT_HARDLINE: &[&str] = &[
    "rm -rf /tmp",
    concat!("rm -rf /home", "/user/x"),
    "rm -rf /.ssh",
    "rm -rf /.config",
    "rm -rf ./build",
    "rm -rf /opt/foo",
    "rm -rf /...",
    "rm -rf /....",
    "rm -rf /.foo",
    "rm -rf /tmp/x",
    "chmod -R 777 .",
    "git reset --hard",
    "git push --force",
];

/// `TestBenignNotFlagged` of the shell-bypass tests, on the hardline floor.
const BENIGN: &[&str] = &[
    "git log --oneline",
    "ls -la",
    "echo hello world",
    "echo rm is a command",
    "curl http://example.com -o out.html",
    "base64 -d payload.b64 > out.bin",
];

/// The commands of `test_read_tool_exec_like_operands_owned_by_other_syntax_are_not_flagged`,
/// `test_non_executing_flags_are_not_flagged`,
/// `test_unrelated_options_do_not_promote_payload_text_to_hardline`,
/// `test_grep_pcre_pattern_with_grouped_root_delete_text_stays_safe` and
/// `test_malformed_quoted_executable_payloads_fail_closed`.
const EXEC_FLAG_ALLOW: &[&str] = &[
    "rg -- --pre sh",
    "sort -- --compress-program sh",
    "rg --pre-glob --pre needle",
    "sort --output --compress-program names.txt",
    "man --config-file --pager printf",
    "ag --ignore --pager needle",
    "rg -g --pre needle",
    "sort -o --compress-program names.txt",
    "man -C --pager printf",
    "ag -G --pager needle",
    "node -c script.js",
    "node --check script.js",
    "ruby -c script.rb",
    "python3 -m http.server",
    "python3 --version",
    "sort names.txt",
    "rg --pretty pattern src/",
    "pip install --pre somepackage",
    "man -k pager",
    "man -p e ls",
    "pip install --pre 'rm -rf --no-preserve-root /'",
    "grep -P 'rm -rf --no-preserve-root /' file.txt",
    "printf '%s' 'man -P rm -rf --no-preserve-root /'",
    "grep -P '(?:safe|rm -rf --no-preserve-root /)' audit.log",
    "sort --compress-program=\"sh -c 'unterminated names",
    "rg --pre=\"bash -lc 'unterminated pattern files",
    "man --pager=\"sh -c 'unterminated ls",
];

/// The commands of `test_leading_dash_program_payloads_reach_hardline_floor` and
/// `test_exec_flag_payload_reaches_hardline_floor`: a root wipe as the program of a read-only
/// tool's program option.
const EXEC_FLAG_PAYLOADS: &[&str] = &[
    "rg --pre '-payload; rm -rf --no-preserve-root /' needle",
    "sort --compress-program='-payload; rm -rf --no-preserve-root /' names",
    "ag --pager='-payload; rm -rf --no-preserve-root /' needle",
    "man --pager '-payload; rm -rf --no-preserve-root /' ls",
    "man -P '-payload; rm -rf --no-preserve-root /' ls",
    "man -H'-payload; rm -rf --no-preserve-root /' ls",
    "sort --compress-program='rm -rf --no-preserve-root /' names.txt",
    "rg --pre 'rm -rf --no-preserve-root /' -e . x",
    "ag --pager 'rm -rf --no-preserve-root /' foo",
    "man -P 'rm -rf --no-preserve-root /' ls",
];

/// The command names of `TestCommandNameObfuscation` spelled through quoting, escapes, literal
/// substitutions and parameter forms, here wiping the root.
const OBFUSCATED_COMMAND_NAMES: &[&str] = &[
    r"r\m -rf /",
    "r''m -rf /",
    r#"r""m -rf /"#,
    "$(echo rm) -rf /",
    "`echo rm` -rf /",
    "${0/x/r}m -rf /",
    "$(printf rm) -rf /",
    "$(printf %s rm) -rf /",
    "$(printf r)m -rf /",
    "$(echo -n rm) -rf /",
    "${unset:-rm} -rf /",
    "sudo $(echo rm) -rf /",
];

/// `test_substitution_argument_not_promoted_to_command`: the same spellings as arguments.
const SUBSTITUTION_ARGUMENTS: &[&str] = &[
    "echo $(echo rm) -rf /",
    "echo $(printf rm) -rf /",
    "echo $(printf %s rm) -rf /",
    "echo $(echo -n rm) -rf /",
    "echo ${unset:-rm} -rf /",
];

/// Synthetic commands whose quoted grep pattern holds `\(` and `['\"]`: once escapes are
/// resolved, the grep words do not tokenize, so the grep parse is malformed.
const MALFORMED_GREP: [&str; 2] = [
    r#"cd /srv/example/app && grep -n -E "@route\.(get|post)\(['\"]/api/[a-z]+" app.py | head -10"#,
    r#"cd /srv/example/site 2>/dev/null && grep -oE "@route\.(get|post)\(['\"][^'\"]+" core.py app.py 2>/dev/null | head -30; grep -n "needle" static/app.js | head -5"#,
];

#[test]
fn the_reference_block_tables_are_blocked() {
    for table in [
        HARDLINE_BLOCK,
        QUOTED_BRACE_BYPASS,
        QUOTED_NEWLINE_THREATS_BLOCK,
        COMMAND_POSITION_ROOT_WIPES,
        NO_BYPASS,
    ] {
        assert_all_blocked(table);
    }
}

#[test]
fn the_reference_allow_tables_run() {
    for table in [
        HARDLINE_ALLOW,
        QUOTED_NEWLINE_DATA_ALLOW,
        DATA_ARG_NOT_A_COMMAND,
        NOT_HARDLINE,
        BENIGN,
        EXEC_FLAG_ALLOW,
        SUBSTITUTION_ARGUMENTS,
    ] {
        assert_all_allowed(table);
    }
}

#[test]
fn the_block_cases_of_the_detection_variants_are_blocked() {
    for table in [
        HARDLINE_BLOCK_VARIANTS,
        QUOTED_NEWLINE_THREATS_BLOCK_VARIANTS,
        COMMAND_POSITION_ROOT_WIPES_VARIANTS,
        SUBSHELL_BRACE_GROUP_VARIANTS,
    ] {
        assert_all_blocked(table);
    }
}

#[test]
fn the_allow_cases_of_the_detection_variants_run() {
    assert_all_allowed(QUOTED_NEWLINE_DATA_ALLOW_VARIANTS);
}

#[test]
fn a_program_handed_to_a_program_option_or_a_shell_is_read_as_a_command() {
    for command in EXEC_FLAG_PAYLOADS {
        assert_eq!(block(command).as_deref(), Some(ROOT), "{command:?}");
    }
    for (command, description) in [
        ("bash -c 'ls; reboot'", SHUTDOWN),
        ("sh -lc 'echo ok && rm -rf /'", ROOT),
        ("bash -o pipefail -c reboot", SHUTDOWN),
        ("env A=1 bash -c 'halt'", SHUTDOWN),
        (r#"ls; bash -c "echo ok; (reboot)""#, SHUTDOWN),
        (r#"rg --pre "sh -c reboot" x"#, SHUTDOWN),
    ] {
        assert_eq!(block(command).as_deref(), Some(description), "{command:?}");
    }
    assert_all_allowed(&[
        "bash --rcfile reboot",
        "bash -O extglob reboot",
        "echo 'bash -c reboot'",
        "grep -e 'bash -c reboot' f",
    ]);
}

#[test]
fn a_command_name_spelled_in_pieces_is_read_whole_in_command_position() {
    for command in OBFUSCATED_COMMAND_NAMES {
        assert_eq!(block(command).as_deref(), Some(ROOT), "{command:?}");
    }
}

#[test]
fn a_drive_path_also_reads_the_command_with_backslashes_as_slashes() {
    assert_eq!(block("echo C:\\x\nrm -rf \\\\").as_deref(), Some(ROOT));
    assert_eq!(block("echo x\nrm -rf \\\\"), None);
}

#[test]
fn a_malformed_grep_runs_by_default_and_block_mode_refuses_it() {
    for command in MALFORMED_GREP {
        assert_eq!(block(command), None, "{command:?}");
        assert_eq!(
            block_in_block_mode(command).as_deref(),
            Some(MALFORMED),
            "{command:?}"
        );
    }
}

#[test]
fn a_hardline_command_next_to_a_malformed_grep_keeps_its_own_description() {
    for (suffix, description) in [
        ("rm -rf /", ROOT),
        ("mkfs.ext4 /dev/sda1", "format filesystem (mkfs)"),
        ("reboot", SHUTDOWN),
        (":(){ :|:& };:", "fork bomb"),
    ] {
        let command = format!("{}; {suffix}", MALFORMED_GREP[0]);
        assert_eq!(block(&command).as_deref(), Some(description), "{command:?}");
    }
}

#[test]
fn a_wrapped_hardline_command_next_to_a_malformed_grep_is_blocked() {
    for suffix in [
        "(reboot)",
        "{ shutdown -h now; }",
        r#"bash -c "rm -rf /""#,
        "sh -c reboot",
        "eval reboot",
    ] {
        for separator in ["; ", " && ", "\n", " || "] {
            let command = format!("{}{separator}{suffix}", MALFORMED_GREP[0]);
            let description = block(&command).unwrap_or_else(|| panic!("{command:?} ran"));
            assert_ne!(description, MALFORMED, "{command:?}");
        }
    }
}

#[test]
fn a_command_substitution_in_command_position_next_to_a_malformed_grep_is_blocked() {
    for suffix in [
        "$(echo rm) -rf /",
        "`echo rm` -rf /",
        "$(echo ls)",
        "`date`",
    ] {
        for separator in ["; ", " && ", "\n", " || "] {
            let command = format!("{}{separator}{suffix}", MALFORMED_GREP[0]);
            assert_eq!(block(&command).as_deref(), Some(MALFORMED), "{command:?}");
        }
    }
    let argument = format!("{}; echo $(date)", MALFORMED_GREP[1]);
    assert_eq!(block(&argument), None);
}

#[test]
fn the_detector_reads_a_command_at_the_size_limit() {
    let command: String = (0..)
        .map(|i| format!("printf segment-{i};"))
        .flat_map(|segment| segment.chars().collect::<Vec<_>>())
        .take(128_000)
        .collect();
    let started = Instant::now();
    assert_eq!(block(&command), None);
    let elapsed = started.elapsed();
    eprintln!("hardline detection of a 128000-character command: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(60), "{elapsed:?}");
}

#[test]
fn each_pattern_answers_its_reference_description() {
    let cases = [
        ("rm -rf /", ROOT),
        ("rm -rf /etc", "recursive delete of system directory"),
        ("rm -rf ~", HOME_WIPE),
        ("mkfs /dev/sdb", "format filesystem (mkfs)"),
        ("dd if=/dev/zero of=/dev/sda", "dd to raw block device"),
        ("echo bad > /dev/sda", "redirect to raw block device"),
        (":(){ :|:& };:", "fork bomb"),
        ("kill -9 -1", "kill all processes"),
        ("reboot", "system shutdown/reboot"),
        ("init 0", "init 0/6 (shutdown/reboot)"),
        ("systemctl kexec", "systemctl poweroff/reboot"),
        ("telinit 6", "telinit 0/6 (shutdown/reboot)"),
    ];
    for (command, description) in cases {
        assert_eq!(block(command).as_deref(), Some(description), "{command:?}");
    }
}

#[test]
fn a_line_continuation_is_joined_before_matching() {
    let cases = [
        ("rm -rf \\\n/", "root"),
        ("rm -r\\\nf /", "root"),
        ("rm -rf \\\n~", "home"),
        ("rm -rf \\\r\n/", "root"),
        ("mkfs.ext4 \\\n/dev/sda1", "mkfs"),
    ];
    for (command, part) in cases {
        let description = block(command).unwrap_or_else(|| panic!("{command:?} ran"));
        assert!(description.contains(part), "{command:?}: {description}");
    }
}

#[test]
fn escape_sequences_nul_and_fullwidth_forms_do_not_hide_a_command() {
    for command in [
        "\u{1b}[31mrm\u{1b}[0m -rf /",
        "\u{1b}]0;title\u{7}rm -rf /",
        "r\0m -rf /",
        "d\0d if=/dev/zero of=/dev/sda",
        "\u{ff52}\u{ff4d} -\u{ff52}\u{ff46} /",
        "\u{ff44}\u{ff44} if=/dev/zero of=/dev/sda",
        "\u{1b}[1m\u{ff52}\u{ff4d}\u{1b}[0m -rf\u{3000}\u{ff0f}",
        "\u{ff52}\u{ff45}\u{ff42}\u{ff4f}\u{ff4f}\u{ff54}",
    ] {
        assert!(block(command).is_some(), "{command:?} ran");
    }
    assert_eq!(block("\u{ff4c}\u{ff53} -la /tmp"), None);
}

#[test]
fn backslash_escapes_and_empty_quotes_do_not_split_a_command() {
    for command in [
        r"r\m -rf /",
        "r''m -rf /",
        r#"r""m -rf /"#,
        r"re\boot",
        "shut''down -h now",
        r#"rm -rf \"/\""#,
    ] {
        assert!(block(command).is_some(), "{command:?} ran");
    }
}

#[test]
fn ifs_expansions_read_as_spaces_at_a_word_boundary() {
    for command in [
        "rm${IFS}-rf${IFS}/",
        "rm$IFS-rf$IFS/",
        "rm${IFS:0:1}-rf /",
        "mkfs${IFS}.ext4 /dev/sda",
        "init${IFS}0",
    ] {
        assert!(block(command).is_some(), "{command:?} ran");
    }
    assert_eq!(block("init$IFS0"), None);
    assert_eq!(block("init$IFS_0"), None);
}

#[test]
fn the_home_prefix_reads_as_tilde() {
    let dir = scratch("home");
    let mut folded = floor(Some(HOME), &dir);
    for command in [
        format!("rm -rf {HOME}/"),
        format!("rm -rf {HOME}/*"),
        format!("rm -rf '{HOME}/'"),
        format!("rm -rf \"{HOME}/\""),
    ] {
        assert_eq!(
            block_with(&mut folded, &command).as_deref(),
            Some(HOME_WIPE),
            "{command:?}"
        );
        assert_eq!(block_with(&mut floor(None, &dir), &command), None);
    }
    for command in [
        format!("rm -rf {HOME}"),
        format!("rm -rf {HOME}/notes"),
        format!("rm -rf {HOME}x/"),
        format!("rm -rf {}/", HOME.to_uppercase()),
    ] {
        assert_eq!(block_with(&mut folded, &command), None, "{command:?}");
    }
    assert_eq!(
        block_with(&mut floor(Some("/srv"), &dir), "rm -rf /srv/"),
        None
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_resolved_home_reads_as_tilde_too() {
    let dir = scratch("home-link");
    fs::create_dir_all(dir.join("real/agent")).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
    let home = dir.join("link/agent");
    let mut policy = Hardline::new(Some(home), dir.join("blocked"), MalformedGrep::Warn);
    let real = dir.join("real/agent");
    assert_eq!(
        block_with(&mut policy, &format!("rm -rf {}/*", real.display())).as_deref(),
        Some(HOME_WIPE)
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_block_result_is_the_reference_text() {
    let content = match floor(Some(HOME), Path::new("/nonexistent"))
        .pre_tool(&terminal("sudo reboot"), Path::new("/"))
    {
        Verdict::Synthetic(content) => content,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        content,
        "{\"output\": \"\", \"exit_code\": -1, \"error\": \"BLOCKED (hardline): system \
         shutdown/reboot. This command is on the unconditional blocklist and cannot be executed \
         via the agent \u{2014} not even with --yolo, /yolo, approvals.mode=off, or cron approve \
         mode. If you genuinely need to run it, run it yourself in a terminal outside the \
         agent.\", \"status\": \"blocked\"}"
    );
}

#[test]
fn other_tools_and_a_non_string_command_pass_to_their_tool() {
    let dir = std::env::temp_dir();
    let mut policy = floor(Some(HOME), &dir);
    let mut call = terminal("rm -rf /");
    call.name = "write_file".into();
    assert_eq!(policy.pre_tool(&call, &dir), Verdict::Allow);
    let mut call = terminal("");
    call.input
        .insert("command".into(), json!(["rm", "-rf", "/"]));
    assert_eq!(policy.pre_tool(&call, &dir), Verdict::Allow);
}

#[test]
fn the_parser_limits_block_before_any_pattern() {
    let dir = scratch("limits");
    let blocked = dir.join("blocked");
    let mut policy = floor(Some(HOME), &blocked);
    let long = "a".repeat(4_097);
    for command in [
        format!("{};", "a".repeat(128_000)),
        long.clone(),
        "\u{e9}".repeat(4_097),
        ";".repeat(25_000),
        "\n".repeat(25_000),
    ] {
        assert_eq!(
            block_with(&mut policy, &command).as_deref(),
            Some(PARSER_LIMIT),
            "{} chars",
            command.chars().count()
        );
    }
    for command in [
        "a".repeat(4_096),
        "\u{e9}".repeat(4_096),
        format!("{long};"),
        ";".repeat(24_999),
    ] {
        assert_eq!(block_with(&mut policy, &command), None);
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn substitutions_nested_past_the_limit_hit_the_parser_limit() {
    let dir = scratch("nested");
    let mut policy = floor(Some(HOME), &dir.join("blocked"));
    let nested = |depth: usize| format!("{}reboot;", "$(".repeat(depth));
    assert_eq!(
        block_with(&mut policy, &nested(64)).as_deref(),
        Some(SHUTDOWN)
    );
    assert_eq!(
        block_with(&mut policy, &nested(65)).as_deref(),
        Some(PARSER_LIMIT)
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_parser_limit_block_saves_the_command_as_a_private_script() {
    let dir = scratch("saved");
    let blocked = dir.join("blocked");
    fs::create_dir_all(&blocked).unwrap();
    let old = blocked.join("blocked-1-00000000.sh");
    let fresh = blocked.join("blocked-2-00000000.sh");
    let other = blocked.join("keep.sh");
    for path in [&old, &fresh, &other] {
        fs::write(path, "echo old\n").unwrap();
    }
    let eight_days_ago = SystemTime::now() - Duration::from_secs(8 * 86_400);
    for path in [&old, &other] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(eight_days_ago)
            .unwrap();
    }
    let command = "a".repeat(5_000);
    let content = match floor(Some(HOME), &blocked).pre_tool(&terminal(&command), &dir) {
        Verdict::Synthetic(content) => content,
        other => panic!("{other:?}"),
    };
    let result: Value = serde_json::from_str(&content).unwrap();
    let error = result["error"].as_str().unwrap();
    let saved = error
        .split("Your command was saved to ")
        .nth(1)
        .and_then(|rest| rest.split(" \u{2014} ").next())
        .unwrap();
    assert!(error.starts_with(
        "BLOCKED (hardline): command parser limit exceeded. This command is on the \
         unconditional blocklist"
    ));
    assert!(error.ends_with(&format!(
        "outside the agent. RECOVERY: this block fires on oversized/unparseable inline command \
         payloads (heredocs, giant one-liners), not on the operation itself. Your command was \
         saved to {saved} \u{2014} review it, then run: terminal(command=\"bash {saved}\"). Do \
         not retry inline."
    )));
    let saved = Path::new(saved);
    assert_eq!(saved.parent(), Some(blocked.as_path()));
    let name = saved.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("blocked-"), "{name}");
    assert_eq!(saved.extension().unwrap(), "sh");
    assert_eq!(
        fs::read_to_string(saved).unwrap(),
        format!(
            "#!/bin/bash\n# Auto-saved by the agent: this command exceeded the inline command\n\
             # parser limit and was blocked from direct execution. Review it,\n# then run it via: \
             bash {}\n{command}\n",
            saved.display()
        )
    );
    assert_eq!(
        fs::metadata(saved).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!old.exists());
    assert!(fresh.exists() && other.exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_payload_that_cannot_be_saved_gets_the_write_file_recovery() {
    let dir = scratch("unsaved");
    let file = dir.join("file");
    fs::write(&file, "").unwrap();
    let mut policy = floor(Some(HOME), &file.join("blocked"));
    let content = match policy.pre_tool(&terminal(&";".repeat(25_000)), &dir) {
        Verdict::Synthetic(content) => content,
        other => panic!("{other:?}"),
    };
    let result: Value = serde_json::from_str(&content).unwrap();
    assert!(result["error"].as_str().unwrap().ends_with(
        "outside the agent. RECOVERY: this block fires on oversized/unparseable inline \
             command payloads (heredocs, giant one-liners), not on the operation itself. Write \
             the script to a file with write_file, then run it: terminal(command=\"bash \
             /path/script.sh\") or \"python3 /path/script.py\". Do not retry inline."
    ));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_blocked_command_does_not_run_and_another_one_does() {
    let dir = scratch("chain");
    let ran = dir.join("ran");
    let mut chain = Chain::new(vec![Box::new(floor(Some(HOME), &dir.join("blocked")))]);
    let tools = Registry::builtin(
        &dir,
        Redactor { enabled: true },
        None,
        &fake_gateway::skills(&dir),
        None,
    );
    let interrupt = Interrupt::new();
    let ctx = Context {
        cwd: &dir,
        interrupt: &interrupt,
    };
    // A shell function shadows the program, so the command would only create the marker.
    let out = chain.run_tool(
        &tools,
        &terminal(&format!("mkfs() {{ touch {}; }}; mkfs", ran.display())),
        ctx,
    );
    assert!(out.is_error);
    assert!(
        out.content
            .contains("BLOCKED (hardline): format filesystem (mkfs).")
    );
    assert!(!ran.exists());

    let out = chain.run_tool(&tools, &terminal(&format!("touch {}", ran.display())), ctx);
    assert!(!out.is_error, "{}", out.content);
    assert!(ran.exists());
    fs::remove_dir_all(&dir).unwrap();
}
