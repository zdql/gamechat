//! Orchestrator module — the bridge between the realtime voice loop and
//! background coding agents.
//!
//! Layout:
//!
//! - `interface` — the [`OrchestratorProvider`]/[`OrchestratorSession`] API
//!   used by everything outside this module, plus the `Provider`/`Session`
//!   traits that backends implement.
//! - `bridge` — realtime tool calls ↔ orchestrator jobs.
//! - `jobs`   — slug-keyed work queue running on one session per slug.
//! - `progress` — shared snapshot store for `sub_agent_progress` polling.
//! - `shared` — cross-provider helpers (log formatting, stdio streaming).
//! - `claude/`, `openai/` — provider backends. Each folder is self-contained
//!   and only exposes its `Provider` type to `interface`.
//!
//! The rest of the voice app imports only the items re-exported below.

pub(crate) mod bridge;
pub(crate) mod interface;
pub(crate) mod jobs;
pub(crate) mod progress;

mod claude;
mod openai;
mod shared;

pub(crate) use bridge::OrchestratorBridge;
pub(crate) use interface::OrchestratorProvider;
pub(crate) use jobs::OrchestratorJobManager;
