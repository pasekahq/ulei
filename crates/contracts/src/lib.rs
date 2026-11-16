//! Wire types of the ulei ecosystem, generated from the language-neutral schemas in
//! `contracts/` (JSON Schema 2020-12 and `OpenAPI` 3.1).
//!
//! `contracts/` is the single source of truth. Everything under `src/generated/` is written
//! by `cargo xtask gen-contracts`; CI regenerates it and fails on any diff. Do not edit it by
//! hand. Hand-written code in this crate stays small (budget: 1 500 lines, checked by
//! `cargo xtask arch`) and contains no business logic.
//!
//! This crate depends on no other workspace crate. Python components (the current agent
//! runtime, the current Hive, the A2A gate) talk to Rust code only over the wire using these
//! same schemas.

#![forbid(unsafe_code)]

pub mod generated;

pub use generated::{CONTRACTS, Contract};
