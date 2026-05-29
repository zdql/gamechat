//! Voice-context reset: tear down the realtime conversation history without
//! breaking audio flow.
//!
//! The Realtime API holds the entire conversation in server state. Long voice
//! sessions accrete enough context to noticeably degrade responses (and to
//! occasionally fail outright). A reset:
//!
//! 1. Cancels any in-flight response (`response.cancel`) so we don't strand a
//!    half-emitted assistant turn whose item id we're about to delete.
//! 2. Deletes every tracked conversation item id with
//!    `conversation.item.delete`. We only track ids the server has already
//!    confirmed via `conversation.item.created`, so this is exact.
//! 3. Re-emits the original `session.update` to re-baseline instructions,
//!    voice, tools, and turn detection. Cheap; the server treats it as a
//!    no-op when nothing changed.
//!
//! Audio flow: the local playback buffer is left intact, so any audio already
//! handed to cpal finishes playing. The mic input stream is never touched.
//! The websocket itself stays open.

use serde_json::{Value, json};
use std::collections::VecDeque;

/// Maximum number of conversation item ids we track. Bounded so a runaway
/// session can't grow the in-memory set without limit even if the user
/// never triggers a reset. The oldest id is dropped first; on reset we
/// best-effort delete whatever is still in the window.
pub(crate) const MAX_TRACKED_ITEMS: usize = 4000;

/// Track conversation item ids in the order they were created so a reset
/// can issue `conversation.item.delete` for each.
#[derive(Debug, Default)]
pub(crate) struct ConversationItemTracker {
    items: VecDeque<String>,
}

impl ConversationItemTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        if self.items.iter().any(|existing| existing == id) {
            return;
        }
        if self.items.len() >= MAX_TRACKED_ITEMS {
            self.items.pop_front();
        }
        self.items.push_back(id.to_string());
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub(crate) fn ids(&self) -> impl Iterator<Item = &String> {
        self.items.iter()
    }

    /// Drain every tracked id and return it. Called as part of a reset so
    /// subsequent voice turns start from an empty tracker.
    pub(crate) fn drain(&mut self) -> Vec<String> {
        std::mem::take(&mut self.items).into_iter().collect()
    }
}

/// Why the reset is happening. Recorded in logs and surfaced to operators.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResetTrigger {
    /// The voice model called the `reset_voice_context` tool.
    Voice,
    /// An external caller invoked `gamechat reset` through the control plane.
    Control,
    /// The tracked item count reached the configured auto-reset threshold.
    Auto,
}

impl ResetTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ResetTrigger::Voice => "voice_tool",
            ResetTrigger::Control => "control_socket",
            ResetTrigger::Auto => "auto_threshold",
        }
    }
}

/// Result of [`build_reset_events`]: the events to send and the count of
/// items being deleted (for logging / control-plane responses).
#[derive(Debug)]
pub(crate) struct ResetPlan {
    pub events: Vec<Value>,
    pub cleared_items: usize,
}

/// Build the Realtime event sequence that performs a context reset.
///
/// * `tracked_ids` — ids previously returned in `conversation.item.created`.
/// * `session_update` — the baseline `session.update` payload to re-send.
/// * `response_active` — when true, prepend `response.cancel` so the
///   in-flight assistant turn doesn't keep streaming into ids we just
///   asked the server to delete.
pub(crate) fn build_reset_events(
    tracked_ids: &[String],
    session_update: &Value,
    response_active: bool,
) -> ResetPlan {
    let mut events = Vec::with_capacity(tracked_ids.len() + 2);
    if response_active {
        events.push(json!({ "type": "response.cancel" }));
    }
    for id in tracked_ids {
        events.push(json!({
            "type": "conversation.item.delete",
            "item_id": id,
        }));
    }
    events.push(session_update.clone());
    ResetPlan {
        events,
        cleared_items: tracked_ids.len(),
    }
}

/// Inspect a parsed Realtime event for a conversation item id worth
/// tracking. Returns the id only when the server confirms creation.
pub(crate) fn item_id_from_event(value: &Value) -> Option<&str> {
    let event_type = value.get("type").and_then(|v| v.as_str())?;
    if event_type != "conversation.item.created" {
        return None;
    }
    value.get("item")?.get("id")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_session_update() -> Value {
        json!({"type": "session.update", "session": {"voice": "marin"}})
    }

    #[test]
    fn tracker_records_unique_ids_in_order() {
        let mut tracker = ConversationItemTracker::new();
        tracker.record("item_a");
        tracker.record("item_b");
        tracker.record("item_a"); // duplicate, ignored
        assert_eq!(tracker.len(), 2);
        let collected: Vec<&String> = tracker.ids().collect();
        assert_eq!(collected[0], "item_a");
        assert_eq!(collected[1], "item_b");
    }

    #[test]
    fn tracker_ignores_empty_id() {
        let mut tracker = ConversationItemTracker::new();
        tracker.record("");
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn tracker_drops_oldest_past_capacity() {
        let mut tracker = ConversationItemTracker::new();
        for i in 0..(MAX_TRACKED_ITEMS + 5) {
            tracker.record(&format!("item_{i}"));
        }
        assert_eq!(tracker.len(), MAX_TRACKED_ITEMS);
        let first = tracker.ids().next().unwrap();
        // The first 5 should have been dropped.
        assert_eq!(first, "item_5");
    }

    #[test]
    fn drain_empties_the_tracker() {
        let mut tracker = ConversationItemTracker::new();
        tracker.record("a");
        tracker.record("b");
        let drained = tracker.drain();
        assert_eq!(drained, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn build_reset_events_emits_cancel_then_deletes_then_session_update() {
        let ids = vec!["item_1".to_string(), "item_2".to_string()];
        let session = mk_session_update();
        let plan = build_reset_events(&ids, &session, true);
        assert_eq!(plan.cleared_items, 2);
        assert_eq!(plan.events.len(), 4);
        assert_eq!(plan.events[0]["type"], "response.cancel");
        assert_eq!(plan.events[1]["type"], "conversation.item.delete");
        assert_eq!(plan.events[1]["item_id"], "item_1");
        assert_eq!(plan.events[2]["type"], "conversation.item.delete");
        assert_eq!(plan.events[2]["item_id"], "item_2");
        assert_eq!(plan.events[3]["type"], "session.update");
    }

    #[test]
    fn build_reset_events_skips_cancel_when_no_response_active() {
        let ids = vec!["only".to_string()];
        let session = mk_session_update();
        let plan = build_reset_events(&ids, &session, false);
        assert_eq!(plan.events.len(), 2);
        assert_eq!(plan.events[0]["type"], "conversation.item.delete");
        assert_eq!(plan.events[1]["type"], "session.update");
    }

    #[test]
    fn build_reset_events_with_empty_tracker_only_resends_session() {
        let plan = build_reset_events(&[], &mk_session_update(), false);
        assert_eq!(plan.cleared_items, 0);
        assert_eq!(plan.events.len(), 1);
        assert_eq!(plan.events[0]["type"], "session.update");
    }

    #[test]
    fn item_id_extracted_from_created_event() {
        let event = json!({
            "type": "conversation.item.created",
            "item": {"id": "item_42", "type": "message"}
        });
        assert_eq!(item_id_from_event(&event), Some("item_42"));
    }

    #[test]
    fn item_id_ignores_other_event_types() {
        let event = json!({
            "type": "response.created",
            "item": {"id": "item_42"}
        });
        assert!(item_id_from_event(&event).is_none());
    }

    #[test]
    fn reset_trigger_string_repr_is_stable() {
        assert_eq!(ResetTrigger::Voice.as_str(), "voice_tool");
        assert_eq!(ResetTrigger::Control.as_str(), "control_socket");
        assert_eq!(ResetTrigger::Auto.as_str(), "auto_threshold");
    }
}
