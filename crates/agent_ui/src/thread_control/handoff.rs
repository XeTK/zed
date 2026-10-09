//! A summary of where a thread stands, for a program that is about to carry on
//! the work itself.
//!
//! It is built from the thread's own record, with no model involved: the goal
//! (the first message), the latest messages, the recent tool calls, the files
//! they touched, and whatever is waiting for the person.

use acp_thread::{AcpThread, AgentThreadEntry, ThreadStatus, ToolCallStatus};
use gpui::App;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const DEFAULT_RECENT_MESSAGES: usize = 6;
pub const MAX_RECENT_MESSAGES: usize = 20;
const RECENT_TOOL_CALLS: usize = 10;
const MAX_MESSAGE_CHARS: usize = 2_000;

/// What a thread has said and done so far, in the plainest form.
#[derive(Default, Debug, PartialEq)]
pub struct Digest {
    /// `user` or `assistant`, and what was said.
    pub messages: Vec<(&'static str, String)>,
    /// Each tool call's title and how it ended up.
    pub tool_calls: Vec<(String, String)>,
    pub files: BTreeSet<String>,
    /// Requests the person has not answered yet.
    pub waiting_for_you: Vec<String>,
}

impl Digest {
    pub fn from_thread(thread: &AcpThread, cx: &App) -> Self {
        let mut digest = Digest::default();
        for entry in thread.entries() {
            match entry {
                AgentThreadEntry::UserMessage(message) => digest
                    .messages
                    .push(("user", message.content.to_markdown(cx).to_string())),
                AgentThreadEntry::AssistantMessage(message) => {
                    digest.messages.push(("assistant", message.to_markdown(cx)))
                }
                AgentThreadEntry::ToolCall(call) => {
                    let title = call.label.read(cx).source().to_string();
                    for location in &call.locations {
                        digest
                            .files
                            .insert(location.path.to_string_lossy().into_owned());
                    }
                    for diff in call.diffs() {
                        digest.files.extend(diff.read(cx).file_path(cx));
                    }
                    if matches!(call.status, ToolCallStatus::WaitingForConfirmation { .. }) {
                        digest.waiting_for_you.push(title.clone());
                    }
                    digest.tool_calls.push((title, call.status.to_string()));
                }
                _ => {}
            }
        }
        digest
    }

    pub fn from_messages(messages: Vec<(&'static str, String)>) -> Self {
        Digest {
            messages,
            ..Default::default()
        }
    }

    pub fn to_json(&self, recent: usize, status: &str, claimed_by: Option<String>) -> Value {
        let recent = recent.clamp(1, MAX_RECENT_MESSAGES);
        let goal = self
            .messages
            .iter()
            .find(|(role, _)| *role == "user")
            .map(|(_, text)| truncate(text));
        let skipped = self.messages.len().saturating_sub(recent);
        let recent_messages: Vec<Value> = self
            .messages
            .iter()
            .skip(skipped)
            .map(|(role, text)| json!({ "role": role, "text": truncate(text) }))
            .collect();
        let tool_calls: Vec<Value> = self
            .tool_calls
            .iter()
            .skip(self.tool_calls.len().saturating_sub(RECENT_TOOL_CALLS))
            .map(|(title, status)| json!({ "title": truncate(title), "status": status }))
            .collect();
        json!({
            "status": status,
            "claimed_by": claimed_by,
            "goal": goal,
            "messages_in_thread": self.messages.len(),
            "earlier_messages_not_shown": skipped,
            "recent_messages": recent_messages,
            "recent_tool_calls": tool_calls,
            "files_touched": self.files,
            "waiting_for_you": self.waiting_for_you,
            "last_message_was_from": self.messages.last().map(|(role, _)| *role),
        })
    }
}

pub fn status_name(thread: &AcpThread) -> &'static str {
    if thread.is_waiting_for_confirmation() {
        "waiting_for_confirmation"
    } else if thread.status() == ThreadStatus::Generating {
        "running"
    } else {
        "idle"
    }
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    cut.push_str("\n[truncated]");
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(messages: &[(&'static str, &str)]) -> Digest {
        Digest::from_messages(
            messages
                .iter()
                .map(|(role, text)| (*role, text.to_string()))
                .collect(),
        )
    }

    #[test]
    fn test_the_goal_is_the_first_user_message_and_recent_messages_are_the_last() {
        let digest = digest(&[
            ("user", "make the build faster"),
            ("assistant", "looking"),
            ("user", "also the tests"),
            ("assistant", "done"),
        ]);
        let handoff = digest.to_json(2, "idle", None);
        assert_eq!(handoff["goal"], "make the build faster");
        assert_eq!(handoff["messages_in_thread"], 4);
        assert_eq!(handoff["earlier_messages_not_shown"], 2);
        assert_eq!(
            handoff["recent_messages"],
            json!([
                { "role": "user", "text": "also the tests" },
                { "role": "assistant", "text": "done" },
            ])
        );
        assert_eq!(handoff["last_message_was_from"], "assistant");
    }

    #[test]
    fn test_the_number_of_recent_messages_is_clamped() {
        let many: Vec<(&'static str, &str)> = (0..40).map(|_| ("user", "x")).collect();
        let digest = digest(&many);
        assert_eq!(
            digest.to_json(0, "idle", None)["recent_messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            digest.to_json(1000, "idle", None)["recent_messages"]
                .as_array()
                .unwrap()
                .len(),
            MAX_RECENT_MESSAGES
        );
    }

    #[test]
    fn test_an_empty_thread_has_no_goal() {
        let handoff = Digest::default().to_json(6, "idle", None);
        assert_eq!(handoff["goal"], Value::Null);
        assert_eq!(handoff["recent_messages"], json!([]));
        assert_eq!(handoff["last_message_was_from"], Value::Null);
    }

    #[test]
    fn test_long_messages_are_cut_and_only_the_latest_tool_calls_are_kept() {
        let mut digest = digest(&[("user", &"a".repeat(MAX_MESSAGE_CHARS + 50))]);
        digest.tool_calls = (0..15)
            .map(|index| (format!("call {index}"), "Completed".to_string()))
            .collect();
        digest.files.insert("/work/app/src/main.rs".to_string());
        digest
            .waiting_for_you
            .push("Run `rm -rf build`".to_string());
        let handoff = digest.to_json(6, "waiting_for_confirmation", Some("Claude".to_string()));
        assert!(handoff["goal"].as_str().unwrap().ends_with("[truncated]"));
        let calls = handoff["recent_tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), RECENT_TOOL_CALLS);
        assert_eq!(calls[0]["title"], "call 5");
        assert_eq!(calls[9]["title"], "call 14");
        assert_eq!(handoff["files_touched"], json!(["/work/app/src/main.rs"]));
        assert_eq!(handoff["waiting_for_you"], json!(["Run `rm -rf build`"]));
        assert_eq!(handoff["status"], "waiting_for_confirmation");
        assert_eq!(handoff["claimed_by"], "Claude");
    }
}
