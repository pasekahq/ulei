// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `read_file`: numbered lines of a text file in pages, behind the device, special-file,
//! binary and secret-file guards, cut to a character budget, with the dedup and repeat guards
//! of the session's read tracker.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _};
use std::os::unix::fs::FileTypeExt as _;
use std::path::Path;
use std::rc::Rc;
use std::time::SystemTime;

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::difflib;
use super::redact::{Flags, Redactor};
use super::safety::read_block_error;
use super::tracker::{Call, Region, Tracker};
use super::{
    Args, Context, Fields, Output, READ_FILE, Tool, basename, expand_home, file_name, json_array,
    json_object, json_str, normalize, object_schema, py_int, realpath, resolve, thousands,
};

/// Most lines one call returns.
pub const MAX_LINES: usize = 2000;
/// Most characters of a line shown; a longer line ends with `... [truncated]`.
pub const MAX_LINE_LENGTH: usize = 2000;
/// Most characters of numbered text one call returns; the rest is offered from `next_offset`.
pub const MAX_READ_CHARS: usize = 100_000;
/// The answer to a repeated read of a region that has not changed.
pub const DEDUP_STATUS_MESSAGE: &str = "File unchanged since last read. The content from the \
earlier read_file result in this conversation is still current \u{2014} refer to that instead \
of re-reading.";

const DEFAULT_LIMIT: usize = 2000;
/// From this size on, a truncated read wider than [`NARROW_LIMIT`] lines carries a hint.
const LARGE_FILE_HINT_BYTES: u64 = 512_000;
const NARROW_LIMIT: usize = 200;
/// Bytes of a line kept before decoding: any line longer than [`MAX_LINE_LENGTH`] characters
/// still decodes to more, so the character cut and its marker apply.
const LINE_CLAMP_BYTES: usize = 4 * MAX_LINE_LENGTH + 1;
const SAMPLE_BYTES: u64 = 1000;
const UTF16_MAX_BYTES: u64 = 10 * 1024 * 1024;
const UTF16_SAMPLE_BYTES: usize = 512;
/// Directory entries the not-found suggestions look at.
const SUGGEST_SCAN: usize = 50;
const SUGGESTIONS: usize = 5;

const DESCRIPTION: &str = "Read a text file with line numbers and pagination. Use this \
instead of cat/head/tail in terminal. Output format: 'LINE_NUM|CONTENT'. Suggests similar \
filenames if not found. Use offset and limit for large files. Reads exceeding ~100K characters \
are truncated on a line boundary and return a next_offset; continue with offset to read the \
rest. Jupyter notebooks (.ipynb), Word documents (.docx), and Excel workbooks (.xlsx) are \
auto-extracted to readable text; PDF, legacy Office (.doc/.ppt/.xls), OpenDocument, RTF, and \
EPUB convert too when the optional anydoc converter is available (auto-installed on first use \
where installs are permitted). PDF conversion reads the text layer only: scanned/image pages \
yield no text, and when many pages come back empty the output ends with an EXTRACTION COVERAGE \
WARNING listing the affected pages \u{2014} follow its instructions (render pages with pdftoppm \
and inspect via vision_analyze, or OCR) instead of treating the extraction as complete. NOTE: \
Cannot read images or other binary files \u{2014} use vision_analyze for images.";

const BLOCKED_DEVICE_PATHS: [&str; 12] = [
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/full",
    "/dev/stdin",
    "/dev/tty",
    "/dev/console",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/fd/0",
    "/dev/fd/1",
    "/dev/fd/2",
];
/// Endings of `/proc` paths that alias stdio or leak process secrets and memory layout.
const BLOCKED_PROC_SUFFIXES: [&str; 12] = [
    "/fd/0",
    "/fd/1",
    "/fd/2",
    "/environ",
    "/cmdline",
    "/maps",
    "/smaps",
    "/smaps_rollup",
    "/numa_maps",
    "/mem",
    "/auxv",
    "/pagemap",
];

#[rustfmt::skip]
pub(super) const BINARY_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".webp", ".tiff", ".tif",
    ".mp4", ".mov", ".avi", ".mkv", ".webm", ".wmv", ".flv", ".m4v", ".mpeg", ".mpg",
    ".mp3", ".wav", ".ogg", ".flac", ".aac", ".m4a", ".wma", ".aiff", ".opus",
    ".zip", ".tar", ".gz", ".bz2", ".7z", ".rar", ".xz", ".z", ".tgz", ".iso",
    ".exe", ".dll", ".so", ".dylib", ".bin", ".o", ".a", ".obj", ".lib",
    ".app", ".msi", ".deb", ".rpm",
    ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx",
    ".odt", ".ods", ".odp",
    ".ttf", ".otf", ".woff", ".woff2", ".eot",
    ".pyc", ".pyo", ".class", ".jar", ".war", ".ear", ".node", ".wasm", ".rlib",
    ".sqlite", ".sqlite3", ".db", ".mdb", ".idx",
    ".psd", ".ai", ".eps", ".sketch", ".fig", ".xd", ".blend", ".3ds", ".max",
    ".swf", ".fla",
    ".lockb", ".dat", ".data",
];

/// Magic prefixes and the names a binary refusal gives them, first match wins.
const MAGIC: [(&[u8], &str); 24] = [
    (b"\x89PNG\r\n\x1a\n", "PNG image data"),
    (b"\xff\xd8\xff", "JPEG image data"),
    (b"GIF87a", "GIF image data"),
    (b"GIF89a", "GIF image data"),
    (b"RIFF", "RIFF container (WAV/AVI/WebP family)"),
    (b"%PDF-", "PDF document"),
    (b"PK\x03\x04", "ZIP archive (also docx/xlsx/jar/apk)"),
    (b"PK\x05\x06", "ZIP archive (empty)"),
    (b"\x1f\x8b", "gzip compressed data"),
    (b"BZh", "bzip2 compressed data"),
    (b"\xfd7zXZ\x00", "xz compressed data"),
    (b"7z\xbc\xaf\x27\x1c", "7-Zip archive"),
    (b"\x7fELF", "ELF executable"),
    (b"MZ", "Windows PE executable"),
    (b"\xcf\xfa\xed\xfe", "Mach-O executable (64-bit)"),
    (b"\xca\xfe\xba\xbe", "Mach-O universal binary / Java class"),
    (b"SQLite format 3\x00", "SQLite database"),
    (b"OggS", "Ogg container"),
    (b"fLaC", "FLAC audio"),
    (b"ID3", "MP3 audio (ID3 tag)"),
    (b"\x00\x00\x00", ISO_MEDIA),
    (b"BM", "BMP image data"),
    (b"II*\x00", "TIFF image data (little-endian)"),
    (b"MM\x00*", "TIFF image data (big-endian)"),
];
/// Named only when `ftyp` follows the size field.
const ISO_MEDIA: &str = "ISO media container (MP4/MOV family)";

/// `read_file`; `redactor` masks secrets in the text it returns, `tracker` holds the session's
/// dedup stamps and repeat counter.
pub(super) struct ReadFile {
    pub(super) redactor: Redactor,
    pub(super) tracker: Rc<RefCell<Tracker>>,
}

impl Tool for ReadFile {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: READ_FILE.into(),
            description: DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read (absolute, relative, or ~/path)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-indexed, default: 1)",
                        "default": 1,
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read (default: 2000, max: \
                                        2000). Reads are additionally capped at a \
                                        ~100K-character budget with a next_offset continuation.",
                        "default": DEFAULT_LIMIT,
                        "maximum": MAX_LINES
                    }
                }),
                &["path"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let args = Args(input);
        let path = match args.str("path") {
            Ok("") => return Output::error("field `path` is empty"),
            Ok(path) => path,
            Err(e) => return Output::error(e),
        };
        let offset = py_int(args.present("offset")).map_or(1, |n| to_usize(n.max(1)));
        let limit = py_int(args.present("limit"))
            .map_or(DEFAULT_LIMIT, |n| to_usize(n.max(1)))
            .min(MAX_LINES);
        if blocked_device(path, ctx.cwd) {
            return Output::error(format!(
                "Cannot read '{path}': this is a device file that would block or produce \
                 infinite output."
            ));
        }
        let target = resolve(ctx.cwd, path);
        let resolved = realpath(&target);
        if let Some(kind) = special_file_kind(&resolved) {
            let note = format!(
                "'{path}' is {kind}, not a regular file \u{2014} reading it would block \
                 indefinitely, so no read was attempted. Use terminal utilities if you need to \
                 interact with it."
            );
            return Output {
                content: json_object([("success", "false".into()), ("note", json_ascii(&note))]),
                is_error: true,
            };
        }
        if has_binary_extension(&resolved.to_string_lossy()) {
            return Output::error(format!(
                "Cannot read binary file '{path}' ({}). Use vision_analyze for images, or \
                 terminal to inspect binary files.",
                suffix(&resolved)
            ));
        }
        if let Some(error) = read_block_error(&resolved) {
            return Output::error(error);
        }

        let region: Region = (resolved, offset, limit);
        let mut tracker = self.tracker.borrow_mut();
        if let Some(answer) = unchanged(&mut tracker, &region, path) {
            return answer;
        }

        let result = read_text(&expand_home(path), &target, ctx.cwd, offset, limit);
        let is_error = result.error.is_some();
        let mut fields = self.render(result, offset, limit);

        tracker.read_in_full(&region);
        let count = tracker.repeat(Call::Read {
            path: path.to_owned(),
            offset,
            limit,
        });
        if let Some(mtime) = modified(&region.0) {
            tracker.record(region, mtime);
        }
        if count >= 4 {
            return Output::error_with(
                format!(
                    "BLOCKED: You have read this exact file region {count} times in a row. The \
                     content has NOT changed. You already have this information. STOP re-reading \
                     and proceed with your task."
                ),
                [
                    ("path", json_str(path)),
                    ("already_read", count.to_string()),
                ],
            );
        }
        if count >= 3 {
            fields.set(
                "_warning",
                json_str(&format!(
                    "You have read this exact file region {count} times consecutively. The \
                     content has not changed since your last read. Use the information you \
                     already have. If you are stuck in a loop, stop reading and proceed with \
                     writing or responding."
                )),
            );
        }
        Output {
            content: json_object(fields.0),
            is_error,
        }
    }
}

/// The answer to a read of `region` when it has not changed since it was last returned: the
/// dedup status, or BLOCKED from the third stub on.
fn unchanged(tracker: &mut Tracker, region: &Region, path: &str) -> Option<Output> {
    let stamp = tracker.stamp(region)?;
    if modified(&region.0) != Some(stamp) {
        return None;
    }
    let hits = tracker.stub_hit(region);
    if hits >= 2 {
        return Some(Output::error_with(
            format!(
                "BLOCKED: You have called read_file on this exact region {} times and the file \
                 has NOT changed. STOP calling read_file for this path \u{2014} the content from \
                 your earlier read_file result in this conversation is still current. Proceed \
                 with your task using the information you already have.",
                hits + 1
            ),
            [
                ("path", json_str(path)),
                ("already_read", (hits + 1).to_string()),
            ],
        ));
    }
    Some(Output::ok(json_object([
        ("status", json_str("unchanged")),
        ("message", json_str(DEDUP_STATUS_MESSAGE)),
        ("path", json_str(path)),
        ("dedup", "true".into()),
        ("content_returned", "false".into()),
    ])))
}

impl ReadFile {
    /// The result fields: the read cut to [`MAX_READ_CHARS`] with its continuation, the
    /// content redacted, and the hint for a wide read of a large file.
    fn render(&self, mut result: ReadResult, offset: usize, limit: usize) -> Fields {
        let mut budget_hint = None;
        if result.content.chars().count() > MAX_READ_CHARS {
            let (trimmed, kept) = truncate_to_char_budget(&result.content, MAX_READ_CHARS);
            let next = offset + kept;
            let mut hint = format!(
                "Output truncated at the {}-char read budget after {kept} line(s) (showing lines \
                 {offset}-{} of {}). Use offset={next} to continue.",
                thousands(MAX_READ_CHARS),
                next - 1,
                result.total_lines
            );
            if trimmed.split('\n').next().unwrap_or("").chars().count() >= MAX_READ_CHARS {
                hint.push_str(
                    " Note: the first line alone exceeded the budget and was clamped mid-line; \
                     its remainder is not retrievable via offset.",
                );
            }
            result.content = trimmed;
            budget_hint = Some((next, hint));
        }
        let file_read = Flags {
            file_read: true,
            ..Flags::default()
        };
        result.content = self
            .redactor
            .sensitive_text(&result.content, file_read)
            .into_owned();
        let file_size = result.file_size;
        let mut fields = result.fields();
        let mut truncated = result.truncated;
        if let Some((next, hint)) = budget_hint {
            truncated = true;
            fields.set("truncated", "true".into());
            fields.set("truncated_by", json_str("bytes"));
            fields.set("next_offset", next.to_string());
            fields.set("hint", json_str(&hint));
        }
        if file_size > LARGE_FILE_HINT_BYTES && limit > NARROW_LIMIT && truncated {
            fields.set(
                "_hint",
                json_str(&format!(
                    "This file is large ({} bytes). Consider reading only the section you need \
                     with offset and limit to keep context usage efficient.",
                    thousands(usize::try_from(file_size).unwrap_or(usize::MAX))
                )),
            );
        }
        fields
    }
}

/// What a read found, as its result fields show it.
#[derive(Debug, Default)]
struct ReadResult {
    content: String,
    total_lines: usize,
    file_size: u64,
    truncated: bool,
    hint: Option<String>,
    is_binary: bool,
    error: Option<String>,
    similar_files: Vec<String>,
}

impl ReadResult {
    fn error(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }

    fn fields(&self) -> Fields {
        let mut fields = vec![
            ("content", json_str(&self.content)),
            ("total_lines", self.total_lines.to_string()),
            ("file_size", self.file_size.to_string()),
            ("truncated", self.truncated.to_string()),
        ];
        if let Some(hint) = &self.hint {
            fields.push(("hint", json_str(hint)));
        }
        fields.push(("is_binary", self.is_binary.to_string()));
        fields.push(("is_image", "false".into()));
        if let Some(error) = &self.error {
            fields.push(("error", json_str(error)));
        }
        if !self.similar_files.is_empty() {
            let similar = self.similar_files.iter().map(|f| json_str(f));
            fields.push(("similar_files", json_array(similar)));
        }
        Fields(fields)
    }
}

/// Lines `offset` to `offset + limit - 1` of the file at `target`, numbered, which the model
/// named `shown`.
fn read_text(shown: &str, target: &Path, cwd: &Path, offset: usize, limit: usize) -> ReadResult {
    let Ok(meta) = fs::metadata(target) else {
        return suggest_similar(shown, cwd);
    };
    if !meta.is_file() {
        return ReadResult::error(not_regular(shown));
    }
    let Ok(mut file) = File::open(target) else {
        return suggest_similar(shown, cwd);
    };
    let file_size = meta.len();
    let failed = |e: io::Error| ReadResult::error(format!("Failed to read file: {e}"));
    let mut sample = Vec::new();
    if let Err(e) = (&mut file).take(SAMPLE_BYTES).read_to_end(&mut sample) {
        return failed(e);
    }
    let ext = splitext(basename(shown)).1.to_lowercase();
    if BINARY_EXTENSIONS.contains(&ext.as_str()) || likely_binary(&sample) {
        if let Some(result) = read_utf16(target, &ext, offset, limit, file_size) {
            return result;
        }
        return ReadResult {
            is_binary: true,
            file_size,
            error: Some(describe_binary(&sample, file_size)),
            ..ReadResult::default()
        };
    }
    let end_line = offset.saturating_add(limit - 1);
    let page = match file.rewind().and_then(|()| page(file, offset, end_line)) {
        Ok(page) => page,
        Err(e) => return failed(e),
    };
    let mut text = String::from_utf8_lossy(&page.bytes).into_owned();
    if offset == 1
        && let Some(rest) = text.strip_prefix('\u{feff}')
    {
        text = rest.to_owned();
    }
    let total_lines = page.newlines;
    let truncated = total_lines > end_line;
    if !truncated && text.ends_with('\n') && page.last_byte != Some(b'\n') {
        text.pop();
    }
    if file_size == 0 {
        return ReadResult {
            hint: Some("File is empty (0 bytes).".into()),
            ..ReadResult::default()
        };
    }
    if offset > total_lines && total_lines > 0 {
        return ReadResult {
            total_lines,
            file_size,
            hint: Some(format!(
                "Note: offset {offset} is beyond the end of the file ({total_lines} lines total). \
                 Retry with offset <= {total_lines}."
            )),
            ..ReadResult::default()
        };
    }
    ReadResult {
        content: number_lines(&text, offset),
        total_lines,
        file_size,
        truncated,
        hint: truncated.then(|| {
            format!(
                "Use offset={} to continue reading (showing {offset}-{end_line} of {total_lines} \
                 lines)",
                end_line + 1
            )
        }),
        ..ReadResult::default()
    }
}

/// The lines of a page, each cut to [`LINE_CLAMP_BYTES`] and newline-terminated, and what the
/// whole file holds.
struct Page {
    bytes: Vec<u8>,
    /// Newlines in the whole file.
    newlines: usize,
    last_byte: Option<u8>,
}

/// Reads the whole file once, keeping lines `offset..=end_line`.
fn page(file: File, offset: usize, end_line: usize) -> io::Result<Page> {
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut page = Page {
        bytes: Vec::new(),
        newlines: 0,
        last_byte: None,
    };
    let mut line = 1;
    let mut kept = 0;
    let mut open_line = false;
    loop {
        let buf = reader.fill_buf()?;
        let Some(&last) = buf.last() else {
            break;
        };
        page.last_byte = Some(last);
        let len = buf.len();
        let mut rest = buf;
        while !rest.is_empty() {
            let newline = rest.iter().position(|&b| b == b'\n');
            let piece = &rest[..newline.unwrap_or(rest.len())];
            let wanted = (offset..=end_line).contains(&line);
            if wanted {
                let take = piece.len().min(LINE_CLAMP_BYTES - kept);
                page.bytes.extend_from_slice(&piece[..take]);
                kept += take;
            }
            if let Some(at) = newline {
                if wanted {
                    page.bytes.push(b'\n');
                }
                page.newlines += 1;
                line += 1;
                kept = 0;
                open_line = false;
                rest = &rest[at + 1..];
            } else {
                open_line = true;
                rest = &[];
            }
        }
        reader.consume(len);
    }
    if open_line && (offset..=end_line).contains(&line) {
        page.bytes.push(b'\n');
    }
    Ok(page)
}

/// `content` with each line prefixed `<number>|`, lines past [`MAX_LINE_LENGTH`] characters
/// cut with `... [truncated]`.
fn number_lines(content: &str, start: usize) -> String {
    let mut out = String::with_capacity(content.len());
    for (i, line) in content.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&(start + i).to_string());
        out.push('|');
        match line.char_indices().nth(MAX_LINE_LENGTH) {
            Some((cut, _)) => {
                out.push_str(&line[..cut]);
                out.push_str("... [truncated]");
            }
            None => out.push_str(line),
        }
    }
    out
}

/// The leading whole lines of `content` within `max` characters and how many they are; a
/// first line longer than `max` alone is cut to it.
fn truncate_to_char_budget(content: &str, max: usize) -> (String, usize) {
    let mut kept: Vec<&str> = Vec::new();
    let mut running = 0;
    for line in content.split('\n') {
        let addition = line.chars().count() + usize::from(!kept.is_empty());
        if running + addition > max {
            break;
        }
        kept.push(line);
        running += addition;
    }
    if kept.is_empty() {
        let first = content.split('\n').next().unwrap_or("");
        let cut = first
            .char_indices()
            .nth(max)
            .map_or(first.len(), |(at, _)| at);
        kept.push(&first[..cut]);
    }
    (kept.join("\n"), kept.len())
}

/// A sample is binary when it holds a NUL or is not UTF-8 short of a sequence cut at its end.
pub(super) fn likely_binary(sample: &[u8]) -> bool {
    if sample.contains(&0) {
        return true;
    }
    match std::str::from_utf8(sample) {
        Ok(_) => false,
        Err(e) => e.valid_up_to() + 3 < sample.len(),
    }
}

/// The page of a UTF-16 file, by BOM or by where its zero bytes fall, transcoded to UTF-8.
fn read_utf16(
    target: &Path,
    ext: &str,
    offset: usize,
    limit: usize,
    file_size: u64,
) -> Option<ReadResult> {
    if BINARY_EXTENSIONS.contains(&ext) || file_size > UTF16_MAX_BYTES {
        return None;
    }
    let data = fs::read(target).ok()?;
    let sample = &data[..data.len().min(UTF16_SAMPLE_BYTES)];
    let little = if sample.starts_with(b"\xfe\xff") {
        false
    } else if sample.starts_with(b"\xff\xfe") {
        true
    } else {
        let zeros = |from: usize| {
            sample
                .iter()
                .skip(from)
                .step_by(2)
                .filter(|&&b| b == 0)
                .count()
        };
        match (zeros(0), zeros(1)) {
            (0, odd) if odd >= 2 => true,
            (even, 0) if even >= 2 => false,
            _ => return None,
        }
    };
    let units = data.as_chunks::<2>().0.iter().map(|&pair| {
        if little {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    let mut text: String = char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if data.len() % 2 == 1 {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    let text = text
        .strip_prefix('\u{feff}')
        .unwrap_or(&text)
        .replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let total_lines = lines.len();
    let from = (offset - 1).min(total_lines);
    let to = from.saturating_add(limit).min(total_lines);
    let end_line = offset.saturating_add(limit - 1);
    let truncated = total_lines > end_line;
    let mut hint = format!(
        "Transcoded from {} to UTF-8 for display. Text edits via patch/write_file would \
         re-encode as UTF-8.",
        if little { "UTF-16-LE" } else { "UTF-16-BE" }
    );
    if truncated {
        let _ = write!(
            hint,
            " Use offset={} to continue reading (showing {offset}-{end_line} of {total_lines} \
             lines)",
            end_line + 1
        );
    }
    Some(ReadResult {
        content: number_lines(&lines[from..to].join("\n"), offset),
        total_lines,
        file_size,
        truncated,
        hint: Some(hint),
        ..ReadResult::default()
    })
}

/// The refusal of a path that exists but is not a regular file.
pub(super) fn not_regular(shown: &str) -> String {
    format!(
        "Cannot read '{shown}': not a regular file (directory, FIFO, socket, or device). Reading \
         it could block indefinitely."
    )
}

/// The refusal of a binary file, naming its kind by magic bytes and its size.
pub(super) fn describe_binary(sample: &[u8], file_size: u64) -> String {
    let kind = MAGIC
        .iter()
        .find(|(prefix, name)| {
            sample.starts_with(prefix) && (*name != ISO_MEDIA || sample.get(4..8) == Some(b"ftyp"))
        })
        .map_or("unknown binary", |(_, name)| name);
    #[allow(clippy::cast_precision_loss)]
    let size = if file_size >= 1024 * 1024 {
        format!("{:.1} MB", file_size as f64 / (1024.0 * 1024.0))
    } else if file_size >= 1024 {
        format!("{:.1} KB", file_size as f64 / 1024.0)
    } else {
        format!("{file_size} bytes")
    };
    format!("Binary file ({kind}, {size}) \u{2014} cannot display as text.")
}

/// `File not found: <shown>` with up to five names of its directory that resemble its own,
/// best first.
fn suggest_similar(shown: &str, cwd: &Path) -> ReadResult {
    let dir = match dirname(shown) {
        "" => ".",
        dir => dir,
    };
    let name = basename(shown).to_lowercase();
    let (stem, ext) = splitext(&name);
    let mut entries: Vec<String> = fs::read_dir(resolve(cwd, dir))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    entries.sort_unstable();
    entries.truncate(SUGGEST_SCAN);
    let mut scored: Vec<(u8, String)> = Vec::new();
    for entry in entries {
        let lower = entry.to_lowercase();
        let (entry_stem, entry_ext) = splitext(&lower);
        let mut score = if lower == name {
            100
        } else if entry_stem == stem {
            90
        } else if lower.starts_with(&name) || name.starts_with(&lower) {
            70
        } else if lower.contains(&name) {
            60
        } else if name.contains(&lower) && lower.chars().count() > 2 {
            40
        } else if !ext.is_empty() && entry_ext == ext && shares_chars(&name, &lower) {
            30
        } else {
            0
        };
        if score == 0 && difflib::ratio(&name, &lower) >= 0.8 {
            score = 50;
        }
        if score > 0 {
            let path = if dir.ends_with('/') {
                format!("{dir}{entry}")
            } else {
                format!("{dir}/{entry}")
            };
            scored.push((score, path));
        }
    }
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    ReadResult {
        error: Some(format!("File not found: {shown}")),
        similar_files: scored
            .into_iter()
            .take(SUGGESTIONS)
            .map(|(_, path)| path)
            .collect(),
        ..ReadResult::default()
    }
}

/// The two names share at least 40 % of the longer one's length in distinct characters.
fn shares_chars(a: &str, b: &str) -> bool {
    let a_chars: HashSet<char> = a.chars().collect();
    let common = b
        .chars()
        .collect::<HashSet<char>>()
        .intersection(&a_chars)
        .count();
    let longest = a.chars().count().max(b.chars().count());
    #[allow(clippy::cast_precision_loss)]
    let enough = common as f64 >= longest as f64 * 0.4;
    enough
}

/// The path, lexically or through its symlinks, is a device or `/proc` entry that never ends,
/// waits for input or leaks process secrets.
fn blocked_device(path: &str, cwd: &Path) -> bool {
    let normalized = normalize(&resolve(cwd, path));
    if blocked_device_path(&normalized) {
        return true;
    }
    let mut seen = Vec::new();
    let mut current = normalized.clone();
    for _ in 0..20 {
        let Ok(link) = fs::read_link(&current) else {
            break;
        };
        let target = normalize(&current.parent().unwrap_or(Path::new("/")).join(link));
        if blocked_device_path(&target) {
            return true;
        }
        if seen.contains(&target) {
            break;
        }
        seen.push(target.clone());
        current = target;
    }
    blocked_device_path(&realpath(&normalized))
}

fn blocked_device_path(path: &Path) -> bool {
    let path = path.to_string_lossy();
    BLOCKED_DEVICE_PATHS.contains(&&*path)
        || (path.starts_with("/proc/") && BLOCKED_PROC_SUFFIXES.iter().any(|s| path.ends_with(s)))
}

/// What `resolved` is when it exists and is neither a regular file nor a directory.
fn special_file_kind(resolved: &Path) -> Option<&'static str> {
    let kind = fs::metadata(resolved).ok()?.file_type();
    if kind.is_file() || kind.is_dir() {
        None
    } else if kind.is_fifo() {
        Some("a FIFO (named pipe)")
    } else if kind.is_socket() {
        Some("a socket")
    } else if kind.is_char_device() {
        Some("a character device")
    } else if kind.is_block_device() {
        Some("a block device")
    } else {
        Some("a special (non-regular) file")
    }
}

/// The text from the last `.` of the whole path is a binary file extension.
fn has_binary_extension(path: &str) -> bool {
    path.rfind('.')
        .is_some_and(|dot| BINARY_EXTENSIONS.contains(&path[dot..].to_lowercase().as_str()))
}

/// The extension of the last component, lowercase, as Python's `PurePath.suffix` finds it.
fn suffix(path: &Path) -> String {
    let name = file_name(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() => name[dot..].to_lowercase(),
        _ => String::new(),
    }
}

/// Python's `os.path.splitext` of a name without `/`: leading dots start no extension.
pub(super) fn splitext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if name[..dot].bytes().any(|b| b != b'.') => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    }
}

/// Python's `os.path.dirname`: up to the last `/`, trailing slashes dropped unless all slashes.
fn dirname(path: &str) -> &str {
    let head = &path[..path.rfind('/').map_or(0, |at| at + 1)];
    if head.bytes().all(|b| b == b'/') {
        head
    } else {
        head.trim_end_matches('/')
    }
}

/// A JSON string literal with every character outside ASCII escaped, as Python's default
/// `json.dumps` writes it.
fn json_ascii(text: &str) -> String {
    let mut out = String::new();
    for c in json_str(text).chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn to_usize(n: i64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
