//! `ulei-engine`: the ecosystem's own agent loop.
//!
//! `run` runs one turn of a journaled session on the qb gateway and prints the answer;
//! `--version` prints the version; anything else is a usage error.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use ulei_contracts::generated::c4g::{Block, BlockType, Message, Role};
use ulei_engine::agent::{Agent, Ended, Template, ThreadSleeper, block};
use ulei_engine::config::{self, Config};
use ulei_engine::gateway::Client;
use ulei_engine::journal::Journal;
use ulei_engine::policy::{Chain, StateDbGuard};
use ulei_engine::tools::{Interrupt, Registry};

const USAGE: &str = "usage: ulei-engine --version | ulei-engine run --session S --route ALIAS --prompt TEXT [--new] [--config PATH]\n";

/// Most steps of one turn that send a request.
const MAX_STEPS: usize = 64;
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
    let input = [Message {
        role: Role::User,
        content: vec![Block {
            text: Some(run.prompt.clone()),
            ..block(BlockType::Text)
        }],
    }];
    history.extend_from_slice(&input);

    let client = Client::new(socket, CONNECT_TIMEOUT, IDLE_TIMEOUT);
    let tools = Registry::builtin();
    let mut chain = Chain::new(vec![Box::new(guard)]);
    let template = Template {
        route: run.route.clone(),
        system: None,
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
    }
    .run(
        &history,
        &|step| next.request_id(step),
        &Interrupt::new(),
        MAX_STEPS,
        &mut ThreadSleeper,
    );
    journal
        .append(next, &input, &outcome)
        .map_err(|e| fail(&e))?;

    match outcome.ended {
        Ended::Stopped(_) => Ok(outcome
            .messages
            .last()
            .filter(|m| m.role == Role::Assistant)
            .map(answer)
            .unwrap_or_default()),
        Ended::QuotaExhausted { line, .. } => Err((EXIT_QUOTA, line)),
        Ended::MaxSteps => Err(fail(&format!("the turn reached {MAX_STEPS} steps"))),
        Ended::Interrupted => Err(fail(&"the turn was interrupted")),
        Ended::Gateway(error) => Err(fail(&format!(
            "gateway error {}: {}",
            error.class.as_str(),
            error.message.as_deref().unwrap_or("no message")
        ))),
        Ended::Transport(error) => Err(fail(&error)),
    }
}

fn env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The text blocks of `message`, joined.
fn answer(message: &Message) -> String {
    message
        .content
        .iter()
        .filter(|b| b.r#type == BlockType::Text)
        .filter_map(|b| b.text.as_deref())
        .collect()
}
