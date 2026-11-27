//! `ulei-hive-mcp`: the hive's typed tools of C2 (`hive_create`, `hive_list`, `hive_show`,
//! `hive_steer`, `hive_cancel`, `hive_resolve`, `hive_reminder` and the read-only snapshots) served to the engine over MCP Streamable HTTP on a loopback
//! address, each call run as the hive's CLI with an argv built from validated typed fields
//! (ADR-0071).

pub mod config;
pub mod hive;
pub mod http;
pub mod server;
pub mod tools;
