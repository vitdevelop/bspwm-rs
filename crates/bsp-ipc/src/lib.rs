//! `bsp-ipc`: the `bspc`-compatible control socket protocol.
//!
//! See `docs/design.md` roadmap, the IPC, and `docs/bsp-ipc.md`.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod adapter;
pub mod command;
pub mod exec;
pub mod registry;
pub mod report;
pub mod selector;
pub mod server;
pub mod value;
pub mod wire;
