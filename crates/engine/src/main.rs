// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `ulei-engine`: the ecosystem's own agent loop.
//!
//! `run` runs one turn of a journaled session on the qb gateway and prints the answer, and
//! SIGINT, SIGTERM or SIGHUP interrupts that turn; `serve` runs such turns for the C6 callers
//! of its socket; `--version` prints the version; anything else is a usage error.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use ulei_contracts::generated::c4g::{CacheMode, Role};
use ulei_engine::agent::{
    Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, DEFAULT_MAX_STEPS, Ended, Outcome, Template,
    ThreadSleeper, text, text_message,
};
use ulei_engine::compress::{self, Compaction, Compressor, Window};
use ulei_engine::config::{self, Config};
use ulei_engine::garden;
use ulei_engine::gateway::Client;
use ulei_engine::journal::{self, Journal, unix_now};
use ulei_engine::mcp::{self, Server};
use ulei_engine::policy::{
    Chain, Classifier, DEFAULT_TIMEOUT, GuardrailConfig, Hardline, INSTRUCTION_FIELDS, NOTE_FIELDS,
    NoAskGuard, Origin, Policy, Sender, StateDbGuard, TextTemplate, ToolGuardrails, TurnReminder,
};
use ulei_engine::prompt::core::Core;
use ulei_engine::prompt::{self, Brain, DEFAULT_MEMORY_CHAR_LIMIT, DEFAULT_USER_CHAR_LIMIT, Host};
use ulei_engine::serve::{self, Ending, Input, READ_TIMEOUT, Runner};
use ulei_engine::skills::Skills;
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::web::{
    DEFAULT_EXTRACT_CHAR_LIMIT, EXA_CALL_TIMEOUT, Web, clamp_char_limit,
};
use ulei_engine::tools::{
    DuplicateTool, Forget, Interrupt, Memory, Registry, ServiceUnit, SessionSearch,
};
use ulei_engine::wire::c6::TurnOutcome;
use ulei_engine::wire::c8::Session;
use ulei_engine::zone::{self, Zone};

const USAGE: &str = "usage: ulei-engine --version | ulei-engine run --session S --route ALIAS --prompt TEXT [--new] [--channel NAME] [--event] [--config PATH] | ulei-engine serve [--config PATH]\n";

/// Upper bound of output tokens per model step of a route that sets none, and of side turns.
const MAX_TOKENS: u64 = 16_384;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A model may think for minutes before its first event.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

const EXIT_FAILURE: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_QUOTA: u8 = 3;
/// An interrupted turn: 128 plus the number of SIGINT, as a shell reports a command SIGINT
/// ended.
const EXIT_INTERRUPTED: u8 = 130;

/// One turn to run.
struct Run {
    session: String,
    route: Route,
    prompt: String,
    new: bool,
    /// The channel the input came on.
    channel: Option<String>,
    /// The context the caller sent with the prompt for this turn only.
    context: Option<String>,
    sender: Sender,
}

/// The route alias a turn runs on.
enum Route {
    /// This alias, whatever the session stores (`run --route`).
    Given(String),
    /// The alias the session stores, else this default (`serve`).
    Stored(String),
}

/// How a turn ended: its C6 outcome, and its answer or an exit code and one stderr line.
type Finish = (TurnOutcome, Result<String, (u8, String)>);

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
    let (run, config) = match parse(args) {
        Some(Command::Run(run, config)) => (run, config),
        Some(Command::Serve(config)) => {
            let line = serve(config.as_deref());
            let _ = writeln!(std::io::stderr().lock(), "{}", line.replace('\n', " "));
            return ExitCode::from(EXIT_FAILURE);
        }
        None => {
            let _ = std::io::stderr().write_all(USAGE.as_bytes());
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let interrupt = Interrupt::new();
    let result = match Config::read(config.as_deref(), &env).and_then(|config| {
        let cache = prompt_cache(&config)?;
        Ok((config, cache))
    }) {
        Ok((config, cache)) => match interrupt_on_signals(&interrupt) {
            Ok(()) => turn(&config, cache, &run, &interrupt, None).1,
            Err(e) => Err((EXIT_FAILURE, format!("ulei-engine: signal handler: {e}"))),
        },
        Err(e) => Err((EXIT_FAILURE, format!("ulei-engine: {e}"))),
    };
    match result {
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

/// Makes the first SIGINT, SIGTERM or SIGHUP fire `interrupt` (`_signal_handler_q`): the
/// signal handler writes to a socket pair whose reader thread sleeps in `read` until then.
/// Later signals change nothing.
fn interrupt_on_signals(interrupt: &Interrupt) -> io::Result<()> {
    let (mut reader, writer) = UnixStream::pair()?;
    for signal in [SIGTERM, SIGHUP] {
        signal_hook::low_level::pipe::register(signal, writer.try_clone()?)?;
    }
    signal_hook::low_level::pipe::register(SIGINT, writer)?;
    let interrupt = interrupt.clone();
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let mut byte = [0; 1];
            loop {
                match reader.read(&mut byte) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        interrupt.interrupt();
                        return;
                    }
                }
            }
        })?;
    Ok(())
}

/// A command with the config path it names.
enum Command {
    Run(Run, Option<PathBuf>),
    Serve(Option<PathBuf>),
}

/// `serve [--config PATH]`, or `run` with each flag at most once and the required ones
/// present; `None` is a usage error.
fn parse(args: Vec<OsString>) -> Option<Command> {
    match args.as_slice() {
        [serve] if serve == "serve" => return Some(Command::Serve(None)),
        [serve, flag, path] if serve == "serve" && flag == "--config" => {
            return Some(Command::Serve(Some(PathBuf::from(path))));
        }
        _ => {}
    }
    let mut args = args.into_iter();
    if args.next()? != "run" {
        return None;
    }
    let (mut session, mut route, mut prompt, mut config) = (None, None, None, None);
    let (mut channel, mut new, mut event) = (None, false, false);
    while let Some(flag) = args.next() {
        let slot = match flag.to_str()? {
            "--new" if !new => {
                new = true;
                continue;
            }
            "--event" if !event => {
                event = true;
                continue;
            }
            "--session" => &mut session,
            "--route" => &mut route,
            "--prompt" => &mut prompt,
            "--channel" => &mut channel,
            "--config" => &mut config,
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(args.next()?);
    }
    let run = Run {
        session: session?.into_string().ok()?,
        route: Route::Given(route?.into_string().ok()?),
        prompt: prompt?.into_string().ok()?,
        new,
        channel: channel.map(OsString::into_string).transpose().ok()?,
        context: None,
        sender: if event { Sender::Event } else { Sender::Owner },
    };
    Some(Command::Run(run, config.map(PathBuf::from)))
}

/// Serves C6 on the configured socket until `accept` fails; the line saying why it stopped.
fn serve(path: Option<&Path>) -> String {
    let fail = |e: &dyn std::fmt::Display| format!("ulei-engine: {e}");
    let invalid = |setting: config::Setting, rule| config::Error::Item {
        key: setting.key.to_owned(),
        env: setting.env.to_owned(),
        fault: config::Fault::Invalid(rule),
    };
    let required = |config: &Config, setting: config::Setting| {
        config
            .text(setting, env(setting.env))?
            .ok_or(config::Error::Missing(setting))
    };
    let config = match Config::read(path, &env) {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };
    let settings = (|| {
        let socket = config.path(config::C6_SOCKET, env(config::C6_SOCKET.env))?;
        let owner = required(&config, config::OWNER_SESSION)?;
        if journal::check_key(&owner).is_err() {
            return Err(invalid(config::OWNER_SESSION, "must have 1 to 128 chars"));
        }
        let route = required(&config, config::DEFAULT_ROUTE)?;
        if !serve::is_route_alias(&route) {
            return Err(invalid(config::DEFAULT_ROUTE, "must be a qb route alias"));
        }
        let db = config.path(config::JOURNAL, env(config::JOURNAL.env))?;
        // Each turn reads the core again; a fault found here refuses the start, not every turn.
        Core::from_settings(&config, &env)?;
        Ok((socket, owner, route, db, prompt_cache(&config)?))
    })();
    let (socket, owner, route, db, cache) = match settings {
        Ok(settings) => settings,
        Err(e) => return fail(&e),
    };
    let journal = match Journal::open(&db) {
        Ok(journal) => journal,
        Err(e) => return fail(&e),
    };
    let uid = rustix::process::geteuid().as_raw();
    let listener = match serve::bind(&socket, uid) {
        Ok(listener) => listener,
        Err(e) => return fail(&format!("{}: {e}", socket.display())),
    };
    let mut out = std::io::stdout().lock();
    if let Err(e) =
        writeln!(out, "ulei-engine ready {}", socket.display()).and_then(|()| out.flush())
    {
        return fail(&format!("stdout: {e}"));
    }
    drop(out);
    let turns = Turns {
        config,
        route: route.clone(),
        cache,
    };
    let e = serve::Server::new(journal, Arc::new(turns), (owner, route), uid, READ_TIMEOUT)
        .run(&listener);
    fail(&format!("accept: {e}"))
}

/// The turns of `serve` on the route their session stores, else the default route, under the
/// settings read at start.
struct Turns {
    config: Config,
    route: String,
    cache: Option<CacheMode>,
}

impl Runner for Turns {
    fn run(&self, input: &Input, interrupt: &Interrupt, interim: &mut dyn FnMut(&str)) -> Ending {
        let submitted = &input.request;
        let run = Run {
            session: submitted.session.clone(),
            route: Route::Stored(self.route.clone()),
            prompt: submitted.prompt.clone(),
            new: submitted.new.unwrap_or(false),
            channel: submitted.channel.clone(),
            context: submitted.context.clone(),
            sender: input.sender,
        };
        let (outcome, result) = turn(&self.config, self.cache, &run, interrupt, Some(interim));
        let (line, answer) = match (outcome, result) {
            (TurnOutcome::Interrupted, _) => (None, None),
            (_, Ok(answer)) => (None, Some(answer)),
            (_, Err((_, line))) => (Some(line.replace('\n', " ")), None),
        };
        Ending {
            outcome,
            line,
            answer,
        }
    }
}

/// Runs and journals one turn under the settings of `config` and the prompt cache mode `cache`
/// until `interrupt` fires, handing `interim` the turn's interim texts.
fn turn(
    config: &Config,
    cache: Option<CacheMode>,
    run: &Run,
    interrupt: &Interrupt,
    interim: Option<&mut dyn FnMut(&str)>,
) -> Finish {
    match journaled(config, cache, run, interrupt, interim) {
        Ok(outcome) => answer(outcome),
        Err(failure) => (TurnOutcome::Failed, Err(failure)),
    }
}

/// Runs one turn until `interrupt` fires, handing `interim` its interim texts, and appends it to
/// the journal; its outcome, or an exit code and one stderr line for a failure before or after
/// the loop. Every request of the turn, its compactions and its classifier calls carry `cache`.
fn journaled(
    config: &Config,
    cache: Option<CacheMode>,
    run: &Run,
    interrupt: &Interrupt,
    interim: Option<&mut dyn FnMut(&str)>,
) -> Result<Outcome, (u8, String)> {
    let fail = |e: &dyn std::fmt::Display| (EXIT_FAILURE, format!("ulei-engine: {e}"));
    let socket = config
        .path(config::QB_SOCKET, env(config::QB_SOCKET.env))
        .map_err(|e| fail(&e))?;
    let db = config
        .path(config::JOURNAL, env(config::JOURNAL.env))
        .map_err(|e| fail(&e))?;
    let terminal_output = config
        .path(config::TERMINAL_OUTPUT, env(config::TERMINAL_OUTPUT.env))
        .map_err(|e| fail(&e))?;
    let (batch_timeout, max_steps, empty_guard) = loop_settings(config).map_err(|e| fail(&e))?;
    let service = service(config).map_err(|e| fail(&*e))?;
    let (brain, timezone) = brain(config).map_err(|e| fail(&e))?;
    let redactor = redactor(config);
    let (mut tools, servers) =
        tools(config, &terminal_output, service, &brain, redactor).map_err(|e| fail(&*e))?;
    let (mut chain, origin) =
        policies(config, run, &db, &terminal_output, &socket, cache).map_err(|e| fail(&*e))?;
    let cwd = std::env::current_dir().map_err(|e| fail(&format!("working directory: {e}")))?;

    let mut journal = Journal::open(&db).map_err(|e| fail(&e))?;
    let session = if run.new {
        journal.new_session(&run.session)
    } else {
        journal.session(&run.session)
    }
    .map_err(|e| fail(&e))?;
    let route = match &run.route {
        Route::Given(alias) => alias,
        Route::Stored(default) => session.route.as_ref().unwrap_or(default),
    };
    let next = journal.next_turn(session.id).map_err(|e| fail(&e))?;
    let garden = garden(config).map_err(|e| fail(&e))?;
    journal_tools(&mut tools, &db, garden, next, origin).map_err(|e| fail(&e))?;
    let ids = next
        .request_ids()
        .map_err(|e| fail(&format!("request ids: {e}")))?;
    let zone = zone(timezone.as_deref()).map_err(|e| fail(&e))?;
    let max_tokens = config
        .route_positive(config::ROUTE_MAX_TOKENS, route, &env)
        .map_err(|e| fail(&e))?
        .unwrap_or(MAX_TOKENS);
    let (mut compressor, window) = compressor(
        config,
        &socket,
        (redactor, zone.clone()),
        (route, max_tokens, cache),
        (&journal, session.id),
    )
    .map_err(|e| fail(&*e))?;
    let mut history = journal.messages(session.id).map_err(|e| fail(&e))?;
    let system = system_prompt(
        &mut journal,
        &session,
        !history.is_empty(),
        (&brain, &zone),
        &cwd,
    )
    .map_err(|e| fail(&e))?;
    let input = [text_message(Role::User, &run.prompt)];
    history.extend_from_slice(&input);

    let sessions = connect_mcp(&servers, &mut tools, interrupt);
    let client = Client::new(socket, CONNECT_TIMEOUT, IDLE_TIMEOUT);
    let template = Template {
        route: route.clone(),
        system: Some(system),
        max_tokens,
        effort: None,
        session_key: Some(run.session.clone()),
        cache,
    };
    let outcome = Agent {
        client: &client,
        tools: &tools,
        chain: &mut chain,
        cwd: &cwd,
        template: &template,
        batch_timeout,
        empty_guard,
        origin,
        compaction: Some(Compaction {
            compressor: &mut compressor,
            window,
            clock: &unix_now,
        }),
        context: run.context.as_deref(),
        interim: interim.map(|f| f as &mut dyn FnMut(&str)),
    }
    .run(
        &history,
        &|step| ids.step(step),
        interrupt,
        max_steps,
        &mut ThreadSleeper,
    );
    sessions.close();
    journal
        .append(next, &input, &outcome, unix_now())
        .map_err(|e| fail(&e))?;
    Ok(outcome)
}

/// The service unit of the engine under the settings of `config`, when it runs in one.
fn service(config: &Config) -> Result<Option<ServiceUnit>, Box<dyn std::error::Error>> {
    let unit = config.text(config::SERVICE_UNIT, env(config::SERVICE_UNIT.env))?;
    let under_systemd = env("INVOCATION_ID").is_some_and(|id| !id.is_empty());
    Ok(ServiceUnit::discover(
        unit.as_deref(),
        under_systemd,
        || std::fs::read_to_string("/proc/self/cgroup"),
    )?)
}

/// The prompt cache mode of every request under the settings of `config`.
fn prompt_cache(config: &Config) -> Result<Option<CacheMode>, config::Error> {
    config.cache_mode(config::PROMPT_CACHE, env(config::PROMPT_CACHE.env))
}

/// The tool batch deadline, the step limit and the empty-reply guard under the settings of
/// `config`.
fn loop_settings(config: &Config) -> Result<(Option<Duration>, usize, bool), config::Error> {
    let batch_timeout = config.timeout(
        config::CONCURRENT_TOOL_TIMEOUT,
        env(config::CONCURRENT_TOOL_TIMEOUT.env),
        DEFAULT_CONCURRENT_TOOL_TIMEOUT,
    )?;
    let max_steps = config.limit(
        config::MAX_STEPS,
        env(config::MAX_STEPS.env),
        DEFAULT_MAX_STEPS,
    )?;
    let empty_guard = config.switch(
        config::EMPTY_RESPONSE_GUARD,
        env(config::EMPTY_RESPONSE_GUARD.env),
        true,
    );
    Ok((batch_timeout, max_steps, empty_guard))
}

/// The compressor bound to `session` of `journal` with the usage of its last step, and the
/// window of `route` whose steps reserve `max_tokens`, under the settings of `config`; its
/// summary turns use the gateway at `socket` with the prompt cache mode `cache`.
fn compressor(
    config: &Config,
    socket: &Path,
    (redactor, zone): (Redactor, Zone),
    (route, max_tokens, cache): (&str, u64, Option<CacheMode>),
    (journal, session): (&Journal, u64),
) -> Result<(Compressor, Option<Window>), Box<dyn std::error::Error>> {
    let settings = compress::Settings::from_settings(config, &env, MAX_TOKENS, cache)?;
    let window = config
        .route_positive(config::ROUTE_CONTEXT_LENGTH, route, &env)?
        .map(|context_length| settings.window(context_length, max_tokens));
    let client = Client::new(socket, CONNECT_TIMEOUT, IDLE_TIMEOUT);
    let mut compressor = Compressor::new(client, settings, redactor, zone);
    compressor.bind(session);
    if let Some(usage) = journal.last_usage(session)? {
        compressor.observe(&usage, window);
    }
    Ok((compressor, window))
}

/// The policy chain under the settings of `config` after `hardline`, which saves its refused
/// payloads under `terminal_output` and masks the secrets of its warning, and the
/// `state-db-guard` of the journal `db`, and the origin of the input of `run`; the classifier of
/// `no-ask-guard` uses the gateway at `socket` with the prompt cache mode `cache`.
fn policies(
    config: &Config,
    run: &Run,
    db: &Path,
    terminal_output: &Path,
    socket: &Path,
    cache: Option<CacheMode>,
) -> Result<(Chain, Origin), Box<dyn std::error::Error>> {
    let malformed = config::HARDLINE_MALFORMED_MODE;
    let hardline = Hardline::new(
        env("HOME").map(PathBuf::from),
        terminal_output.join("blocked-scripts"),
        config.malformed_grep(malformed, env(malformed.env))?,
        redactor(config),
    );
    let guard = StateDbGuard::new(db.to_path_buf())?;
    let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(hardline), Box::new(guard)];
    let reminder = config::TURN_REMINDER_FILE;
    if let Some(text) = config.file_text(reminder, env(reminder.env))? {
        policies.push(Box::new(TurnReminder::new(text)));
    }
    if let Some(guard) = no_ask_guard(config, socket, &run.session, cache)? {
        policies.push(Box::new(guard));
    }
    policies.push(Box::new(ToolGuardrails::new(
        GuardrailConfig::from_settings(config, &env)?,
    )));
    let main = config.text(config::MAIN_CHANNEL, env(config::MAIN_CHANNEL.env))?;
    let origin = Origin::of(run.sender, run.channel.as_deref(), main.as_deref());
    Ok((Chain::new(policies), origin))
}

/// The `no-ask-guard` policy of session `session` under the settings of `config` whose
/// classifier calls carry `cache`, or none when no classifier route is set.
fn no_ask_guard(
    config: &Config,
    socket: &Path,
    session: &str,
    cache: Option<CacheMode>,
) -> Result<Option<NoAskGuard>, config::Error> {
    let Some(route) = config.text(
        config::NO_ASK_GUARD_ROUTE,
        env(config::NO_ASK_GUARD_ROUTE.env),
    )?
    else {
        return Ok(None);
    };
    let template = |setting: config::Setting, fields: &[&str]| {
        let text = config
            .file_text(setting, env(setting.env))?
            .ok_or(config::Error::Missing(setting))?;
        TextTemplate::parse(&text, fields)
            .map_err(|reason| config::Error::Template(setting, reason))
    };
    let note = template(config::NO_ASK_GUARD_NOTE_FILE, NOTE_FIELDS)?;
    let instructions = template(config::NO_ASK_GUARD_INSTRUCTIONS_FILE, INSTRUCTION_FIELDS)?;
    let log = config.path(config::NO_ASK_GUARD_LOG, env(config::NO_ASK_GUARD_LOG.env))?;
    let timeout = config
        .timeout(
            config::NO_ASK_GUARD_TIMEOUT,
            env(config::NO_ASK_GUARD_TIMEOUT.env),
            DEFAULT_TIMEOUT,
        )?
        .unwrap_or(config::MAX_TIMEOUT);
    let classifier = Classifier {
        client: Client::new(socket, CONNECT_TIMEOUT, timeout),
        route,
        instructions,
        max_tokens: MAX_TOKENS,
        cache,
        timeout,
    };
    Ok(Some(NoAskGuard::new(
        classifier,
        note,
        log,
        session.to_owned(),
    )))
}

/// The C6 outcome of `outcome`, and its final assistant text or an exit code and one stderr
/// line.
fn answer(outcome: Outcome) -> Finish {
    let reply = || {
        Ok(outcome
            .messages
            .last()
            .filter(|m| m.role == Role::Assistant)
            .map(text)
            .unwrap_or_default())
    };
    match outcome.ended {
        Ended::Stopped(_) => (TurnOutcome::Completed, reply()),
        Ended::MaxSteps => (TurnOutcome::MaxSteps, reply()),
        Ended::Halted => (TurnOutcome::Halted, reply()),
        Ended::QuotaExhausted { line, .. } => {
            (TurnOutcome::QuotaExhausted, Err((EXIT_QUOTA, line)))
        }
        Ended::ContextExhausted { line, .. } => {
            (TurnOutcome::ContextExhausted, Err((EXIT_FAILURE, line)))
        }
        ended @ Ended::Interrupted => (
            TurnOutcome::Interrupted,
            Err((EXIT_INTERRUPTED, format!("ulei-engine: {ended}"))),
        ),
        ended @ (Ended::Gateway(_) | Ended::Transport(_)) => (
            TurnOutcome::Failed,
            Err((EXIT_FAILURE, format!("ulei-engine: {ended}"))),
        ),
    }
}

/// The prompt files, char limits, skills and core, and the time zone of the prompt when one is set.
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
            disabled: config.names(config::DISABLED_SKILLS, env(config::DISABLED_SKILLS.env)),
        },
        skills_index: config.switch(config::SKILLS_INDEX, env(config::SKILLS_INDEX.env), false),
        core: Core::from_settings(config, &env)?,
    };
    let timezone = config.text(config::TIMEZONE, env(config::TIMEZONE.env))?;
    Ok((brain, timezone))
}

/// The secret redaction of tool output and compaction under the settings of `config`.
fn redactor(config: &Config) -> Redactor {
    Redactor {
        enabled: config.switch(
            config::REDACT_SECRETS,
            env(config::REDACT_SECRETS.env),
            true,
        ),
    }
}

/// The built-in tools with `memory` over the stores of `brain`, and the MCP servers under the
/// settings of `config`.
fn tools(
    config: &Config,
    terminal_output: &Path,
    service: Option<ServiceUnit>,
    brain: &Brain,
    redactor: Redactor,
) -> Result<(Registry, Vec<Server>), Box<dyn std::error::Error>> {
    let web = web(config)?;
    let servers = Server::from_settings(config, &env)?;
    let skills = &brain.skills;
    let mut registry = Registry::builtin(terminal_output, redactor, service, skills, web.as_ref());
    registry.register(Box::new(Memory::new(brain)))?;
    Ok((registry, servers))
}

/// Registers `session_search` and `forget` over the journal file `db` and the garden of
/// `garden` for turn `turn`, whose prompt has `origin`.
fn journal_tools(
    tools: &mut Registry,
    db: &Path,
    garden: Option<garden::Client>,
    turn: journal::Turn,
    origin: Origin,
) -> Result<(), DuplicateTool> {
    tools.register(Box::new(SessionSearch::new(db.to_path_buf(), turn.session)))?;
    tools.register(Box::new(Forget::new(
        db.to_path_buf(),
        garden,
        turn,
        origin,
    )))
}

/// The client of the garden `garden.socket` names, or none when it is unset.
fn garden(config: &Config) -> Result<Option<garden::Client>, config::Error> {
    match config.path(config::GARDEN_SOCKET, env(config::GARDEN_SOCKET.env)) {
        Ok(socket) => Ok(Some(garden::Client::new(socket))),
        Err(config::Error::Missing(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Registers the tools of every server of `servers` that connects before `interrupt` fires into
/// `tools`; each warning goes to stderr as one line.
fn connect_mcp(servers: &[Server], tools: &mut Registry, interrupt: &Interrupt) -> mcp::Sessions {
    let clock: Arc<dyn mcp::Clock> = Arc::new(mcp::SystemClock);
    let (sessions, warnings) = mcp::connect(servers, tools, &clock, interrupt);
    for warning in warnings {
        let _ = writeln!(
            std::io::stderr().lock(),
            "ulei-engine: {}",
            warning.replace('\n', " ")
        );
    }
    sessions
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
    (brain, zone): (&Brain, &Zone),
    cwd: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(stored) = prompt::restore(session.system_prompt.as_deref(), has_history, cwd) {
        return Ok(stored.to_owned());
    }
    let built = build_prompt(brain, zone, cwd)?;
    journal.store_system_prompt(session.id, &built)?;
    Ok(built)
}

/// The system prompt of a new session, built now on this host.
fn build_prompt(
    brain: &Brain,
    zone: &Zone,
    cwd: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let host = Host::detect(env("HOME"), cwd.to_owned())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(prompt::build(
        brain,
        &host,
        zone,
        now.as_secs().cast_signed(),
    )?)
}

/// The zone named `timezone`, else the host's.
fn zone(timezone: Option<&str>) -> Result<Zone, Box<dyn std::error::Error>> {
    Ok(match timezone {
        Some(name) => Zone::named(name, &zone::ZONEINFO_DIRS.map(Path::new))?,
        None => Zone::system()?,
    })
}

fn env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}
