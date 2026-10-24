//! The desktop app (`maki gui`, or the `maki-desktop` binary). The window
//! lives in `maki-gui`; this file is its [`Engine`]: per project, the same
//! stack `cmd::tui` builds, and a session driven in-process.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::{self, JoinHandle};

use clap::Parser;
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use flume::{Receiver, Sender, WeakSender};
use maki_agent::agent::live_history;
use maki_agent::command::CustomCommand;
use maki_agent::headless::{self, InteractiveHandle, InteractiveParams};
use maki_agent::mcp;
use maki_agent::permissions::TaggedAnswer;
use maki_agent::tools::{LocalTool, QUESTION_TOOL_NAME, ToolAudience, ToolRegistry, local_tool};
use maki_agent::{
    AgentEvent, AgentInput, AgentMode, Envelope, InputSource, SessionEndReason, UNAVAILABLE_RESULT,
    close_dangling_tool_calls,
};
use maki_config::project::{self, TrustAnswer, TrustMode};
use maki_config::{ModelPolicy, SessionDefaults, load_env_files};
use maki_gui::{Boot, BtwEvent, Command, Engine, Session, SlashCommand, Update};
use maki_lua::session_snapshot::{HeadlessMeta, HeadlessSnapshot, MODE_BUILD, MODE_PLAN};
use maki_lua::{GuiMessage, Interaction};
use maki_providers::model::Model;
use maki_providers::provider::{available_model_specs, from_model};
use maki_providers::{ProviderEvent, RequestOptions, Timeouts, adapt_images_for_model};
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use maki_storage::sessions::SessionSummary;
use maki_ui::AppSession;
use maki_ui::app::btw::{BTW_FALLBACK_SYSTEM, btw_question};
use serde_json::Value;
use tracing::warn;

use super::tui::{Launch, build_stack};
use crate::cli::Cli;
use crate::{provider_scripts, resume, setup};

const BIN_NAME: &str = "maki";
const GUI_DIR: &str = "gui";
const PLAN_FILE: &str = "plan.md";
const COMMAND_PREFIX: char = '/';
const NO_ANSWER: &str = "The user dismissed the question without answering.";

static LOGGING: Once = Once::new();

pub fn run(cli: &Cli, project: Option<PathBuf>) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::model_registry::load_from_storage(&storage);
    let gui_dir = maki_storage::paths::config_dir()
        .ok()
        .map(|dir| dir.join(GUI_DIR));
    let engine = Arc::new(MakiEngine::new(cli, storage));
    let project = project.map(|p| p.canonicalize().unwrap_or(p));
    maki_gui::run(engine, gui_dir, project);
    Ok(())
}

/// The flags a project open honours. Each open builds a fresh [`Cli`] from
/// these, so `resume` can differ per open without the engine owning a `Cli`.
#[derive(Clone)]
struct Flags {
    model: Option<String>,
    no_plugins: bool,
    no_jit: bool,
    no_commands: bool,
    yolo: bool,
}

struct Running {
    commands: WeakSender<Command>,
    thread: JoinHandle<()>,
}

struct MakiEngine {
    flags: Flags,
    storage: StateDir,
    running: Mutex<Option<Running>>,
    /// Where `maki.gui.emit` lands: the open session's update channel.
    lua_route: Arc<Mutex<Option<Sender<Update>>>>,
}

impl MakiEngine {
    fn new(cli: &Cli, storage: StateDir) -> Self {
        let lua_route: Arc<Mutex<Option<Sender<Update>>>> = Arc::default();
        let (lua_tx, lua_rx) = flume::unbounded::<GuiMessage>();
        maki_lua::attach_gui(lua_tx);
        let route = Arc::clone(&lua_route);
        thread::spawn(move || {
            for GuiMessage { topic, data } in lua_rx {
                if let Some(updates) = route.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    let _ = updates.send(Update::Lua { topic, data });
                }
            }
        });
        Self {
            flags: Flags {
                model: cli.model.clone(),
                no_plugins: cli.no_plugins,
                no_jit: cli.no_jit,
                no_commands: cli.no_commands,
                yolo: cli.yolo,
            },
            storage,
            running: Mutex::new(None),
            lua_route,
        }
    }

    /// The agent reads the process cwd and the tool registry is global, so a
    /// session must be fully gone before the next one starts building.
    fn close_running(&self) {
        let running = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(Running { commands, thread }) = running {
            if let Some(commands) = commands.upgrade() {
                let _ = commands.send(Command::Close);
            }
            if thread.join().is_err() {
                warn!("gui session thread panicked");
            }
        }
    }
}

impl Engine for MakiEngine {
    fn open(&self, project: &Path, resume: Option<String>) -> Result<Session> {
        self.close_running();
        let (ready_tx, ready_rx) = flume::bounded::<Result<Session>>(1);
        let flags = self.flags.clone();
        let storage = self.storage.clone();
        let project = project.to_path_buf();
        let lua_route = Arc::clone(&self.lua_route);
        let thread = thread::Builder::new()
            .name("maki-session".into())
            .spawn(move || {
                let fail = ready_tx.clone();
                if let Err(e) =
                    run_project(&flags, &storage, &project, resume, &lua_route, ready_tx)
                {
                    let _ = fail.send(Err(e));
                }
            })
            .context("spawn session thread")?;
        let session = ready_rx
            .recv()
            .map_err(|_| eyre!("session thread exited before starting"))??;
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running {
            commands: session.commands.downgrade(),
            thread,
        });
        Ok(session)
    }

    fn sessions(&self, project: &Path) -> Vec<SessionSummary> {
        AppSession::list(&project.to_string_lossy(), &self.storage).unwrap_or_default()
    }

    fn trust(&self, project: &Path) -> Result<()> {
        let decision = project::resolve(&self.storage, project, TrustMode::Consult);
        let decision = project::apply_answer(&self.storage, decision, TrustAnswer::Trust);
        match decision.warning {
            Some(warning) => Err(eyre!(warning)),
            None => Ok(()),
        }
    }
}

/// Builds the project's stack, hands the session back through `ready`, then
/// serves it until the window lets go of it.
fn run_project(
    flags: &Flags,
    storage: &StateDir,
    project: &Path,
    resume: Option<String>,
    lua_route: &Mutex<Option<Sender<Update>>>,
    ready: Sender<Result<Session>>,
) -> Result<()> {
    env::set_current_dir(project).with_context(|| format!("enter {}", project.display()))?;
    let cwd = env::current_dir().unwrap_or_else(|_| project.to_path_buf());
    let mut cli = Cli::parse_from([BIN_NAME]);
    cli.model.clone_from(&flags.model);
    cli.no_plugins = flags.no_plugins;
    cli.no_jit = flags.no_jit;
    cli.no_commands = flags.no_commands;
    cli.yolo = flags.yolo;
    cli.resume = resume;

    let trust = project::resolve(storage, &cwd, TrustMode::Consult);
    load_env_files(&trust.project_config);
    let launch = Launch {
        cli: &cli,
        cwd: &cwd,
        storage,
        interaction: Interaction::None,
    };
    let (stack, mut warnings) = build_stack(&launch, &trust, None)?;
    warnings.extend(trust.state.restricted_warning());
    warnings.extend(provider_scripts::startup_warning());
    LOGGING.call_once(|| {
        setup::init_logging(&stack.config.storage);
        setup::init_telemetry(&stack.config.telemetry);
        setup::install_panic_log_hook();
    });

    let cwd_str = cwd.to_string_lossy().into_owned();
    let resolved = resume::resolve(&cli, &cwd_str, storage)?;
    setup::report_session_start(resolved.start_type, Some(&resolved.id));
    let (resumed, claim) = resolved.into_resumed();
    let history = resumed.history.clone();

    let (mcp_handle, mcp_errors) =
        smol::block_on(mcp::start_connected(&cwd, trust.project_config.clone()));
    if !mcp_errors.is_empty() {
        warnings.push(format!("MCP config error: {mcp_errors}"));
    }

    let (updates_tx, updates_rx) = flume::unbounded::<Update>();
    let (commands_tx, commands_rx) = flume::unbounded::<Command>();
    let model_policy = Arc::new(stack.config.provider.model_policy.clone());
    let timeouts = stack.timeouts();
    let lua_handle = stack.plugin_host.event_handle();
    let startup_model = stack.model.clone();
    let (handle, events) = headless::spawn_interactive(InteractiveParams {
        model: stack.model.clone(),
        config: stack.config.agent.clone(),
        permissions_config: stack.config.permissions.clone(),
        timeouts,
        prompt_slots: Arc::new(lua_handle.collect_prompt_slots()),
        excluded_tools: Vec::new(),
        mcp_handle,
        initial_wd: cwd.clone(),
        resumed,
        claim,
        storage: storage.clone(),
        yolo: cli.yolo || stack.config.always_yolo,
        system_prompt_override: None,
        append_system_prompt: None,
        defaults: stack.config.session_defaults,
        model_policy: Arc::clone(&model_policy),
        plugin_rules: stack.plugin_host.plugin_rules(),
        project_config: trust.project_config.clone(),
        local_tools: Arc::new(HashMap::from([(
            QUESTION_TOOL_NAME.to_owned(),
            question_tool(updates_tx.downgrade()),
        )])),
    });

    let plan = Arc::new(AtomicBool::new(false));
    let snapshot = HeadlessSnapshot::default();
    let snapshot_plan = Arc::clone(&plan);
    snapshot.install(
        &lua_handle,
        HeadlessMeta {
            id: handle.session_id.to_string(),
            cwd: cwd_str,
            model: startup_model.spec(),
        },
        move || match snapshot_plan.load(Ordering::Relaxed) {
            true => MODE_PLAN,
            false => MODE_BUILD,
        },
    );
    let session_id = handle.session_id.to_string();
    let pump = smol::spawn({
        let updates = updates_tx.clone();
        let lua_handle = lua_handle.clone();
        let session_id = session_id.clone();
        let mut events = events;
        async move {
            while let Some(envelope) = events.next().await {
                snapshot.observe(&envelope);
                maki_lua::agent_autocmd::dispatch(
                    &lua_handle,
                    &session_id,
                    &envelope,
                    envelope.subagent.is_some(),
                );
                if forwards(&envelope) {
                    let _ = updates.send(Update::Agent(Box::new(envelope)));
                }
            }
        }
    });

    *lua_route.lock().unwrap_or_else(|e| e.into_inner()) = Some(updates_tx.clone());
    drop(updates_tx);
    let boot = Boot {
        session_id: session_id.clone(),
        model: startup_model.spec(),
        models: available_model_specs(&model_policy),
        tools: handle.tool_names.clone(),
        warnings,
        history,
        slash_commands: stack
            .commands
            .iter()
            .map(|c| SlashCommand {
                name: c.name.clone(),
                description: c.description.clone(),
            })
            .collect(),
        trusted: trust.state.restricted_warning().is_none(),
        yolo: handle.permissions.is_yolo(),
    };
    let aside = Aside {
        model: startup_model,
        timeouts,
        session: handle.session_id.clone(),
        updates: lua_route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(Sender::downgrade),
    };
    let sent = ready.send(Ok(Session {
        boot,
        commands: commands_tx,
        updates: updates_rx,
    }));
    if sent.is_ok() {
        serve(
            &commands_rx,
            &handle,
            &stack.commands,
            stack.config.session_defaults,
            &model_policy,
            &cwd,
            &plan,
            aside,
        );
    }

    *lua_route.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let InteractiveHandle {
        input_tx,
        cancel_tx,
        task,
        session_id: session_ref,
        ..
    } = handle;
    let _ = cancel_tx.try_send(());
    drop(input_tx);
    smol::block_on(async {
        pump.await;
        headless::await_shutdown(task).await;
    });
    lua_handle.end_sessions_blocking([session_ref.id()], SessionEndReason::Completed);
    let mut stack = stack;
    stack.plugin_host.begin_shutdown();
    ToolRegistry::global().clear_lua();
    drop(stack);
    Ok(())
}

/// What a `/btw` side question needs: the model the session is on now, and
/// where its answer goes.
struct Aside {
    model: Model,
    timeouts: Timeouts,
    session: SessionRef,
    updates: Option<WeakSender<Update>>,
}

/// Until the window drops its sender or the engine closes the session.
#[allow(clippy::too_many_arguments)]
fn serve(
    commands: &Receiver<Command>,
    handle: &InteractiveHandle,
    slash_commands: &[CustomCommand],
    defaults: SessionDefaults,
    model_policy: &ModelPolicy,
    cwd: &Path,
    plan: &AtomicBool,
    mut aside: Aside,
) {
    for command in commands.iter() {
        match command {
            Command::Prompt {
                text,
                plan: wants_plan,
                images,
            } => {
                plan.store(wants_plan, Ordering::Relaxed);
                let mode = match wants_plan {
                    true => AgentMode::Plan(cwd.join(PLAN_FILE)),
                    false => AgentMode::Build,
                };
                let _ = handle.input_tx.send(AgentInput::from_defaults(
                    expand_command(slash_commands, text),
                    mode,
                    images,
                    defaults,
                    InputSource::Gui,
                ));
            }
            Command::Cancel => {
                let _ = handle.cancel_tx.try_send(());
            }
            Command::Permission { id, answer } => {
                let _ = handle
                    .answer_tx
                    .send(TaggedAnswer::new(&id, answer).encode());
            }
            Command::Answer(text) => {
                let _ = handle
                    .answer_tx
                    .send(text.unwrap_or_else(|| NO_ANSWER.to_owned()));
            }
            Command::Lua { topic, data } => maki_lua::deliver_to_lua(&topic, data),
            Command::Btw(question) => {
                if let Some(updates) = aside.updates.clone() {
                    smol::spawn(answer_aside(
                        question,
                        aside.model.clone(),
                        aside.timeouts,
                        aside.session.clone(),
                        updates,
                    ))
                    .detach();
                }
            }
            Command::Model(spec) => match Model::from_spec_with_policy(&spec, model_policy) {
                Ok(model) => {
                    aside.model = model.clone();
                    let _ = handle.model_tx.send(model);
                }
                Err(e) => warn!(%spec, error = %e, "gui picked an unusable model"),
            },
            Command::Yolo(on) => {
                if handle.permissions.is_yolo() != on {
                    handle.permissions.toggle_yolo();
                }
            }
            Command::Close => return,
        }
    }
}

/// The TUI's `/btw`: the live transcript plus the question, no tools, and
/// nothing written back to the session.
async fn answer_aside(
    question: String,
    mut model: Model,
    timeouts: Timeouts,
    session: SessionRef,
    updates: WeakSender<Update>,
) {
    let send = move |event: BtwEvent| {
        updates
            .upgrade()
            .is_some_and(|u| u.send(Update::Btw(event)).is_ok())
    };
    let provider = match from_model(&mut model, timeouts) {
        Ok(provider) => provider,
        Err(e) => {
            send(BtwEvent::Failed(e.to_string()));
            return;
        }
    };
    let mut messages = live_history(session.id())
        .map(|m| Vec::clone(&m))
        .unwrap_or_default();
    close_dangling_tool_calls(&mut messages, UNAVAILABLE_RESULT);
    messages.push(btw_question(&question));
    let messages = adapt_images_for_model(&model, &messages).await;

    let (events_tx, events_rx) = flume::unbounded();
    let forward = smol::spawn({
        let send = send.clone();
        async move {
            while let Ok(event) = events_rx.recv_async().await {
                if let ProviderEvent::TextDelta { text } = event
                    && !send(BtwEvent::Delta(text))
                {
                    return;
                }
            }
        }
    });
    let result = provider
        .stream_message(
            &model,
            &messages,
            BTW_FALLBACK_SYSTEM,
            &Value::Array(Vec::new()),
            &events_tx,
            RequestOptions::default(),
            Some(&session),
        )
        .await;
    drop(events_tx);
    forward.await;
    send(match result {
        Ok(_) => BtwEvent::Done,
        Err(e) => BtwEvent::Failed(e.to_string()),
    });
}

/// Snapshots are terminal cells baked for the TUI; the window has nothing to
/// draw them with, and they are the bulk of the stream.
fn forwards(envelope: &Envelope) -> bool {
    !matches!(
        envelope.event,
        AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. }
    )
}

/// Weak, so a parked question cannot keep the window's channel open after
/// the session has gone.
fn question_tool(updates: WeakSender<Update>) -> LocalTool {
    local_tool(ToolAudience::MAIN, move |input: Value, ctx| {
        let updates = updates.clone();
        Box::pin(async move {
            let updates = updates.upgrade().ok_or("window closed")?;
            let rx = ctx.user_response_rx.as_ref().ok_or("no answer channel")?;
            let guard = rx.lock().await;
            updates
                .send(Update::Question { input })
                .map_err(|_| "window closed")?;
            Ok(match ctx.cancel.race(guard.recv_async()).await {
                Ok(Ok(text)) => text,
                _ => NO_ANSWER.to_owned(),
            })
        })
    })
}

/// `/name args` runs a custom command the way the TUI would; anything else is
/// sent as typed.
fn expand_command(commands: &[CustomCommand], text: String) -> String {
    let Some(rest) = text.strip_prefix(COMMAND_PREFIX) else {
        return text;
    };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    match commands.iter().find(|c| c.name == name) {
        Some(command) => command.render(args.trim()),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use maki_agent::command::CommandScope;
    use test_case::test_case;

    use super::*;

    fn command(name: &str) -> CustomCommand {
        CustomCommand {
            name: name.into(),
            description: String::new(),
            content: "review $ARGUMENTS".into(),
            scope: CommandScope::User,
            accepts_args: true,
        }
    }

    #[test_case("/review src/main.rs", "review src/main.rs" ; "known_command_expands")]
    #[test_case("/unknown x", "/unknown x" ; "unknown_command_passes_through")]
    #[test_case("plain text", "plain text" ; "plain_text_passes_through")]
    fn custom_commands_expand_like_the_tui(input: &str, expected: &str) {
        assert_eq!(expand_command(&[command("review")], input.into()), expected);
    }
}
