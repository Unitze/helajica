//! The seam between the window and the agent. The binary implements
//! [`Engine`] with the same stack the TUI builds, so a session opened here is
//! the same agent, tools, plugins and prompt cache as `maki` in a terminal.

use std::path::Path;

use color_eyre::Result;
use flume::{Receiver, Sender};
use maki_agent::Envelope;
use maki_agent::permissions::PermissionAnswer;
use maki_providers::{ImageSource, Message};
use maki_storage::sessions::SessionSummary;
use serde_json::Value;

pub trait Engine: Send + Sync + 'static {
    /// Builds the project's stack and starts a session in it, closing any
    /// session still open: the agent reads the process cwd, so one project
    /// runs at a time. Blocks while plugins load.
    fn open(&self, project: &Path, resume: Option<String>) -> Result<Session>;

    fn sessions(&self, project: &Path) -> Vec<SessionSummary>;

    /// Records the user's trust in `project`, so its `.maki` config, plugins
    /// and GUI extensions load on the next [`Engine::open`].
    fn trust(&self, project: &Path) -> Result<()>;
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
}

pub enum BtwEvent {
    Delta(String),
    Done,
    Failed(String),
}
