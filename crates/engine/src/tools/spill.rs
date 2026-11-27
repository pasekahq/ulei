// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Tool results too large for the context. After a step's calls ran, a result over its tool's
//! threshold, then the largest results while the step's results together pass the step budget,
//! are saved whole to a file of the spill directory, and the model gets a preview of the result
//! with the file's path instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use sha2::{Digest as _, Sha256};
use ulei_contracts::generated::c4g::ToolCall;

use super::{
    Output, READ_FILE, char_byte, hex, remove_stale, thousands, untrusted, untrusted_parts,
    write_exclusive,
};
use crate::config::{self, Config, Setting};
use crate::mcp::{ELISION_NOTICE, TOOL_NAME_PREFIX};

/// Characters of one result past which it is saved to a file.
pub const DEFAULT_RESULT_SIZE_CHARS: usize = 100_000;
/// Characters of all the results of one step past which the largest are saved to files.
pub const DEFAULT_TURN_BUDGET_CHARS: usize = 200_000;
/// Most characters of the preview of a saved result.
pub const DEFAULT_PREVIEW_SIZE_CHARS: usize = 1_500;
/// Characters of one result of an MCP tool past which it is saved to a file.
pub const DEFAULT_MCP_RESULT_SIZE_CHARS: usize = 50_000;
/// Directory of the saved results under the engine's spill directory.
pub const SPILLOVER_SUBDIR: &str = "spillover";

/// Characters per token when the budget is scaled to a window.
const CHARS_PER_TOKEN: u128 = 4;
/// Percent of a window's characters one result may take.
const PER_RESULT_WINDOW_PERCENT: u128 = 15;
/// Percent of a window's characters the results of one step may take.
const PER_TURN_WINDOW_PERCENT: u128 = 30;
/// Least threshold of one result in a scaled budget.
const MIN_RESULT_SIZE_CHARS: usize = 8_000;
/// Least step budget in a scaled budget.
const MIN_TURN_BUDGET_CHARS: usize = 16_000;
/// Age past which a saved result is removed.
const SPILLOVER_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
const PERSISTED_OUTPUT_TAG: &str = "<persisted-output>";
const PERSISTED_OUTPUT_CLOSING_TAG: &str = "</persisted-output>";
const MAX_RESULT_FILENAME_STEM: usize = 120;

/// Set once the first saved result of the process pruned the directory.
static PRUNED: AtomicBool = AtomicBool::new(false);

/// The thresholds of saving results to files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Threshold of one result of a tool that is not an MCP tool.
    pub result_size: usize,
    /// Threshold of all the results of one step.
    pub turn_budget: usize,
    /// Most characters of a preview.
    pub preview_size: usize,
    /// Threshold of one result of an MCP tool, at most `result_size`.
    pub mcp_result_size: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            result_size: DEFAULT_RESULT_SIZE_CHARS,
            turn_budget: DEFAULT_TURN_BUDGET_CHARS,
            preview_size: DEFAULT_PREVIEW_SIZE_CHARS,
            mcp_result_size: DEFAULT_MCP_RESULT_SIZE_CHARS,
        }
    }
}

impl Budget {
    /// The budget of `config`: `engine.spill_result_chars`, `engine.spill_mcp_result_chars`,
    /// `engine.spill_step_chars` and `engine.spill_preview_chars`, each overridden by its
    /// environment variable as `env` reads it, else its default.
    ///
    /// # Errors
    /// [`config::Error::NotPositive`] for a value that is not a positive integer.
    pub fn from_settings(
        config: &Config,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Self, config::Error> {
        let positive =
            |setting: Setting, default| config.positive(setting, env(setting.env), default);
        Ok(Self {
            result_size: positive(config::SPILL_RESULT_CHARS, DEFAULT_RESULT_SIZE_CHARS)?,
            turn_budget: positive(config::SPILL_STEP_CHARS, DEFAULT_TURN_BUDGET_CHARS)?,
            preview_size: positive(config::SPILL_PREVIEW_CHARS, DEFAULT_PREVIEW_SIZE_CHARS)?,
            mcp_result_size: positive(
                config::SPILL_MCP_RESULT_CHARS,
                DEFAULT_MCP_RESULT_SIZE_CHARS,
            )?,
        })
    }

    /// This budget on a route of `context_length` tokens: unchanged for an unknown window or
    /// one large enough, else the thresholds of one result and of the step cut to their share
    /// of the window, never below the floors nor above this budget's own values.
    #[must_use]
    pub fn for_window(&self, context_length: Option<u64>) -> Self {
        let Some(tokens) = context_length.filter(|&t| t > 0) else {
            return *self;
        };
        let chars = u128::from(tokens) * CHARS_PER_TOKEN;
        let share = |percent: u128, min: usize, max: usize| {
            usize::try_from(chars * percent / 100)
                .unwrap_or(usize::MAX)
                .max(min)
                .min(max)
        };
        Self {
            result_size: share(
                PER_RESULT_WINDOW_PERCENT,
                MIN_RESULT_SIZE_CHARS,
                self.result_size,
            ),
            turn_budget: share(
                PER_TURN_WINDOW_PERCENT,
                MIN_TURN_BUDGET_CHARS,
                self.turn_budget,
            ),
            ..*self
        }
    }

    /// The threshold of one result of the tool `name`; `None` for `read_file`, whose results
    /// are never saved alone, so that reading a saved result back cannot save it again.
    pub fn threshold(&self, name: &str) -> Option<usize> {
        if name == READ_FILE {
            return None;
        }
        if name.starts_with(TOOL_NAME_PREFIX) {
            return Some(self.mcp_result_size.min(self.result_size));
        }
        Some(self.result_size)
    }
}

/// Where and under which thresholds the result of a background job is saved: its turn's.
#[derive(Debug, Clone)]
pub struct JobSpill {
    dir: Option<PathBuf>,
    budget: Budget,
}

impl JobSpill {
    pub(super) fn new(dir: Option<PathBuf>, budget: Budget) -> Self {
        Self { dir, budget }
    }

    /// The result `output` of job `call` within `max` characters: saved to a file and replaced
    /// by a preview with its path past its tool's threshold or `max`, as a step's results are,
    /// then cut to `max` characters with a truncation marker.
    pub fn cap(&self, call: &ToolCall, mut output: Output, max: usize) -> String {
        if let Some(dir) = &self.dir {
            let budget = Budget {
                result_size: self.budget.result_size.min(max),
                mcp_result_size: self.budget.mcp_result_size.min(max),
                ..self.budget
            };
            persist_step(
                dir,
                &budget,
                std::slice::from_ref(call),
                std::slice::from_mut(&mut output),
            );
        }
        let mut content = output.content;
        if content.chars().count() > max {
            content.truncate(char_byte(&content, max));
            content.push_str(super::TRUNCATION_MARKER);
        }
        content
    }
}

/// Saves the results of one step that are too large to files of `dir` and replaces them in
/// `outputs` by their previews: first each result over the threshold of its call's tool, then,
/// while the results together pass the step budget, the largest result not saved yet. One flag
/// per output tells whether it was replaced.
pub(crate) fn persist_step(
    dir: &Path,
    budget: &Budget,
    calls: &[ToolCall],
    outputs: &mut [Output],
) -> Vec<bool> {
    let mut replaced = vec![false; outputs.len()];
    for ((call, output), replaced) in calls.iter().zip(outputs.iter_mut()).zip(&mut replaced) {
        let Some(threshold) = budget.threshold(&call.name) else {
            continue;
        };
        if let Some(content) = persist_framed(dir, &output.content, &call.id, threshold, budget) {
            output.content = content;
            *replaced = true;
        }
    }
    enforce_turn_budget(dir, budget, calls, outputs, &mut replaced);
    replaced
}

/// The step budget: the largest results not saved yet go to files, largest first, until the
/// results together are within `budget.turn_budget` characters.
fn enforce_turn_budget(
    dir: &Path,
    budget: &Budget,
    calls: &[ToolCall],
    outputs: &mut [Output],
    replaced: &mut [bool],
) {
    let sizes: Vec<usize> = outputs.iter().map(|o| o.content.chars().count()).collect();
    let mut total: usize = sizes.iter().sum();
    if total <= budget.turn_budget {
        return;
    }
    let mut candidates: Vec<usize> = (0..outputs.len())
        .filter(|&i| !outputs[i].content.contains(PERSISTED_OUTPUT_TAG))
        .collect();
    crate::sort::sort_by(&mut candidates, |&a, &b| sizes[b].cmp(&sizes[a]));
    for i in candidates {
        if total <= budget.turn_budget {
            break;
        }
        let id = calls
            .get(i)
            .map_or_else(|| format!("budget_{i}"), |call| call.id.clone());
        if let Some(content) = maybe_persist(dir, &outputs[i].content, &id, 0, budget) {
            total = total - sizes[i] + content.chars().count();
            outputs[i].content = content;
            replaced[i] = true;
        }
    }
}

/// [`maybe_persist`] of a result framed as untrusted data: the file keeps the data, the frame
/// goes around the preview. A provider-side elision notice after the data stays after the
/// preview.
fn persist_framed(
    dir: &Path,
    content: &str,
    id: &str,
    threshold: usize,
    budget: &Budget,
) -> Option<String> {
    let Some((source, data)) = untrusted_parts(content) else {
        return maybe_persist(dir, content, id, threshold, budget);
    };
    let (data, notice) = match data.strip_suffix(ELISION_NOTICE) {
        Some(data) => (data, ELISION_NOTICE),
        None => (data, ""),
    };
    let persisted = maybe_persist(dir, data, id, threshold, budget)?;
    Some(untrusted(source, Output::ok(format!("{persisted}{notice}"))).content)
}

/// `content` saved to a file of `dir` named after the call `id` and the text the model gets
/// instead, when it is longer than `threshold` characters; when the file cannot be written, the
/// preview with a truncation note.
fn maybe_persist(
    dir: &Path,
    content: &str,
    id: &str,
    threshold: usize,
    budget: &Budget,
) -> Option<String> {
    let size = content.chars().count();
    if size <= threshold {
        return None;
    }
    let (preview, has_more) = generate_preview(content, budget.preview_size);
    let path = dir.join(safe_result_filename(id));
    match write_spillover(dir, &path, content, &PRUNED, SystemTime::now()) {
        Ok(()) => Some(persisted_message(preview, has_more, size, &path)),
        Err(e) => {
            eprintln!(
                "ulei-engine: could not save a large tool result to {}: {e}",
                path.display()
            );
            Some(format!(
                "{preview}\n\n[Truncated: tool response was {} chars. Full output could not be \
                 saved to sandbox.]",
                thousands(size)
            ))
        }
    }
}

/// Writes `content` to `path` in `dir` as a file of mode 0600, `dir` a private directory; the
/// first file while `pruned` is unset sets it and removes the files of `dir` older than a day
/// at `now`.
fn write_spillover(
    dir: &Path,
    path: &Path,
    content: &str,
    pruned: &AtomicBool,
    now: SystemTime,
) -> std::io::Result<()> {
    super::terminal::ensure_private_dir(dir)?;
    write_exclusive(path, content, 0o600)?;
    if !pruned.swap(true, Ordering::Relaxed) {
        remove_stale(dir, "", ".txt", SPILLOVER_MAX_AGE, now);
    }
    Ok(())
}

/// The first `max_chars` characters of `content`, cut after its last newline when that keeps
/// more than half of them, and whether anything was cut.
pub(crate) fn generate_preview(content: &str, max_chars: usize) -> (&str, bool) {
    if content.chars().count() <= max_chars {
        return (content, false);
    }
    let truncated = &content[..char_byte(content, max_chars)];
    if let Some(last_nl) = truncated.rfind('\n')
        && truncated[..last_nl].chars().count() > max_chars / 2
    {
        return (&truncated[..=last_nl], true);
    }
    (truncated, true)
}

/// One safe file name for the result of the call `id`: unsafe runs become `_`, and a name that
/// changed or is too long gets a digest of the id.
pub(crate) fn safe_result_filename(id: &str) -> String {
    let raw = if id.is_empty() { "tool_result" } else { id };
    let mut stem = String::with_capacity(raw.len());
    let mut in_unsafe = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            stem.push(c);
            in_unsafe = false;
        } else if !in_unsafe {
            stem.push('_');
            in_unsafe = true;
        }
    }
    let trim = |s: &str| s.trim_matches(['.', '_', '-']).to_owned();
    let mut stem = trim(&stem);
    let mut changed = stem != raw;
    if stem.is_empty() {
        stem = "tool_result".into();
        changed = true;
    }
    if changed || stem.chars().count() > MAX_RESULT_FILENAME_STEM {
        let digest = hex(&Sha256::digest(raw.as_bytes()));
        let cut: String = stem.chars().take(MAX_RESULT_FILENAME_STEM).collect();
        let cut = cut.trim_end_matches(['.', '_', '-']);
        let cut = if cut.is_empty() { "tool_result" } else { cut };
        stem = format!("{cut}_{}", &digest[..12]);
    }
    format!("{stem}.txt")
}

/// The `<persisted-output>` block that stands for a saved result of `size` characters.
fn persisted_message(preview: &str, has_more: bool, size: usize, path: &Path) -> String {
    #[allow(clippy::cast_precision_loss)]
    let size_kb = size as f64 / 1024.0;
    let size_str = if size_kb >= 1024.0 {
        format!("{:.1} MB", size_kb / 1024.0)
    } else {
        format!("{size_kb:.1} KB")
    };
    let more = if has_more { "\n..." } else { "" };
    format!(
        "{PERSISTED_OUTPUT_TAG}\nThis tool result was too large ({} characters, {size_str}).\n\
         Full output saved to: {}\n\
         Use the read_file tool with offset and limit to access specific sections of this \
         output.\n\
         Recovery: page through the saved file with read_file (offset/limit) or process it \
         with terminal \u{2014} do NOT re-request the same data from the remote API; the full \
         result is already on disk.\n\n\
         Preview (first {} chars):\n{preview}{more}\n{PERSISTED_OUTPUT_CLOSING_TAG}",
        thousands(size),
        path.display(),
        preview.chars().count(),
    )
}

/// The spill directory of saved results under the engine's spill directory `spill_dir`.
pub(crate) fn spillover_dir(spill_dir: &Path) -> PathBuf {
    spill_dir.join(SPILLOVER_SUBDIR)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use serde_json::Map;

    use super::*;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            input: Map::new(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ulei-spill-{name}-{}-{}",
            std::process::id(),
            super::super::random_hex(4).unwrap()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_budget_scales_to_small_windows_only() {
        let defaults = Budget::default();
        assert_eq!(defaults.for_window(None), defaults);
        assert_eq!(defaults.for_window(Some(1_000_000)), defaults);
        let small = defaults.for_window(Some(65_536));
        assert_eq!(small.result_size, 39_321);
        assert_eq!(small.turn_budget, 78_643);
        assert_eq!(small.threshold("mcp__hive__health"), Some(39_321));
        let tiny = defaults.for_window(Some(1_000));
        assert_eq!((tiny.result_size, tiny.turn_budget), (8_000, 16_000));
    }

    #[test]
    fn a_configured_budget_is_the_ceiling_a_window_cuts() {
        let configured = Budget {
            result_size: 30_000,
            turn_budget: 60_000,
            preview_size: 500,
            mcp_result_size: 20_000,
        };
        assert_eq!(configured.for_window(None), configured);
        // A window whose shares pass the configured values keeps them.
        assert_eq!(configured.for_window(Some(1_000_000)), configured);
        let small = configured.for_window(Some(32_768));
        assert_eq!((small.result_size, small.turn_budget), (19_660, 39_321));
        assert_eq!((small.preview_size, small.mcp_result_size), (500, 20_000));
        assert_eq!(small.threshold("mcp__s__t"), Some(19_660));
        // Values configured below the floors stay as configured.
        let low = Budget {
            result_size: 1_000,
            turn_budget: 2_000,
            ..configured
        };
        let cut = low.for_window(Some(1_000));
        assert_eq!((cut.result_size, cut.turn_budget), (1_000, 2_000));
    }

    #[test]
    fn the_first_saved_result_removes_saved_results_older_than_a_day() {
        let dir = scratch("stale");
        let now = SystemTime::now();
        let aged = |name: &str, age: Duration| {
            let path = dir.join(name);
            fs::write(&path, "old").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now.checked_sub(age).unwrap())
                .unwrap();
            path
        };
        let stale = aged("stale.txt", Duration::from_secs(25 * 3600));
        let fresh = aged("fresh.txt", Duration::from_secs(23 * 3600));
        let other = aged("notes.log", Duration::from_secs(48 * 3600));
        assert_eq!(SPILLOVER_MAX_AGE, Duration::from_secs(24 * 3600));
        let pruned = AtomicBool::new(false);

        write_spillover(&dir, &dir.join("first.txt"), "first", &pruned, now).unwrap();
        assert!(!stale.exists(), "a result older than a day was kept");
        assert!(fresh.exists(), "a result younger than a day was removed");
        assert!(
            other.exists(),
            "a file that is not a saved result was removed"
        );
        assert_eq!(fs::read_to_string(dir.join("first.txt")).unwrap(), "first");
        assert!(pruned.load(Ordering::Relaxed));

        // Only the first saved result of the process prunes.
        let later = aged("later.txt", Duration::from_secs(48 * 3600));
        write_spillover(&dir, &dir.join("second.txt"), "second", &pruned, now).unwrap();
        assert!(later.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn thresholds_by_tool() {
        let budget = Budget::default();
        assert_eq!(budget.threshold("mcp__hive__hive_health"), Some(50_000));
        assert_eq!(budget.threshold("archive_read"), Some(100_000));
        assert_eq!(budget.threshold("read_file"), None);
    }

    #[test]
    fn a_preview_ends_at_a_late_newline() {
        assert_eq!(generate_preview("short", 10), ("short", false));
        assert_eq!(generate_preview("abcdefgh\nijkl", 10), ("abcdefgh\n", true));
        assert_eq!(
            generate_preview("ab\ncdefghijkl", 10),
            ("ab\ncdefghi", true)
        );
        assert_eq!(generate_preview("ééééé", 3), ("ééé", true));
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(safe_result_filename("toolu_01Abc"), "toolu_01Abc.txt");
        let odd = safe_result_filename("../x y");
        assert!(odd.starts_with("x_y_"), "{odd}");
        assert_eq!(Path::new(&odd).extension().unwrap(), "txt");
        assert_eq!(odd.len(), "x_y_".len() + 12 + ".txt".len());
        assert_eq!(safe_result_filename(""), "tool_result.txt");
        let long = safe_result_filename(&"a".repeat(200));
        assert_eq!(long.len(), 120 + 1 + 12 + 4);
    }

    #[test]
    fn a_large_mcp_result_goes_to_a_private_file_with_a_preview() {
        let dir = scratch("mcp");
        let spill = dir.join(SPILLOVER_SUBDIR);
        let data = format!("{{\"result\": \"{}\"}}", "x".repeat(60_000));
        let framed = untrusted("mcp__hive__hive_health", Output::ok(data.clone()));
        let mut outputs = vec![framed, Output::ok("small")];
        let calls = [call("c1", "mcp__hive__hive_health"), call("c2", "memory")];
        let replaced = persist_step(&spill, &Budget::default(), &calls, &mut outputs);

        assert_eq!(replaced, [true, false]);
        let path = spill.join("c1.txt");
        assert_eq!(fs::read_to_string(&path).unwrap(), data);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&spill).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let content = &outputs[0].content;
        assert!(
            content.starts_with("<untrusted_tool_result source=\"mcp__hive__hive_health\">"),
            "{content}"
        );
        assert!(content.contains(
            "<persisted-output>\nThis tool result was too large (60,014 characters, 58.6 KB).\n"
        ));
        assert!(content.contains(&format!("Full output saved to: {}\n", path.display())));
        assert!(content.contains("Preview (first 1500 chars):\n{\"result\": \"xxx"));
        assert!(content.ends_with("\n...\n</persisted-output>\n</untrusted_tool_result>"));
        assert!(content.chars().count() < 3_000, "{}", content.len());
        assert_eq!(outputs[1].content, "small");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn results_under_their_threshold_and_read_file_stay() {
        let dir = scratch("under");
        let mut outputs = vec![
            Output::ok("y".repeat(50_000)),
            Output::ok("z".repeat(100_000)),
            Output::ok("w".repeat(40_000)),
        ];
        let calls = [
            call("a", "mcp__s__t"),
            call("b", "read_file"),
            call("c", "archive_read"),
        ];
        // 190,000 characters together: within the step budget.
        let replaced = persist_step(&dir, &Budget::default(), &calls, &mut outputs);
        assert_eq!(replaced, [false, false, false]);
        assert!(!dir.join("a.txt").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_step_budget_saves_the_largest_results_first() {
        let dir = scratch("turn");
        // The four results of turn 3 of a measured session, 256,793 characters together: the
        // MCP result goes to a file alone, and the rest then fits the step budget.
        let sizes = [100_869, 63_520, 47_879, 44_525];
        let mut outputs: Vec<Output> = sizes.iter().map(|&n| Output::ok("q".repeat(n))).collect();
        let calls = [
            call("r1", "read_file"),
            call("r2", "mcp__hive__hive_health"),
            call("r3", "read_file"),
            call("r4", "read_file"),
        ];
        let replaced = persist_step(&dir, &Budget::default(), &calls, &mut outputs);
        assert_eq!(replaced, [false, true, false, false]);
        let total: usize = outputs.iter().map(|o| o.content.chars().count()).sum();
        assert!(total <= DEFAULT_TURN_BUDGET_CHARS, "{total}");
        assert_eq!(
            fs::read_to_string(dir.join("r2.txt")).unwrap().len(),
            sizes[1]
        );

        // Reads count toward the step budget even though no read is saved alone.
        let mut reads: Vec<Output> = (0..3).map(|_| Output::ok("r".repeat(90_000))).collect();
        let calls = [
            call("a", "read_file"),
            call("b", "read_file"),
            call("c", "read_file"),
        ];
        let replaced = persist_step(&dir, &Budget::default(), &calls, &mut reads);
        assert_eq!(replaced, [true, false, false]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unwritable_directory_leaves_a_truncated_preview() {
        let dir = scratch("ro");
        let blocker = dir.join("file");
        fs::write(&blocker, "").unwrap();
        let mut outputs = vec![Output::ok("v".repeat(120_000))];
        let replaced = persist_step(
            &blocker.join("sub"),
            &Budget::default(),
            &[call("c", "terminal")],
            &mut outputs,
        );
        assert_eq!(replaced, [true]);
        assert!(outputs[0].content.ends_with(
            "\n\n[Truncated: tool response was 120,000 chars. Full output could not be saved to \
             sandbox.]"
        ));
        assert_eq!(outputs[0].content.chars().count(), 1_500 + 90);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_kept_elision_notice_follows_the_preview() {
        let dir = scratch("elision");
        let data = format!("{}{ELISION_NOTICE}", "e".repeat(55_000));
        let framed = untrusted("mcp__s__t", Output::ok(data));
        let mut outputs = vec![framed];
        persist_step(
            &dir,
            &Budget::default(),
            &[call("n", "mcp__s__t")],
            &mut outputs,
        );
        assert_eq!(
            fs::read_to_string(dir.join("n.txt")).unwrap(),
            "e".repeat(55_000)
        );
        assert!(outputs[0].content.ends_with(&format!(
            "</persisted-output>{ELISION_NOTICE}\n</untrusted_tool_result>"
        )));
        fs::remove_dir_all(dir).unwrap();
    }
}
