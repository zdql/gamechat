use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Maximum number of progress entries retained per job.
const MAX_BUFFER_ENTRIES: usize = 20;

/// Minimum interval (in seconds) between progress queries for the same job.
/// Voice cadence: well under a typical conversational gap, but enough to
/// discourage mechanical re-polling in the same speaking turn.
const RATE_LIMIT_SECS: u64 = 5;

/// Default character window for the recent_snippet field.
pub(crate) const DEFAULT_WINDOW_SIZE: usize = 1000;

// ── Public types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        })
    }
}

/// Lightweight per-slug summary used by the control socket. Unlike
/// [`ProgressSnapshot`], this never consumes the per-slug rate-limit window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SlugSummary {
    pub slug: String,
    pub provider: String,
    pub status: JobStatus,
    pub last_message: String,
    pub elapsed_seconds: f64,
    pub session_id: Option<String>,
    /// Sequence number of the most recent buffered entry, or `None` if the
    /// buffer is empty. Clients use this as the initial tail cursor.
    pub last_seq: Option<u64>,
}

/// Batch returned by [`ProgressStore::entries_after`]: any newly-buffered
/// entries together with the updated cursor.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TailBatch {
    pub entries: Vec<String>,
    pub next_cursor: Option<u64>,
    pub status: JobStatus,
    pub done: bool,
}

/// Read-only snapshot returned to callers that query a job's progress.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProgressSnapshot {
    pub status: JobStatus,
    pub provider: String,
    pub last_message: String,
    pub recent_snippet: String,
    /// Seconds since the job was registered (or since it went `running` if
    /// it has progressed past `Queued`). Always >= 0.
    pub elapsed_seconds: f64,
    /// True when the caller queried before the per-slug rate limit elapsed.
    /// The snapshot still carries the most recent cached fields so the model
    /// has something to say; this flag signals "don't poll again yet."
    pub rate_limited: bool,
    /// When `rate_limited` is true, the number of seconds the caller should
    /// wait before re-querying. Zero otherwise.
    pub retry_after_seconds: u64,
}

/// Lightweight writer that provider clients can clone into async read loops.
#[derive(Clone)]
pub(crate) struct ProgressReporter {
    store: Arc<ProgressStore>,
    slug: String,
}

impl ProgressReporter {
    pub(crate) fn new(store: Arc<ProgressStore>, slug: String) -> Self {
        Self { store, slug }
    }

    pub(crate) fn push(&self, message: &str) {
        self.store.push_progress(&self.slug, message);
    }

    pub(crate) fn set_session_id(&self, id: &str) {
        self.store.set_session_id(&self.slug, id);
    }
}

// ── Internal tracking ──────────────────────────────────────────────────

struct JobProgress {
    status: JobStatus,
    provider: String,
    /// The most recent single progress line (e.g. last agent reply).
    last_message: String,
    /// Bounded ring of recent progress entries (newest last). Each carries a
    /// monotonically increasing sequence number so cursor-based tail readers
    /// can stream only new content.
    recent_buffer: VecDeque<(u64, String)>,
    /// Next sequence number to assign on `push_progress`.
    next_seq: u64,
    last_updated: Instant,
    /// Wall-clock origin for `elapsed_seconds`. Set on `register_job` and
    /// re-stamped on `set_running` so the voice model gets time-in-flight
    /// rather than time-in-queue once the job actually starts.
    started_at: Instant,
    /// Backend-reported conversation/session id (e.g. Claude Code's session
    /// id), once known. Used by external inspection clients to resume the
    /// underlying agent UI.
    session_id: Option<String>,
}

struct ProgressInner {
    jobs: HashMap<String, JobProgress>,
    last_query: HashMap<String, Instant>,
}

// ── Store ──────────────────────────────────────────────────────────────

pub(crate) struct ProgressStore {
    inner: Mutex<ProgressInner>,
}

impl ProgressStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ProgressInner {
                jobs: HashMap::new(),
                last_query: HashMap::new(),
            }),
        }
    }

    /// Register or replace a slug in the progress store.
    pub fn register_job(&self, slug: &str, provider: &str) {
        let mut inner = self.inner.lock().expect("progress store mutex poisoned");
        let now = Instant::now();
        inner.jobs.insert(
            slug.to_string(),
            JobProgress {
                status: JobStatus::Queued,
                provider: provider.to_string(),
                last_message: String::new(),
                recent_buffer: VecDeque::new(),
                next_seq: 0,
                last_updated: now,
                started_at: now,
                session_id: None,
            },
        );
    }

    /// Mark a previously registered job as running.
    pub fn set_running(&self, slug: &str) {
        let mut inner = self.inner.lock().expect("progress store mutex poisoned");
        if let Some(progress) = inner.jobs.get_mut(slug) {
            let now = Instant::now();
            progress.status = JobStatus::Running;
            progress.last_updated = now;
            // Reset the clock at run-start so elapsed reflects working time.
            progress.started_at = now;
        }
    }

    /// Append a progress entry for a running job.
    /// The message is trimmed and empty strings are silently ignored.
    pub fn push_progress(&self, slug: &str, message: &str) {
        let trimmed = message.trim();
        if trimmed.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().expect("progress store mutex poisoned");
        if let Some(progress) = inner.jobs.get_mut(slug) {
            progress.last_message = trimmed.to_string();
            if progress.recent_buffer.len() >= MAX_BUFFER_ENTRIES {
                progress.recent_buffer.pop_front();
            }
            let seq = progress.next_seq;
            progress.next_seq = progress.next_seq.wrapping_add(1);
            progress.recent_buffer.push_back((seq, trimmed.to_string()));
            progress.last_updated = Instant::now();
        }
    }

    /// Mark a job as completed with the final reply.
    pub fn set_completed(&self, slug: &str, result: &str) {
        let mut inner = self.inner.lock().expect("progress store mutex poisoned");
        if let Some(progress) = inner.jobs.get_mut(slug) {
            progress.status = JobStatus::Completed;
            let trimmed = result.trim();
            if !trimmed.is_empty() {
                progress.last_message = trimmed.to_string();
                if progress.recent_buffer.len() >= MAX_BUFFER_ENTRIES {
                    progress.recent_buffer.pop_front();
                }
                let seq = progress.next_seq;
                progress.next_seq = progress.next_seq.wrapping_add(1);
                progress.recent_buffer.push_back((seq, trimmed.to_string()));
            }
            progress.last_updated = Instant::now();
        }
    }

    /// Mark a job as failed with an error message.
    pub fn set_failed(&self, slug: &str, error: &str) {
        let mut inner = self.inner.lock().expect("progress store mutex poisoned");
        if let Some(progress) = inner.jobs.get_mut(slug) {
            progress.status = JobStatus::Failed;
            let trimmed = error.trim();
            if !trimmed.is_empty() {
                progress.last_message = trimmed.to_string();
            }
            progress.last_updated = Instant::now();
        }
    }

    /// Attach (or update) the backend session id for a slug. Used by external
    /// inspection clients to launch the underlying agent UI on the right
    /// conversation.
    pub fn set_session_id(&self, slug: &str, session_id: &str) {
        let mut inner = self.inner.lock().expect("progress store mutex poisoned");
        if let Some(progress) = inner.jobs.get_mut(slug) {
            progress.session_id = Some(session_id.to_string());
        }
    }

    /// Non-rate-limited summary of every known slug. Intended for the control
    /// socket, not the voice model — does not consume the per-slug rate-limit
    /// window.
    pub fn snapshot_all(&self) -> Vec<SlugSummary> {
        let inner = self.inner.lock().expect("progress store mutex poisoned");
        let mut entries: Vec<SlugSummary> = inner
            .jobs
            .iter()
            .map(|(slug, p)| SlugSummary {
                slug: slug.clone(),
                provider: p.provider.clone(),
                status: p.status,
                last_message: p.last_message.clone(),
                elapsed_seconds: p.started_at.elapsed().as_secs_f64(),
                session_id: p.session_id.clone(),
                last_seq: p.recent_buffer.back().map(|(seq, _)| *seq),
            })
            .collect();
        entries.sort_by(|a, b| a.slug.cmp(&b.slug));
        entries
    }

    /// Return buffered entries whose sequence number is greater than
    /// `after_seq`. `None` if the slug is unknown. The returned cursor is the
    /// largest sequence number returned (or `after_seq` if nothing new).
    pub fn entries_after(&self, slug: &str, after_seq: Option<u64>) -> Option<TailBatch> {
        let inner = self.inner.lock().expect("progress store mutex poisoned");
        let progress = inner.jobs.get(slug)?;
        let mut entries: Vec<String> = Vec::new();
        let mut max_seq = after_seq;
        for (seq, msg) in &progress.recent_buffer {
            let include = match after_seq {
                Some(cursor) => *seq > cursor,
                None => true,
            };
            if include {
                entries.push(msg.clone());
                max_seq = Some(max_seq.map_or(*seq, |existing| existing.max(*seq)));
            }
        }
        Some(TailBatch {
            entries,
            next_cursor: max_seq,
            status: progress.status,
            done: matches!(progress.status, JobStatus::Completed | JobStatus::Failed),
        })
    }

    /// Query the progress of a job. Returns `None` if the job is unknown.
    /// Applies rate limiting: when the caller polls within `RATE_LIMIT_SECS`,
    /// the snapshot still carries the cached fields but `rate_limited` is set
    /// so the voice model knows to back off.
    pub fn get_update(&self, slug: &str, window_size: Option<usize>) -> Option<ProgressSnapshot> {
        let mut inner = self.inner.lock().expect("progress store mutex poisoned");

        let progress = inner.jobs.get(slug)?;
        let status = progress.status;
        let provider = progress.provider.clone();
        let last_message = progress.last_message.clone();
        let recent_buffer = progress.recent_buffer.clone();
        let elapsed_seconds = progress.started_at.elapsed().as_secs_f64();

        let now = Instant::now();
        let (rate_limited, retry_after_seconds) = match inner.last_query.get(slug) {
            Some(last) => {
                let since = now.duration_since(*last).as_secs();
                if since < RATE_LIMIT_SECS {
                    (true, RATE_LIMIT_SECS - since)
                } else {
                    (false, 0)
                }
            }
            None => (false, 0),
        };
        // Only stamp the last_query when the caller is actually allowed to
        // pull a fresh snapshot. A blocked poll should not extend the window.
        if !rate_limited {
            inner.last_query.insert(slug.to_string(), now);
        }

        let max_chars = window_size.unwrap_or(DEFAULT_WINDOW_SIZE);
        let recent_snippet = truncate_buffer(&recent_buffer, max_chars);

        Some(ProgressSnapshot {
            status,
            provider,
            last_message,
            recent_snippet,
            elapsed_seconds,
            rate_limited,
            retry_after_seconds,
        })
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Concatenate buffer entries and truncate to `max_chars`, keeping the
/// tail (most recent) content.
fn truncate_buffer(buffer: &VecDeque<(u64, String)>, max_chars: usize) -> String {
    if buffer.is_empty() {
        return String::new();
    }

    // Join all entries with newlines.
    let joined = buffer
        .iter()
        .enumerate()
        .map(|(i, (_, entry))| {
            if i == 0 {
                entry.clone()
            } else {
                format!("\n{entry}")
            }
        })
        .collect::<String>();

    truncate_tail(&joined, max_chars)
}

/// Truncate a string to at most `max_chars`, keeping the tail. If
/// truncated, prepends "…".
fn truncate_tail(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }

    // Work with char boundaries to avoid panicking on multi-byte UTF-8.
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }

    let start = chars.len() - max_chars;
    let truncated: String = chars[start..].iter().collect();
    format!("…{truncated}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_update_job() {
        let store = ProgressStore::new();
        store.register_job("job-1", "claude");
        store.set_running("job-1");
        store.push_progress("job-1", "reading files");
        store.push_progress("job-1", "editing code");

        let snap = store.get_update("job-1", None).unwrap();
        assert_eq!(snap.status, JobStatus::Running);
        assert_eq!(snap.provider, "claude");
        assert_eq!(snap.last_message, "editing code");
        assert!(snap.recent_snippet.contains("reading files"));
        assert!(snap.recent_snippet.contains("editing code"));
    }

    #[test]
    fn completed_job_snapshot() {
        let store = ProgressStore::new();
        store.register_job("job-2", "codex");
        store.set_running("job-2");
        store.set_completed("job-2", "all done");

        let snap = store.get_update("job-2", None).unwrap();
        assert_eq!(snap.status, JobStatus::Completed);
        assert_eq!(snap.last_message, "all done");
    }

    #[test]
    fn failed_job_snapshot() {
        let store = ProgressStore::new();
        store.register_job("job-3", "claude");
        store.set_failed("job-3", "something went wrong");

        let snap = store.get_update("job-3", None).unwrap();
        assert_eq!(snap.status, JobStatus::Failed);
        assert_eq!(snap.last_message, "something went wrong");
    }

    #[test]
    fn unknown_job_returns_none() {
        let store = ProgressStore::new();
        assert!(store.get_update("no-such-job", None).is_none());
    }

    #[test]
    fn rate_limiting() {
        let store = ProgressStore::new();
        store.register_job("job-rl", "claude");
        store.set_running("job-rl");
        store.push_progress("job-rl", "working");

        // First query should succeed.
        let snap1 = store.get_update("job-rl", None).unwrap();
        assert!(!snap1.rate_limited);
        assert_eq!(snap1.retry_after_seconds, 0);
        assert_eq!(snap1.last_message, "working");

        // Immediate second query should be rate limited but still return
        // the cached snapshot (last_message + recent_snippet).
        let snap2 = store.get_update("job-rl", None).unwrap();
        assert!(snap2.rate_limited);
        assert!(snap2.retry_after_seconds > 0);
        assert_eq!(snap2.status, JobStatus::Running);
        assert_eq!(snap2.last_message, "working");
        assert!(snap2.recent_snippet.contains("working"));
    }

    #[test]
    fn elapsed_seconds_reported() {
        let store = ProgressStore::new();
        store.register_job("job-el", "claude");
        store.set_running("job-el");
        let snap = store.get_update("job-el", None).unwrap();
        assert!(snap.elapsed_seconds >= 0.0);
    }

    #[test]
    fn truncation_keeps_tail() {
        let long = "x".repeat(500);
        let result = truncate_tail(&long, 100);
        assert_eq!(result.chars().count(), 101); // 100 chars + "…"
        assert!(result.starts_with('…'));
        assert!(result.ends_with('x'));
    }

    #[test]
    fn buffer_truncation() {
        let store = ProgressStore::new();
        store.register_job("job-tr", "claude");
        store.set_running("job-tr");

        // Push entries that together exceed the window size.
        for i in 0..5 {
            store.push_progress("job-tr", &format!("step {} {}", i, "x".repeat(300)));
        }

        let snap = store.get_update("job-tr", Some(500)).unwrap();
        // Should be truncated but still contain the most recent entry.
        assert!(snap.recent_snippet.len() <= 510); // some slack for "…"
        assert!(snap.recent_snippet.contains("step 4"));
    }

    #[test]
    fn push_empty_message_ignored() {
        let store = ProgressStore::new();
        store.register_job("job-emp", "claude");
        store.set_running("job-emp");
        store.push_progress("job-emp", "");
        store.push_progress("job-emp", "   ");

        let snap = store.get_update("job-emp", None).unwrap();
        assert!(snap.last_message.is_empty());
    }

    #[test]
    fn max_buffer_entries_bounded() {
        let store = ProgressStore::new();
        store.register_job("job-buf", "claude");
        store.set_running("job-buf");

        for i in 0..(MAX_BUFFER_ENTRIES + 5) {
            store.push_progress("job-buf", &format!("entry {i}"));
        }

        // Internal buffer should be capped at MAX_BUFFER_ENTRIES.
        let inner = store.inner.lock().unwrap();
        let progress = inner.jobs.get("job-buf").unwrap();
        assert_eq!(progress.recent_buffer.len(), MAX_BUFFER_ENTRIES);
        // The oldest entries should have been dropped.
        assert!(
            progress
                .recent_buffer
                .front()
                .unwrap()
                .1
                .starts_with("entry 5")
        );
        assert!(
            progress
                .recent_buffer
                .back()
                .unwrap()
                .1
                .starts_with("entry 24")
        );
    }

    #[test]
    fn snapshot_all_returns_sorted_summaries() {
        let store = ProgressStore::new();
        store.register_job("zeta", "claude");
        store.register_job("alpha", "codex");
        store.set_running("alpha");
        store.push_progress("alpha", "step one");
        store.set_session_id("alpha", "sess-1");

        let summaries = store.snapshot_all();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].slug, "alpha");
        assert_eq!(summaries[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(summaries[0].status, JobStatus::Running);
        assert_eq!(summaries[1].slug, "zeta");
        assert_eq!(summaries[1].session_id, None);
    }

    #[test]
    fn entries_after_streams_only_new_lines() {
        let store = ProgressStore::new();
        store.register_job("tailme", "claude");
        store.set_running("tailme");
        store.push_progress("tailme", "alpha");
        store.push_progress("tailme", "bravo");

        let first = store.entries_after("tailme", None).unwrap();
        assert_eq!(first.entries, vec!["alpha", "bravo"]);
        let cursor = first.next_cursor;
        assert!(cursor.is_some());

        // No new entries — empty batch, cursor unchanged.
        let stale = store.entries_after("tailme", cursor).unwrap();
        assert!(stale.entries.is_empty());
        assert_eq!(stale.next_cursor, cursor);

        store.push_progress("tailme", "charlie");
        let next = store.entries_after("tailme", cursor).unwrap();
        assert_eq!(next.entries, vec!["charlie"]);
        assert!(next.next_cursor > cursor);
    }

    #[test]
    fn entries_after_unknown_slug_is_none() {
        let store = ProgressStore::new();
        assert!(store.entries_after("nope", None).is_none());
    }

    #[test]
    fn entries_after_flags_done_on_terminal_status() {
        let store = ProgressStore::new();
        store.register_job("finis", "claude");
        store.set_running("finis");
        store.set_completed("finis", "all done");
        let batch = store.entries_after("finis", None).unwrap();
        assert!(batch.done);
        assert_eq!(batch.status, JobStatus::Completed);
    }
}
