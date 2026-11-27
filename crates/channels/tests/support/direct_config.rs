//! The `ulei.toml` of the one-shot subcommands that reach the owner directly.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::fake_bot_api::FakeBotApi;
use super::scratch::Scratch;

/// The bot token the written config names.
pub(crate) const TOKEN: &str = "123456:SECRET-TOKEN-OF-THE-NOTIFY-BOT";

/// Writes the token file and a `ulei.toml` naming the engine's C6 socket `dir.socket("c6")`,
/// the inbox's C1 socket `dir.socket("c1")` and the fake Bot API; `telegram` is the
/// `[telegram]` section after `base_url`, with `TOKEN_FILE` standing for the token file's path.
pub(crate) fn write_config(dir: &Scratch, fake: &FakeBotApi, telegram: &str) -> PathBuf {
    let token = dir.path("token");
    fs::write(&token, format!("{TOKEN}\n")).expect("token");
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("chmod");
    let text = format!(
        "[engine]\nc6_socket = \"{c6}\"\n\n[inbox]\nsocket = \"{c1}\"\n\n[telegram]\n\
         base_url = \"{base}\"\n{telegram}",
        c6 = dir.socket("c6").display(),
        c1 = dir.socket("c1").display(),
        base = fake.base_url(),
        telegram = telegram.replace("TOKEN_FILE", &token.display().to_string()),
    );
    let path = dir.path("ulei.toml");
    fs::write(&path, text).expect("config");
    path
}

/// The `[telegram]` lines of the token file and the owner ids `ids`.
pub(crate) fn owners(ids: &str) -> String {
    format!("token_file = \"TOKEN_FILE\"\nowner_ids = \"{ids}\"\n")
}
