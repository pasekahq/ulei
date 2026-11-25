//! `ulei-hive-mcp`: `serve [--config PATH]` serves the hive's five C2 tools over MCP on the
//! loopback address of `[hive_mcp]` (ADR-0071); `--version` prints the version.
//!
//! Exit status: 2 for a refused command line or config, 1 when the address cannot be bound.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ulei_contracts::config::Config;
use ulei_hive_mcp::config::Settings;
use ulei_hive_mcp::server::Server;

const USAGE: &str = "usage: ulei-hive-mcp serve [--config PATH] | --version";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--version"] => {
            let mut out = std::io::stdout().lock();
            let _ = std::io::Write::write_all(
                &mut out,
                format!("ulei-hive-mcp {}\n", env!("CARGO_PKG_VERSION")).as_bytes(),
            );
            return ExitCode::SUCCESS;
        }
        ["serve"] => None,
        ["serve", "--config", path] => Some(PathBuf::from(path)),
        _ => {
            eprintln!("ulei-hive-mcp: {USAGE}");
            return ExitCode::from(2);
        }
    };
    let settings = match Config::read(flag.as_deref())
        .map_err(|e| e.to_string())
        .and_then(|config| Settings::from_config(&config))
    {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("ulei-hive-mcp: {e}");
            return ExitCode::from(2);
        }
    };
    let listener = match TcpListener::bind(settings.listen) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("ulei-hive-mcp: bind {}: {e}", settings.listen);
            return ExitCode::FAILURE;
        }
    };
    let address = listener
        .local_addr()
        .map_or_else(|_| settings.listen.to_string(), |a| a.to_string());
    eprintln!("ulei-hive-mcp: ready: {address}");
    Arc::new(Server::new(settings)).serve(&listener);
    ExitCode::FAILURE
}
