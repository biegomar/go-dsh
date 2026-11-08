//! go-dsh: agent-first CLI to run DeepSeek Harness (dsh) sessions in parallel.
//! Runs on Windows, macOS and Linux.

pub mod cli;
pub mod collect;
pub mod config;
pub mod dsh;
pub mod events;
pub mod paths;
pub mod profile;
pub mod runner;
pub mod skills;
pub mod store;
pub mod supervisor;
pub mod templates;
pub mod util;
pub mod web;
