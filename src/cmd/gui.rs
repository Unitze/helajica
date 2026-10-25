//! The desktop app (`maki gui`, or the `maki-desktop` binary). The window
//! lives in `maki-gui`; this file is its [`Engine`]: per project, the same
//! stack `cmd::tui` builds, and a session driven in-process.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once};
use std::thread::{self, JoinHandle};

use clap::Parser;
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use flume::{Receiver, Sender, WeakSender};
use maki_agent::agent::live_history;
use maki_agent::command::CustomCommand;
use maki_agent::headless::{self, InteractiveHandle, InteractiveParams};
use maki_agent::mcp;
use maki_agent::permissions::{PluginRuleStore, TaggedAnswer};
use maki_agent::tools::{LocalTool, QUESTION_TOOL_NAME, ToolAudience, ToolRegistry, local_tool};
use maki_agent::{
    AgentEvent, AgentInput, AgentMode, Envelope, InputSource, SessionEndReason, UNAVAILABLE_RESULT,
    close_dangling_tool_calls,
};
use maki_config::project::{self, TrustAnswer, TrustMode};
use maki_config::{
    AgentConfig, DEFAULT_BUILTINS, ModelPolicy, OPTIONAL_BUILTINS, PROVIDER_BUILTINS,
    PermissionsConfig, PluginOverlay, ProjectConfig, SessionDefaults, load_env_files,
};
use maki_gui::{
    Boot, BtwEvent, Command, Engine, PluginInfo, PluginKind, Session, SlashCommand, Update,
    UsageReport,
};
use maki_lua::session_snapshot::{HeadlessMeta, HeadlessSnapshot, MODE_BUILD, MODE_PLAN};
use maki_lua::{EventHandle, GuiMessage, Interaction};
use maki_providers::model::Model;
use maki_providers::provider::{available_model_specs, from_model};
use maki_providers::{ProviderEvent, RequestOptions, Timeouts, adapt_images_for_model};
use maki_storage::StateDir;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::{SessionClaim, SessionSummary, StoredThinking};
use maki_ui::AppSession;
use maki_ui::app::btw::{BTW_FALLBACK_SYSTEM, btw_question};
use serde_json::Value;
use tracing::warn;

use super::tui::{Launch, Stack, build_stack};
use crate::cli::Cli;
use crate::{provider_scripts, resume, setup};

const BIN_NAME: &str = "maki";
const GUI_DIR: &str = "gui";
const PLAN_FILE: &str = "plan.md";
const COMMAND_PREFIX: char = '/';
/// A package group of its own, so the user's add-ons sit apart from anything
/// else installed by hand. Mirrors `ADDONS_DIR` in the skill plugin.
const ADDONS_DIR: &str = "pack/addons/start";
const NO_ANSWER: &str = "The user dismissed the question without answering.";

static LOGGING: Once = Once::new();

pub fn run(cli: &Cli, project: Option<PathBuf>) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::model_registry::load_from_storage(&storage);
    let gui_dir = maki_storage::paths::config_dir()
        .ok()
        .map(|dir| dir.join(GUI_DIR));
    let engine = Arc::new(MakiEngine::new(cli, storage, gui_dir.clone()));
    let project = project.map(|p| p.canonicalize().unwrap_or(p));
    maki_gui::run(engine, gui_dir, project);
    Ok(())
}

/// The flags a project open honours. Each open builds a fresh [`Cli`] from
/// these, so `resume` can differ per open without the engine owning a `Cli`.
#[derive(Clone)]
struct Flags {
    model: Option<String>,
    thinking: Option<StoredThinking>,
    max_turns: Option<u32>,
    no_plugins: bool,
    no_jit: bool,
    no_commands: bool,
    yolo: bool,
}

impl Flags {
    fn cli(&self, resume: Option<String>) -> Cli {
        let mut cli = Cli::parse_from([BIN_NAME]);
        cli.model.clone_from(&self.model);
        cli.thinking = self.thinking;
        cli.max_turns = self.max_turns;
        cli.no_plugins = self.no_plugins;
        cli.no_jit = self.no_jit;
        cli.no_commands = self.no_commands;
        cli.yolo = self.yolo;
        cli.resume = resume;
        cli
    }
}

struct Running {
    commands: WeakSender<Command>,
    thread: JoinHandle<()>,
}

type RunningSessions = Arc<Mutex<HashMap<PathBuf, Vec<(String, Running)>>>>;

type Hosts = Mutex<HashMap<PathBuf, Host>>;

/// A project whose stack is built, and the thread that owns it.
struct Host {
    /// The plugin settings it was built with: a change rebuilds it.
    overlay: PluginOverlay,
    requests: Sender<OpenRequest>,
    thread: JoinHandle<()>,
}

struct OpenRequest {
    resume: Option<String>,
    ready: Sender<Result<Session>>,
}

struct MakiEngine {
    flags: Flags,
    storage: StateDir,
    /// Where the window keeps its settings, the plugin switches among them.
    gui_dir: Option<PathBuf>,
    /// One stack per open project: a project's sessions share its plugins,
    /// its tools and its prompt, and none of them reads another's.
    hosts: Hosts,
    sessions: RunningSessions,
    /// Where `maki.gui.emit` lands: the focused session's update channel.
    lua_route: LuaRoute,
}

#[derive(Default)]
struct LuaRoutes {
    focused: Option<Sender<Update>>,
    sessions: HashMap<MakiId, WeakSender<Update>>,
}

type LuaRoute = Arc<Mutex<LuaRoutes>>;

impl MakiEngine {
    fn new(cli: &Cli, storage: StateDir, gui_dir: Option<PathBuf>) -> Self {
        let lua_route: LuaRoute = Arc::default();
        let (lua_tx, lua_rx) = flume::unbounded::<GuiMessage>();
        maki_lua::attach_gui(lua_tx);
        let route = Arc::clone(&lua_route);
        thread::spawn(move || {
            for message in lua_rx {
                let routes = lock(&route);
                match message {
                    GuiMessage::Emit { topic, data } => {
                        let scoped = data
                            .get("session")
                            .and_then(Value::as_str)
                            .and_then(|id| id.parse::<MakiId>().ok());
                        let updates = match scoped {
                            Some(session) => {
                                routes.sessions.get(&session).and_then(WeakSender::upgrade)
                            }
                            None => routes.focused.clone(),
                        };
                        if let Some(updates) = updates {
                            let _ = updates.send(Update::Lua { topic, data });
                        }
                    }
                    GuiMessage::Task { session, envelope } => {
                        if let Some(updates) =
                            routes.sessions.get(&session).and_then(WeakSender::upgrade)
                        {
                            let _ = updates.send(Update::Agent(envelope));
                        }
                    }
                }
            }
        });
        Self {
            flags: Flags {
                model: cli.model.clone(),
                thinking: cli.thinking,
                max_turns: cli.max_turns,
                no_plugins: cli.no_plugins,
                no_jit: cli.no_jit,
                no_commands: cli.no_commands,
                yolo: cli.yolo,
            },
            storage,
            gui_dir,
            hosts: Hosts::default(),
            sessions: RunningSessions::default(),
            lua_route,
        }
    }

    fn close_session(&self, project: &Path, id: &str) {
        let running = lock(&self.sessions).get_mut(project).and_then(|running| {
            let at = running.iter().position(|(session, _)| session == id)?;
            Some(running.remove(at).1)
        });
        if let Some(running) = running {
            stop(running);
        }
    }

    /// Ends every session of `project`, then its thread: the stack's plugins
    /// and its tool registry belong to that project and to nothing else.
    fn close_project(&self, project: &Path) {
        let running: Vec<Running> = lock(&self.sessions)
            .remove(project)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, running)| running)
            .collect();
        running.into_iter().for_each(stop);
        if let Some(Host {
            requests, thread, ..
        }) = lock(&self.hosts).remove(project)
        {
            drop(requests);
            if thread.join().is_err() {
                warn!("gui project thread panicked");
            }
        }
    }

    fn start_host(&self, project: &Path, overlay: PluginOverlay) -> Result<Host> {
        let (built_tx, built_rx) = flume::bounded::<Result<()>>(1);
        let (requests, requests_rx) = flume::unbounded::<OpenRequest>();
        let flags = self.flags.clone();
        let storage = self.storage.clone();
        let lua_route = Arc::clone(&self.lua_route);
        let sessions = Arc::clone(&self.sessions);
        let thread = thread::Builder::new()
            .name(format!("maki-project-{}", project.display()))
            .spawn({
                let project = project.to_path_buf();
                let overlay = overlay.clone();
                move || {
                    host_project(
                        flags,
                        storage,
                        &project,
                        &overlay,
                        lua_route,
                        &sessions,
                        &built_tx,
                        &requests_rx,
                    )
                }
            })
            .context("spawn project thread")?;
        let built = built_rx
            .recv()
            .map_err(|_| eyre!("project thread exited before building"));
        if let Err(e) = built.and_then(|built| built) {
            let _ = thread.join();
            return Err(e);
        }
        Ok(Host {
            overlay,
            requests,
            thread,
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn stop(Running { commands, thread }: Running) {
    if let Some(commands) = commands.upgrade() {
        let _ = commands.send(Command::Close);
    }
    if thread.join().is_err() {
        warn!("gui session thread panicked");
    }
}

impl Engine for MakiEngine {
    fn open(&self, project: &Path, resume: Option<String>) -> Result<Session> {
        if let Some(id) = &resume {
            self.close_session(project, id);
        }
        let overlay = maki_gui::plugin_overlay(self.gui_dir.as_deref());
        // Held over the open so two projects cannot both swap the process
        // directory under each other while a stack builds.
        let mut hosts = lock(&self.hosts);
        if hosts
            .get(project)
            .is_some_and(|host| host.overlay != overlay)
        {
            drop(hosts);
            self.close_project(project);
            hosts = lock(&self.hosts);
        }
        if !hosts.contains_key(project) {
            let host = self.start_host(project, overlay)?;
            hosts.insert(project.to_path_buf(), host);
        }
        let (ready, ready_rx) = flume::bounded::<Result<Session>>(1);
        let requests = &hosts.get(project).expect("host started above").requests;
        requests
            .send(OpenRequest { resume, ready })
            .map_err(|_| eyre!("project thread is gone"))?;
        drop(hosts);
        ready_rx
            .recv()
            .map_err(|_| eyre!("session thread exited before starting"))?
    }

    fn reload(&self, project: Option<&Path>) {
        match project {
            Some(project) => self.close_project(project),
            // The stack of a project is built in its own folder, so one at a
            // time and only after its sessions went.
            None => {
                let projects: Vec<PathBuf> = lock(&self.hosts).keys().cloned().collect();
                projects
                    .iter()
                    .for_each(|project| self.close_project(project));
            }
        }
    }

    fn sessions(&self, project: &Path) -> Vec<SessionSummary> {
        AppSession::list(&project.to_string_lossy(), &self.storage).unwrap_or_default()
    }

    /// By `project`, not just by id: the directory it was started in is what
    /// decides which saved log it is, and an id alone could name another
    /// project's session.
    fn delete_session(&self, project: &Path, id: &str) -> Result<()> {
        let parsed: MakiId = id
            .parse()
            .map_err(|e| eyre!("invalid session id {id:?}: {e}"))?;
        self.close_session(project, id);
        // Claiming refuses a session another maki is writing, rather than
        // unlinking the file it keeps appending to.
        let claim = SessionClaim::acquire(parsed, &self.storage).context("claim session")?;
        AppSession::delete(&claim, &self.storage).context("delete session")
    }

    fn trust(&self, project: &Path) -> Result<()> {
        let decision = project::resolve(&self.storage, project, TrustMode::Consult);
        let decision = project::apply_answer(&self.storage, decision, TrustAnswer::Trust);
        match decision.warning {
            Some(warning) => Err(eyre!(warning)),
            None => Ok(()),
        }
    }

    fn plugins(&self) -> Vec<PluginInfo> {
        let bundled = DEFAULT_BUILTINS.iter().map(|name| PluginInfo {
            name: (*name).to_owned(),
            kind: match PROVIDER_BUILTINS.contains(name) {
                true => PluginKind::Provider,
                false => PluginKind::Tool,
            },
        });
        let optional = OPTIONAL_BUILTINS.iter().map(|name| PluginInfo {
            name: (*name).to_owned(),
            kind: PluginKind::Optional,
        });
        let packages = maki_lua::discover_installed(self.flags.no_plugins)
            .packages
            .into_iter()
            .map(|package| PluginInfo {
                name: package.name,
                kind: PluginKind::Package,
            });
        bundled.chain(optional).chain(packages).collect()
    }

    fn addons_dir(&self) -> Option<PathBuf> {
        maki_lua::site_dir().ok().map(|site| site.join(ADDONS_DIR))
    }
}

/// What every session of a project shares, taken from its stack.
#[derive(Clone)]
struct ProjectBase {
    flags: Flags,
    storage: StateDir,
    cwd: PathBuf,
    model: Model,
    agent: AgentConfig,
    permissions: PermissionsConfig,
    timeouts: Timeouts,
    defaults: SessionDefaults,
    always_yolo: bool,
    model_policy: Arc<ModelPolicy>,
    plugin_rules: Arc<PluginRuleStore>,
    project_config: ProjectConfig,
    commands: Arc<[CustomCommand]>,
    lua_handle: EventHandle,
    trusted: bool,
    lua_route: LuaRoute,
}

/// Builds the project's stack, then starts a session on it for every
/// request, until the engine lets go of the project.
#[allow(clippy::too_many_arguments)]
fn host_project(
    flags: Flags,
    storage: StateDir,
    project: &Path,
    overlay: &PluginOverlay,
    lua_route: LuaRoute,
    sessions: &RunningSessions,
    built: &Sender<Result<()>>,
    requests: &Receiver<OpenRequest>,
) {
    let (stack, warnings, base) = match build_project(flags, storage, project, overlay, lua_route) {
        Ok(built) => built,
        Err(e) => {
            let _ = built.send(Err(e));
            return;
        }
    };
    let _ = built.send(Ok(()));
    // The stack's warnings are said once, in the first session.
    let mut warnings = Some(warnings);
    for OpenRequest { resume, ready } in requests.iter() {
        let base = base.clone();
        let warnings = warnings.take().unwrap_or_default();
        let (started_tx, started_rx) = flume::bounded::<Result<Session>>(1);
        let spawned = thread::Builder::new()
            .name("maki-session".into())
            .spawn(move || {
                let fail = started_tx.clone();
                if let Err(e) = serve_session(&base, resume, warnings, started_tx) {
                    let _ = fail.send(Err(e));
                }
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                let _ = ready.send(Err(eyre!("spawn session thread: {e}")));
                continue;
            }
        };
        let started = started_rx
            .recv()
            .unwrap_or_else(|_| Err(eyre!("session thread exited before starting")));
        if let Ok(session) = &started {
            let mut sessions = lock(sessions);
            let running = sessions.entry(project.to_path_buf()).or_default();
            running.retain(|(_, r)| !r.thread.is_finished());
            let id = session.boot.session_id.clone();
            running.push((
                id,
                Running {
                    commands: session.commands.downgrade(),
                    thread,
                },
            ));
        }
        let _ = ready.send(started);
    }
    let mut stack = stack;
    stack.plugin_host.begin_shutdown();
    ToolRegistry::global().clear_lua();
}

fn build_project(
    flags: Flags,
    storage: StateDir,
    project: &Path,
    overlay: &PluginOverlay,
    lua_route: LuaRoute,
) -> Result<(Stack, Vec<String>, ProjectBase)> {
    env::set_current_dir(project).with_context(|| format!("enter {}", project.display()))?;
    let cwd = env::current_dir().unwrap_or_else(|_| project.to_path_buf());
    let cli = flags.cli(None);
    let trust = project::resolve(&storage, &cwd, TrustMode::Consult);
    load_env_files(&trust.project_config);
    let launch = Launch {
        cli: &cli,
        cwd: &cwd,
        storage: &storage,
        interaction: Interaction::None,
        plugin_overlay: overlay,
    };
    let (stack, mut warnings) = build_stack(&launch, &trust, None)?;
    warnings.extend(trust.state.restricted_warning());
    warnings.extend(provider_scripts::startup_warning());
    LOGGING.call_once(|| {
        setup::init_logging(&stack.config.storage);
        setup::init_telemetry(&stack.config.telemetry);
        setup::install_panic_log_hook();
    });
    let base = ProjectBase {
        flags,
        storage,
        cwd,
        model: stack.model.clone(),
        agent: stack.config.agent.clone(),
        permissions: stack.config.permissions.clone(),
        timeouts: stack.timeouts(),
        defaults: stack.config.session_defaults,
        always_yolo: stack.config.always_yolo,
        model_policy: Arc::new(stack.config.provider.model_policy.clone()),
        plugin_rules: stack.plugin_host.plugin_rules(),
        project_config: trust.project_config.clone(),
        commands: stack.commands.clone().into(),
        lua_handle: stack.plugin_host.event_handle(),
        trusted: trust.state.restricted_warning().is_none(),
        lua_route,
    };
    Ok((stack, warnings, base))
}

/// Starts one session on the project's stack, hands it back through
/// `ready`, then serves it until the window lets go of it.
fn serve_session(
    base: &ProjectBase,
    resume: Option<String>,
    warnings: Vec<String>,
    ready: Sender<Result<Session>>,
) -> Result<()> {
    let cli = base.flags.cli(resume);
    let cwd = &base.cwd;
    let cwd_str = cwd.to_string_lossy().into_owned();
    let resolved = resume::resolve(&cli, &cwd_str, &base.storage)?;
    setup::report_session_start(resolved.start_type, Some(&resolved.id));
    let (resumed, claim) = resolved.into_resumed();
    let history = resumed.history.clone();
    // Claims the id, so the session is saved under it and the sidebar lists it
    // the moment it is started rather than once its first turn lands.
    let session_id = resumed.id.clone();

    let (mcp_handle, mcp_errors) =
        smol::block_on(mcp::start_connected(cwd, base.project_config.clone()));
    let mut warnings = warnings;
    if !mcp_errors.is_empty() {
        warnings.push(format!("MCP config error: {mcp_errors}"));
    }

    let (updates_tx, updates_rx) = flume::unbounded::<Update>();
    let (commands_tx, commands_rx) = flume::unbounded::<Command>();
    let lua_handle = &base.lua_handle;
    // The session belongs to the project it was started in, even though the
    // process itself sits wherever the last stack was built.
    let (handle, events) = headless::spawn_interactive(InteractiveParams {
        model: base.model.clone(),
        config: base.agent.clone(),
        permissions_config: base.permissions.clone(),
        timeouts: base.timeouts,
        prompt_slots: Arc::new(lua_handle.collect_prompt_slots()),
        excluded_tools: Vec::new(),
        mcp_handle,
        initial_wd: cwd.clone(),
        working_dir: Some(cwd.clone()),
        persist_on_start: true,
        resumed,
        claim,
        storage: base.storage.clone(),
        yolo: cli.yolo || base.always_yolo,
        system_prompt_override: None,
        append_system_prompt: None,
        defaults: base.defaults,
        model_policy: Arc::clone(&base.model_policy),
        plugin_rules: Arc::clone(&base.plugin_rules),
        project_config: base.project_config.clone(),
        local_tools: Arc::new(HashMap::from([(
            QUESTION_TOOL_NAME.to_owned(),
            question_tool(updates_tx.downgrade()),
        )])),
    });

    let plan = Arc::new(AtomicBool::new(false));
    let snapshot = HeadlessSnapshot::default();
    let session_id = session_id.to_string();
    let focus = {
        let snapshot = snapshot.clone();
        let plan = Arc::clone(&plan);
        let meta = HeadlessMeta {
            id: session_id.clone(),
            cwd: cwd_str,
            model: base.model.spec(),
        };
        let updates = updates_tx.clone();
        let route = Arc::clone(&base.lua_route);
        let session_key = handle.session_id.id();
        let lua_handle = lua_handle.clone();
        move || {
            let plan = Arc::clone(&plan);
            snapshot.install(&lua_handle, meta.clone(), move || {
                match plan.load(Ordering::Relaxed) {
                    true => MODE_PLAN,
                    false => MODE_BUILD,
                }
            });
            let mut routes = lock(&route);
            routes.sessions.insert(session_key, updates.downgrade());
            routes.focused = Some(updates.clone());
        }
    };
    focus();
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

    let boot = Boot {
        session_id,
        model: base.model.spec(),
        models: available_model_specs(&base.model_policy),
        context_window: base.model.context_window,
        thinking: base.defaults.thinking.unwrap_or(StoredThinking::Off),
        tools: handle.tool_names.clone(),
        warnings,
        history,
        slash_commands: base
            .commands
            .iter()
            .map(|c| SlashCommand {
                name: c.name.clone(),
                description: c.description.clone(),
            })
            .collect(),
        trusted: base.trusted,
        yolo: handle.permissions.is_yolo(),
    };
    let aside = Aside {
        model: base.model.clone(),
        timeouts: base.timeouts,
        session: handle.session_id.clone(),
        root: Arc::new(cwd.clone()),
        updates: Some(updates_tx.downgrade()),
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
            &base.lua_handle,
            &base.commands,
            base.defaults,
            &base.model_policy,
            cwd,
            &plan,
            aside,
            focus,
        );
    }

    {
        let mut route = lock(&base.lua_route);
        route.sessions.remove(&handle.session_id.id());
        if route
            .focused
            .as_ref()
            .is_some_and(|r| r.same_channel(&updates_tx))
        {
            route.focused = None;
        }
    }
    drop(updates_tx);
    let InteractiveHandle {
        interrupt,
        cancel_tx,
        task,
        session_id: session_ref,
        ..
    } = handle;
    let _ = cancel_tx.try_send(());
    drop(cancel_tx);
    // Dropping the last way in is what tells the session loop nobody will
    // prompt it again.
    drop(interrupt);
    smol::block_on(async {
        pump.await;
        headless::await_shutdown(task).await;
    });
    lua_handle.end_sessions_blocking([session_ref.id()], SessionEndReason::Completed);
    Ok(())
}

/// What a `/btw` side question needs: the model the session is on now, and
/// where its answer goes.
struct Aside {
    model: Model,
    timeouts: Timeouts,
    session: SessionRef,
    /// The session's project, what its paths resolve against.
    root: Arc<PathBuf>,
    updates: Option<WeakSender<Update>>,
}

/// Until the window drops its sender or the engine closes the session.
#[allow(clippy::too_many_arguments)]
fn serve(
    commands: &Receiver<Command>,
    handle: &InteractiveHandle,
    lua: &EventHandle,
    slash_commands: &[CustomCommand],
    mut defaults: SessionDefaults,
    model_policy: &ModelPolicy,
    cwd: &Path,
    plan: &AtomicBool,
    mut aside: Aside,
    focus: impl Fn(),
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
                // While a turn runs, the agent takes this at its next request
                // and carries on with it, so the user can redirect the work
                // without waiting for the turn, or for a whole new run.
                handle.interrupt.push(AgentInput::from_defaults(
                    expand_command(slash_commands, text),
                    mode,
                    images,
                    defaults,
                    InputSource::Gui,
                ));
                handle.interrupt.wake();
            }
            Command::Cancel => {
                handle.subagent_cancels.cancel_all();
                let _ = handle.cancel_tx.try_send(());
            }
            Command::CancelTask(id) => {
                handle.subagent_cancels.cancel(id.clone());
                lua.cancel_plugin_session(&id);
                lua.fire_autocmd(
                    "TaskStopped",
                    serde_json::json!({ "session_id": handle.session_id.id(), "id": id }),
                );
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
                        Arc::clone(&aside.root),
                        updates,
                    ))
                    .detach();
                }
            }
            Command::Thinking(thinking) => defaults.thinking = Some(thinking),
            Command::Usage => {
                if let Some(updates) = aside.updates.clone() {
                    smol::spawn(report_usage(aside.model.clone(), aside.timeouts, updates))
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
            Command::Focus => focus(),
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
    root: Arc<PathBuf>,
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
    messages.push(btw_question(&format!(
        "The session's working directory is {}.\n\n{question}",
        root.display()
    )));
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

async fn report_usage(mut model: Model, timeouts: Timeouts, updates: WeakSender<Update>) {
    let report = match from_model(&mut model, timeouts) {
        Err(e) => UsageReport::Failed(e.to_string()),
        Ok(provider) => match provider.fetch_usage().await {
            Ok(Some(usage)) => UsageReport::Ready(usage),
            Ok(None) => UsageReport::Unsupported,
            Err(e) => UsageReport::Failed(e.user_message()),
        },
    };
    if let Some(updates) = updates.upgrade() {
        let _ = updates.send(Update::Usage(report));
    }
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
