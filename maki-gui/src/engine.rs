//! The seam between the window and the agent. The binary implements
//! [`Engine`] with the same stack the TUI builds, so a session opened here is
//! the same agent, tools, plugins and prompt cache as `maki` in a terminal.

use std::path::Path;

use color_eyre::Result;
use flume::{Receiver, Sender};
use maki_agent::Envelope;
use maki_agent::permissions::PermissionAnswer;
use maki_providers::{ImageSource, Message, ProviderUsage};
use maki_storage::sessions::{SessionSummary, StoredThinking};
use serde_json::Value;

pub trait Engine: Send + Sync + 'static {
    /// Builds the project's stack and starts a session in it, closing any
    /// session still open: the agent reads the process cwd, so one project
    /// runs at a time. Blocks while plugins load.
    fn open(&self, project: &Path, resume: Option<String>) -> Result<Session>;

    fn sessions(&self, project: &Path) -> Vec<SessionSummary>;

    /// Deletes a saved session, closing it first if it is the open one.
    fn delete_session(&self, id: &str) -> Result<()>;

    /// Records the user's trust in `project`, so its `.maki` config, plugins
    /// and GUI extensions load on the next [`Engine::open`].
    fn trust(&self, project: &Path) -> Result<()>;

    /// Every plugin a session could load: bundled ones and installed packages.
    fn plugins(&self) -> Vec<PluginInfo>;
}

pub struct PluginInfo {
    pub name: String,
    pub kind: PluginKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PluginKind {
    /// Bundled, adds tools.
    Tool,
    /// Bundled, adds a model provider.
    Provider,
    /// Bundled, off unless switched on.
    Optional,
    /// Installed by the user.
    Package,
}

impl PluginKind {
    pub fn on_by_default(self) -> bool {
        self != Self::Optional
    }
}

pub struct Session {
    pub boot: Boot,
    pub commands: Sender<Command>,
    pub updates: Receiver<Update>,
}

pub struct Boot {
    pub session_id: String,
    pub model: String,
    pub models: Vec<String>,
    /// The startup model's, until a turn reports the one in use.
    pub context_window: u32,
    /// What the config asks for; the window may override it.
    pub thinking: StoredThinking,
    pub tools: Vec<String>,
    pub warnings: Vec<String>,
    pub history: Vec<Message>,
    pub slash_commands: Vec<SlashCommand>,
    pub trusted: bool,
    /// Tools run without asking (maki's yolo).
    pub yolo: bool,
}

pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

pub enum Command {
    Prompt {
        text: String,
        plan: bool,
        images: Vec<ImageSource>,
    },
    /// A side question answered from the conversation so far, with no tools
    /// and nothing written to the session. The answer streams back as
    /// [`Update::Btw`].
    Btw(String),
    Cancel,
    Permission {
        id: String,
        answer: PermissionAnswer,
    },
    /// The answer to a `question` tool call; `None` when dismissed.
    Answer(Option<String>),
    /// To a Lua plugin waiting in `maki.gui.recv(topic)`.
    Lua {
        topic: String,
        data: Value,
    },
    Model(String),
    /// Thinking for the prompts sent from now on.
    Thinking(StoredThinking),
    /// Asks the current model's provider for its usage limits, answered by
    /// [`Update::Usage`].
    Usage,
    /// Turns yolo on or off: tools stop (or start) asking for permission.
    Yolo(bool),
    /// Ends the session. The engine sends it itself before opening another.
    Close,
}

pub enum Update {
    Agent(Box<Envelope>),
    /// From a Lua plugin's `maki.gui.emit(topic, data)`.
    Lua {
        topic: String,
        data: Value,
    },
    Question {
        input: Value,
    },
    Btw(BtwEvent),
    Usage(UsageReport),
}

#[derive(Clone)]
pub enum UsageReport {
    Ready(ProviderUsage),
    /// The provider has no quota endpoint.
    Unsupported,
    Failed(String),
}

pub enum BtwEvent {
    Delta(String),
    Done,
    Failed(String),
}
