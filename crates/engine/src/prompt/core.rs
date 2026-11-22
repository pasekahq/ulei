//! The core tier of the system prompt: the safeguards and the meta-skills, shipped as data with
//! ids. `[engine.core.<id>]` replaces the text of an id with a file or disables it.

use std::ffi::OsString;
use std::path::Path;

use crate::config::{self, Config, Fault};
use crate::tools::py_strip;

/// Whether an item is a safeguard or a meta-skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A rule the agent keeps unless the owner says otherwise.
    Safeguard,
    /// How the agent finds what the prompt does not hold.
    MetaSkill,
}

/// One safeguard or meta-skill of the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Its id in `engine.core.<id>`.
    pub id: &'static str,
    /// Safeguard or meta-skill.
    pub kind: Kind,
    /// The text the prompt carries.
    pub text: String,
}

/// The id of the meta-skill whose default text points to [`config::TASK_BOARD_FILE`].
const TASK_DELEGATION: &str = "task_delegation";

/// The shipped items in prompt order; the task-delegation text is set by [`Core::defaults`].
const DEFAULTS: [(&str, Kind, &str); 6] = [
    (
        "no_unasked_messages",
        Kind::Safeguard,
        "Never write to other people without a direct command from the owner: no email, no \
         messenger message, no issue in someone else's public repository, no blog post, nothing \
         of the kind.",
    ),
    (
        "no_unfiled_promises",
        Kind::Safeguard,
        "Never say that you will do something later unless a task for it is actually filed.",
    ),
    (
        "no_irreversible_deletion",
        Kind::Safeguard,
        "Never delete data irreversibly without a direct command from the owner. The exception \
         is a routine the owner has already agreed to, such as the weekly compaction of \
         correspondence.",
    ),
    (
        "search",
        Kind::MetaSkill,
        "Search and retrieval: what this prompt does not hold, facts about the owner, notes, \
         preferences, skills and tools, you find yourself. Find files and text with search_files \
         and read them with read_file. List the skills with skills_list and load the one a task \
         needs with skill_view before you start. Use web_search and the retrieval tools of your \
         MCP servers when you have them. Look things up before you answer from memory or ask the \
         owner.",
    ),
    (
        "terminal_and_files",
        Kind::MetaSkill,
        "Terminal and files: run commands with terminal; read files with read_file and change \
         them with patch or write_file. Read a file before you change it, and check what a \
         command really returned before you report it.",
    ),
    (TASK_DELEGATION, Kind::MetaSkill, ""),
];

const TASK_BOARD: &str = "Task delegation: to file a task or hand one out, read {task_board_file} \
                          with read_file. It gives the address of the task board and the commands \
                          to use.";

const NO_TASK_BOARD: &str = "Task delegation: no task board is configured, so you cannot file a \
                             task or hand one out. Say so instead.";

/// The enabled safeguards and meta-skills, in shipped order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Core {
    /// The items the prompt carries.
    pub items: Vec<Item>,
}

impl Core {
    /// Every shipped item with its default text; the task-delegation text points to `task_board`
    /// or says that no task board is configured.
    pub fn defaults(task_board: Option<&Path>) -> Self {
        let items = DEFAULTS
            .iter()
            .map(|&(id, kind, text)| Item {
                id,
                kind,
                text: match (id, task_board) {
                    (TASK_DELEGATION, Some(path)) => {
                        TASK_BOARD.replace("{task_board_file}", &path.display().to_string())
                    }
                    (TASK_DELEGATION, None) => NO_TASK_BOARD.to_owned(),
                    _ => text.to_owned(),
                },
            })
            .collect();
        Self { items }
    }

    /// The defaults under the settings of `config`: [`config::TASK_BOARD_FILE`], then for each
    /// id [`config::CORE_ENABLED`] and [`config::CORE_FILE`], whose stripped text replaces it.
    ///
    /// # Errors
    /// [`config::Error::UnknownItem`] for a key or section of an id that is not shipped,
    /// [`config::Error::Item`] for an empty setting or a blank file, and
    /// [`config::Error::ItemUnreadable`] for a file that is not readable UTF-8 text.
    pub fn from_settings(
        config: &Config,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Self, config::Error> {
        let ids = DEFAULTS.map(|(id, _, _)| id);
        config.known_items(config::CORE_FILE.section, &ids)?;
        let task_board =
            match config.path(config::TASK_BOARD_FILE, env(config::TASK_BOARD_FILE.env)) {
                Ok(path) => Some(path),
                Err(config::Error::Missing(_)) => None,
                Err(e) => return Err(e),
            };
        let mut items = Vec::new();
        for mut item in Self::defaults(task_board.as_deref()).items {
            if !config.item_switch(config::CORE_ENABLED, item.id, env, true) {
                continue;
            }
            if let Some(text) = config.item_file_text(config::CORE_FILE, item.id, env)? {
                let text = py_strip(&text);
                if text.is_empty() {
                    return Err(
                        config::CORE_FILE.error(item.id, Fault::Invalid("names a blank file"))
                    );
                }
                text.clone_into(&mut item.text);
            }
            items.push(item);
        }
        Ok(Self { items })
    }

    /// The tier text: the safeguards under `# Safeguards`, then the meta-skills under
    /// `# Meta-skills`, one item per line; a heading without items is left out.
    pub fn render(&self) -> String {
        let section = |kind: Kind, heading: &str| {
            let texts: Vec<&str> = self
                .items
                .iter()
                .filter(|item| item.kind == kind)
                .map(|item| py_strip(&item.text))
                .collect();
            if texts.is_empty() {
                String::new()
            } else {
                format!("{heading}\n{}", texts.join("\n"))
            }
        };
        [
            section(Kind::Safeguard, "# Safeguards"),
            section(Kind::MetaSkill, "# Meta-skills"),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
    }
}
