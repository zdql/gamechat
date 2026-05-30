//! Coding-agent backends.
//!
//! Each subfolder is a self-contained provider that implements the
//! `interface` traits and exposes only its `Provider` type. The rest of the
//! orchestrator imports the providers re-exported below.

mod claude;
mod openai;

pub(crate) use claude::ClaudeProvider;
pub(crate) use openai::OpenAiProvider;
