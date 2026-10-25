//! The conversation as the window shows it, folded from agent events. Kept
//! free of GPUI so the folding is testable on its own.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;
use std::time::{Duration, Instant};

use maki_agent::types::TodoItem;
use maki_agent::{AgentEvent, DoneReason, Envelope, SubagentInfo, ToolOutput};
use maki_providers::{ContentBlock, ImageSource, Message, MessageKind, Role, TokenUsage};
use serde::Deserialize as _;
use serde_json::Value;
use similar::TextDiff;

const DIFF_CONTEXT_LINES: usize = 3;
const CANCELLED: &str = "Cancelled";
const TODO_TOOL: &str = "todo_write";
/// How a tool result that returned an image is captioned in the history.
const IMAGE_CAPTION_PREFIX: &str = "[image: ";
const TODOS_FIELD: &str = "todos";
const TEXT_BLOCK_SEPARATOR: &str = "\n";

/// Input fields that say what a call was pointed at, most telling first.
const TARGET_FIELDS: [&str; 8] = [
    "description",
    "command",
    "path",
    "file_path",
    "pattern",
    "url",
    "query",
    "code",
];

fn retry_activity(attempt: u32, message: &str, delay_ms: u64) -> String {
    if delay_ms == 0 {
        format!("Retry {attempt}: {message}")
    } else {
        format!("Retry {attempt} in {}s: {message}", delay_ms.div_ceil(1000))
    }
}

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
    /// When it ran, for calls seen live; a resumed session has no times.
    pub started: Option<Instant>,
    pub finished: Option<Instant>,
    /// The picture a call read, such as a screenshot it looked at.
    pub image: Option<ImageSource>,
}

impl ToolCall {
    /// The summary the agent gave, or, for a call from a resumed session that
    /// has none, the first line of its most telling input field.
    pub fn target(&self) -> &str {
        if !self.summary.is_empty() {
            return &self.summary;
        }
        self.input
            .as_ref()
            .and_then(|input| {
                TARGET_FIELDS
                    .iter()
                    .find_map(|field| input[*field].as_str())
            })
            .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
            .map_or("", str::trim)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub enum Item {
    User {
        text: String,
        images: Vec<ImageSource>,
    },
    Assistant(String),
    Thinking(String),
    Tool(ToolCall),
    Notice {
        text: String,
        level: Level,
    },
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
    pub stopping: bool,
    pub pending_messages: usize,
    pub subagents: BTreeMap<String, SubagentProgress>,
    turn_start: usize,
    /// When the agent last started working, for the live status line.
    pub turn_started: Option<Instant>,
    /// Time spent thinking since then, shown instead of the thinking itself.
    pub thinking: ThinkingClock,
    pub totals: Totals,
}

#[derive(Debug)]
pub struct SubagentProgress {
    pub info: SubagentInfo,
    pub activity: String,
    pub status: ToolStatus,
    pub turns: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ThinkingClock {
    spent: Duration,
    since: Option<Instant>,
}

impl ThinkingClock {
    fn start(&mut self) {
        self.since.get_or_insert_with(Instant::now);
    }

    fn stop(&mut self) {
        if let Some(since) = self.since.take() {
            self.spent += since.elapsed();
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.spent + self.since.map_or(Duration::ZERO, |since| since.elapsed())
    }
}

/// What a fold changed, so the view remeasures only what moved.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    None,
    Status,
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
        transcript.turn_start = transcript
            .items
            .iter()
            .rposition(|item| matches!(item, Item::User { .. }))
            .unwrap_or(0);
        transcript
    }

    fn push_message(&mut self, message: &Message) {
        let results = message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
        if results {
            return self.push_results(message);
        }
        if message.role == Role::User {
            let images: Vec<ImageSource> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Image { source } => Some(source.clone()),
                    _ => None,
                })
                .collect();
            let text = message
                .display_text
                .clone()
                .unwrap_or_else(|| joined_text(message));
            if !text.is_empty() || !images.is_empty() {
                self.items.push(Item::User { text, images });
            }
            if message.display_text.is_some() {
                return;
            }
        }
        for block in &message.content {
            match block {
                ContentBlock::Text { text } if message.role == Role::Assistant => {
                    self.items.push(Item::Assistant(text.clone()))
                }
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
                    started: None,
                    finished: None,
                    image: None,
                })),
                _ => {}
            }
        }
    }

    /// Tool results carry the pictures they returned after all of them, in
    /// the order of the results captioned as images.
    fn push_results(&mut self, message: &Message) {
        let mut images = message.content.iter().filter_map(|block| match block {
            ContentBlock::Image { source } => Some(source),
            _ => None,
        });
        for block in &message.content {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } = block
            else {
                continue;
            };
            let image = content
                .starts_with(IMAGE_CAPTION_PREFIX)
                .then(|| images.next().cloned())
                .flatten();
            if let Some(call) = self.tool_mut(tool_use_id) {
                call.output = content.clone();
                call.status = status(*is_error);
                call.image = image;
            }
        }
        let text = joined_text(message);
        if !text.is_empty() {
            self.items.push(Item::User {
                text,
                images: Vec::new(),
            });
        }
    }

    pub fn push_user(&mut self, text: String, images: Vec<ImageSource>) {
        self.items.push(Item::User { text, images });
        self.pending_messages += 1;
        if !self.working {
            self.begin_turn();
        }
    }

    pub fn begin_turn(&mut self) {
        self.turn_start = self.items.len().saturating_sub(1);
        self.working = true;
        self.stopping = false;
        self.turn_started = Some(Instant::now());
        self.thinking = ThinkingClock::default();
    }

    pub fn request_stop(&mut self) -> bool {
        if !self.working || self.stopping {
            return false;
        }
        self.stopping = true;
        true
    }

    /// Follow-ups steer the same run, so its checklist stays visible.
    pub fn todos(&self) -> Vec<TodoItem> {
        self.items[self.turn_start.min(self.items.len())..]
            .iter()
            .rev()
            .find_map(|item| match item {
                Item::Tool(call) if call.name == TODO_TOOL => call.input.as_ref(),
                _ => None,
            })
            .and_then(|input| Vec::<TodoItem>::deserialize(&input[TODOS_FIELD]).ok())
            .unwrap_or_default()
    }

    pub fn notice(&mut self, text: impl Into<String>, level: Level) {
        self.items.push(Item::Notice {
            text: text.into(),
            level,
        });
    }

    /// A notice nothing reformats beyond its own line breaks.
    fn notice_plain(&mut self, text: String, level: Level) -> Change {
        self.notice(format!("```text\n{text}\n```"), level);
        Change::Appended
    }

    /// Folds one event in. Subagent events are left to their parent's tool
    /// row: interleaving them would scramble the main conversation.
    pub fn apply(&mut self, envelope: &Envelope) -> Change {
        if let Some(info) = &envelope.subagent {
            let progress = self
                .subagents
                .entry(info.parent_tool_use_id.clone())
                .or_insert_with(|| SubagentProgress {
                    info: info.clone(),
                    activity: "Starting…".into(),
                    status: ToolStatus::Running,
                    turns: 0,
                });
            if matches!(
                envelope.event,
                AgentEvent::ThinkingDelta { .. }
                    | AgentEvent::TextDelta { .. }
                    | AgentEvent::ToolStart(_)
            ) {
                progress.status = ToolStatus::Running;
            }
            match &envelope.event {
                AgentEvent::Notice { text } => progress.activity.clone_from(text),
                AgentEvent::ThinkingDelta { .. } if progress.activity != "Thinking…" => {
                    progress.activity = "Thinking…".into()
                }
                AgentEvent::TextDelta { .. } if progress.activity != "Writing…" => {
                    progress.activity = "Writing…".into()
                }
                AgentEvent::ToolStart(start) => {
                    progress.activity = format!("{} {}", start.tool, start.summary)
                }
                AgentEvent::TurnComplete(_) => progress.turns += 1,
                AgentEvent::Retry {
                    attempt,
                    message,
                    delay_ms,
                } => progress.activity = retry_activity(*attempt, message, *delay_ms),
                AgentEvent::Done { reason, .. } => {
                    let (state, activity) = match reason {
                        DoneReason::EndTurn => (ToolStatus::Done, "Finished"),
                        DoneReason::Compact => (ToolStatus::Done, "Compacted"),
                        DoneReason::Cancelled => (ToolStatus::Failed, "Stopped"),
                        DoneReason::MaxTurns => (ToolStatus::Failed, "Turn limit reached"),
                        DoneReason::MaxTokens => (ToolStatus::Failed, "Token limit reached"),
                        DoneReason::Dropped => (ToolStatus::Failed, "Skipped"),
                    };
                    progress.status = state;
                    progress.activity = activity.into();
                }
                AgentEvent::Error { message, .. } => {
                    progress.status = ToolStatus::Failed;
                    progress.activity.clone_from(message);
                }
                AgentEvent::StreamClosed if progress.status == ToolStatus::Running => {
                    progress.status = ToolStatus::Done;
                    progress.activity = "Finished".into();
                }
                _ => return Change::None,
            }
            return Change::Status;
        }
        if !self.working
            && matches!(
                envelope.event,
                AgentEvent::TextDelta { .. }
                    | AgentEvent::ThinkingDelta { .. }
                    | AgentEvent::ToolPending { .. }
                    | AgentEvent::ToolStart(_)
            )
        {
            self.begin_turn();
        }
        match &envelope.event {
            AgentEvent::ThinkingDelta { .. } => self.thinking.start(),
            _ => self.thinking.stop(),
        }
        match &envelope.event {
            AgentEvent::QueueItemConsumed { .. } => {
                if !self.working && self.pending_messages > 0 {
                    self.begin_turn();
                }
                self.pending_messages = self.pending_messages.saturating_sub(1);
                Change::Status
            }
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
            AgentEvent::ToolDone(done) => {
                if done.is_error
                    && let Some(progress) = self.subagents.get_mut(&done.id)
                {
                    progress.status = ToolStatus::Failed;
                    progress.activity = "Failed".into();
                }
                self.upsert_tool(&done.id, |call| {
                    call.name = done.tool.to_string();
                    call.output = output_text(&done.output);
                    call.status = status(done.is_error);
                    if let ToolOutput::Image { source, .. } = done.output.as_ref() {
                        call.image = Some(source.clone());
                    }
                    call.finished = Some(Instant::now());
                })
            }
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
                self.stopping = false;
                self.totals.cost = sum(self.totals.cost, *cost);
                self.totals.context_size = *context_size;
                self.totals.context_window = *context_window;
                self.settle_running();
                match reason {
                    DoneReason::Cancelled => self.push_notice(CANCELLED, Level::Info),
                    DoneReason::MaxTurns => self.push_notice(
                        "Turn limit reached. The task is incomplete.",
                        Level::Warning,
                    ),
                    DoneReason::MaxTokens => self.push_notice(
                        "Token limit reached. The task is incomplete.",
                        Level::Warning,
                    ),
                    _ => Change::None,
                }
            }
            AgentEvent::Error { message, .. } => {
                self.working = false;
                self.stopping = false;
                self.settle_running();
                // An error is shown as it came. The transcript is markdown, so
                // a provider body (JSON, HTML) is fenced: unfenced it would be
                // reflowed and its angle brackets read as markup.
                self.notice_plain(message.clone(), Level::Error)
            }
            AgentEvent::Retry {
                attempt,
                message,
                delay_ms,
            } => {
                let text = retry_activity(*attempt, message, *delay_ms);
                if text.contains('\n') {
                    self.notice_plain(text, Level::Warning)
                } else {
                    self.push_notice(text, Level::Warning)
                }
            }
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
            started: Some(Instant::now()),
            finished: None,
            image: None,
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
                call.finished = Some(Instant::now());
            }
        }
    }
}

/// The tool calls around `index`, with the thinking between them, that read
/// as one piece of work. `None` when `index` is not part of one.
pub fn step_run(items: &[Item], index: usize) -> Option<Range<usize>> {
    let is_step = |item: &Item| matches!(item, Item::Tool(_) | Item::Thinking(_));
    if !items.get(index).is_some_and(is_step) {
        return None;
    }
    let start = items[..index]
        .iter()
        .rposition(|item| !is_step(item))
        .map_or(0, |i| i + 1);
    let end = items[index..]
        .iter()
        .position(|item| !is_step(item))
        .map_or(items.len(), |i| index + i);
    items[start..end]
        .iter()
        .any(|item| matches!(item, Item::Tool(_)))
        .then_some(start..end)
}

pub fn step_runs(items: &[Item]) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut has_tool = false;
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::Tool(_) => has_tool = true,
            Item::Thinking(_) => {}
            _ => {
                if has_tool {
                    runs.push(start..index);
                }
                start = index + 1;
                has_tool = false;
            }
        }
    }
    if has_tool {
        runs.push(start..items.len());
    }
    runs
}

/// From the first call starting to the last one finishing, or to `now` while
/// one still runs. `None` when the calls were not seen live.
pub fn run_elapsed(steps: &[Item], now: Instant) -> Option<Duration> {
    let mut calls = steps.iter().filter_map(|item| match item {
        Item::Tool(call) => Some(call),
        _ => None,
    });
    let started = calls.next()?.started?;
    let finished = steps
        .iter()
        .filter_map(|item| match item {
            Item::Tool(call) => Some(call.finished.unwrap_or(now)),
            _ => None,
        })
        .max()?;
    Some(finished.saturating_duration_since(started))
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

fn joined_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(TEXT_BLOCK_SEPARATOR)
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
        ToolOutput::Image { text, .. } => text.clone(),
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
    use std::sync::Arc;

    use maki_agent::ToolDoneEvent;
    use maki_providers::ImageMediaType;
    use test_case::test_case;

    use super::*;

    const TOOL_ID: &str = "t1";
    const TOOL_NAME: &str = "bash";
    const OTHER_TOOL_ID: &str = "t2";
    const PIXELS: &str = "aGVsbG8=";
    const TEXT_RESULT: &str = "ok";
    const IMAGE_RESULT: &str = "[image: shot.png 1KB]";
    const NO_USER_ITEM: &str = "tool results are not a user message";
    const ONE_TODO: &str = r#"[{"content":"a","status":"pending"}]"#;
    const TWO_TODOS: &str =
        r#"[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"}]"#;
    const RETRY_MESSAGE: &str = "provider request timed out";
    const RETRY_NOTICE: &str = "Retry 3: provider request timed out";

    #[test_case(false ; "cancel_confirmed")]
    #[test_case(true ; "request_failed")]
    fn stop_request_waits_for_terminal_confirmation(failed: bool) {
        let mut transcript = Transcript::default();
        assert!(!transcript.request_stop());
        transcript.begin_turn();
        assert!(transcript.request_stop());
        assert!(!transcript.request_stop());
        transcript.apply(&envelope(AgentEvent::TextDelta {
            text: TEXT_RESULT.into(),
        }));
        assert!(transcript.working && transcript.stopping);
        transcript.apply(&envelope(if failed {
            AgentEvent::Error {
                message: RETRY_MESSAGE.into(),
                auth: false,
            }
        } else {
            AgentEvent::Done {
                reason: DoneReason::Cancelled,
                usage: TokenUsage::default(),
                cost: None,
                list_cost: None,
                context_size: 0,
                context_window: 0,
                num_turns: 0,
            }
        }));
        assert!(!transcript.working && !transcript.stopping);
        transcript.begin_turn();
        assert!(transcript.request_stop());
    }

    #[test]
    fn retries_show_the_attempt_and_reason() {
        let mut transcript = Transcript::default();
        transcript.begin_turn();
        transcript.apply(&envelope(AgentEvent::Retry {
            attempt: 3,
            message: RETRY_MESSAGE.into(),
            delay_ms: 0,
        }));
        assert!(transcript.working);
        assert!(matches!(
            transcript.items.last(),
            Some(Item::Notice { text, level: Level::Warning }) if text == RETRY_NOTICE
        ));
    }

    #[test]
    fn retries_show_the_server_cooldown() {
        let mut transcript = Transcript::default();
        transcript.begin_turn();
        transcript.apply(&envelope(AgentEvent::Retry {
            attempt: 1,
            message: "Rate limited (429)".into(),
            delay_ms: 90_001,
        }));
        assert!(matches!(
            transcript.items.last(),
            Some(Item::Notice { text, level: Level::Warning }) if text == "Retry 1 in 91s: Rate limited (429)"
        ));
    }

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
    fn followups_are_acknowledged_without_resetting_the_running_turn() {
        let mut t = Transcript::default();
        t.push_user("first".into(), Vec::new());
        t.apply(&envelope(AgentEvent::QueueItemConsumed {
            text: "first".into(),
            images: Vec::new(),
        }));
        let started = t.turn_started;
        t.push_user("follow up".into(), Vec::new());
        assert_eq!(t.pending_messages, 1);
        assert_eq!(t.turn_started, started);
        assert_eq!(
            t.apply(&envelope(AgentEvent::QueueItemConsumed {
                text: "follow up".into(),
                images: Vec::new()
            })),
            Change::Status
        );
        assert_eq!(t.pending_messages, 0);
        assert_eq!(t.turn_started, started);
    }

    #[test]
    fn a_background_wakeup_becomes_visible_as_working() {
        let mut t = Transcript::default();
        t.apply(&envelope(AgentEvent::ToolPending {
            id: TOOL_ID.into(),
            name: TOOL_NAME.into(),
        }));
        assert!(t.working);
        assert!(t.turn_started.is_some());
    }

    #[test]
    fn thinking_time_stops_once_the_reply_starts() {
        let mut t = Transcript::default();
        t.apply(&envelope(AgentEvent::ThinkingDelta { text: "hmm".into() }));
        t.apply(&envelope(AgentEvent::TextDelta { text: "hi".into() }));
        let frozen = t.thinking.elapsed();
        assert_eq!(t.thinking.elapsed(), frozen);
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

    fn tool() -> Item {
        Item::Tool(ToolCall {
            id: TOOL_ID.into(),
            name: TOOL_NAME.into(),
            summary: String::new(),
            input: None,
            output: String::new(),
            status: ToolStatus::Done,
            started: None,
            finished: None,
            image: None,
        })
    }

    #[test_case("ran it", None, "ran it" ; "summary_wins")]
    #[test_case("", Some(r#"{"command":"cargo build","path":"x"}"#), "cargo build" ; "command")]
    #[test_case("", Some(r#"{"code":"\n  import os\nprint(1)"}"#), "import os" ; "first_code_line")]
    #[test_case("", Some(r#"{"other":1}"#), "" ; "nothing_telling")]
    #[test_case("", None, "" ; "no_input")]
    fn calls_say_what_they_were_pointed_at(summary: &str, input: Option<&str>, expected: &str) {
        let Item::Tool(mut call) = tool() else {
            unreachable!()
        };
        call.summary = summary.into();
        call.input = input.map(|json| serde_json::from_str(json).unwrap());
        assert_eq!(call.target(), expected);
    }

    fn todo_call(list: &str) -> Item {
        let Item::Tool(mut call) = tool() else {
            unreachable!()
        };
        call.name = TODO_TOOL.into();
        call.input =
            Some(serde_json::json!({ TODOS_FIELD: serde_json::from_str::<Value>(list).unwrap() }));
        Item::Tool(call)
    }

    #[test_case(&[], 0 ; "no_list")]
    #[test_case(&[ONE_TODO], 1 ; "one_list")]
    #[test_case(&[ONE_TODO, TWO_TODOS], 2 ; "the_latest_list_wins")]
    fn todos_come_from_the_latest_todo_write(lists: &[&str], expected: usize) {
        let mut transcript = Transcript::default();
        transcript.push_user(String::new(), Vec::new());
        transcript
            .items
            .extend(lists.iter().map(|list| todo_call(list)));
        assert_eq!(transcript.todos().len(), expected);
    }

    #[test]
    fn a_new_prompt_clears_the_todos() {
        let mut transcript = Transcript::default();
        transcript.items.push(todo_call(ONE_TODO));
        transcript.push_user(String::new(), Vec::new());
        assert!(transcript.todos().is_empty());
    }

    #[test]
    fn a_followup_keeps_the_running_checklist() {
        let mut transcript = Transcript::default();
        transcript.push_user("start".into(), Vec::new());
        transcript.items.push(todo_call(ONE_TODO));
        transcript.push_user("follow up".into(), Vec::new());
        assert_eq!(transcript.todos().len(), 1);
    }

    #[test]
    fn a_resumed_image_result_keeps_its_picture_on_the_call() {
        let pixels = ImageSource::new(ImageMediaType::Png, PIXELS.into());
        let history = [
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::tool_use(TOOL_ID, TOOL_NAME, Value::Null),
                    ContentBlock::tool_use(OTHER_TOOL_ID, TOOL_NAME, Value::Null),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::tool_result(OTHER_TOOL_ID, TEXT_RESULT, false),
                    ContentBlock::tool_result(TOOL_ID, IMAGE_RESULT, false),
                    ContentBlock::Image {
                        source: pixels.clone(),
                    },
                ],
                ..Default::default()
            },
        ];
        let transcript = Transcript::from_history(&history);
        let images: Vec<_> = transcript
            .items
            .iter()
            .map(|item| match item {
                Item::Tool(call) => call.image.as_ref().map(|image| image.data.clone()),
                _ => panic!("{NO_USER_ITEM}"),
            })
            .collect();
        assert_eq!(images, [Some(pixels.data), None]);
    }

    #[test_case(0, None ; "a_reply_is_no_step")]
    #[test_case(1, Some(1..4) ; "first_call")]
    #[test_case(2, Some(1..4) ; "thinking_between_calls")]
    #[test_case(3, Some(1..4) ; "last_call")]
    #[test_case(5, None ; "thinking_alone")]
    fn calls_run_together_until_a_reply(index: usize, expected: Option<Range<usize>>) {
        let items = [
            Item::Assistant("looking".into()),
            tool(),
            Item::Thinking("hmm".into()),
            tool(),
            Item::Assistant("done".into()),
            Item::Thinking("later".into()),
        ];
        assert_eq!(step_run(&items, index), expected);
    }

    #[test]
    fn a_run_lasts_from_first_start_to_last_finish() {
        let start = Instant::now();
        let timed = |from: u64, to: Option<u64>| {
            let Item::Tool(mut call) = tool() else {
                unreachable!()
            };
            call.started = Some(start + Duration::from_secs(from));
            call.finished = to.map(|to| start + Duration::from_secs(to));
            Item::Tool(call)
        };
        let now = start + Duration::from_secs(60);
        assert_eq!(
            run_elapsed(&[timed(0, Some(5)), timed(6, Some(39))], now),
            Some(Duration::from_secs(39))
        );
        assert_eq!(
            run_elapsed(&[timed(0, Some(5)), timed(6, None)], now),
            Some(Duration::from_secs(60))
        );
        assert_eq!(run_elapsed(&[tool()], now), None);
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
        assert_eq!(t.apply(&env), Change::Status);
        assert!(t.items.is_empty());
        assert_eq!(t.subagents[TOOL_ID].info.name, TOOL_NAME);
        env.event = AgentEvent::ToolStart(Box::new(maki_agent::ToolStartEvent {
            id: "child".into(),
            tool: Arc::from("read"),
            summary: "src/lib.rs".into(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
        }));
        assert_eq!(t.apply(&env), Change::Status);
        assert_eq!(t.subagents[TOOL_ID].activity, "read src/lib.rs");
        t.apply(&envelope(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: TOOL_ID.into(),
            tool: Arc::from("task"),
            output: Arc::new(ToolOutput::Plain("Background task started".into())),
            is_error: false,
            annotation: None,
            written_path: None,
            call: None,
        }))));
        assert_eq!(t.subagents[TOOL_ID].status, ToolStatus::Running);
        env.event = AgentEvent::Error {
            message: "failed".into(),
            auth: false,
        };
        t.apply(&env);
        env.event = AgentEvent::StreamClosed;
        t.apply(&env);
        assert_eq!(t.subagents[TOOL_ID].status, ToolStatus::Failed);
        env.event = AgentEvent::TextDelta {
            text: "resumed".into(),
        };
        t.apply(&env);
        assert_eq!(t.subagents[TOOL_ID].status, ToolStatus::Running);
        assert_eq!(t.subagents[TOOL_ID].activity, "Writing…");
        assert_eq!(t.items.len(), 1);
    }

    #[test_case(DoneReason::EndTurn, ToolStatus::Done, "Finished" ; "finished")]
    #[test_case(DoneReason::MaxTurns, ToolStatus::Failed, "Turn limit reached" ; "turn_limit")]
    #[test_case(DoneReason::MaxTokens, ToolStatus::Failed, "Token limit reached" ; "token_limit")]
    #[test_case(DoneReason::Cancelled, ToolStatus::Failed, "Stopped" ; "stopped")]
    fn subagent_completion_explains_why_it_stopped(
        reason: DoneReason,
        status: ToolStatus,
        activity: &str,
    ) {
        let mut t = Transcript::default();
        let mut env = envelope(AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size: 0,
            context_window: 0,
            num_turns: 1,
            reason,
        });
        env.subagent = Some(SubagentInfo {
            parent_tool_use_id: TOOL_ID.into(),
            name: TOOL_NAME.into(),
            prompt: None,
            model: None,
            opts: None,
            answer_tx: None,
            inbox: None,
        });
        t.apply(&env);
        assert_eq!(t.subagents[TOOL_ID].status, status);
        assert_eq!(t.subagents[TOOL_ID].activity, activity);
    }

    #[test]
    fn grouped_rows_match_individual_lookup_across_long_runs() {
        let mut items = vec![
            Item::Thinking("alone".into()),
            Item::Assistant("reply".into()),
        ];
        items.extend((0..500).map(|_| tool()));
        items.push(Item::Assistant("done".into()));
        items.push(Item::Thinking("between".into()));
        items.push(tool());
        assert_eq!(step_runs(&items), vec![2..502, 503..505]);
        let runs = step_runs(&items);
        for i in 0..items.len() {
            assert_eq!(
                runs.iter().find(|run| run.contains(&i)).cloned(),
                step_run(&items, i)
            );
        }
    }
}
