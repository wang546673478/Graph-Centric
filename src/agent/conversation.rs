//! Conversation state — multi-turn dialogue + a graph snapshot.
//!
//! A `Conversation` carries the system prompt, the original task description,
//! and the running history of user/assistant turns. It does **not** own the
//! graph itself (that lives at the agent layer above); rather, when we need
//! to invoke the model, we render the current graph into a `ModelRequest`
//! as a system-level reminder.
//!
//! Design notes:
//!
//! - The system prompt is kept separate from the message history so we can
//!   re-render it each turn (cheaper than embedding it in every message).
//! - The graph snapshot is re-injected on every model call. This costs some
//!   prompt tokens but keeps the model's belief about graph state always
//!   in sync — there is no "stale graph in the model's head" failure mode.
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

    /// Turn the conversation into a `ModelRequest`. The graph snapshot is
    /// inserted as a second system message so the model sees it before any
    /// user message.
    pub fn to_request(
        &self,
        graph_snapshot: &str,
        temperature: f64,
        max_tokens: Option<usize>,
    ) -> ModelRequest {
        let mut messages = Vec::with_capacity(self.messages.len() + 2);
        messages.push(Message::system(self.system_prompt.clone()));
        messages.push(Message::system(format!(
            "Current relationship-graph snapshot (authoritative — your beliefs about the graph should match this):\n{graph_snapshot}"
        )));
        messages.extend(self.messages.iter().cloned());
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
    fn to_request_injects_system_and_snapshot_before_history() {
        let mut c = Conversation::new("be helpful", "do thing");
        c.add_assistant("understood");
        c.add_user("here is more");

        let req = c.to_request("nodes: 2 edges: 1", 0.2, Some(512));
        // [system, system-snapshot, user-task, assistant, user]
        assert_eq!(req.messages.len(), 5);
        assert!(matches!(req.messages[0].role, Role::System));
        assert!(req.messages[0].content.contains("be helpful"));
        assert!(matches!(req.messages[1].role, Role::System));
        assert!(req.messages[1].content.contains("nodes: 2 edges: 1"));
        assert!(matches!(req.messages[2].role, Role::User));
        assert!(req.messages[2].content.contains("do thing"));
        assert_eq!(req.temperature, 0.2);
        assert_eq!(req.max_tokens, Some(512));
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
