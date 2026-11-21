//! The real `ulei-engine serve` on the fake gateway and the real `ulei-tg` on the fake Bot API,
//! from one synthetic `ulei.toml` in a scratch directory. The engine is the binary of the same
//! workspace build, reached only over C6; `cargo test --workspace` builds it.

use std::fs::{self, DirBuilder, File, Permissions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::fake_bot_api::{FakeBotApi, ok};
use super::fake_gateway::FakeGateway;
use super::process::{finish, sigterm, stderr};

pub(crate) const OWNER: i64 = 42;
pub(crate) const CHAT: i64 = 1001;
/// The engine's owner session: the session key of the owner's direct chat.
pub(crate) const OWNER_SESSION: &str = "agent:main:telegram:dm:42";
const TOKEN: &str = "123456:SECRET-TOKEN-OF-THE-TEST-BOT";
/// The key the fake STT endpoint is called with.
pub(crate) const STT_KEY: &str = "gsk_synthetic-stt-key-for-tests";

/// Every party of one channel run; dropping it kills what is still running.
pub(crate) struct Stack {
    pub(crate) bot: FakeBotApi,
    pub(crate) gateway: FakeGateway,
    engine: Option<Child>,
    tg: Option<Child>,
    dir: Scratch,
}

impl Stack {
    /// Starts the gateway, the engine on it, the Bot API and `ulei-tg` on both; the bot
    /// answers `getMe`, the owner's coalescing window is 0.3 s.
    pub(crate) fn start() -> Self {
        let dir = Scratch::new();
        let gateway = FakeGateway::start(&dir.path("gw.sock"));
        let bot = FakeBotApi::start();
        bot.script(
            "getMe",
            ok(json!({ "id": 7, "is_bot": true, "username": "test_bot" })),
        );
        let config = write_config(&dir, &bot);
        let engine = start_engine(&dir, &config);
        let tg = Command::new(env!("CARGO_BIN_EXE_ulei-tg"))
            .arg("--config")
            .arg(&config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &dir.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ulei-tg");
        Self {
            bot,
            gateway,
            engine: Some(engine),
            tg: Some(tg),
            dir,
        }
    }

    /// The attachments directory of `ulei-tg`.
    pub(crate) fn inbox(&self) -> PathBuf {
        self.dir.path("inbox")
    }

    /// `name` in the scratch directory, the engine's working directory and `$HOME` of both.
    pub(crate) fn file(&self, name: &str) -> PathBuf {
        self.dir.path(name)
    }

    /// Stops `ulei-tg` with SIGTERM, which must exit 0, then the engine; the stderr of
    /// `ulei-tg`.
    pub(crate) fn stop(&mut self) -> String {
        let tg = self.tg.take().expect("ulei-tg");
        sigterm(&tg);
        let output = finish(tg);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let engine = self.engine.take().expect("the engine");
        sigterm(&engine);
        finish(engine);
        stderr(&output)
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        for mut child in [self.tg.take(), self.engine.take()].into_iter().flatten() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// `ulei.toml` of both daemons: the engine on the gateway with owner session
/// [`OWNER_SESSION`], `ulei-tg` on the Bot API and the STT endpoint of `bot`, owner [`OWNER`],
/// token file 0600.
fn write_config(dir: &Scratch, bot: &FakeBotApi) -> PathBuf {
    let brain = dir.path("brain");
    fs::create_dir(&brain).expect("brain");
    fs::write(brain.join("SOUL.md"), "You are the test assistant.\n").expect("soul");
    let token = dir.path("token");
    fs::write(&token, format!("{TOKEN}\n")).expect("token");
    fs::set_permissions(&token, Permissions::from_mode(0o600)).expect("chmod");
    let stt_env = dir.path("stt.env");
    fs::write(&stt_env, format!("GROQ_API_KEY={STT_KEY}\n")).expect("stt env-file");
    let text = format!(
        "[qb]\nsocket = \"{gateway}\"\n\n\
         [engine]\njournal = \"{journal}\"\nterminal_output = \"{terminal}\"\n\
         soul_file = \"{brain}/SOUL.md\"\nindex_file = \"{brain}/INDEX.md\"\n\
         memory_file = \"{brain}/MEMORY.md\"\nuser_file = \"{brain}/USER.md\"\n\
         skills_dir = \"{brain}/skills\"\nc6_socket = \"{c6}\"\n\
         owner_session = \"{OWNER_SESSION}\"\ndefault_route = \"main\"\n\n\
         [telegram]\ntoken_file = \"{token}\"\nowner_ids = \"{OWNER}\"\n\
         attachments_dir = \"{inbox}\"\nbase_url = \"{base}\"\nfile_base_url = \"{files}\"\n\
         poll_timeout = 1\nread_timeout = 2\ncoalesce_quiet = \"0.3\"\n\
         stt_env_file = \"{stt_env}\"\nstt_base_url = \"{base}\"\n",
        gateway = dir.path("gw.sock").display(),
        journal = dir.path("journal.sqlite3").display(),
        terminal = dir.path("terminal-output").display(),
        brain = brain.display(),
        c6 = dir.path("c6.sock").display(),
        token = token.display(),
        inbox = dir.path("inbox").display(),
        base = bot.base_url(),
        files = bot.file_base_url(),
        stt_env = stt_env.display(),
    );
    let path = dir.path("ulei.toml");
    fs::write(&path, text).expect("config");
    path
}

/// `ulei-engine serve` in `dir` once it is ready; its stderr goes to `engine.log` there.
fn start_engine(dir: &Scratch, config: &Path) -> Child {
    let log = File::create(dir.path("engine.log")).expect("engine log");
    let mut child = Command::new(engine_binary())
        .args(["serve", "--config"])
        .arg(config)
        .current_dir(&dir.0)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &dir.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .expect("spawn the engine");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("ready line");
    assert!(
        line.starts_with("ulei-engine ready "),
        "no ready line: {line:?}, {}",
        fs::read_to_string(dir.path("engine.log")).unwrap_or_default()
    );
    child
}

/// The `ulei-engine` binary of this build: in the target directory that holds this test.
fn engine_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable");
    exe.ancestors()
        .map(|dir| dir.join("ulei-engine"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "no ulei-engine above {}; build the workspace (cargo test --workspace)",
                exe.display()
            )
        })
}

/// A fresh 0700 directory, short enough for the socket paths inside it; removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let temp = std::env::temp_dir();
        let base = if temp.as_os_str().len() <= 48 {
            temp
        } else {
            PathBuf::from("/tmp")
        };
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("ulei-e2e-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("scratch dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
