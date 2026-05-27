//! Per-line cleaner for the Codex CLI (`codex exec`).
//!
//! Codex emits human-readable text on stdout/stderr already, so cleaning is
//! a near-passthrough: trim and drop blank lines. We deliberately do not
//! parse the output further — its shape isn't a stable contract and any
//! filtering we did here would risk swallowing genuine activity.

/// Cleaner entry point. Wired into [`crate::orchestrator::interface::CleanLogLine`].
pub(crate) fn clean_log_line(line: &str, _stream: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_non_empty_lines() {
        assert_eq!(
            clean_log_line("running command: cargo test", "stdout"),
            Some("running command: cargo test".to_string())
        );
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(
            clean_log_line("  hello  \n", "stdout"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn drops_empty_lines() {
        assert_eq!(clean_log_line("", "stdout"), None);
        assert_eq!(clean_log_line("   \n", "stderr"), None);
    }
}
