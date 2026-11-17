// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Owner commands `/new`, `/stop`, `/status`, `/model`, the engine seam they act through, and
//! the replies the owner sees.

use std::fmt;

use super::localtime::Zone;
use super::prepare::Turn;

/// A parsed owner command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/new` (or `/reset`): start a fresh session.
    New,
    /// `/stop`: interrupt the running turn, keep the session.
    Stop,
    /// `/status`: show the session state.
    Status,
    /// `/model [alias]`: switch the model route, or show the current one.
    Model(Option<String>),
}

impl Command {
    /// Parses `/name[@bot] args`; `None` for other commands.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim_start();
        let (head, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let name = head.strip_prefix('/')?.to_lowercase();
        let name = name.split('@').next().unwrap_or_default();
        let args = args.trim();
        match name {
            "new" | "reset" => Some(Self::New),
            "stop" => Some(Self::Stop),
            "status" => Some(Self::Status),
            "model" => Some(Self::Model((!args.is_empty()).then(|| args.to_owned()))),
            _ => None,
        }
    }
}

/// A failed engine call; `message` is shown to the owner where the reference shows errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    /// Short reason.
    pub message: String,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EngineError {}

/// What `/stop` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupted {
    /// A turn was running and is stopped.
    Running,
    /// A turn was accepted but had not started.
    Pending,
    /// Nothing was running.
    Nothing,
}

/// Session state for `/status`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EngineStatus {
    /// Session id.
    pub session_id: String,
    /// Session title.
    pub title: Option<String>,
    /// Unix time the session was created.
    pub created: i64,
    /// Unix time of the last activity.
    pub last_activity: i64,
    /// Model route alias.
    pub model: Option<String>,
    /// Prompt tokens of the last call.
    pub context_used: u64,
    /// Context window of the route.
    pub context_total: u64,
    /// Tokens billed over the session.
    pub tokens_billed: u64,
    /// A turn is running.
    pub running: bool,
    /// Turns waiting in the queue.
    pub queued: u64,
}

/// Result of `/new`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewSession {
    /// A previous session existed and was reset.
    pub reset_existing: bool,
    /// Model route alias of the new session.
    pub model: Option<String>,
    /// Context window of that route.
    pub context_total: Option<u64>,
}

/// Result of `/model <alias>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteSwitch {
    /// The alias now in use.
    pub alias: String,
    /// Its context window.
    pub context_total: Option<u64>,
}

/// The agent engine as the channel uses it.
pub trait Engine {
    /// Queues one turn.
    ///
    /// Seam: C6 `submit_turn`.
    ///
    /// # Errors
    /// The engine did not accept the turn.
    fn submit_turn(&mut self, turn: &Turn) -> Result<(), EngineError>;

    /// Interrupts the running turn at once, keeping the session.
    ///
    /// Seam: C6 `interrupt`.
    ///
    /// # Errors
    /// The engine could not be reached.
    fn interrupt(&mut self) -> Result<Interrupted, EngineError>;

    /// Session state.
    ///
    /// Seam: C6 `status`.
    ///
    /// # Errors
    /// The engine could not be reached.
    fn status(&mut self) -> Result<EngineStatus, EngineError>;

    /// Switches the model route of the session to `alias`.
    ///
    /// Seam: C6 `set_route`.
    ///
    /// # Errors
    /// The alias is unknown (the message is shown to the owner) or the engine is unreachable.
    fn set_route(&mut self, alias: &str) -> Result<RouteSwitch, EngineError>;

    /// Ends the session and starts a fresh one.
    ///
    /// Seam: C6 new session.
    ///
    /// # Errors
    /// The engine could not be reached.
    fn new_session(&mut self) -> Result<NewSession, EngineError>;
}

/// Runs a command against the engine and returns the reply for the owner.
///
/// # Errors
/// The engine call failed (for `/model <alias>` the failure is the reply instead).
pub fn run_command(
    engine: &mut dyn Engine,
    command: &Command,
    zone: &Zone,
) -> Result<String, EngineError> {
    Ok(match command {
        Command::New => {
            let s = engine.new_session()?;
            let header = if s.reset_existing {
                "✨ Сеанс сброшен! Начинаем с чистого листа."
            } else {
                "✨ Новый сеанс запущен!"
            };
            let mut info = Vec::new();
            if let Some(model) = &s.model {
                info.push(format!("◆ Model: `{model}`"));
            }
            if let Some(total) = s.context_total {
                info.push(format!("◆ Context: {} tokens", context_display(total)));
            }
            if info.is_empty() {
                header.to_owned()
            } else {
                format!("{header}\n\n{}", info.join("\n"))
            }
        }
        Command::Stop => match engine.interrupt()? {
            Interrupted::Running => "⚡ Остановлено. Вы можете продолжить этот сеанс.".to_owned(),
            Interrupted::Pending => {
                "⚡ Остановлено. Агент ещё не начинал — вы можете продолжить этот сеанс.".to_owned()
            }
            Interrupted::Nothing => "Нет активной задачи для остановки.".to_owned(),
        },
        Command::Status => status_text(&engine.status()?, zone),
        Command::Model(None) => {
            let current = engine
                .status()?
                .model
                .unwrap_or_else(|| "unknown".to_owned());
            format!("Текущая: `{current}`\n\n`/model <name>` — сменить модель")
        }
        Command::Model(Some(alias)) => match engine.set_route(alias) {
            Ok(switch) => {
                let mut lines = vec![format!("Модель изменена на `{}`", switch.alias)];
                if let Some(total) = switch.context_total {
                    lines.push(format!("Контекст: {} токенов", thousands(total)));
                }
                lines.push(
                    "_(только для этого сеанса — добавьте `--global`, чтобы сохранить)_".to_owned(),
                );
                lines.join("\n")
            }
            Err(e) => format!("Ошибка: {}", e.message),
        },
    })
}

fn status_text(s: &EngineStatus, zone: &Zone) -> String {
    let mut lines = vec![
        "📊 **Состояние канала**".to_owned(),
        String::new(),
        format!("**ID сеанса:** `{}`", s.session_id),
    ];
    if let Some(title) = s.title.as_deref().filter(|t| !t.is_empty()) {
        lines.push(format!("**Название:** {title}"));
    }
    lines.push(format!("**Создано:** {}", zone.local(s.created).ymd_hm()));
    lines.push(format!(
        "**Последняя активность:** {}",
        zone.local(s.last_activity).ymd_hm()
    ));
    if let Some(model) = &s.model {
        lines.push(format!("**Model:** `{model}`"));
    }
    if s.context_total > 0 {
        let pct = ((u128::from(s.context_used) * 100 + u128::from(s.context_total) / 2)
            / u128::from(s.context_total))
        .min(100);
        lines.push(format!(
            "**Context:** {} / {} ({pct}%)",
            thousands(s.context_used),
            thousands(s.context_total)
        ));
    } else if s.context_used > 0 {
        lines.push(format!(
            "**Context:** ~{} tokens",
            thousands(s.context_used)
        ));
    }
    lines.push(format!(
        "**Lifetime tokens billed:** {} _(not your current context size; use `/context`)_",
        thousands(s.tokens_billed)
    ));
    lines.push(format!(
        "**Агент активен:** {}",
        if s.running { "Да ⚡" } else { "Нет" }
    ));
    if s.queued > 0 {
        lines.push(format!("**Очередь продолжений:** {}", s.queued));
    }
    lines.push(String::new());
    lines.push("**Подключённые платформы:** telegram".to_owned());
    lines.join("\n")
}

/// `1234567` as `1,234,567`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1.0M`, `200K` or the plain number.
fn context_display(n: u64) -> String {
    if n >= 1_000_000 {
        let tenths = (n + 50_000) / 100_000;
        format!("{}.{}M", tenths / 10, tenths % 10)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}
