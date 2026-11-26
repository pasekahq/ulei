//! The daemon `ulei-garden serve` (ADR-0047 section 2): its settings from `ulei.toml`, the
//! private socket, and the C7 `x-methods` over `x-transport`, synchronous HTTP/1.1 JSON on a
//! Unix socket, one request per connection, served one connection at a time.

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use ulei_contracts::config::{self, Config, Entry};

use crate::api::{ApiConfig, ApiError, Garden, Hybrid, MAX_SEARCH_K};
use crate::edge::EdgeError;
use crate::embed::ENGINE_FILE;
use crate::embed::check::{self, Verdict};
use crate::embed::pipe::{CALL_DEADLINE, Helper, PASSAGE_DEADLINE, StartError};
use crate::embed::supervisor::Supervisor;
use crate::miss::{self, KeyError, ProducerKey, Queued};
use crate::model::{Domain, Ttl, TtlKind, TtlRule, TtlTable, UnixTime};
use crate::places;
use crate::profile::{DEFAULT_CHAR_LIMIT, Profile};
use crate::rerank;
use crate::second::Provider;
use crate::store::{Store, parse_domains, starting_domains};
use crate::wire::c7::{self, ErrorCode};
use crate::worker::{Clock, Schedule, SystemClock, Wake};

/// Longest socket path in bytes: `sun_path` holds 108 bytes with the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 107;
/// Deadline of every read and of every write of a connection.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// Most bytes of a request body.
pub const MAX_BODY: usize = 1024 * 1024;
/// Most bytes of the request line, header lines and empty line together.
pub const MAX_HEAD: usize = 8 * 1024;
/// The server name `health` answers.
pub const VERSION: &str = concat!("ulei-garden ", env!("CARGO_PKG_VERSION"));

const CHUNK: usize = 8 * 1024;

/// What `serve` reads from `ulei.toml`.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `garden.store`.
    pub store: PathBuf,
    /// `garden.socket`.
    pub socket: PathBuf,
    /// The key in `garden.producer_key_file`.
    pub producer_key: ProducerKey,
    /// `inbox.socket`, where misses are published.
    pub inbox_socket: PathBuf,
    /// The seed: the domains of `garden.domains_file`, else [`starting_domains`].
    pub domains: Vec<Domain>,
    /// [`TtlTable::default`] with the rows `garden.ttl.<kind>` sets.
    pub ttl: TtlTable,
    /// [`ApiConfig::default`] with the `garden.api` and `garden.edge` keys set.
    pub api: ApiConfig,
    /// [`Schedule::default`] with the `garden.schedule` intervals set.
    pub schedule: Schedule,
    /// `garden.second_level`, [`Provider::None`] when unset.
    pub second_level: Provider,
    /// `garden.reranker`, [`rerank::Choice::None`] when unset.
    pub reranker: rerank::Choice,
    /// The `garden.embedder` keys when `garden.embedder.enabled` is true.
    pub embedder: Option<EmbedderSettings>,
    /// `garden.core_file` with its budget; `None` renders no core profile.
    pub core: Option<CoreSettings>,
}

/// The core profile's keys when `garden.core_file` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreSettings {
    /// `garden.core_file`.
    pub file: PathBuf,
    /// `garden.core_char_limit`, above 0, [`DEFAULT_CHAR_LIMIT`] when unset.
    pub char_limit: usize,
}

/// The `garden.embedder` keys of an enabled embedder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbedderSettings {
    /// `garden.embedder.helper`, the `ulei-garden-embed` binary.
    pub helper: PathBuf,
    /// `garden.embedder.model_dir`.
    pub model_dir: PathBuf,
    /// `garden.embedder.ort_library`.
    pub ort_library: PathBuf,
    /// `garden.embedder.threads`, above 0.
    pub threads: u32,
    /// `garden.embedder.load_timeout`: how long [`EmbedderSettings::start`] waits for the ready
    /// line.
    pub load_timeout: Duration,
}

impl EmbedderSettings {
    /// Starts the helper with the model directory, the ONNX Runtime library and the thread count
    /// as arguments and waits for its ready line. A query gets [`CALL_DEADLINE`]; a passage on
    /// the indexing lane of a helper with lanes [`PASSAGE_DEADLINE`] (ADR-0087 section 9).
    ///
    /// # Errors
    ///
    /// [`StartError`].
    pub fn start(&self) -> Result<Helper, StartError> {
        let mut command = Command::new(&self.helper);
        command
            .arg("--model-dir")
            .arg(&self.model_dir)
            .arg("--ort-lib")
            .arg(&self.ort_library)
            .arg("--threads")
            .arg(self.threads.to_string());
        Helper::start(command, self.load_timeout, CALL_DEADLINE)
            .map(|helper| helper.with_passage_deadline(PASSAGE_DEADLINE))
    }

    /// The content check of the model file (ADR-0087 open question 15), `<helper> verify
    /// <model_dir>/model.ulei-infer`, when the model directory holds the encoder engine's weight
    /// file, so the helper runs the `native` backend; `None` for the ONNX backend, whose start
    /// hashes its files.
    pub fn check(&self) -> Option<impl Fn() -> Verdict + Send + 'static> {
        let file = self.model_dir.join(ENGINE_FILE);
        file.exists()
            .then(|| check::helper_verify(self.helper.clone(), file))
    }
}

impl Settings {
    /// The settings of the file `flag` names, else of the one [`Config::read`] finds; an error
    /// is one line naming the file, the line when the key is set, and the key.
    ///
    /// # Errors
    /// The file cannot be found or decoded, a required key is missing, or a value is empty,
    /// out of its range or of the wrong form, names a domains file that does not seed, a
    /// producer key file [`ProducerKey::read`] refuses for the daemon's uid, or a second-level
    /// provider or a reranker this build does not have. Only a producer key file that does not
    /// exist yet is a [`Refusal`] that does not last.
    pub fn read(flag: Option<&Path>) -> Result<Self, Refusal> {
        let config = Config::read(flag).map_err(|e| e.to_string())?;
        let required = |key: &str| {
            string(&config, key)?
                .map(|(_, s)| PathBuf::from(s))
                .ok_or_else(|| format!("config {}: {key} is missing", config.path.display()))
        };
        Ok(Self {
            store: required("garden.store")?,
            socket: required("garden.socket")?,
            producer_key: producer_key(&config)?,
            inbox_socket: required("inbox.socket")?,
            domains: domains(&config)?,
            ttl: ttl(&config)?,
            api: api(&config)?,
            schedule: schedule(&config)?,
            second_level: second_level(&config)?,
            reranker: reranker(&config)?,
            embedder: embedder(&config)?,
            core: core(&config)?,
        })
    }
}

/// The `garden.socket` of the file `flag` names, else of the one [`Config::read`] finds: the
/// one key a client of the daemon reads.
///
/// # Errors
/// One line naming the file, and the line when the key is set: the file cannot be found or
/// decoded, or the key is missing, empty or not a string.
pub fn socket(flag: Option<&Path>) -> Result<PathBuf, String> {
    let config = Config::read(flag).map_err(|e| e.to_string())?;
    string(&config, "garden.socket")?
        .map(|(_, s)| PathBuf::from(s))
        .ok_or_else(|| format!("config {}: garden.socket is missing", config.path.display()))
}

/// Why [`Settings::read`] refused the config: one line, and whether the refusal lasts until the
/// config or a file it names changes, so a restart cannot fix it (`serve` exits 2, ADR-0075).
/// A producer key file that does not exist does not last: the inbox creates it at its first start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The one stderr line.
    pub line: String,
    /// Whether a restart cannot fix it.
    pub lasting: bool,
}

impl From<String> for Refusal {
    fn from(line: String) -> Self {
        Self {
            line,
            lasting: true,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.line)
    }
}

/// The one-line refusal of `entry`.
fn refuse(config: &Config, entry: &Entry, fault: impl fmt::Display) -> String {
    format!(
        "config {}:{}: {} {fault}",
        config.path.display(),
        entry.line,
        entry.key
    )
}

/// The entry at `key` and its non-empty string, if set.
fn string<'a>(config: &'a Config, key: &str) -> Result<Option<(&'a Entry, &'a str)>, String> {
    match config.get(key) {
        None => Ok(None),
        Some(entry) => match &entry.value {
            config::Value::String(s) if !s.is_empty() => Ok(Some((entry, s))),
            config::Value::String(_) => Err(refuse(config, entry, "is empty")),
            _ => Err(refuse(config, entry, "must be a string")),
        },
    }
}

/// The key of `garden.producer_key_file`, read for the daemon's uid.
fn producer_key(config: &Config) -> Result<ProducerKey, Refusal> {
    let key = "garden.producer_key_file";
    let (entry, file) = string(config, key)?
        .ok_or_else(|| format!("config {}: {key} is missing", config.path.display()))?;
    ProducerKey::read(Path::new(file), rustix::process::geteuid().as_raw()).map_err(|e| Refusal {
        lasting: !matches!(&e, KeyError::Io(io) if io.kind() == io::ErrorKind::NotFound),
        line: refuse(config, entry, format_args!("{file}: {e}")),
    })
}

/// The domains of `garden.domains_file`, checked as a seed of an empty store, else the
/// starting domains.
fn domains(config: &Config) -> Result<Vec<Domain>, String> {
    let Some((entry, file)) = string(config, "garden.domains_file")? else {
        return starting_domains().map_err(|e| format!("starting domains: {e}"));
    };
    let fault = |e: &dyn fmt::Display| refuse(config, entry, format_args!("{file}: {e}"));
    let text = fs::read_to_string(file).map_err(|e| fault(&e))?;
    let domains = parse_domains(&text).map_err(|e| fault(&e))?;
    Store::open_in_memory()
        .and_then(|mut probe| probe.seed_domains(&domains))
        .map_err(|e| fault(&e))?;
    Ok(domains)
}

/// [`TtlTable::default`] with every `garden.ttl.<kind>` set: seconds above 0, or `"never"`.
fn ttl(config: &Config) -> Result<TtlTable, String> {
    let mut table = TtlTable::default();
    for kind in TtlKind::ALL {
        let Some(entry) = config.get(&format!("garden.ttl.{kind}")) else {
            continue;
        };
        let ttl = match &entry.value {
            config::Value::Integer(secs) if *secs > 0 => Ttl::Secs(*secs),
            config::Value::String(s) if s == "never" => Ttl::Forever,
            _ => {
                return Err(refuse(
                    config,
                    entry,
                    "must be seconds above 0 or \"never\"",
                ));
            }
        };
        table.set(
            kind,
            TtlRule {
                ttl,
                ..table.rule(kind)
            },
        );
    }
    Ok(table)
}

/// [`ApiConfig::default`] with the `garden.api` and `garden.edge` keys set.
fn api(config: &Config) -> Result<ApiConfig, String> {
    let mut api = ApiConfig::default();
    if let Some(entry) = config.get("garden.api.search_k") {
        api.search_k = match entry.value {
            config::Value::Integer(k) => usize::try_from(k)
                .ok()
                .filter(|k| (1..=MAX_SEARCH_K).contains(k)),
            _ => None,
        }
        .ok_or_else(|| refuse(config, entry, format!("must be from 1 to {MAX_SEARCH_K}")))?;
    }
    if let Some(entry) = config.get("garden.edge.min_confidence") {
        api.edge.min_confidence = number(entry, 0.0..=1.0)
            .ok_or_else(|| refuse(config, entry, "must be a decimal from 0 to 1"))?;
    }
    if let Some(entry) = config.get("garden.edge.reconcile_min_cosine") {
        api.edge.reconcile_min_cosine = number(entry, 0.0..=1.0)
            .map(narrow)
            .ok_or_else(|| refuse(config, entry, "must be a decimal from 0 to 1"))?;
    }
    if let Some(entry) = config.get("garden.api.pointer_min_cosine") {
        api.pointers.min_cosine = number(entry, 0.0..=1.0)
            .map(narrow)
            .ok_or_else(|| refuse(config, entry, "must be a decimal from 0 to 1"))?;
    }
    if let Some(entry) = config.get("garden.api.pointer_min_lexical") {
        api.pointers.min_lexical = number(entry, 0.0..=f64::from(f32::MAX))
            .map(narrow)
            .ok_or_else(|| refuse(config, entry, "must be a decimal of 0 or more"))?;
    }
    Ok(api)
}

/// The value of `entry`, an integer or a decimal string, when it lies in `range`.
fn number(entry: &Entry, range: std::ops::RangeInclusive<f64>) -> Option<f64> {
    match &entry.value {
        config::Value::Integer(n) => i32::try_from(*n).ok().map(f64::from),
        config::Value::String(s) => decimal(s),
        config::Value::Bool(_) => None,
    }
    .filter(|x| range.contains(x))
}

/// `x` as `f32`; callers keep it within the `f32` range.
#[allow(clippy::cast_possible_truncation)]
fn narrow(x: f64) -> f32 {
    x as f32
}

/// [`Schedule::default`] with every `garden.schedule` key set: the intervals in seconds above
/// 0, `recheck_batch` a count above 0.
fn schedule(config: &Config) -> Result<Schedule, String> {
    let mut schedule = Schedule::default();
    for (key, slot) in [
        (
            "garden.schedule.expire_interval",
            &mut schedule.expire_interval,
        ),
        (
            "garden.schedule.recheck_interval",
            &mut schedule.recheck_interval,
        ),
        (
            "garden.schedule.maintain_interval",
            &mut schedule.maintain_interval,
        ),
        (
            "garden.schedule.verify_interval",
            &mut schedule.verify_interval,
        ),
    ] {
        if let Some(entry) = config.get(key) {
            *slot = match entry.value {
                config::Value::Integer(secs) if secs > 0 => secs,
                _ => return Err(refuse(config, entry, "must be seconds above 0")),
            };
        }
    }
    if let Some(entry) = config.get("garden.schedule.recheck_batch") {
        schedule.recheck_batch = match entry.value {
            config::Value::Integer(n) if n > 0 => usize::try_from(n)
                .map_err(|_| refuse(config, entry, "is too large for this platform"))?,
            _ => return Err(refuse(config, entry, "must be a count above 0")),
        };
    }
    Ok(schedule)
}

/// The provider `garden.second_level` names, [`Provider::None`] when unset.
fn second_level(config: &Config) -> Result<Provider, String> {
    match string(config, "garden.second_level")? {
        None => Ok(Provider::None),
        Some((entry, name)) => Provider::from_name(name).ok_or_else(|| {
            refuse(
                config,
                entry,
                format_args!("{name:?}: only \"none\" is built"),
            )
        }),
    }
}

/// The reranker `garden.reranker` names, [`rerank::Choice::None`] when unset.
fn reranker(config: &Config) -> Result<rerank::Choice, String> {
    match string(config, "garden.reranker")? {
        None => Ok(rerank::Choice::None),
        Some((entry, name)) => rerank::Choice::from_name(name).ok_or_else(|| {
            refuse(
                config,
                entry,
                format_args!("{name:?}: only \"none\" is built"),
            )
        }),
    }
}

/// `garden.core_file` with `garden.core_char_limit`, a count above 0, when the file is set.
fn core(config: &Config) -> Result<Option<CoreSettings>, String> {
    let char_limit = match config.get("garden.core_char_limit") {
        None => DEFAULT_CHAR_LIMIT,
        Some(entry) => match entry.value {
            config::Value::Integer(n) => usize::try_from(n).ok().filter(|n| *n > 0),
            _ => None,
        }
        .ok_or_else(|| refuse(config, entry, "must be an integer above 0"))?,
    };
    Ok(
        string(config, "garden.core_file")?.map(|(_, file)| CoreSettings {
            file: PathBuf::from(file),
            char_limit,
        }),
    )
}

/// The `garden.embedder` keys when `garden.embedder.enabled` is true: `helper`, `model_dir` and
/// `ort_library` required, `threads` (default 2) and `load_timeout` (default 120 seconds) above 0.
fn embedder(config: &Config) -> Result<Option<EmbedderSettings>, String> {
    match config.get("garden.embedder.enabled") {
        None => return Ok(None),
        Some(entry) => match entry.value {
            config::Value::Bool(true) => {}
            config::Value::Bool(false) => return Ok(None),
            _ => return Err(refuse(config, entry, "must be true or false")),
        },
    }
    let required = |key: &str| {
        string(config, key)?
            .map(|(_, s)| PathBuf::from(s))
            .ok_or_else(|| {
                format!(
                    "config {}: {key} is missing, garden.embedder.enabled is true",
                    config.path.display()
                )
            })
    };
    let above_zero = |key: &str, default: u32| match config.get(key) {
        None => Ok(default),
        Some(entry) => match entry.value {
            config::Value::Integer(n) => u32::try_from(n).ok().filter(|n| *n > 0),
            _ => None,
        }
        .ok_or_else(|| refuse(config, entry, "must be an integer above 0")),
    };
    Ok(Some(EmbedderSettings {
        helper: required("garden.embedder.helper")?,
        model_dir: required("garden.embedder.model_dir")?,
        ort_library: required("garden.embedder.ort_library")?,
        threads: above_zero("garden.embedder.threads", 2)?,
        load_timeout: Duration::from_secs(u64::from(above_zero(
            "garden.embedder.load_timeout",
            120,
        )?)),
    }))
}

/// The value of plain decimal text: digits with at most one `.` between digits.
fn decimal(text: &str) -> Option<f64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, "0"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(whole) && digits(fraction))
        .then(|| text.parse().ok())
        .flatten()
}

/// The garden of `settings`: the store created owner-only, migrated, with `settings.domains`
/// seeded, the TTL table and API tunables of `settings`, and the hybrid retriever over `helper`
/// (lexical only without one) behind the reranker of `settings`.
///
/// # Errors
/// [`OpenError`].
pub fn open(settings: &Settings, helper: Option<Arc<Supervisor>>) -> Result<Garden, OpenError> {
    private_file(&settings.store)?;
    let mut store = Store::open(&settings.store).map_err(ApiError::Store)?;
    store
        .seed_domains(&settings.domains)
        .map_err(ApiError::Store)?;
    store.set_ttl_table(settings.ttl.clone());
    let reranker = settings.reranker.reranker();
    let retriever = match helper {
        Some(helper) => Hybrid::with_helper(helper, reranker),
        None => Hybrid::new(None, reranker),
    };
    Ok(Garden::new(
        store,
        retriever,
        settings.api,
        settings.producer_key.clone(),
    ))
}

/// Creates the file at `path` with mode 0600, and its missing directories with mode 0700, or
/// checks that the existing file is open to no other user.
fn private_file(path: &Path) -> Result<(), OpenError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            match fs::metadata(path)?.mode() & 0o777 {
                mode if mode & 0o077 != 0 => Err(OpenError::OpenMode(mode)),
                _ => Ok(()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Why the garden could not be opened.
#[derive(Debug)]
pub enum OpenError {
    /// The existing store file has this mode, open to other users.
    OpenMode(u32),
    /// Filesystem failure on the store file or its directory.
    Io(io::Error),
    /// The store cannot be opened, migrated or seeded.
    Garden(ApiError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenMode(mode) => write!(
                f,
                "store file has mode {mode:04o}, it must be 0600 or stricter"
            ),
            Self::Io(e) => write!(f, "{e}"),
            Self::Garden(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ApiError> for OpenError {
    fn from(e: ApiError) -> Self {
        Self::Garden(e)
    }
}

/// Why the socket could not be bound.
#[derive(Debug)]
pub enum BindError {
    /// The path is longer than [`MAX_SOCKET_PATH`] bytes.
    PathTooLong(usize),
    /// The socket's directory is not owned by the daemon's uid or is open to other users.
    OpenDirectory(PathBuf),
    /// Something other than a stale socket exists at the path.
    Occupied,
    /// Filesystem or socket failure.
    Io(io::Error),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathTooLong(len) => write!(
                f,
                "socket path has {len} bytes, at most {MAX_SOCKET_PATH} are allowed"
            ),
            Self::OpenDirectory(dir) => write!(
                f,
                "socket directory {} must belong to this user with mode 0700 or stricter",
                dir.display()
            ),
            Self::Occupied => f.write_str(
                "socket path exists and is not a stale socket (a live socket or another file)",
            ),
            Self::Io(e) => write!(f, "socket: {e}"),
        }
    }
}

impl std::error::Error for BindError {}

impl From<io::Error> for BindError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Binds and listens at `path` with mode 0600, in a directory of `uid` that no other user can
/// enter; a missing directory is created 0700. An existing path is replaced only when it is a
/// socket that refuses a connection (stale).
///
/// # Errors
/// [`BindError`].
pub fn bind(path: &Path, uid: u32) -> Result<UnixListener, BindError> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(BindError::PathTooLong(len));
    }
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = fs::metadata(dir)?;
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(BindError::OpenDirectory(dir.to_owned()));
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => match UnixStream::connect(path) {
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path)?,
            _ => return Err(BindError::Occupied),
        },
        Ok(_) => return Err(BindError::Occupied),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// A C7 `x-methods` entry.
#[derive(Clone, Copy, Debug)]
enum Method {
    Search,
    Pointers,
    Propose,
    Digest,
    Resolve,
    Health,
    SeedbedTake,
    SeedbedDone,
    Forget,
    ForgetTake,
    ForgetDone,
    Places(PlacesMethod),
}

/// A C7 method of the places bed.
#[derive(Clone, Copy, Debug)]
enum PlacesMethod {
    Search,
    Write,
    Verdict,
    Profile,
}

impl Method {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "search" => Self::Search,
            "pointers" => Self::Pointers,
            "propose" => Self::Propose,
            "digest" => Self::Digest,
            "resolve" => Self::Resolve,
            "health" => Self::Health,
            "seedbed_take" => Self::SeedbedTake,
            "seedbed_done" => Self::SeedbedDone,
            "forget" => Self::Forget,
            "forget_take" => Self::ForgetTake,
            "forget_done" => Self::ForgetDone,
            "places_search" => Self::Places(PlacesMethod::Search),
            "places_write" => Self::Places(PlacesMethod::Write),
            "places_verdict" => Self::Places(PlacesMethod::Verdict),
            "places_profile" => Self::Places(PlacesMethod::Profile),
            _ => return None,
        })
    }
}

/// An HTTP status and its JSON body.
type Reply = (u16, Vec<u8>);

/// The garden served to peers of one uid.
pub struct Server<C = SystemClock> {
    garden: Garden,
    uid: u32,
    timeout: Duration,
    wake: Arc<Wake>,
    clock: C,
    failed_proposes: u64,
    profile: Option<Arc<Profile>>,
}

impl<C: Clock> Server<C> {
    /// A server over `garden` that serves peers of `uid`, gives every read and write of a
    /// connection `timeout`, reads the time from `clock` and signals `wake` when a request
    /// writes a fact or queues a miss.
    pub fn new(garden: Garden, uid: u32, timeout: Duration, wake: Arc<Wake>, clock: C) -> Self {
        Self {
            garden,
            uid,
            timeout,
            wake,
            clock,
            failed_proposes: 0,
            profile: None,
        }
    }

    /// The server that renders `profile` after every `resolve` and `seedbed_done` it answers
    /// and every `forget_done` that closed a fact.
    #[must_use]
    pub fn with_profile(mut self, profile: Arc<Profile>) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Serves connections one at a time, blocking in `accept`; returns only on an accept
    /// failure other than an aborted connection.
    pub fn run(&mut self, listener: &UnixListener) -> io::Error {
        loop {
            match listener.accept() {
                Ok((stream, _)) => self.serve(stream),
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {}
                Err(e) => return e,
            }
        }
    }

    /// Serves one connection: checks the peer uid before reading, then reads one request,
    /// answers it with `Connection: close` and closes. A connection that fails or misses its
    /// deadline is closed without an answer.
    pub fn serve(&mut self, mut stream: UnixStream) {
        let reply = match rustix::net::sockopt::socket_peercred(&stream) {
            Ok(cred) if cred.uid.as_raw() == self.uid => self.answer(&mut stream),
            Ok(_) => Some(error(
                ErrorCode::Forbidden,
                "peer uid differs from the daemon",
            )),
            Err(_) => Some(error(ErrorCode::Forbidden, "peer credentials unavailable")),
        };
        if let Some(reply) = reply {
            self.send(&mut stream, &reply);
        }
    }

    fn send(&self, stream: &mut UnixStream, (status, body): &Reply) {
        if stream.set_write_timeout(Some(self.timeout)).is_ok() {
            let _ = stream.write_all(&response(*status, body));
        }
    }

    fn answer(&mut self, stream: &mut UnixStream) -> Option<Reply> {
        let deadline = Instant::now() + self.timeout;
        match read_request(stream, deadline) {
            Ok((line, body)) => Some(
                route(&line)
                    .and_then(|m| self.dispatch(m, &body))
                    .unwrap_or_else(|e| e),
            ),
            Err(Refused::Answer(code, message)) => Some(error(code, message)),
            Err(Refused::TooLarge(unread)) => {
                // Closing with the body unread would reset the client's read of the answer.
                self.send(stream, &error(ErrorCode::TooLarge, "body over 1 MiB"));
                let _ = stream.shutdown(Shutdown::Write);
                discard(stream, unread, deadline);
                None
            }
            Err(Refused::Close) => None,
        }
    }

    /// Runs `method` on the garden at the server's clock; `Err` is a C7 `Error` reply. A failed
    /// `propose`, `seedbed_done` or `forget_done` item, which the store rolled back, is counted
    /// for `health` and named on stderr by its class alone.
    fn dispatch(&mut self, method: Method, body: &[u8]) -> Result<Reply, Reply> {
        let invalid = |_| error(ErrorCode::InvalidRequest, "body is not the method input");
        let now = self.clock.now();
        let g = &mut self.garden;
        let out = match method {
            Method::Search => {
                let req: c7::SearchRequest = serde_json::from_slice(body).map_err(invalid)?;
                g.retriever().queried(now);
                let answer = g.garden_search(&req, now).map_err(|e| failure(&e))?;
                self.missed(answer.miss);
                serde_json::to_vec(&answer.response)
            }
            Method::Pointers => {
                let req: c7::PointersRequest = serde_json::from_slice(body).map_err(invalid)?;
                g.retriever().queried(now);
                serde_json::to_vec(&g.garden_pointers(&req, now).map_err(|e| failure(&e))?)
            }
            Method::Propose => {
                let req: c7::ProposeRequest = serde_json::from_slice(body).map_err(invalid)?;
                match g.garden_propose(&req, now) {
                    Ok(answer) => serde_json::to_vec(&answer),
                    Err(e) => return Err(self.rolled_back("propose", &e)),
                }
            }
            Method::Digest => {
                let req: c7::DigestRequest = serde_json::from_slice(body).map_err(invalid)?;
                serde_json::to_vec(&g.garden_digest(&req).map_err(|e| failure(&e))?)
            }
            Method::Resolve => {
                let req: c7::ResolveRequest = serde_json::from_slice(body).map_err(invalid)?;
                let answer = g.garden_resolve(&req, now).map_err(|e| failure(&e))?;
                self.written([&answer], now);
                serde_json::to_vec(&answer)
            }
            Method::SeedbedTake => {
                let req: c7::SeedbedTakeRequest = serde_json::from_slice(body).map_err(invalid)?;
                serde_json::to_vec(&g.garden_seedbed_take(&req, now).map_err(|e| failure(&e))?)
            }
            Method::SeedbedDone => {
                let req: c7::SeedbedDoneRequest = serde_json::from_slice(body).map_err(invalid)?;
                let done = g.garden_seedbed_done(&req, now).map_err(|e| failure(&e))?;
                self.written(done.items.iter().flat_map(|i| &i.outcomes), now);
                if let Some(e) = done.failed {
                    return Err(self.rolled_back("seedbed_done", &e.into()));
                }
                serde_json::to_vec(&c7::SeedbedDoneResponse { items: done.items })
            }
            Method::Forget => {
                let req: c7::ForgetRequest = serde_json::from_slice(body).map_err(invalid)?;
                serde_json::to_vec(&g.garden_forget(&req, now).map_err(|e| failure(&e))?)
            }
            Method::ForgetTake => {
                let req: c7::ForgetTakeRequest = serde_json::from_slice(body).map_err(invalid)?;
                serde_json::to_vec(&g.garden_forget_take(&req).map_err(|e| failure(&e))?)
            }
            Method::ForgetDone => {
                let req: c7::ForgetDoneRequest = serde_json::from_slice(body).map_err(invalid)?;
                let done = g.garden_forget_done(&req, now).map_err(|e| failure(&e))?;
                if done.closed() {
                    self.closed(now);
                }
                if let Some(e) = done.failed {
                    return Err(self.rolled_back("forget_done", &e.into()));
                }
                serde_json::to_vec(&c7::ForgetDoneResponse { items: done.items })
            }
            Method::Places(m) => places(g, m, body, now)?,
            Method::Health => {
                let c7::HealthRequest {} = serde_json::from_slice(body).map_err(invalid)?;
                serde_json::to_vec(&self.health()?)
            }
        };
        out.map(|body| (200, body))
            .map_err(|_| error(ErrorCode::StoreFailed, "answer does not serialize"))
    }

    /// The C7 `health` answer; `Err` is a C7 `Error` reply.
    fn health(&self) -> Result<c7::Health, Reply> {
        let g = &self.garden;
        let (dense, dense_error) = g.retriever().dense_state();
        let coverage = g
            .retriever()
            .dense_coverage(g.store())
            .map_err(|e| failure(&e.into()))?;
        Ok(c7::Health {
            version: VERSION.to_owned(),
            dense,
            dense_error,
            failed_proposes: self.failed_proposes,
            unpublished_misses: miss::unpublished(g.store()).map_err(|e| failure(&e.into()))?,
            vectors_missing: coverage.vectors_missing,
            passages_skipped: coverage.passages_skipped,
            passages_deferred: None,
            model_check: coverage.model_check,
        })
    }

    /// Counts a write of `method` the store rolled back for `health`, with one stderr line
    /// naming its class, and returns its reply.
    fn rolled_back(&mut self, method: &str, e: &ApiError) -> Reply {
        self.failed_proposes += 1;
        let _ = writeln!(io::stderr(), "garden: {method} failed: {}", e.class());
        failure(e)
    }

    /// Wakes the worker when a search queued a miss for it to publish; a queued miss that
    /// dropped the oldest unpublished one is one stderr line.
    fn missed(&self, miss: Option<Queued>) {
        match miss {
            None | Some(Queued::Repeat) => {}
            Some(Queued::Added) => self.wake.wake(),
            Some(Queued::AddedOverCap) => {
                let _ = writeln!(
                    io::stderr(),
                    "garden: miss outbox holds {} unpublished misses, dropped the oldest",
                    miss::MAX_UNPUBLISHED
                );
                self.wake.wake();
            }
        }
    }

    /// Renders the core profile at `now`, since a `resolve` that rejects may retract a fact too,
    /// and wakes the worker when one of `answers` wrote a fact, whose expiry may be its nearest
    /// deadline.
    fn written<'a>(
        &self,
        answers: impl IntoIterator<Item = &'a c7::ProposeResponse>,
        now: UnixTime,
    ) {
        if let Some(profile) = &self.profile {
            profile.update(self.garden.store(), now);
        }
        if answers
            .into_iter()
            .any(|a| a.status == c7::ProposalStatus::Accepted)
        {
            self.wake.wake();
        }
    }

    /// Renders the core profile at `now` and wakes the worker, which drops the facts a
    /// `forget_done` closed from the resident matrix.
    fn closed(&self, now: UnixTime) {
        if let Some(profile) = &self.profile {
            profile.update(self.garden.store(), now);
        }
        self.wake.wake();
    }
}

/// Why a request was not dispatched.
enum Refused {
    Answer(ErrorCode, &'static str),
    /// A body over [`MAX_BODY`], with this many of its bytes not read yet.
    TooLarge(usize),
    /// Deadline, I/O failure or end of stream before a whole request.
    Close,
}

/// Reads one request until `deadline`: its request line and its body.
fn read_request(stream: &mut UnixStream, deadline: Instant) -> Result<(String, Vec<u8>), Refused> {
    let too_large = Refused::Answer(ErrorCode::TooLarge, "headers over 8 KiB");
    let mut buf = Vec::with_capacity(CHUNK);
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEAD {
            return Err(too_large);
        }
        read_some(stream, &mut buf, CHUNK, deadline)?;
    };
    if head_end + 4 > MAX_HEAD {
        return Err(too_large);
    }
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| bad("headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default().to_owned();
    let length = content_length(lines)?;
    let mut body = buf.split_off(head_end + 4);
    if length > MAX_BODY {
        return Err(Refused::TooLarge(length.saturating_sub(body.len())));
    }
    while body.len() < length {
        let missing = length - body.len();
        read_some(stream, &mut body, missing, deadline)?;
    }
    if body.len() > length {
        return Err(bad("bytes after the body"));
    }
    Ok((line, body))
}

/// Appends at most `max` bytes that arrive before `deadline`.
fn read_some(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    max: usize,
    deadline: Instant,
) -> Result<(), Refused> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
        return Err(Refused::Close);
    }
    let mut chunk = [0u8; CHUNK];
    match stream.read(&mut chunk[..max.min(CHUNK)]) {
        Ok(0) | Err(_) => Err(Refused::Close),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(())
        }
    }
}

/// Reads and drops up to `length` bytes that arrive before `deadline`, until the end of the
/// stream or a failure.
fn discard(stream: &mut UnixStream, mut length: usize, deadline: Instant) {
    let mut scratch = Vec::with_capacity(CHUNK);
    while length > 0 {
        scratch.clear();
        if read_some(stream, &mut scratch, length, deadline).is_err() {
            return;
        }
        length -= scratch.len();
    }
}

/// The method of a request line `POST /v1/<name> HTTP/1.1`.
fn route(line: &str) -> Result<Method, Reply> {
    let mut parts = line.split(' ');
    let (Some(verb), Some(target), Some("HTTP/1.1"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(error(ErrorCode::InvalidRequest, "malformed request line"));
    };
    target
        .strip_prefix("/v1/")
        .filter(|_| verb == "POST")
        .and_then(Method::from_name)
        .ok_or_else(|| error(ErrorCode::UnknownMethod, "no such method or path"))
}

/// The `Content-Length` of the header lines; `Transfer-Encoding` is refused.
fn content_length<'a>(lines: impl Iterator<Item = &'a str>) -> Result<usize, Refused> {
    let mut length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad("malformed header"))?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_graphic())
            || value.contains(['\r', '\n'])
        {
            return Err(bad("malformed header"));
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(bad("Transfer-Encoding is refused"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad("malformed Content-Length"));
            }
            // All digits, so a parse failure is an overflow: far over the body limit.
            length = Some(value.parse::<usize>().unwrap_or(usize::MAX));
        }
    }
    length.ok_or_else(|| bad("Content-Length is required"))
}

fn bad(message: &'static str) -> Refused {
    Refused::Answer(ErrorCode::InvalidRequest, message)
}

/// Runs the places bed's `method` on `g` at `now`; `Err` is a C7 `Error` reply.
fn places(
    g: &mut Garden,
    method: PlacesMethod,
    body: &[u8],
    now: UnixTime,
) -> Result<serde_json::Result<Vec<u8>>, Reply> {
    let invalid = |_| error(ErrorCode::InvalidRequest, "body is not the method input");
    Ok(match method {
        PlacesMethod::Search => {
            let req: c7::PlacesSearchRequest = serde_json::from_slice(body).map_err(invalid)?;
            serde_json::to_vec(&g.garden_places_search(&req, now).map_err(|e| failure(&e))?)
        }
        PlacesMethod::Write => {
            let req: c7::PlacesWriteRequest = serde_json::from_slice(body).map_err(invalid)?;
            serde_json::to_vec(&g.garden_places_write(&req, now).map_err(|e| failure(&e))?)
        }
        PlacesMethod::Verdict => {
            let req: c7::PlacesVerdictRequest = serde_json::from_slice(body).map_err(invalid)?;
            serde_json::to_vec(
                &g.garden_places_verdict(&req, now)
                    .map_err(|e| failure(&e))?,
            )
        }
        PlacesMethod::Profile => {
            let req: c7::PlacesProfileRequest = serde_json::from_slice(body).map_err(invalid)?;
            serde_json::to_vec(&g.garden_places_profile(&req).map_err(|e| failure(&e))?)
        }
    })
}

/// The C7 `Error` reply of a failure of the garden.
fn failure(e: &ApiError) -> Reply {
    let code = match e {
        ApiError::Edge(EdgeError::NotOpen(_))
        | ApiError::UnknownSeed(_)
        | ApiError::UnknownRow(_)
        | ApiError::Refused(places::Refusal::UnknownPlace(_) | places::Refusal::NotAPlace(_)) => {
            ErrorCode::NotFound
        }
        ApiError::Edge(EdgeError::ReplacementRefused(_)) => ErrorCode::ReplacementRefused,
        ApiError::Edge(
            EdgeError::MissingReplacement | EdgeError::MissingEntity | EdgeError::NotACandidate(_),
        )
        | ApiError::Invalid(_)
        | ApiError::Refused(_) => ErrorCode::InvalidRequest,
        ApiError::Store(_) | ApiError::Edge(_) | ApiError::Search(_) => ErrorCode::StoreFailed,
    };
    error(code, &e.to_string())
}

/// The status and C7 `Error` body of `code`.
fn error(code: ErrorCode, message: &str) -> Reply {
    let status = match code {
        ErrorCode::InvalidRequest => 400,
        ErrorCode::Forbidden => 403,
        ErrorCode::UnknownMethod | ErrorCode::NotFound => 404,
        ErrorCode::ReplacementRefused => 409,
        ErrorCode::TooLarge => 413,
        ErrorCode::StoreFailed => 500,
    };
    let body = format!(
        "{{\"code\":\"{}\",\"message\":{}}}",
        code.as_str(),
        Value::from(message)
    );
    (status, body.into_bytes())
}

fn response(status: u16, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Content Too Large",
        _ => "Internal Server Error",
    };
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}
