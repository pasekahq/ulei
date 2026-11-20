//! Wire types of the ulei ecosystem, generated from the language-neutral schemas in
//! `contracts/` (JSON Schema 2020-12 and `OpenAPI` 3.1), and the two decoders of C9.
//!
//! `contracts/` is the single source of truth. Everything under `src/generated/` is written
//! by `cargo xtask gen-contracts`; CI regenerates it and fails on any diff. Do not edit it by
//! hand. Hand-written code in this crate stays small (budget: 1 500 lines, checked by
//! `cargo xtask arch`): thin wire helpers plus the two decoders of C9, [`config`] (finds
//! `ulei.toml`, reads it and checks it against `c9::KEYS`) and [`env_file`] (reads an env-file
//! the config names). These two are the only code in the crate that reads a file or the
//! environment; nothing else in it holds logic.
//!
//! This crate depends on no other workspace crate. Python components (the current agent
//! runtime, the current Hive, the A2A gate) talk to Rust code only over the wire using these
//! same schemas.

#![forbid(unsafe_code)]

pub mod config;
pub mod env_file;
pub mod generated;

pub use generated::{CONTRACTS, Contract};
