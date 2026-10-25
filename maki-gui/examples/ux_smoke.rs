//! Offline GUI smoke test: send a prompt, a follow-up, then `finish` or Stop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use color_eyre::Result;
use flume::Sender;
use maki_agent::{
    AgentEvent, DoneReason, Envelope, SubagentInfo, ToolDoneEvent, ToolOutput, ToolStartEvent,
};
use maki_gui::{Boot, Command, Engine, PluginInfo, Session, Update, UsageReport};
use maki_providers::{ContentBlock, Message, Role, TokenUsage};
use maki_storage::id::MakiId;
use maki_storage::sessions::{SessionSummary, StoredThinking};
use serde_json::json;

const MODEL: &str = "offline/ui-smoke";
const TOOL: &str = "smoke-build";
const CHILD: &str = "smoke-research";
const INTRO: &str = "Offline UI smoke test. No model requests or shell commands are executed.\n\nSend any message to start a simulated build and subagent. Send a follow-up while running. Send `finish` to complete, or click Stop. Click the shell count to inspect its panel.";

struct SmokeEngine;

fn emit(tx: &Sender<Update>, event: AgentEvent, subagent: Option<SubagentInfo>) {
    let _ = tx.send(Update::Agent(Box::new(Envelope {
        event,
        subagent,
        run_id: 0,
    })));
}

fn child() -> SubagentInfo {
    SubagentInfo {
        parent_tool_use_id: CHILD.into(),
        name: "Independent research".into(),
        prompt: None,
        model: Some(MODEL.into()),
        opts: None,
        answer_tx: None,
        inbox: None,
    }
}

fn done(reason: DoneReason) -> AgentEvent {
    AgentEvent::Done {
        usage: TokenUsage::default(),
        cost: None,
        list_cost: None,
        context_size: 1024,
        context_window: 128_000,
        num_turns: 2,
        reason,
    }
}

fn status(tx: &Sender<Update>, remove: bool) {
    let _ = tx.send(Update::Lua { topic: "ui.status".into(), data: json!({ "id": "smoke-jobs", "text": "1 shell", "tooltip": "Simulated background build", "topic": "smoke.jobs", "remove": remove }) });
    if remove {
        let _ = tx.send(Update::Lua {
            topic: "ui.panel".into(),
            data: json!({ "id": "smoke-jobs", "remove": true }),
        });
    }
}

fn serve(commands: flume::Receiver<Command>, tx: Sender<Update>) {
    let mut running = false;
    let mut panel = false;
    for command in commands {
        match command {
            Command::Prompt { text, images, .. } => {
                emit(
                    &tx,
                    AgentEvent::QueueItemConsumed {
                        text: text.clone(),
                        images,
                    },
                    None,
                );
                if !running {
                    running = true;
                    emit(
                        &tx,
                        AgentEvent::ToolStart(Box::new(ToolStartEvent {
                            id: TOOL.into(),
                            tool: Arc::from("bash"),
                            summary: "Simulated build".into(),
                            render_header: None,
                            annotation: None,
                            input: None,
                            raw_input: Some(
                                json!({ "command": "simulated build", "background": true }),
                            ),
                            output: None,
                        })),
                        None,
                    );
                    emit(
                        &tx,
                        AgentEvent::ToolOutput {
                            id: TOOL.into(),
                            content: (1..=200)
                                .map(|i| format!("Build output {i}: 日本語 · UTF-8\n"))
                                .collect(),
                        },
                        None,
                    );
                    emit(
                        &tx,
                        AgentEvent::Notice {
                            text: "Reading source files…".into(),
                        },
                        Some(child()),
                    );
                    status(&tx, false);
                } else if text.trim() == "question" {
                    emit(
                        &tx,
                        AgentEvent::ToolStart(Box::new(ToolStartEvent {
                            id: "smoke-todos".into(),
                            tool: Arc::from("todo_write"),
                            summary: "Plan".into(),
                            render_header: None,
                            annotation: None,
                            input: None,
                            output: None,
                            raw_input: Some(
                                json!({"todos":[{"content":"Confirm the inline question","status":"in_progress"}]}),
                            ),
                        })),
                        None,
                    );
                    let _ = tx.send(Update::Question {
                        input: json!({"questions":[{
                            "question":"Choose a UI test option, or type a custom answer.",
                            "options":[{"label":"Option A"},{"label":"Option B"}],
                            "multiSelect":false
                        }]}),
                    });
                } else if text.trim() == "finish" {
                    running = false;
                    emit(
                        &tx,
                        AgentEvent::ToolDone(Box::new(ToolDoneEvent {
                            id: TOOL.into(),
                            tool: Arc::from("bash"),
                            output: Arc::new(ToolOutput::Plain(
                                "Build succeeded\nExit code: 0".into(),
                            )),
                            is_error: false,
                            annotation: None,
                            written_path: None,
                            call: None,
                        })),
                        None,
                    );
                    emit(&tx, done(DoneReason::EndTurn), Some(child()));
                    emit(&tx, AgentEvent::TextDelta { text: "Build and independent research completed. Your follow-ups were received during the task.".into() }, None);
                    emit(&tx, done(DoneReason::EndTurn), None);
                    status(&tx, true);
                } else {
                    emit(
                        &tx,
                        AgentEvent::Notice {
                            text: format!("Follow-up received: {text}"),
                        },
                        None,
                    );
                }
            }
            Command::Cancel => {
                running = false;
                emit(&tx, done(DoneReason::Cancelled), Some(child()));
                emit(&tx, done(DoneReason::Cancelled), None);
                status(&tx, true);
            }
            Command::CancelTask(_) => emit(&tx, done(DoneReason::Cancelled), Some(child())),
            Command::Answer(answer) => emit(
                &tx,
                AgentEvent::Notice {
                    text: format!(
                        "Answer received: {}",
                        answer.as_deref().unwrap_or("skipped")
                    ),
                },
                None,
            ),
            Command::Usage => {
                let _ = tx.send(Update::Usage(UsageReport::Unsupported));
            }
            Command::Lua { topic, .. } if topic == "smoke.jobs" => {
                panel = !panel;
                let _ = tx.send(Update::Lua { topic: "ui.panel".into(), data: json!({ "id": "smoke-jobs", "title": "Background shells", "remove": !panel, "body": [{ "type": "text", "text": "Simulated build · running" }, { "type": "code", "text": "No command is executed. Send finish or click Stop to complete the scenario." }] }) });
            }
            Command::Close => return,
            _ => {}
        }
    }
}

impl Engine for SmokeEngine {
    fn open(&self, _: &Path, _: Option<String>) -> Result<Session> {
        let (commands, rx) = flume::unbounded();
        let (tx, updates) = flume::unbounded();
        thread::spawn(move || serve(rx, tx));
        Ok(Session {
            boot: Boot {
                session_id: MakiId::generate().to_string(),
                model: MODEL.into(),
                models: vec![MODEL.into()],
                context_window: 128_000,
                thinking: StoredThinking::Off,
                tools: vec!["bash".into(), "task".into()],
                warnings: Vec::new(),
                history: vec![Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text: INTRO.into() }],
                    ..Default::default()
                }],
                slash_commands: Vec::new(),
                trusted: true,
                yolo: false,
            },
            commands,
            updates,
        })
    }
    fn reload(&self, _: Option<&Path>) {}
    fn sessions(&self, _: &Path) -> Vec<SessionSummary> {
        Vec::new()
    }
    fn delete_session(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    fn trust(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn plugins(&self) -> Vec<PluginInfo> {
        Vec::new()
    }
    fn addons_dir(&self) -> Option<PathBuf> {
        None
    }
}

fn main() -> Result<()> {
    maki_gui::run(Arc::new(SmokeEngine), None, Some(std::env::current_dir()?));
    Ok(())
}
