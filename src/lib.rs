//! `duet` is both a GTK application (`src/main.rs`) and a lightweight CLI
//! control client (`src/bin/duetctl.rs`). Declaring every module here, once,
//! is what lets both binaries share the exact same orchestration/application
//! services instead of the CLI reimplementing anything the GUI already does
//! — see `.claude/steps.md`'s "Five questions before every feature" (4 and
//! 5): GTK and the CLI must invoke the same service, never parallel copies.

pub mod account;
pub mod agent;
pub mod app;
pub mod canvas;
pub mod control;
pub mod environment;
pub mod handoff;
pub mod layout;
pub mod markdown;
pub mod message;
pub mod migration;
pub mod model;
pub mod node;
pub mod orchestration;
pub mod role;
pub mod runtime;
pub mod session;
pub mod store;
