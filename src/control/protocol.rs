//! Wire protocol for the gamechat control socket.
//!
//! Newline-delimited JSON: one [`Request`] in, one [`Response`] back. The
//! socket closes immediately after the response — clients reconnect for the
//! next call (including the polling tail loop). Simple, stateless, and
//! avoids any subscription bookkeeping inside the server.

use crate::orchestrator::progress::{JobStatus, SlugSummary};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Request {
    /// Server identity + a brief "alive" handshake used by `inspect`.
    Hello,
    /// Snapshot of every known slug.
    List,
    /// Pull buffered progress entries newer than `after_seq`. `after_seq =
    /// None` returns everything currently buffered.
    Tail {
        slug: String,
        after_seq: Option<u64>,
    },
    /// Look up the resume target for a slug — provider + session id.
    Resume { slug: String },
    /// Trigger a voice-context reset on the running voice loop. Optional
    /// `reason` is recorded in the server log alongside the trigger source.
    Reset {
        #[serde(default)]
        reason: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Response {
    Hello {
        pid: u32,
        version: String,
        provider: String,
    },
    List {
        slugs: Vec<SlugSummary>,
    },
    Tail {
        entries: Vec<String>,
        next_cursor: Option<u64>,
        status: JobStatus,
        done: bool,
    },
    Resume {
        slug: String,
        provider: String,
        session_id: Option<String>,
    },
    /// Acknowledgement that the reset signal was accepted by the server.
    /// `dispatched` is true when the voice loop is alive and received the
    /// signal; false when the voice loop is absent (read-only socket).
    Reset {
        dispatched: bool,
    },
    Error {
        message: String,
    },
}
