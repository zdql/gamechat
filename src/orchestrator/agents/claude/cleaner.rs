//! Turn one raw line of `claude -p --output-format stream-json` output into
//! a human-readable progress entry — or `None` to drop it.
//!
//! The summarizer LLM downstream has a tight character budget; spending it
//! on JSON envelopes (`{"type":"system",...}`, `session_id` fields, our own
//! prompt echo) wastes signal. We keep the parts a reader actually cares
//! about: assistant text, tool calls, the final result.

use serde_json::Value;

/// Maximum characters retained for any single field we surface. Keeps a
/// chatty assistant turn or a giant tool input from monopolizing the buffer.
const FIELD_MAX_CHARS: usize = 200;
const TOOL_VALUE_MAX_CHARS: usize = 80;
const TOOL_INPUT_MAX_CHARS: usize = 160;
const TOOL_RESULT_MAX_CHARS: usize = 160;

/// Cleaner entry point. Wired into [`crate::orchestrator::interface::CleanLogLine`].
pub(crate) fn clean_log_line(line: &str, stream: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // stderr is human-readable diagnostics, not stream-json — pass through.
    if stream != "stdout" {
        return Some(trimmed.to_string());
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        // Not JSON. Keep the raw line so we don't silently lose signal.
        return Some(trimmed.to_string());
    };
    match value.get("type").and_then(Value::as_str)? {
        "system" => None,
        "user" => clean_user(&value),
        "assistant" => clean_assistant(&value),
        "result" => clean_result(&value),
        _ => None,
    }
}

fn clean_user(value: &Value) -> Option<String> {
    // Drop the prompt echo entirely. Surface tool_result blocks so the
    // summarizer can see what the assistant just learned.
    let content = value.get("message")?.get("content")?;
    if content.is_string() {
        return None;
    }
    let array = content.as_array()?;
    let mut parts = Vec::new();
    for block in array {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let text = block
            .get("content")
            .map(extract_text)
            .unwrap_or_default();
        let summary = collapse_and_truncate(&text, TOOL_RESULT_MAX_CHARS);
        if summary.is_empty() {
            parts.push("tool result: (empty)".to_string());
        } else {
            parts.push(format!("tool result: {summary}"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

fn clean_assistant(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?.as_array()?;
    let mut parts = Vec::new();
    for block in content {
        let Some(block_type) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        match block_type {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                let cleaned = collapse_and_truncate(text, FIELD_MAX_CHARS);
                if !cleaned.is_empty() {
                    parts.push(format!("assistant: {cleaned}"));
                }
            }
            "tool_use" => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                let input = block
                    .get("input")
                    .map(format_tool_input)
                    .unwrap_or_default();
                parts.push(format!("tool call: {name}({input})"));
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

fn clean_result(value: &Value) -> Option<String> {
    let result = value.get("result").and_then(Value::as_str)?;
    let cleaned = collapse_and_truncate(result, FIELD_MAX_CHARS);
    if cleaned.is_empty() {
        None
    } else {
        Some(format!("final: {cleaned}"))
    }
}

/// Pull plain text out of a `tool_result.content` field, which Claude emits
/// either as a bare string or as an array of `{"type":"text","text":"..."}`
/// blocks.
fn extract_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(array) = content.as_array() {
        return array
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    b.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    String::new()
}

fn format_tool_input(input: &Value) -> String {
    let Some(map) = input.as_object() else {
        return collapse_and_truncate(&input.to_string(), TOOL_INPUT_MAX_CHARS);
    };
    let parts: Vec<String> = map
        .iter()
        .map(|(k, v)| {
            let rendered = match v {
                Value::String(s) => collapse_and_truncate(s, TOOL_VALUE_MAX_CHARS),
                other => collapse_and_truncate(&other.to_string(), TOOL_VALUE_MAX_CHARS),
            };
            format!("{k}={rendered}")
        })
        .collect();
    collapse_and_truncate(&parts.join(", "), TOOL_INPUT_MAX_CHARS)
}

fn collapse_and_truncate(text: &str, max: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_system_init_blob() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/x","tools":["Read"],"session_id":"abc"}"#;
        assert_eq!(clean_log_line(line, "stdout"), None);
    }

    #[test]
    fn skips_user_prompt_echo() {
        let line = r#"{"type":"user","message":{"role":"user","content":"please refactor the docs"},"session_id":"abc"}"#;
        assert_eq!(clean_log_line(line, "stdout"), None);
    }

    #[test]
    fn surfaces_tool_result_from_user_event() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"found 3 matches"}]},"session_id":"abc"}"#;
        let cleaned = clean_log_line(line, "stdout").unwrap();
        assert_eq!(cleaned, "tool result: found 3 matches");
    }

    #[test]
    fn surfaces_tool_result_with_array_content() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}]},"session_id":"abc"}"#;
        let cleaned = clean_log_line(line, "stdout").unwrap();
        assert_eq!(cleaned, "tool result: hello world");
    }

    #[test]
    fn extracts_assistant_text() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll start by reading main.rs"}]},"session_id":"abc"}"#;
        let cleaned = clean_log_line(line, "stdout").unwrap();
        assert_eq!(cleaned, "assistant: I'll start by reading main.rs");
    }

    #[test]
    fn extracts_tool_call_with_input_kv() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/main.rs"}}]},"session_id":"abc"}"#;
        let cleaned = clean_log_line(line, "stdout").unwrap();
        assert_eq!(cleaned, "tool call: Read(file_path=src/main.rs)");
    }

    #[test]
    fn assistant_text_and_tool_use_joined() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Reading"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.rs"}}]},"session_id":"abc"}"#;
        let cleaned = clean_log_line(line, "stdout").unwrap();
        assert_eq!(
            cleaned,
            "assistant: Reading | tool call: Read(file_path=a.rs)"
        );
    }

    #[test]
    fn surfaces_final_result() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"result":"Fixed the bug on line 42","session_id":"abc"}"#;
        let cleaned = clean_log_line(line, "stdout").unwrap();
        assert_eq!(cleaned, "final: Fixed the bug on line 42");
    }

    #[test]
    fn unparseable_line_passes_through() {
        // Real-world stderr or random text shouldn't be silently dropped.
        let cleaned = clean_log_line("hello world", "stdout").unwrap();
        assert_eq!(cleaned, "hello world");
    }

    #[test]
    fn stderr_passes_through_verbatim() {
        let cleaned = clean_log_line("  warning: foo bar  ", "stderr").unwrap();
        assert_eq!(cleaned, "warning: foo bar");
    }

    #[test]
    fn empty_line_is_dropped() {
        assert_eq!(clean_log_line("   \n", "stdout"), None);
        assert_eq!(clean_log_line("", "stderr"), None);
    }

    #[test]
    fn empty_assistant_text_skipped() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"   "}]},"session_id":"abc"}"#;
        assert_eq!(clean_log_line(line, "stdout"), None);
    }

    #[test]
    fn long_text_is_truncated_with_ellipsis() {
        let big = "x".repeat(500);
        let line = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{big}"}}]}},"session_id":"abc"}}"#
        );
        let cleaned = clean_log_line(&line, "stdout").unwrap();
        assert!(cleaned.starts_with("assistant: "));
        assert!(cleaned.ends_with('…'));
        // "assistant: " prefix + FIELD_MAX_CHARS + "…"
        assert_eq!(cleaned.chars().count(), "assistant: ".len() + FIELD_MAX_CHARS + 1);
    }

    #[test]
    fn unknown_event_type_dropped() {
        let line = r#"{"type":"telemetry","payload":{}}"#;
        assert_eq!(clean_log_line(line, "stdout"), None);
    }

    #[test]
    fn whitespace_collapsed_in_assistant_text() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"line one\nline\ttwo"}]}}"#;
        let cleaned = clean_log_line(line, "stdout").unwrap();
        assert_eq!(cleaned, "assistant: line one line two");
    }
}
