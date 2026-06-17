//! Conversation state — multi-turn dialogue + a graph snapshot.
//!
//! A `Conversation` carries the system prompt, the original task description,
//! and the running history of user/assistant/tool turns. It does **not** own
//! the graph itself (that lives at the agent layer above); rather, when we
//! need to invoke the model, we render the current graph into a `ModelRequest`
//! as a system-level reminder.
//!
//! Design notes:
//!
//! - The system prompt is kept separate from the message history so we can
//!   re-render it each turn (cheaper than embedding it in every message).
//! - The graph is the authoritative state. Each model call sends ONLY the
//!   system prompt and the current graph snapshot — the accumulated
//!   `messages` history is NOT replayed. This keeps per-call token cost
//!   bounded by graph size rather than growing linearly with rounds. The
//!   history is still accumulated (and exposed via [`Self::transcript`])
//!   for audit logs and the chat UI.
//! - `round` increments on each assistant turn so callers can cap iteration.

use crate::model::{Message, ModelRequest, Role};

#[derive(Debug, Clone)]
pub struct Conversation {
    pub system_prompt: String,
    pub task_description: String,
    pub messages: Vec<Message>,
    pub round: usize,
}

impl Conversation {
    /// Start a new conversation. The task description is pinned as the first
    /// user message so the model always sees it at the top of the history.
    pub fn new(system_prompt: impl Into<String>, task: impl Into<String>) -> Self {
        let task = task.into();
        let messages = vec![Message::user(format!("Task: {task}"))];
        Self {
            system_prompt: system_prompt.into(),
            task_description: task,
            messages,
            round: 0,
        }
    }

    pub fn add_user(&mut self, content: impl Into<String>) {
        self.messages.push(Message::user(content));
    }

    pub fn add_assistant(&mut self, content: impl Into<String>) {
        self.messages.push(Message::assistant(content));
        self.round += 1;
    }

    pub fn add_tool_result(&mut self, content: impl Into<String>) {
        self.messages.push(Message {
            role: Role::Tool,
            content: content.into(),
        });
    }

    /// Turn the conversation into a `ModelRequest`.
    ///
    /// Emits three messages: the system prompt, the current graph
    /// snapshot, and the original task as a user message. The accumulated
    /// `messages` history is **not** replayed — the graph is the
    /// authoritative state, and replaying every past assistant step +
    /// tool result would let per-call token cost grow linearly with
    /// rounds. The history is still kept on the `Conversation` and is
    /// available via [`Self::transcript`] for audit logs and the chat UI.
    ///
    /// The trailing user message exists to satisfy OpenAI-compatible
    /// model APIs (e.g. the MiniMax M3 endpoint) that reject a
    /// system-only payload with "chat content is empty". Its content is
    /// the original task description, which is already in the system
    /// prompt — small duplication, but it keeps the call valid.
    pub fn to_request(
        &self,
        graph_snapshot: &str,
        temperature: f64,
        max_tokens: Option<usize>,
    ) -> ModelRequest {
        let mut messages = vec![
            Message::system(self.system_prompt.clone()),
            Message::system(format!(
                "Current relationship-graph snapshot (authoritative — your beliefs about the graph should match this):\n{graph_snapshot}"
            )),
            Message::user(format!("Task: {}", self.task_description)),
        ];
        // Include last 8 conversation messages so model sees recent results.
        let recent = &self.messages[self.messages.len().saturating_sub(8)..];
        for m in recent {
            messages.push(m.clone());
        }
        ModelRequest {
            messages,
            tools: Vec::new(),
            temperature,
            max_tokens,
            stop: Vec::new(),
        }
    }

    /// Render the conversation history (without the system prompt) as a
    /// human-readable transcript. Useful for demo logs and audit dumps.
    pub fn transcript(&self) -> String {
        let mut s = String::new();
        for m in &self.messages {
            let tag = match m.role {
                Role::System => "SYS",
                Role::User => "USER",
                Role::Assistant => "AGENT",
                Role::Tool => "TOOL",
            };
            s.push('[');
            s.push_str(tag);
            s.push_str("] ");
            s.push_str(&m.content);
            s.push('\n');
        }
        s
    }

    /// Convenience: count distinct user-turn entries (after the initial task line).
    pub fn user_turns(&self) -> usize {
        self.messages
            .iter()
            .skip(1)
            .filter(|m| matches!(m.role, Role::User))
            .count()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_pins_task_as_first_user_message() {
        let c = Conversation::new("you are an agent", "plan a relocation");
        assert_eq!(c.messages.len(), 1);
        assert!(matches!(c.messages[0].role, Role::User));
        assert!(c.messages[0].content.contains("plan a relocation"));
        assert_eq!(c.round, 0);
    }

    #[test]
    fn add_assistant_increments_round() {
        let mut c = Conversation::new("sys", "task");
        c.add_assistant("ok");
        assert_eq!(c.round, 1);
        c.add_assistant("ok again");
        assert_eq!(c.round, 2);
    }

    #[test]
    fn add_user_does_not_increment_round() {
        let mut c = Conversation::new("sys", "task");
        c.add_user("more info");
        assert_eq!(c.round, 0);
    }

    #[test]
    fn to_request_emits_system_snapshot_and_task_user() {
        // The request always starts with system prompt, graph snapshot,
        // and task line. Recent conversation history is appended AFTER
        // these three headers (up to last 8 messages).
        let mut c = Conversation::new("be helpful", "do thing");
        c.add_assistant("understood");
        c.add_user("here is more");

        let req = c.to_request("nodes: 2 edges: 1", 0.2, Some(512));
        // 3 headers + 2 history messages = 5
        assert_eq!(req.messages.len(), 5);
        assert!(req.messages[0].content.contains("be helpful"));
        assert!(req.messages[1].content.contains("nodes: 2 edges: 1"));
        assert!(req.messages[2].content.contains("Task: do thing"));
        assert!(req.messages[3].content.contains("understood"));
        assert!(req.messages[4].content.contains("here is more"));
    }

    #[test]
    fn to_request_includes_recent_history() {
        // Recent history (last 8 messages) is replayed so the model
        // knows what happened in previous steps.
        let mut c = Conversation::new("be helpful", "do thing");
        c.add_assistant("agent step 1 said this");
        c.add_tool_result("tool result 1 was this");
        c.add_assistant("agent step 2 said this");
        c.add_tool_result("tool result 2 was this");
        c.add_user("and a user message in the middle");

        let req = c.to_request("graph: 1 node", 0.0, None);
        // 3 headers + 5 history = 8
        assert_eq!(req.messages.len(), 8);
        // History messages appear after the headers.
        assert!(req.messages[3].content.contains("agent step 1"));
        assert!(req.messages[4].content.contains("tool result 1"));
        assert!(req.messages[7].content.contains("user message in the middle"));
    }

    #[test]
    fn user_turns_excludes_initial_task_line() {
        let mut c = Conversation::new("sys", "task");
        // The task itself is a user turn under the hood, but we don't count it.
        assert_eq!(c.user_turns(), 0);
        c.add_user("answer 1");
        c.add_assistant("ack 1");
        c.add_user("answer 2");
        assert_eq!(c.user_turns(), 2);
    }

    #[test]
    fn transcript_has_one_line_per_message() {
        let mut c = Conversation::new("sys", "task");
        c.add_assistant("hello");
        c.add_user("hi back");
        let t = c.transcript();
        assert_eq!(t.lines().count(), 3);
        assert!(t.contains("[USER]"));
        assert!(t.contains("[AGENT]"));
    }
}
