//! `ulei-priv`: run one of the root commands the host's policy allows (ADR-0073).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use ulei_priv::{DEFAULT_AUDIT, DEFAULT_POLICY, DEFAULT_SOCKET, client, serve};

const HELP: &str = "\
ulei-priv: run a root command from the host's fixed list, without sudo.

  ulei-priv list               the commands you may run, one per line
  ulei-priv run <id> [<arg>]   run one; its output and exit code are the command's
  ulei-priv serve              the root daemon (systemd unit ulei-priv.service)

Only the listed ids and arguments run; anything else is refused with
`ulei-priv: refused: <reason>` and exit 77; exit 69: the daemon is not reachable.
Every call is audited. Options: --socket PATH (default /run/ulei-priv/priv.sock);
serve also --policy PATH (/etc/ulei/priv.toml), --audit PATH
(/var/log/ulei-priv/audit.jsonl).
";

fn usage() -> ExitCode {
    eprint!("{HELP}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut socket = PathBuf::from(DEFAULT_SOCKET);
    let mut policy = PathBuf::from(DEFAULT_POLICY);
    let mut audit = PathBuf::from(DEFAULT_AUDIT);
    let mut words: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let slot = match a.as_str() {
            "--socket" => &mut socket,
            "--policy" => &mut policy,
            "--audit" => &mut audit,
            "-h" | "--help" | "help" if words.is_empty() => {
                let _ = std::io::stdout().write_all(HELP.as_bytes());
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" if words.is_empty() => {
                let _ = writeln!(std::io::stdout(), "ulei-priv {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => {
                words.push(a);
                continue;
            }
        };
        match args.next() {
            Some(v) => *slot = PathBuf::from(v),
            None => return usage(),
        }
    }
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["serve"] => {
            let opts = serve::Options {
                policy,
                socket,
                audit,
            };
            match serve::serve(&opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(why) => {
                    eprintln!("ulei-priv: {why}");
                    ExitCode::from(2)
                }
            }
        }
        ["list"] => ExitCode::from(client::list(&socket)),
        ["run", id, rest @ ..] => {
            let rest: Vec<String> = rest.iter().map(|s| (*s).to_owned()).collect();
            ExitCode::from(client::run(&socket, id, &rest))
        }
        _ => usage(),
    }
}
