//! `ulei-priv`: the privileged helper (ADR-0073). `serve` runs as root and executes a fixed list
//! of commands for allowed uids over a Unix socket, audited; `list` and `run` are its client.

pub mod audit;
pub mod child;
pub mod client;
pub mod policy;
pub mod serve;
pub mod wire;

/// The policy `serve` reads without `--policy`.
pub const DEFAULT_POLICY: &str = "/etc/ulei/priv.toml";
/// The socket without `--socket`.
pub const DEFAULT_SOCKET: &str = "/run/ulei-priv/priv.sock";
/// The audit file without `--audit`.
pub const DEFAULT_AUDIT: &str = "/var/log/ulei-priv/audit.jsonl";
