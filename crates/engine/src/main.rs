//! `ulei-engine`: the ecosystem's own agent loop.
//!
//! `run` runs one turn of a journaled session on the qb gateway and prints the answer;
//! `--version` prints the version; anything else is a usage error.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ulei_contracts::generated::c4g::Role;
use ulei_engine::agent::{
    Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, DEFAULT_MAX_STEPS, Ended, Outcome, Template,
    ThreadSleeper, text, text_message,
};
use ulei_engine::config::{self, Config};
use ulei_engine::gateway::Client;
use ulei_engine::journal::Journal;
use ulei_engine::policy::{Chain, StateDbGuard};
use ulei_engine::prompt::{self, Brain, DEFAULT_MEMORY_CHAR_LIMIT, DEFAULT_USER_CHAR_LIMIT, Host};
use ulei_engine::skills::Skills;
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::web::{
    DEFAULT_EXTRACT_CHAR_LIMIT, EXA_CALL_TIMEOUT, Web, clamp_char_limit,
};
use ulei_engine::tools::{Interrupt, Registry, ServiceUnit};
use ulei_engine::wire::c8::Session;
use ulei_engine::zone::{self, Zone};

const USAGE: &str = "usage: ulei-engine --version | ulei-engine run --session S --route ALIAS --prompt TEXT [--new] [--config PATH]\n";

/// Upper bound of output tokens per model step.
const MAX_TOKENS: u64 = 16_384;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A model may think for minutes before its first event.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

const EXIT_FAILURE: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_QUOTA: u8 = 3;

/// The arguments of `run`.
struct Run {
    session: String,
    route: String,
    prompt: String,
    new: bool,
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        return match writeln!(
            std::io::stdout().lock(),
            "ulei-engine {}",
            env!("CARGO_PKG_VERSION")
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(EXIT_FAILURE),
        };
    }
    let Some(run) = parse_run(args) else {
        let _ = std::io::stderr().write_all(USAGE.as_bytes());
        return ExitCode::from(EXIT_USAGE);
    };
    match turn(&run) {
        Ok(answer) => match writeln!(std::io::stdout().lock(), "{answer}") {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(EXIT_FAILURE),
        },
        Err((code, line)) => {
            let _ = writeln!(std::io::stderr().lock(), "{}", line.replace('\n', " "));
            ExitCode::from(code)
        }
    }
}

/// `run` with each flag at most once and the required ones present; `None` is a usage error.
fn parse_run(args: Vec<OsString>) -> Option<Run> {
    let mut args = args.into_iter();
    if args.next()? != "run" {
        return None;
    }
    let (mut session, mut route, mut prompt, mut config, mut new) = (None, None, None, None, false);
    while let Some(flag) = args.next() {
        let slot = match flag.to_str()? {
            "--new" if !new => {
                new = true;
                continue;
            }
            "--session" => &mut session,
            "--route" => &mut route,
            "--prompt" => &mut prompt,
            "--config" => &mut config,
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(args.next()?);
    }
    Some(Run {
        session: session?.into_string().ok()?,
        route: route?.into_string().ok()?,
        prompt: prompt?.into_string().ok()?,
        new,
        config: config.map(PathBuf::from),
    })
}

/// Runs and journals one turn; the final assistant text, or an exit code and one stderr line.
fn turn(run: &Run) -> Result<String, (u8, String)> {
    let fail = |e: &dyn std::fmt::Display| (EXIT_FAILURE, format!("ulei-engine: {e}"));
    let config = match run
        .config
        .clone()
        .or_else(|| env(config::CONFIG_ENV).map(PathBuf::from))
    {
        Some(path) => Config::load(&path).map_err(|e| fail(&e))?,
        None => Config::default(),
    };
    let socket = config
        .path(config::QB_SOCKET, env(config::QB_SOCKET.env))
        .map_err(|e| fail(&e))?;
    let db = config
        .path(config::JOURNAL, env(config::JOURNAL.env))
        .map_err(|e| fail(&e))?;
    let terminal_output = config
        .path(config::TERMINAL_OUTPUT, env(config::TERMINAL_OUTPUT.env))
        .map_err(|e| fail(&e))?;
    let batch_timeout = config
        .timeout(
            config::CONCURRENT_TOOL_TIMEOUT,
            env(config::CONCURRENT_TOOL_TIMEOUT.env),
            DEFAULT_CONCURRENT_TOOL_TIMEOUT,
        )
        .map_err(|e| fail(&e))?;
    let max_steps = config
        .limit(
            config::MAX_STEPS,
            env(config::MAX_STEPS.env),
            DEFAULT_MAX_STEPS,
        )
        .map_err(|e| fail(&e))?;
    let empty_guard = config.switch(
        config::EMPTY_RESPONSE_GUARD,
        env(config::EMPTY_RESPONSE_GUARD.env),
    );
    let unit = config
        .text(config::SERVICE_UNIT, env(config::SERVICE_UNIT.env))
        .map_err(|e| fail(&e))?;
    let under_systemd = env("INVOCATION_ID").is_some_and(|id| !id.is_empty());
    let service = ServiceUnit::discover(unit.as_deref(), under_systemd, || {
        std::fs::read_to_string("/proc/self/cgroup")
    })
    .map_err(|e| fail(&e))?;
    let (brain, timezone) = brain(&config).map_err(|e| fail(&e))?;
    let tools = tools(&config, &terminal_output, service, &brain.skills).map_err(|e| fail(&e))?;
    let guard = StateDbGuard::new(db.clone()).map_err(|e| fail(&e))?;
    let cwd = std::env::current_dir().map_err(|e| fail(&format!("working directory: {e}")))?;

    let mut journal = Journal::open(&db).map_err(|e| fail(&e))?;
    let session = if run.new {
        journal.new_session(&run.session)
    } else {
        journal.session(&run.session)
    }
    .map_err(|e| fail(&e))?;
    let next = journal.next_turn(session.id).map_err(|e| fail(&e))?;
    let mut history = journal.messages(session.id).map_err(|e| fail(&e))?;
    let system = system_prompt(
        &mut journal,
        &session,
        !history.is_empty(),
        (&brain, timezone.as_deref()),
        &cwd,
    )
    .map_err(|e| fail(&e))?;
    let input = [text_message(Role::User, &run.prompt)];
    history.extend_from_slice(&input);

    let client = Client::new(socket, CONNECT_TIMEOUT, IDLE_TIMEOUT);
    let mut chain = Chain::new(vec![Box::new(guard)]);
    let template = Template {
        route: run.route.clone(),
        system: Some(system),
        max_tokens: MAX_TOKENS,
        effort: None,
        session_key: Some(run.session.clone()),
    };
    let outcome = Agent {
        client: &client,
        tools: &tools,
        chain: &mut chain,
        cwd: &cwd,
        template: &template,
        batch_timeout,
        empty_guard,
    }
    .run(
        &history,
        &|step| next.request_id(step),
        &Interrupt::new(),
        max_steps,
        &mut ThreadSleeper,
    );
    journal
        .append(next, &input, &outcome)
        .map_err(|e| fail(&e))?;

    answer(outcome)
}

/// The final assistant text of `outcome`, or an exit code and one stderr line.
fn answer(outcome: Outcome) -> Result<String, (u8, String)> {
    match outcome.ended {
        Ended::Stopped(_) | Ended::MaxSteps => Ok(outcome
            .messages
            .last()
            .filter(|m| m.role == Role::Assistant)
            .map(text)
            .unwrap_or_default()),
        Ended::QuotaExhausted { line, .. } => Err((EXIT_QUOTA, line)),
        ended @ (Ended::Interrupted | Ended::Gateway(_) | Ended::Transport(_)) => {
            Err((EXIT_FAILURE, format!("ulei-engine: {ended}")))
        }
    }
}

/// The prompt files, char limits and skills, and the time zone of the prompt when one is set.
fn brain(config: &Config) -> Result<(Brain, Option<String>), config::Error> {
    let path = |setting: config::Setting| config.path(setting, env(setting.env));
    let count =
        |setting: config::Setting, default| config.count(setting, env(setting.env), default);
    let brain = Brain {
        soul: path(config::SOUL_FILE)?,
        index: path(config::INDEX_FILE)?,
        memory: path(config::MEMORY_FILE)?,
        user: path(config::USER_FILE)?,
        memory_char_limit: count(config::MEMORY_CHAR_LIMIT, DEFAULT_MEMORY_CHAR_LIMIT)?,
        user_char_limit: count(config::USER_CHAR_LIMIT, DEFAULT_USER_CHAR_LIMIT)?,
        skills: Skills {
            dir: path(config::SKILLS_DIR)?,
            disabled: config.names(config::DISABLED_SKILLS, env(config::DISABLED_SKILLS.env))?,
        },
    };
    let timezone = config.text(config::TIMEZONE, env(config::TIMEZONE.env))?;
    Ok((brain, timezone))
}

/// The built-in tools under the settings of `config`.
fn tools(
    config: &Config,
    terminal_output: &Path,
    service: Option<ServiceUnit>,
    skills: &Skills,
) -> Result<Registry, config::Error> {
    let redactor = Redactor {
        enabled: config.switch(config::REDACT_SECRETS, env(config::REDACT_SECRETS.env)),
    };
    let web = web(config)?;
    Ok(Registry::builtin(
        terminal_output,
        redactor,
        service,
        skills,
        web.as_ref(),
    ))
}

/// The settings of the web tools, or none when no helper is configured.
fn web(config: &Config) -> Result<Option<Web>, config::Error> {
    let Some(helper) = config.text(config::WEB_HELPER, env(config::WEB_HELPER.env))? else {
        return Ok(None);
    };
    let path = |setting: config::Setting| config.path(setting, env(setting.env));
    let char_limit = config.count(
        config::WEB_EXTRACT_CHAR_LIMIT,
        env(config::WEB_EXTRACT_CHAR_LIMIT.env),
        DEFAULT_EXTRACT_CHAR_LIMIT,
    )?;
    Ok(Some(Web {
        helper: PathBuf::from(helper),
        base_url: config.https_url(config::WEB_BASE_URL, env(config::WEB_BASE_URL.env))?,
        env_file: path(config::WEB_ENV_FILE)?,
        store_dir: path(config::WEB_STORE)?,
        char_limit: clamp_char_limit(Some(&char_limit.into()), DEFAULT_EXTRACT_CHAR_LIMIT),
        timeout: EXA_CALL_TIMEOUT,
    }))
}

/// The stored system prompt of `session`, or one built now and stored with it.
fn system_prompt(
    journal: &mut Journal,
    session: &Session,
    has_history: bool,
    (brain, timezone): (&Brain, Option<&str>),
    cwd: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(stored) = prompt::restore(session.system_prompt.as_deref(), has_history, cwd) {
        return Ok(stored.to_owned());
    }
    let built = build_prompt(brain, timezone, cwd)?;
    journal.store_system_prompt(session.id, &built)?;
    Ok(built)
}

/// The system prompt of a new session, built now on this host.
fn build_prompt(
    brain: &Brain,
    timezone: Option<&str>,
    cwd: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let zone = match timezone {
        Some(name) => Zone::named(name, &zone::ZONEINFO_DIRS.map(Path::new))?,
        None => Zone::system()?,
    };
    let host = Host::detect(env("HOME"), cwd.to_owned())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(prompt::build(
        brain,
        &host,
        &zone,
        now.as_secs().cast_signed(),
    )?)
}

fn env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}
