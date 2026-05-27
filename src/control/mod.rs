//! Control plane for inspecting a running `gamechat --realtime` process from
//! another terminal.
//!
//! Self-contained: only depends on `Arc<ProgressStore>` from the orchestrator
//! and never touches the voice loop, audio, or the realtime websocket. The
//! voice loop calls [`spawn_server`] once at startup; everything else lives
//! inside this module.
//!
//! Wire protocol: newline-delimited JSON over a Unix domain socket at
//! `$RUNTIME_DIR/$pid.sock`. One request per connection. See
//! [`protocol`] for the shape of each message.
//!
//! CLI surface (handled by [`run_cli`]):
//!
//! - `gamechat inspect [--pid N]` — list all active sub-agents.
//! - `gamechat tail <slug> [--pid N]` — follow a slug's progress buffer.
//! - `gamechat open <slug> [--pid N] [--launch]` — print (or launch) the
//!   resume command for the underlying agent UI.

mod client;
mod discovery;
mod protocol;
mod runtime_dir;
mod server;

use std::path::PathBuf;

pub(crate) use discovery::{discover_existing_subagents, seed_discovered_subagents};
pub(crate) use server::{ResetSignal, spawn_server};

/// Subcommand parsed from `gamechat`'s argv.
#[derive(Debug)]
pub(crate) enum ControlSubcommand {
    Inspect,
    Tail { slug: String },
    Open { slug: String, launch: bool },
    Reset { reason: Option<String> },
    Discover,
}

/// Optional connection target. `pid` picks a specific running gamechat; if
/// `None` the client looks for exactly one socket in the runtime dir.
#[derive(Debug, Default)]
pub(crate) struct ControlTarget {
    pub pid: Option<u32>,
    pub socket: Option<PathBuf>,
}

pub(crate) async fn run_cli(
    subcommand: ControlSubcommand,
    target: ControlTarget,
) -> Result<(), String> {
    client::run(subcommand, target).await
}
