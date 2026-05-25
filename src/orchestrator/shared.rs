//! Cross-provider helpers used by more than one provider implementation.
//!
//! Everything in this module is provider-agnostic: log line formatting,
//! string trimming, and a generic "drain a child process stream" helper.

use crate::orchestrator::progress::ProgressReporter;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Render a ` job=<id>` suffix when a job is in scope, else an empty string.
pub(crate) fn format_job(job_id: Option<&str>) -> String {
    job_id.map(|id| format!(" job={id}")).unwrap_or_default()
}

/// Compact a string for log output: collapse whitespace and cap length.
pub(crate) fn preview(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = compact.chars().take(160).collect::<String>();
    if compact.chars().count() > 160 {
        preview.push_str("...");
    }
    preview
}

/// Drain a child process stream line-by-line, mirroring each line to stderr
/// and to the optional progress reporter, and returning the collected bytes.
pub(crate) async fn read_logged_stream<R>(
    provider: &'static str,
    stream_name: &'static str,
    job_id: String,
    reader: R,
    progress: Option<ProgressReporter>,
) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut collected = Vec::new();
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes = reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| format!("failed to read {provider} {stream_name}: {e}"))?;
        if bytes == 0 {
            break;
        }
        collected.extend_from_slice(&line);
        let text = String::from_utf8_lossy(&line);
        eprintln!(
            "{provider} {stream_name}{} bytes={} preview={}",
            format_job(Some(&job_id)),
            line.len(),
            preview(&text)
        );
        if let Some(reporter) = progress.as_ref() {
            reporter.push(&format!("{provider} {stream_name}: {}", text.trim()));
        }
    }

    Ok(collected)
}
