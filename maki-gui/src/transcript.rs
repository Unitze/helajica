//! The conversation as the window shows it, folded from agent events. Kept
//! free of GPUI so the folding is testable on its own.

use std::fmt::Write as _;

use maki_agent::{AgentEvent, Envelope, ToolOutput};
use maki_providers::{ContentBlock, Message, MessageKind, Role, TokenUsage};
use serde_json::Value;
use similar::TextDiff;

const DIFF_CONTEXT_LINES: usize = 3;
const CANCELLED: &str = "Cancelled";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub input: Option<Value>,
    pub output: String,
    pub status: ToolStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub enum Item {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(ToolCall),
    Notice { text: String, level: Level },
}

#[derive(Debug, Default)]
pub struct Totals {
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub context_size: u32,
    pub context_window: u32,
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub items: Vec<Item>,
    pub working: bool,
    pub totals: Totals,
}

/// What a fold changed, so the view remeasures only what moved.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    None,
    Appended,
    Updated(usize),
}

impl Transcript {
    pub fn from_history(history: &[Message]) -> Self {
        let mut transcript = Self::default();
        for message in history
            .iter()
            .filter(|m| matches!(m.kind, MessageKind::Turn))
        {
            transcript.push_message(message);
        }
        transcript
    }

    fn push_message(&mut self, message: &Message) {
        if message.role == Role::User
            && let Some(text) = &message.display_text
        {
            self.items.push(Item::User(text.clone()));
            return;
        }
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => self.items.push(match message.role {
                    Role::User => Item::User(text.clone()),
                    Role::Assistant => Item::Assistant(text.clone()),
                }),
                ContentBlock::Thinking { thinking, .. } => {
                    self.items.push(Item::Thinking(thinking.clone()))
                }
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => self.items.push(Item::Tool(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    summary: String::new(),
                    input: Some(input.clone()),
                    output: String::new(),
                    status: ToolStatus::Done,
                })),
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    if let Some(call) = self.tool_mut(tool_use_id) {
                        call.output = content.clone();
                        call.status = status(*is_error);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn push_user(&mut self, text: String) {
        self.items.push(Item::User(text));
        self.working = true;
    }

    pub fn notice(&mut self, text: impl Into<String>, level: Level) {
        self.items.push(Item::Notice {
            text: text.into(),
            level,
        });
    }

    /// Folds one event in. Subagent events are left to their parent's tool
    /// row: interleaving them would scramble the main conversation.
    pub fn apply(&mut self, envelope: &Envelope) -> Change {
        if envelope.subagent.is_some() {
            return Change::None;
        }
        match &envelope.event {
            AgentEvent::TextDelta { text } => self.extend(text, false),
            AgentEvent::ThinkingDelta { text } => self.extend(text, true),
            AgentEvent::ToolPending { id, name } => self.upsert_tool(id, |call| {
                call.name.clone_from(name);
            }),
            AgentEvent::ToolStart(start) => self.upsert_tool(&start.id, |call| {
                call.name = start.tool.to_string();
                call.summary.clone_from(&start.summary);
                call.input.clone_from(&start.raw_input);
            }),
            AgentEvent::ToolOutput { id, content } => self.upsert_tool(id, |call| {
                call.output.clone_from(content);
            }),
            AgentEvent::ToolDone(done) => self.upsert_tool(&done.id, |call| {
                call.name = done.tool.to_string();
                call.output = output_text(&done.output);
                call.status = status(done.is_error);
            }),
            AgentEvent::TurnComplete(turn) => {
                add_usage(&mut self.totals.usage, &turn.usage);
                self.totals.context_window = turn.context_window;
                if let Some(size) = turn.context_size {
                    self.totals.context_size = size;
                }
                Change::None
            }
            AgentEvent::Done {
                cost,
                context_size,
                context_window,
                reason,
                ..
            } => {
                self.working = false;
                self.totals.cost = sum(self.totals.cost, *cost);
                self.totals.context_size = *context_size;
                self.totals.context_window = *context_window;
                self.settle_running();
                match reason.to_string().as_str() {
                    "cancelled" => self.push_notice(CANCELLED, Level::Info),
                    _ => Change::None,
                }
            }
            AgentEvent::Error { message, .. } => {
                self.working = false;
                self.settle_running();
                self.push_notice(message, Level::Error)
            }
            AgentEvent::Retry { message, .. } => self.push_notice(message, Level::Warning),
            AgentEvent::Notice { text } | AgentEvent::Steered { text, .. } => {
                self.push_notice(text, Level::Info)
            }
            AgentEvent::CompactionDone {
                context_size_before,
                context_size_after,
                ..
            } => self.push_notice(
                format!("Compacted context {context_size_before} → {context_size_after} tokens"),
                Level::Info,
            ),
            _ => Change::None,
        }
    }

    fn push_notice(&mut self, text: impl Into<String>, level: Level) -> Change {
        self.notice(text, level);
        Change::Appended
    }

    fn extend(&mut self, text: &str, thinking: bool) -> Change {
        let last = self.items.len().wrapping_sub(1);
        match (self.items.last_mut(), thinking) {
            (Some(Item::Assistant(buf)), false) | (Some(Item::Thinking(buf)), true) => {
                buf.push_str(text);
                Change::Updated(last)
            }
            _ => {
                let text = text.to_owned();
                self.items.push(match thinking {
                    true => Item::Thinking(text),
                    false => Item::Assistant(text),
                });
                Change::Appended
            }
        }
    }

    fn upsert_tool(&mut self, id: &str, update: impl FnOnce(&mut ToolCall)) -> Change {
        if let Some(index) = self.tool_index(id) {
            if let Item::Tool(call) = &mut self.items[index] {
                update(call);
            }
            return Change::Updated(index);
        }
        let mut call = ToolCall {
            id: id.to_owned(),
            name: String::new(),
            summary: String::new(),
            input: None,
            output: String::new(),
            status: ToolStatus::Running,
        };
        update(&mut call);
        self.items.push(Item::Tool(call));
        Change::Appended
    }

    fn tool_index(&self, id: &str) -> Option<usize> {
        self.items
            .iter()
            .rposition(|item| matches!(item, Item::Tool(call) if call.id == id))
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        let index = self.tool_index(id)?;
        match &mut self.items[index] {
            Item::Tool(call) => Some(call),
            _ => None,
        }
    }

    pub fn tool(&self, id: &str) -> Option<&ToolCall> {
        match self.items.get(self.tool_index(id)?) {
            Some(Item::Tool(call)) => Some(call),
            _ => None,
        }
    }

    /// A run that ended cannot finish the tools it left behind.
    fn settle_running(&mut self) {
        for item in &mut self.items {
            if let Item::Tool(call) = item
                && call.status == ToolStatus::Running
            {
                call.status = ToolStatus::Failed;
            }
        }
    }
}

fn status(is_error: bool) -> ToolStatus {
    match is_error {
        true => ToolStatus::Failed,
        false => ToolStatus::Done,
    }
}

fn sum(total: Option<f64>, add: Option<f64>) -> Option<f64> {
    match (total, add) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    }
}

fn add_usage(total: &mut TokenUsage, add: &TokenUsage) {
    total.input += add.input;
    total.output += add.output;
    total.cache_read += add.cache_read;
    total.cache_creation += add.cache_creation;
}

pub fn output_text(output: &ToolOutput) -> String {
    match output {
        ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text) => {
            text.text.clone()
        }
        ToolOutput::Diff {
            path,
            before,
            after,
            summary,
        } => {
            let diff = TextDiff::from_lines(before, after);
            let unified = diff
                .unified_diff()
                .context_radius(DIFF_CONTEXT_LINES)
                .header(path, path)
                .to_string();
            format!("{summary}\n{unified}")
        }
        ToolOutput::ReadCode {
            path,
            start_line,
            lines,
            ..
        } => format!(
            "{path}  lines {start_line}–{}",
            start_line + lines.len().saturating_sub(1)
        ),
        ToolOutput::WriteCode {
            path, byte_count, ..
        } => format!("Wrote {byte_count} bytes to {path}"),
        ToolOutput::TodoList(items) => items.iter().fold(String::new(), |mut out, item| {
            let _ = writeln!(out, "{} {}", item.status.marker(), item.content);
            out
        }),
        ToolOutput::GrepResult { entries } => {
            entries.iter().fold(String::new(), |mut out, entry| {
                for line in entry.groups.iter().flat_map(|g| &g.lines) {
                    let _ = writeln!(out, "{}:{}: {}", entry.path, line.line_nr, line.text);
                }
                out
            })
        }
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use maki_agent::ToolDoneEvent;
    use std::sync::Arc;

    use super::*;

    const TOOL_ID: &str = "t1";
    const TOOL_NAME: &str = "bash";

    fn envelope(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: 0,
        }
    }

    #[test]
    fn text_deltas_stream_into_one_reply() {
        let mut t = Transcript::default();
        assert_eq!(
            t.apply(&envelope(AgentEvent::TextDelta { text: "he".into() })),
            Change::Appended
        );
        assert_eq!(
            t.apply(&envelope(AgentEvent::TextDelta { text: "llo".into() })),
            Change::Updated(0)
        );
        assert!(matches!(&t.items[..], [Item::Assistant(s)] if s == "hello"));
    }

    #[test]
    fn tool_lifecycle_updates_one_row() {
        let mut t = Transcript::default();
        t.apply(&envelope(AgentEvent::ToolPending {
            id: TOOL_ID.into(),
            name: TOOL_NAME.into(),
        }));
        let change = t.apply(&envelope(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: TOOL_ID.into(),
            tool: Arc::from(TOOL_NAME),
            output: Arc::new(ToolOutput::Plain("ok".into())),
            is_error: false,
            annotation: None,
            written_path: None,
            call: None,
        }))));
        assert_eq!(change, Change::Updated(0));
        let call = t.tool(TOOL_ID).unwrap();
        assert_eq!(
            (call.status, call.output.as_str()),
            (ToolStatus::Done, "ok")
        );
    }

    #[test]
    fn subagent_events_stay_out_of_the_main_thread() {
        let mut t = Transcript::default();
        let mut env = envelope(AgentEvent::TextDelta { text: "x".into() });
        env.subagent = Some(maki_agent::SubagentInfo {
            parent_tool_use_id: TOOL_ID.into(),
            name: TOOL_NAME.into(),
            prompt: None,
            model: None,
            opts: None,
            answer_tx: None,
            inbox: None,
        });
        assert_eq!(t.apply(&env), Change::None);
        assert!(t.items.is_empty());
    }
}
