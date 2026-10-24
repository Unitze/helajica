//! The one window: projects and their sessions down the side, the
//! conversation in the middle, the composer under it, and whatever Lua
//! plugins add around them.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use flume::Sender;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::progress::{Progress, ProgressCircle};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Anchor, Animation, AnimationExt as _, AnyWindowHandle, App, AppContext as _,
    AsyncWindowContext, ClipboardEntry, Context, DispatchPhase, Entity, ExternalPaths, FocusHandle,
    Focusable, FollowMode, HitboxBehavior, Image, InteractiveElement as _, IntoElement, KeyBinding,
    KeyDownEvent, ListAlignment, ListOffset, ListState, ObjectFit, ParentElement as _,
    PathPromptOptions, Render, ScrollWheelEvent, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Subscription, Task, WeakEntity, Window, actions, canvas, div,
    ease_out_quint, img, list, prelude::FluentBuilder as _, px,
};
use maki_agent::AgentEvent;
use maki_agent::permissions::PermissionAnswer;
use maki_storage::now_epoch;
use maki_storage::sessions::{Effort, SessionSummary, StoredThinking};
use serde_json::Value;

use crate::app::{apply_theme_message, toggle_dark};
use crate::attachments::{self, Attachment};
use crate::commands::{self, Builtin, Suggestion};
use crate::engine::{Boot, BtwEvent, Command, Engine, Update, UsageReport};
use crate::ext::{COMMAND_TOPIC_PREFIX, Effect, Extensions, NotifyLevel};
use crate::goal::{self, Goal, Next};
use crate::model_picker::{ModelPicker, OnPick};
use crate::new_project::{self, OnProject};
use crate::nodes;
use crate::projects::ProjectStore;
use crate::settings::Settings;
use crate::settings_window;
use crate::titlebar::{Handler, title_bar};
use crate::transcript::{Change, Item, Level, ToolCall, ToolStatus, Transcript};
use crate::update::{self, Release};

actions!(
    maki_gui,
    [OpenPalette, NewSession, OpenFolder, OpenSettings]
);

const KEY_CONTEXT: &str = "Workspace";
const SIDEBAR_WIDTH: f32 = 264.;
const TRANSCRIPT_WIDTH: f32 = 820.;
const TOOL_PREVIEW_LINES: usize = 12;
const THINKING_PREVIEW_LINES: usize = 3;
const LIST_OVERDRAW: f32 = 800.;
const SCROLL_LINE_HEIGHT: f32 = 40.;
/// Share of the outstanding wheel distance covered each frame.
const SCROLL_EASING: f32 = 0.35;
const SCROLL_SETTLED: f32 = 0.5;
const TURN_TICK_WIDTH: f32 = 10.;
const TURN_TICK_ACTIVE_WIDTH: f32 = 18.;
const TURN_TICK_HEIGHT: f32 = 1.5;
const TURN_TICK_GAP: f32 = 6.;
const TURN_CARD_WIDTH: f32 = 320.;
const TURN_PREVIEW_LINES: usize = 3;
const FADE_IN: Duration = Duration::from_millis(180);
const RISE_IN: f32 = 6.;
const SESSIONS_PER_PROJECT: usize = 30;
const USAGE_RING_SIZE: f32 = 14.;
const USAGE_CARD_WIDTH: f32 = 340.;
const USAGE_WARNING_PERCENT: u32 = 70;
const USAGE_DANGER_PERCENT: u32 = 90;
const CONTEXT_TICKS: u32 = 20;
const CONTEXT_TICK_WIDTH: f32 = 1.5;
const CONTEXT_TICK_HEIGHT: f32 = 9.;
const CONTEXT_TICK_GAP: f32 = 2.;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3_600;
const SECONDS_PER_DAY: u64 = 86_400;
const MILLIS_PER_SECOND: u64 = 1_000;
const MENU_WIDTH: f32 = 240.;
const MENU_HEIGHT: f32 = 360.;
const UNTITLED: &str = "Untitled session";
const COMPOSER_PLACEHOLDER: &str = "Enter to send, Shift+Enter for a newline, / for commands";
const THUMB_SIZE: f32 = 56.;
const MESSAGE_THUMB_SIZE: f32 = 120.;
const SUGGESTION_KEYS: [&str; 5] = ["up", "down", "tab", "enter", "escape"];
const PASTE_KEY: &str = "v";
const ATTACH_PROMPT: &str = "Attach";
const GOAL_STOP_WORDS: [&str; 3] = ["clear", "stop", "off"];
const ASIDE_HEIGHT: f32 = 240.;
const SUGGESTIONS_HEIGHT: f32 = 320.;
const DELETE_DIALOG_WIDTH: f32 = 420.;
const MODEL_QUERY: &str = "Model: ";
const CANCELLED_REASON: &str = "cancelled";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-k", OpenPalette, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-n", NewSession, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-o", OpenFolder, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-,", OpenSettings, Some(KEY_CONTEXT)),
    ]);
}

enum Phase {
    /// No project open yet.
    Idle,
    Loading,
    Ready {
        boot: Boot,
        commands: Sender<Command>,
    },
    Failed(SharedString),
}

pub struct WorkspaceView {
    engine: Arc<dyn Engine>,
    gui_dir: Option<PathBuf>,
    store: ProjectStore,
    project: Option<PathBuf>,
    /// Projects unfolded in the sidebar, and the sessions listed under them.
    unfolded: HashSet<PathBuf>,
    sessions: HashMap<PathBuf, Vec<SessionSummary>>,
    phase: Phase,
    transcript: Transcript,
    ext: Extensions,
    composer: Entity<TextareaState>,
    attachments: Vec<Attachment>,
    /// Images of the user messages in the transcript, by item index.
    thumbs: HashMap<usize, Vec<Arc<Image>>>,
    /// The highlighted suggestion while a `/command` is being typed.
    suggestion: usize,
    suggestions_dismissed: bool,
    aside: Option<Aside>,
    goal: Option<Goal>,
    settings_window: Option<AnyWindowHandle>,
    model_picker: Option<Entity<ModelPicker>>,
    /// The thinking level picked here, over the config's.
    thinking: Option<StoredThinking>,
    /// The provider's usage limits; `None` while they load.
    usage: Option<UsageReport>,
    /// Providers changed mid-run; the session reopens once the run ends.
    reload_pending: bool,
    mode: Mode,
    update: UpdateState,
    expanded: HashSet<String>,
    list: ListState,
    /// Items from here on arrived live and fade in; the loaded history does not.
    animate_from: usize,
    /// Wheel distance not scrolled yet; eased out over the next frames.
    scroll_pending: Rc<Cell<f32>>,
    focus: FocusHandle,
    _session: Option<Task<()>>,
    _subscription: Subscription,
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WorkspaceView {
    /// Opens `project`, or else the most recent project that still exists.
    pub fn new(
        engine: Arc<dyn Engine>,
        gui_dir: Option<PathBuf>,
        project: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 10)
                .submit_on_enter(true)
                .placeholder(COMPOSER_PLACEHOLDER)
        });
        let subscription =
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                    InputEvent::Change => {
                        this.suggestion = 0;
                        this.suggestions_dismissed = false;
                        cx.notify();
                    }
                    _ => {}
                },
            );
        let store = ProjectStore::load(gui_dir.as_deref());
        let thinking = Settings::load(gui_dir.as_deref())
            .thinking
            .and_then(|t| StoredThinking::parse_setting(&t).ok());
        let initial = project.or_else(|| {
            store
                .projects()
                .iter()
                .map(|p| p.path.clone())
                .find(|p| p.is_dir())
        });
        let mut this = Self {
            engine,
            gui_dir,
            store,
            project: None,
            unfolded: HashSet::new(),
            sessions: HashMap::new(),
            phase: Phase::Idle,
            transcript: Transcript::default(),
            ext: Extensions::default(),
            composer,
            attachments: Vec::new(),
            thumbs: HashMap::new(),
            suggestion: 0,
            suggestions_dismissed: false,
            aside: None,
            goal: None,
            settings_window: None,
            model_picker: None,
            thinking,
            usage: None,
            reload_pending: false,
            mode: Mode::Ask,
            update: UpdateState::Idle,
            expanded: HashSet::new(),
            list: ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW)),
            scroll_pending: Rc::default(),
            animate_from: 0,
            focus: cx.focus_handle(),
            _session: None,
            _subscription: subscription,
        };
        if let Some(project) = initial {
            this.open_project(project, None, window, cx);
        }
        this.check_for_update(cx);
        this
    }

    fn check_for_update(&mut self, cx: &mut Context<Self>) {
        if !update::enabled() {
            return;
        }
        update::clean_up();
        cx.spawn(async move |this, cx| {
            let checked = cx.background_spawn(async { update::check() }).await;
            let _ = this.update(cx, |this, cx| match checked {
                Ok(Some(release)) => {
                    this.update = UpdateState::Available(release);
                    cx.notify();
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "update check"),
            });
        })
        .detach();
    }

    /// Downloads the release over this executable and starts it.
    fn install_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let UpdateState::Available(release) = &self.update else {
            return;
        };
        let release = release.clone();
        self.update = UpdateState::Installing;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let installed = cx
                .background_spawn({
                    let release = release.clone();
                    async move { update::install(&release).and_then(|exe| update::relaunch(&exe)) }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match installed {
                Ok(()) => cx.quit(),
                Err(e) => {
                    this.update = UpdateState::Available(release);
                    window.push_notification(
                        Notification::new()
                            .message(format!("Update failed: {e:#}"))
                            .with_type(NotificationType::Error),
                        cx,
                    );
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn render_update(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let button = match &self.update {
            UpdateState::Idle => return None,
            UpdateState::Available(release) => Button::new("update")
                .primary()
                .small()
                .icon(Icon::new(Lucide::Download))
                .label(format!("Update to v{}", release.version))
                .tooltip(format!("You have v{}", update::CURRENT))
                .on_click(cx.listener(|this, _, window, cx| this.install_update(window, cx))),
            UpdateState::Installing => Button::new("update")
                .outline()
                .small()
                .loading(true)
                .label("Updating…"),
        };
        Some(button.into_any_element())
    }

    fn project_name(&self) -> Option<String> {
        self.project.as_deref().map(name_of)
    }

    fn commands(&self) -> Option<&Sender<Command>> {
        match &self.phase {
            Phase::Ready { commands, .. } => Some(commands),
            _ => None,
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        if (mode == Mode::Bypass) != (self.mode == Mode::Bypass) {
            self.send(Command::Yolo(mode == Mode::Bypass));
        }
        self.mode = mode;
    }

    fn send(&self, command: Command) {
        if let Some(commands) = self.commands() {
            let _ = commands.send(command);
        }
    }

    fn current_session(&self) -> Option<&str> {
        match &self.phase {
            Phase::Ready { boot, .. } => Some(&boot.session_id),
            _ => None,
        }
    }

    /// Switches to `project` and starts a session there, fresh or `resume`.
    fn open_project(
        &mut self,
        project: PathBuf,
        resume: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !project.is_dir() {
            self.store.remove(&project);
            let message = format!("{} no longer exists", project.display());
            window.push_notification(
                Notification::new()
                    .message(message)
                    .with_type(NotificationType::Error),
                cx,
            );
            return cx.notify();
        }
        self.store.touch(&project, now_epoch());
        self.unfolded.insert(project.clone());
        self.refresh_sessions(&project);
        self.project = Some(project);
        self.start(resume, window, cx);
    }

    fn refresh_sessions(&mut self, project: &Path) {
        let mut sessions = self.engine.sessions(project);
        // Newest first by when they began (ids are UUIDv7), so using a session
        // does not move it.
        sessions.sort_by(|a, b| b.id.as_bytes().cmp(a.id.as_bytes()));
        sessions.truncate(SESSIONS_PER_PROJECT);
        self.sessions.insert(project.to_path_buf(), sessions);
    }

    fn toggle_project(&mut self, project: &Path, cx: &mut Context<Self>) {
        if !self.unfolded.remove(project) {
            self.unfolded.insert(project.to_path_buf());
            if !self.sessions.contains_key(project) {
                self.refresh_sessions(project);
            }
        }
        cx.notify();
    }

    fn confirm_delete_session(
        &mut self,
        project: PathBuf,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let workspace = workspace.clone();
            let project = project.clone();
            let id = id.clone();
            dialog
                .title("Delete session?")
                .w(px(DELETE_DIALOG_WIDTH))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "\u{201c}{title}\u{201d} and its history will be gone for good."
                        )),
                )
                .footer(
                    DialogFooter::new().children([
                        Button::new("cancel-delete")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                        Button::new("confirm-delete")
                            .danger()
                            .label("Delete")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                let _ = workspace.update(cx, |this, cx| {
                                    this.delete_session(&project, &id, window, cx)
                                });
                            }),
                    ]),
                )
        });
    }

    fn delete_session(
        &mut self,
        project: &Path,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current =
            self.project.as_deref() == Some(project) && self.current_session() == Some(id);
        if let Err(e) = self.engine.delete_session(id) {
            window.push_notification(
                Notification::new()
                    .message(format!("Could not delete the session: {e:#}"))
                    .with_type(NotificationType::Error),
                cx,
            );
        }
        self.refresh_sessions(project);
        if current {
            self.start(None, window, cx);
        }
        cx.notify();
    }

    fn forget_project(&mut self, project: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.store.remove(project);
        self.unfolded.remove(project);
        self.sessions.remove(project);
        if self.project.as_deref() == Some(project) {
            self.project = None;
            self.phase = Phase::Idle;
            self._session = None;
            self.transcript = Transcript::default();
            self.ext = Extensions::default();
            window.refresh();
        }
        cx.notify();
    }

    /// A new session in the current project, or a folder to open if none.
    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.project.clone() {
            Some(project) => self.open_project(project, None, window, cx),
            None => self.open_folder(window, cx),
        }
    }

    fn opener(&self, cx: &mut Context<Self>) -> OnProject {
        let workspace = cx.entity().downgrade();
        Rc::new(move |path, window, cx| {
            let _ = workspace.update(cx, |this, cx| this.open_project(path, None, window, cx));
        })
    }

    fn open_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        new_project::pick_folder(self.opener(cx), window, cx);
    }

    fn create_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        new_project::open_dialog(self.default_parent(), self.opener(cx), window, cx);
    }

    /// Next to the most recent project, where the user evidently keeps them.
    fn default_parent(&self) -> PathBuf {
        self.store
            .projects()
            .first()
            .and_then(|p| p.path.parent().map(Path::to_path_buf))
            .or_else(maki_storage::paths::home)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Opens a session in the current project, a fresh one or `resume`.
    /// Dropping the previous task drops its command sender, which is what
    /// ends the old session.
    fn start(&mut self, resume: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        self.phase = Phase::Loading;
        self.transcript = Transcript::default();
        self.thumbs.clear();
        self.ext = Extensions::default();
        self.aside = None;
        self.goal = None;
        self.usage = None;
        self.model_picker = None;
        self.reload_pending = false;
        self._session = None;
        cx.notify();

        let engine = Arc::clone(&self.engine);
        self._session = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = cx
                .background_spawn({
                    let project = project.clone();
                    async move { engine.open(&project, resume) }
                })
                .await;
            let session = match opened {
                Ok(session) => session,
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        this.phase = Phase::Failed(format!("{e:#}").into());
                        cx.notify();
                    });
                    return;
                }
            };
            let updates = session.updates;
            let _ = this.update_in(cx, |this, window, cx| {
                this.transcript = Transcript::from_history(&session.boot.history);
                this.refresh_thumbs();
                for warning in &session.boot.warnings {
                    this.transcript.notice(warning.clone(), Level::Warning);
                }
                this.refresh_sessions(&project);
                if session.boot.yolo {
                    this.mode = Mode::Bypass;
                } else if this.mode == Mode::Bypass {
                    let _ = session.commands.send(Command::Yolo(true));
                }
                if let Some(thinking) = this.thinking {
                    let _ = session.commands.send(Command::Thinking(thinking));
                }
                let _ = session.commands.send(Command::Usage);
                this.model_picker = Some(this.model_picker(&session.boot, window, cx));
                this.phase = Phase::Ready {
                    boot: session.boot,
                    commands: session.commands,
                };
                this.list.reset(0);
                this.animate_from = this.transcript.items.len();
                this.scroll_to_end();
                this.composer.read(cx).focus_handle(cx).focus(window, cx);
                cx.notify();
            });
            while let Ok(update) = updates.recv_async().await {
                if Self::deliver(&this, update, cx).is_err() {
                    return;
                }
            }
        }));
    }

    fn deliver(
        this: &WeakEntity<Self>,
        update: Update,
        cx: &mut AsyncWindowContext,
    ) -> gpui_kit::Result<()> {
        this.update_in(cx, |this, window, cx| match update {
            Update::Agent(envelope) => this.on_agent(&envelope, window, cx),
            Update::Lua { topic, data } => this.on_lua(&topic, data, window, cx),
            Update::Question { input } => this.ask(&input, window, cx),
            Update::Btw(event) => this.on_aside(event, cx),
            Update::Usage(report) => {
                this.usage = Some(report);
                cx.notify();
            }
        })
    }

    fn model_picker(
        &self,
        boot: &Boot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ModelPicker> {
        let workspace = cx.entity().downgrade();
        // Deferred: the picker is still being updated when it calls this, and
        // the workspace updates it back.
        let on_pick: OnPick = Rc::new(move |spec, window, cx| {
            let workspace = workspace.clone();
            window.defer(cx, move |window, cx| {
                let _ = workspace.update(cx, |this, cx| {
                    this.run(PaletteAction::Model(spec), window, cx)
                });
            });
        });
        let (models, current) = (boot.models.clone(), boot.model.clone());
        cx.new(|cx| ModelPicker::new(models, current, on_pick, window, cx))
    }

    fn set_thinking(&mut self, thinking: StoredThinking) {
        self.thinking = Some(thinking);
        let dir = self.gui_dir.as_deref();
        let mut settings = Settings::load(dir);
        settings.thinking = Some(thinking.to_string());
        settings.save(dir);
        self.send(Command::Thinking(thinking));
    }

    /// Decodes the images of user messages not drawn yet.
    fn refresh_thumbs(&mut self) {
        for (index, item) in self.transcript.items.iter().enumerate() {
            if let Item::User { images, .. } = item
                && !images.is_empty()
                && !self.thumbs.contains_key(&index)
            {
                let thumbs = images
                    .iter()
                    .filter_map(Attachment::from_source)
                    .map(|a| a.image)
                    .collect();
                self.thumbs.insert(index, thumbs);
            }
        }
    }

    fn on_aside(&mut self, event: BtwEvent, cx: &mut Context<Self>) {
        let Some(aside) = &mut self.aside else {
            return;
        };
        match event {
            BtwEvent::Delta(text) => aside.answer.push_str(&text),
            BtwEvent::Done => aside.done = true,
            BtwEvent::Failed(error) => {
                aside.done = true;
                aside.error = Some(error);
            }
        }
        cx.notify();
    }

    /// A run of the main agent ended: carry a goal on, and reopen the session
    /// if providers changed while it ran.
    fn after_run(&mut self, cancelled: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.reload_pending {
            return self.reload_session(window, cx);
        }
        let Some(goal) = &mut self.goal else {
            return;
        };
        if cancelled {
            self.goal = None;
            return self.transcript.notice("Goal stopped.", Level::Info);
        }
        let reply = self
            .transcript
            .items
            .iter()
            .rev()
            .take_while(|item| !matches!(item, Item::User { .. }))
            .find_map(|item| match item {
                Item::Assistant(text) => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or_default();
        match goal.after_run(reply) {
            Next::Continue(prompt) => {
                let note = format!("Goal: continuing (run {} of {})", goal.runs, goal::MAX_RUNS);
                self.transcript.notice(note, Level::Info);
                self.transcript.working = true;
                self.send(Command::Prompt {
                    text: prompt,
                    plan: self.mode == Mode::Plan,
                    images: Vec::new(),
                });
            }
            Next::Achieved => {
                self.goal = None;
                window.push_notification(
                    Notification::new()
                        .message("Goal achieved")
                        .with_type(NotificationType::Success),
                    cx,
                );
            }
            Next::Blocked => {
                self.goal = None;
                self.transcript
                    .notice("Goal paused: the agent needs you.", Level::Warning);
            }
            Next::OutOfRuns => {
                self.goal = None;
                self.transcript.notice(
                    format!("Goal stopped after {} runs.", goal::MAX_RUNS),
                    Level::Warning,
                );
            }
        }
    }

    /// Reopens the current session so it sees changed providers and models.
    fn reload_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_pending = false;
        let resume = self.current_session().map(str::to_owned);
        self.start(resume, window, cx);
    }

    fn providers_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Ready { .. }) {
            return;
        }
        match self.transcript.working {
            true => self.reload_pending = true,
            false => self.reload_session(window, cx),
        }
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let main = window.window_handle();
        let on_change: settings_window::OnChange = Rc::new(move |cx| {
            let workspace = workspace.clone();
            let _ = main.update(cx, |_, window, cx| {
                let _ = workspace.update(cx, |this, cx| this.providers_changed(window, cx));
            });
        });
        self.settings_window =
            settings_window::open(self.settings_window, self.gui_dir.clone(), on_change, cx);
    }

    fn attach(&mut self, attachment: Attachment, cx: &mut Context<Self>) {
        self.attachments.push(attachment);
        cx.notify();
    }

    fn attach_paths(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        for path in paths.iter().filter(|p| attachments::is_image(p)) {
            match Attachment::from_path(path) {
                Ok(attachment) => self.attach(attachment, cx),
                Err(e) => window.push_notification(
                    Notification::new()
                        .message(format!("{e:#}"))
                        .with_type(NotificationType::Error),
                    cx,
                ),
            }
        }
    }

    fn pick_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(ATTACH_PROMPT.into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| this.attach_paths(&paths, window, cx));
        })
        .detach();
    }

    /// Takes images off the clipboard. `false` leaves the paste to the
    /// input, for text.
    fn paste_images(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let mut pasted = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    if let Some(attachment) = Attachment::from_image(image) {
                        self.attach(attachment, cx);
                        pasted = true;
                    }
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    if paths.paths().iter().any(|p| attachments::is_image(p)) {
                        self.attach_paths(paths.paths(), window, cx);
                        pasted = true;
                    }
                }
                ClipboardEntry::String(_) => {}
            }
        }
        pasted
    }

    fn suggestions(&self, cx: &App) -> Vec<Suggestion> {
        let Phase::Ready { boot, .. } = &self.phase else {
            return Vec::new();
        };
        if self.suggestions_dismissed {
            return Vec::new();
        }
        let custom: Vec<(String, String)> = boot
            .slash_commands
            .iter()
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect();
        commands::suggest(self.composer.read(cx).value().as_ref(), &custom)
    }

    fn complete(&mut self, suggestion: &Suggestion, window: &mut Window, cx: &mut Context<Self>) {
        let completion = suggestion.completion();
        let typed = self.composer.read(cx).value().trim().to_owned();
        if !suggestion.takes_args && typed == completion {
            return self.submit(window, cx);
        }
        self.composer
            .update(cx, |state, cx| state.set_value(completion, window, cx));
        self.suggestions_dismissed = !suggestion.takes_args;
        cx.notify();
    }

    /// Before the input sees a key: the suggestion list keys, and pasting
    /// an image.
    fn on_composer_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.key == PASTE_KEY
            && keystroke.modifiers.secondary()
            && self.paste_images(window, cx)
        {
            return cx.stop_propagation();
        }
        if keystroke.modifiers.modified() || !SUGGESTION_KEYS.contains(&keystroke.key.as_str()) {
            return;
        }
        let suggestions = self.suggestions(cx);
        if suggestions.is_empty() {
            return;
        }
        cx.stop_propagation();
        let last = suggestions.len() - 1;
        match keystroke.key.as_str() {
            "up" => self.suggestion = self.suggestion.checked_sub(1).unwrap_or(last),
            "down" => {
                self.suggestion = if self.suggestion >= last {
                    0
                } else {
                    self.suggestion + 1
                }
            }
            "escape" => self.suggestions_dismissed = true,
            _ => {
                let chosen = suggestions[self.suggestion.min(last)].clone();
                return self.complete(&chosen, window, cx);
            }
        }
        cx.notify();
    }

    fn on_agent(
        &mut self,
        envelope: &maki_agent::Envelope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let AgentEvent::PermissionRequest {
            id, tool, reason, ..
        } = &envelope.event
        {
            return self.ask_permission(id, &tool.to_string(), reason.as_deref(), window, cx);
        }
        match self.transcript.apply(envelope) {
            Change::None => {}
            Change::Appended => cx.notify(),
            Change::Updated(index) => {
                self.list.remeasure_items(index..index + 1);
                cx.notify();
            }
        }
        if envelope.subagent.is_some() {
            return;
        }
        match &envelope.event {
            AgentEvent::Done { reason, .. } => {
                if let Some(project) = self.project.clone() {
                    self.refresh_sessions(&project);
                }
                self.send(Command::Usage);
                let cancelled = reason.to_string() == CANCELLED_REASON;
                self.after_run(cancelled, window, cx);
                cx.notify();
            }
            AgentEvent::Error { .. } => self.goal = None,
            _ => {}
        }
    }

    /// Items only ever get appended to a transcript; a new one starts the
    /// list over (see [`Self::start`]).
    fn sync_list(&self) {
        let (shown, len) = (self.list.item_count(), self.transcript.items.len());
        if len < shown {
            self.list.reset(len);
        } else if len > shown {
            self.list.splice(shown..shown, len - shown);
        }
    }

    fn scroll_to_end(&self) {
        self.sync_list();
        self.list.set_follow_mode(FollowMode::Tail);
    }

    fn on_lua(&mut self, topic: &str, data: Value, window: &mut Window, cx: &mut Context<Self>) {
        match self.ext.apply(topic, data) {
            Effect::Notify { message, level } => {
                let kind = match level {
                    NotifyLevel::Info => NotificationType::Info,
                    NotifyLevel::Success => NotificationType::Success,
                    NotifyLevel::Warning => NotificationType::Warning,
                    NotifyLevel::Error => NotificationType::Error,
                };
                window.push_notification(Notification::new().message(message).with_type(kind), cx);
            }
            Effect::Prompt { text, send } => {
                self.composer
                    .update(cx, |state, cx| state.set_value(text, window, cx));
                if send {
                    self.submit(window, cx);
                }
            }
            Effect::Theme(data) => apply_theme_message(&data, window, cx),
            Effect::None | Effect::Unhandled => {}
        }
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_owned();
        if (text.is_empty() && self.attachments.is_empty()) || self.commands().is_none() {
            return;
        }
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        if let Some((command, args)) = commands::builtin(&text) {
            let args = args.to_owned();
            return self.run_builtin(command, &args, &text, window, cx);
        }
        self.prompt(text.clone(), text, cx);
    }

    /// Sends `prompt` with the attached images, showing `shown` for it.
    fn prompt(&mut self, shown: String, prompt: String, cx: &mut Context<Self>) {
        let attachments = std::mem::take(&mut self.attachments);
        let images: Vec<_> = attachments.iter().map(Attachment::source).collect();
        self.transcript.push_user(shown, images.clone());
        self.thumbs.insert(
            self.transcript.items.len() - 1,
            attachments.into_iter().map(|a| a.image).collect(),
        );
        self.send(Command::Prompt {
            text: prompt,
            plan: self.mode == Mode::Plan,
            images,
        });
        self.scroll_to_end();
        cx.notify();
    }

    fn run_builtin(
        &mut self,
        command: Builtin,
        args: &str,
        typed: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            Builtin::New => self.new_session(window, cx),
            Builtin::Btw if args.is_empty() => self
                .transcript
                .notice("Usage: /btw <question>", Level::Warning),
            Builtin::Btw => {
                self.aside = Some(Aside::new(args));
                self.send(Command::Btw(args.to_owned()));
            }
            Builtin::Goal if args.is_empty() => {
                let status = match &self.goal {
                    Some(goal) => format!(
                        "Goal: {} (run {} of {})",
                        goal.objective,
                        goal.runs,
                        goal::MAX_RUNS
                    ),
                    None => "No goal set. Usage: /goal <objective>".to_owned(),
                };
                self.transcript.notice(status, Level::Info);
            }
            Builtin::Goal if GOAL_STOP_WORDS.contains(&args.to_lowercase().as_str()) => {
                if self.goal.take().is_some() {
                    self.transcript.notice("Goal cleared.", Level::Info);
                }
            }
            Builtin::Goal => {
                let goal = Goal::new(args);
                let kickoff = goal.kickoff();
                self.goal = Some(goal);
                self.prompt(typed.to_owned(), kickoff, cx);
            }
            Builtin::Model => self.switch_model(args, window, cx),
            Builtin::Plan => self.set_mode(Mode::Plan),
            Builtin::Ask => self.set_mode(Mode::Ask),
            Builtin::Yolo => self.set_mode(match self.mode {
                Mode::Bypass => Mode::Ask,
                _ => Mode::Bypass,
            }),
            Builtin::Stop => {
                self.goal = None;
                self.send(Command::Cancel);
            }
            Builtin::Settings => self.open_settings(window, cx),
            Builtin::Theme => toggle_dark(self.gui_dir.as_deref(), window, cx),
            Builtin::Help => {
                let help = Builtin::ALL
                    .into_iter()
                    .map(|b| format!("- `/{} {}` {}", b.name(), b.hint(), b.description()))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.transcript.notice(help, Level::Info);
            }
            Builtin::Exit => cx.quit(),
        }
        self.scroll_to_end();
        cx.notify();
    }

    /// `/model name` picks the model whose spec or id matches; bare `/model`
    /// opens the palette on the model list.
    fn switch_model(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Phase::Ready { boot, .. } = &self.phase else {
            return;
        };
        if name.is_empty() {
            return self.open_palette_with(MODEL_QUERY, window, cx);
        }
        let found = boot
            .models
            .iter()
            .find(|spec| spec.as_str() == name || short_model(spec) == name)
            .cloned();
        match found {
            Some(spec) => self.run(PaletteAction::Model(spec), window, cx),
            None => self
                .transcript
                .notice(format!("No model named {name}"), Level::Warning),
        }
    }

    fn ask_permission(
        &mut self,
        id: &str,
        tool: &str,
        reason: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commands) = self.commands().cloned() else {
            return;
        };
        let detail = self
            .transcript
            .tool(id)
            .map(|call| call.summary.clone())
            .filter(|s| !s.is_empty());
        let id = id.to_owned();
        let tool = tool.to_owned();
        let reason = reason.map(str::to_owned);
        window.open_dialog(cx, move |dialog, _, cx| {
            let answer = |label: &'static str, answer: PermissionAnswer| {
                let commands = commands.clone();
                let id = id.clone();
                Button::new(label)
                    .label(label)
                    .on_click(move |_, window, cx| {
                        let _ = commands.send(Command::Permission {
                            id: id.clone(),
                            answer: answer.clone(),
                        });
                        window.close_dialog(cx);
                    })
            };
            dialog
                .title(format!("Allow {tool}?"))
                .w(px(560.))
                .overlay_closable(false)
                .child(
                    v_flex()
                        .gap_2()
                        .when_some(detail.clone(), |this, detail| {
                            this.child(
                                div()
                                    .p_2()
                                    .rounded(cx.theme().radius)
                                    .bg(cx.theme().muted)
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_xs()
                                    .child(detail),
                            )
                        })
                        .when_some(reason.clone(), |this, reason| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(reason),
                            )
                        }),
                )
                .footer({
                    DialogFooter::new().children([
                        answer("Deny", PermissionAnswer::Deny).danger(),
                        answer("Always for project", PermissionAnswer::AllowAlwaysProject)
                            .outline(),
                        answer("Allow for session", PermissionAnswer::AllowSession).outline(),
                        answer("Allow once", PermissionAnswer::AllowOnce).primary(),
                    ])
                })
        });
    }

    fn ask(&mut self, input: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commands) = self.commands().cloned() else {
            return;
        };
        let form = cx.new(|cx| QuestionForm::new(input, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            let submit_form = form.clone();
            let submit_commands = commands.clone();
            let dismiss_commands = commands.clone();
            dialog
                .title("maki has a question")
                .w(px(600.))
                .overlay_closable(false)
                .child(form.clone())
                .footer({
                    let form = submit_form.clone();
                    let submit = submit_commands.clone();
                    let dismiss = dismiss_commands.clone();
                    DialogFooter::new().children([
                        Button::new("skip")
                            .label("Skip")
                            .on_click(move |_, window, cx| {
                                let _ = dismiss.send(Command::Answer(None));
                                window.close_dialog(cx);
                            }),
                        Button::new("answer").primary().label("Answer").on_click(
                            move |_, window, cx| {
                                let text = form.read(cx).answer(cx);
                                let _ = submit.send(Command::Answer(Some(text)));
                                window.close_dialog(cx);
                            },
                        ),
                    ])
                })
        });
    }

    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette_with("", window, cx);
    }

    fn open_palette_with(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.palette_entries();
        let workspace = cx.entity().downgrade();
        let palette = cx.new(|cx| Palette::new(entries, query, workspace, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.w(px(560.)).p_0().child(palette.clone())
        });
    }

    fn palette_entries(&self) -> Vec<PaletteEntry> {
        let mut entries = vec![
            PaletteEntry::new("New session", PaletteAction::NewSession),
            PaletteEntry::new("Open folder…", PaletteAction::OpenFolder),
            PaletteEntry::new("New project…", PaletteAction::NewProject),
            PaletteEntry::new("Settings", PaletteAction::Settings),
            PaletteEntry::new("Attach images…", PaletteAction::AttachImages),
            PaletteEntry::new("Toggle dark mode", PaletteAction::ToggleDark),
            PaletteEntry::new("Mode: Ask before edits", PaletteAction::Mode(Mode::Ask)),
            PaletteEntry::new("Mode: Plan", PaletteAction::Mode(Mode::Plan)),
            PaletteEntry::new(
                "Mode: Bypass permissions",
                PaletteAction::Mode(Mode::Bypass),
            ),
        ];
        entries.extend(self.store.projects().iter().map(|p| {
            PaletteEntry::new(
                format!("Project: {}", p.name),
                PaletteAction::Project(p.path.clone()),
            )
            .detail(p.path.display().to_string())
        }));
        if let Phase::Ready { boot, .. } = &self.phase {
            if !boot.trusted {
                entries.push(PaletteEntry::new(
                    "Trust this project",
                    PaletteAction::Trust,
                ));
            }
            entries.extend(boot.slash_commands.iter().map(|c| {
                PaletteEntry::new(
                    format!("/{}", c.name),
                    PaletteAction::Insert(format!("/{} ", c.name)),
                )
                .detail(c.description.clone())
            }));
            entries.extend(boot.models.iter().map(|spec| {
                PaletteEntry::new(format!("Model: {spec}"), PaletteAction::Model(spec.clone()))
            }));
        }
        entries.extend(self.ext.commands.iter().map(|c| {
            PaletteEntry::new(c.title.clone(), PaletteAction::Lua(c.id.clone()))
                .detail(c.description.clone())
        }));
        entries
    }

    fn run(&mut self, action: PaletteAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            PaletteAction::NewSession => self.new_session(window, cx),
            PaletteAction::OpenFolder => self.open_folder(window, cx),
            PaletteAction::NewProject => self.create_project(window, cx),
            PaletteAction::Project(path) => self.open_project(path, None, window, cx),
            PaletteAction::Palette => self.open_palette(&OpenPalette, window, cx),
            PaletteAction::Settings => self.open_settings(window, cx),
            PaletteAction::AttachImages => self.pick_images(window, cx),
            PaletteAction::ToggleDark => toggle_dark(self.gui_dir.as_deref(), window, cx),
            PaletteAction::Mode(mode) => self.set_mode(mode),
            PaletteAction::Trust => match self.project.clone().map(|p| self.engine.trust(&p)) {
                Some(Ok(())) => self.start(None, window, cx),
                Some(Err(e)) => self.transcript.notice(format!("{e:#}"), Level::Error),
                None => {}
            },
            PaletteAction::Insert(text) => {
                self.composer
                    .update(cx, |state, cx| state.set_value(text, window, cx));
                self.composer.read(cx).focus_handle(cx).focus(window, cx);
            }
            PaletteAction::Model(spec) => {
                if let Phase::Ready { boot, .. } = &mut self.phase {
                    boot.model.clone_from(&spec);
                }
                if let Some(picker) = &self.model_picker {
                    picker.update(cx, |picker, _| picker.set_current(&spec));
                }
                self.usage = None;
                self.send(Command::Model(spec));
                self.send(Command::Usage);
            }
            PaletteAction::Thinking(thinking) => self.set_thinking(thinking),
            PaletteAction::Lua(id) => self.send(Command::Lua {
                topic: format!("{COMMAND_TOPIC_PREFIX}{id}"),
                data: Value::Null,
            }),
        }
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut groups = Vec::with_capacity(self.store.projects().len());
        for (index, project) in self.store.projects().iter().enumerate() {
            groups.push(
                self.render_project(index, &project.path, &project.name, cx)
                    .into_any_element(),
            );
        }
        let empty = groups.is_empty();
        let commands = self.commands().cloned();
        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .bg(cx.theme().sidebar)
            .child(
                v_flex().px_2().pt_3().pb_2().child(
                    h_flex()
                        .id("new-session")
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded(cx.theme().radius)
                        .cursor_pointer()
                        .text_sm()
                        .font_medium()
                        .hover(|style| style.bg(cx.theme().sidebar_accent))
                        .on_click(cx.listener(|this, _, window, cx| this.new_session(window, cx)))
                        .child(Icon::new(Lucide::SquarePen).small())
                        .child("New session"),
                ),
            )
            .child(
                h_flex()
                    .pl_2()
                    .pr_3()
                    .justify_between()
                    .child(section_label("Projects", cx))
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Button::new("open-folder")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(Lucide::FolderOpen))
                                    .tooltip("Open folder")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_folder(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("new-project")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(Lucide::FolderPlus))
                                    .tooltip("New project")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.create_project(window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .px_2()
                            .pb_3()
                            .gap_4()
                            .child(v_flex().gap_0p5().children(groups).when(empty, |this| {
                                this.child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Open a folder to get started."),
                                )
                            }))
                            .children(commands.map(|commands| {
                                v_flex()
                                    .gap_4()
                                    .children(self.ext.panels.iter().map(|panel| {
                                        v_flex()
                                            .gap_2()
                                            .child(section_label(&panel.title, cx))
                                            .child(v_flex().px_2().gap_2().children(
                                                nodes::render_all(
                                                    &panel.body,
                                                    &panel.id,
                                                    &commands,
                                                    cx,
                                                ),
                                            ))
                                    }))
                            })),
                    ),
            )
    }

    fn render_project(
        &self,
        index: usize,
        path: &Path,
        name: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.unfolded.contains(path);
        let current = self.project.as_deref() == Some(path);
        let now = now_epoch();
        let sessions = match open {
            true => self
                .sessions
                .get(path)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            false => &[],
        };
        let toggle = path.to_path_buf();
        let fresh = path.to_path_buf();
        let forget = path.to_path_buf();
        let group = SharedString::from(format!("project-row-{index}"));
        let mut rows = Vec::with_capacity(sessions.len().max(1));
        if open && sessions.is_empty() {
            rows.push(
                div()
                    .pl_7()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("No sessions yet")
                    .into_any_element(),
            );
        }
        for session in sessions {
            let id = session.id.to_string();
            let active = current && self.current_session() == Some(id.as_str());
            let project = path.to_path_buf();
            let resume = id.clone();
            let doomed = (path.to_path_buf(), id.clone(), title_of(&session.title));
            let row = SharedString::from(format!("session-row-{id}"));
            rows.push(
                h_flex()
                    .id(SharedString::from(format!("session-{id}")))
                    .group(row.clone())
                    .gap_2()
                    .pl_7()
                    .pr_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .text_sm()
                    .when(active, |this| {
                        this.bg(cx.theme().sidebar_accent)
                            .text_color(cx.theme().sidebar_accent_foreground)
                    })
                    .hover(|style| style.bg(cx.theme().sidebar_accent))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_project(project.clone(), Some(resume.clone()), window, cx)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(title_of(&session.title)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(short_ago(now, session.updated_at)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .invisible()
                            .group_hover(row, |s| s.visible())
                            .child(
                                Button::new(SharedString::from(format!("delete-session-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(Lucide::Trash))
                                    .tooltip("Delete session")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        let (project, id, title) = doomed.clone();
                                        this.confirm_delete_session(project, id, title, window, cx);
                                    })),
                            ),
                    )
                    .into_any_element(),
            );
        }
        v_flex()
            .child(
                h_flex()
                    .id(("project", index))
                    .group(group.clone())
                    .gap_1p5()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .text_sm()
                    .hover(|style| style.bg(cx.theme().sidebar_accent))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_project(&toggle, cx)))
                    .child(
                        Icon::new(match open {
                            true => Lucide::ChevronDown,
                            false => Lucide::ChevronRight,
                        })
                        .xsmall()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Icon::new(match open {
                            true => Lucide::FolderOpen,
                            false => Lucide::Folder,
                        })
                        .small()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(current, |this| this.font_medium())
                            .child(name.to_owned()),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .invisible()
                            .group_hover(group, |style| style.visible())
                            .child(
                                Button::new(("forget-project", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip("Remove from list")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.forget_project(&forget, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("project-new-session", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(Lucide::Plus))
                                    .tooltip("New session here")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.open_project(fresh.clone(), None, window, cx);
                                    })),
                            ),
                    ),
            )
            .children(rows)
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let Some(item) = self.transcript.items.get(index) else {
            return div().into_any_element();
        };
        let len = self.transcript.items.len();
        let last = index + 1 == len;
        let fresh = index >= self.animate_from && index + 2 >= len;
        let row = h_flex().w_full().justify_center().child(
            div()
                .w(px(TRANSCRIPT_WIDTH))
                .max_w_full()
                .px_6()
                .pt(px(if index == 0 { 24. } else { 16. }))
                .when(last, |this| this.pb_6())
                .child(self.render_item(index, item, cx)),
        );
        match fresh {
            true => fade_in(row, ("row-in", index)).into_any_element(),
            false => row.into_any_element(),
        }
    }

    /// Takes wheel input over the transcript before the list does, so
    /// [`Self::ease_scroll`] can spread each notch over a few frames.
    fn wheel_catcher(&self) -> impl IntoElement {
        let pending = Rc::clone(&self.scroll_pending);
        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |_, hitbox, window, _| {
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                    let delta = event.delta.pixel_delta(px(SCROLL_LINE_HEIGHT));
                    if phase == DispatchPhase::Capture
                        && hitbox.should_handle_scroll(window)
                        && delta.y.abs() > delta.x.abs()
                    {
                        pending.set(pending.get() + f32::from(delta.y));
                        cx.stop_propagation();
                        window.refresh();
                    }
                });
            },
        )
        .absolute()
        .size_full()
    }

    fn ease_scroll(&self, window: &mut Window) {
        let pending = self.scroll_pending.get();
        if pending.abs() < SCROLL_SETTLED {
            self.scroll_pending.set(0.);
            return;
        }
        let step = if (pending * SCROLL_EASING).abs() < SCROLL_SETTLED {
            pending
        } else {
            pending * SCROLL_EASING
        };
        self.list.scroll_by(px(-step));
        self.scroll_pending.set(pending - step);
        window.request_animation_frame();
    }

    /// A tick per prompt down the side of the transcript; hovering one
    /// previews the turn, clicking it scrolls there.
    fn render_turn_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let turns = turns(&self.transcript.items);
        let top = self.list.logical_scroll_top().item_ix;
        let workspace = cx.entity().downgrade();
        let current = turns.iter().rposition(|t| t.index <= top).unwrap_or(0);
        v_flex()
            .absolute()
            .left_1()
            .top_0()
            .bottom_0()
            .justify_center()
            .when(turns.len() > 1, |this| {
                this.children(turns.into_iter().enumerate().map(|(n, turn)| {
                    let active = n == current;
                    let index = turn.index;
                    let workspace = workspace.clone();
                    div()
                        .id(("turn-tick", n))
                        .py(px(TURN_TICK_GAP / 2.))
                        .cursor_pointer()
                        .child(
                            div()
                                .w(px(if active {
                                    TURN_TICK_ACTIVE_WIDTH
                                } else {
                                    TURN_TICK_WIDTH
                                }))
                                .h(px(TURN_TICK_HEIGHT))
                                .bg(if active {
                                    cx.theme().foreground
                                } else {
                                    cx.theme().muted_foreground.opacity(0.5)
                                }),
                        )
                        .tooltip(move |window, cx| {
                            let (prompt, reply) = workspace
                                .upgrade()
                                .map(|w| turn.texts(&w.read(cx).transcript.items))
                                .unwrap_or_default();
                            Tooltip::element(move |_, cx| turn_card(&prompt, &reply, cx))
                                .build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.list.scroll_to(ListOffset {
                                item_ix: index,
                                offset_in_item: px(0.),
                            });
                            cx.notify();
                        }))
                }))
            })
    }

    fn render_item(
        &self,
        index: usize,
        item: &Item,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        match item {
            Item::User { text, .. } => v_flex()
                .items_end()
                .gap_2()
                .when_some(self.thumbs.get(&index), |this, thumbs| {
                    this.child(
                        h_flex().gap_2().flex_wrap().justify_end().children(
                            thumbs
                                .iter()
                                .map(|image| thumbnail(image, MESSAGE_THUMB_SIZE, cx)),
                        ),
                    )
                })
                .when(!text.is_empty(), |this| {
                    this.child(
                        div()
                            .max_w(px(TRANSCRIPT_WIDTH * 0.8))
                            .px_4()
                            .py_2p5()
                            .rounded(cx.theme().radius_lg)
                            .bg(cx.theme().secondary)
                            .text_sm()
                            .child(text.clone()),
                    )
                })
                .into_any_element(),
            Item::Assistant(text) => {
                TextView::markdown(SharedString::from(format!("item-{index}")), text.clone())
                    .text_sm()
                    .into_any_element()
            }
            Item::Thinking(text) => {
                let key = format!("thinking-{index}");
                let open = self.expanded.contains(&key);
                let shown = match open {
                    true => text.clone(),
                    false => preview(text, THINKING_PREVIEW_LINES),
                };
                let text_id = SharedString::from(format!("{key}-text"));
                div()
                    .id(SharedString::from(key.clone()))
                    .pl_3()
                    .border_l_2()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(&key, cx)))
                    .child(TextView::markdown(text_id, shown))
                    .into_any_element()
            }
            Item::Tool(call) => self.render_tool(call, cx).into_any_element(),
            Item::Notice { text, level } => {
                let color = match level {
                    Level::Info => cx.theme().muted_foreground,
                    Level::Warning => cx.theme().warning,
                    Level::Error => cx.theme().danger,
                };
                div()
                    .text_xs()
                    .text_color(color)
                    .child(TextView::markdown(
                        SharedString::from(format!("notice-{index}")),
                        text.clone(),
                    ))
                    .into_any_element()
            }
        }
    }

    fn render_tool(&self, call: &ToolCall, cx: &mut Context<Self>) -> impl IntoElement {
        let key = format!("tool-{}", call.id);
        let open = self.expanded.contains(&key);
        let status = match call.status {
            ToolStatus::Running => Spinner::new().xsmall().into_any_element(),
            ToolStatus::Done => Icon::new(IconName::Check)
                .xsmall()
                .text_color(cx.theme().success)
                .into_any_element(),
            ToolStatus::Failed => Icon::new(IconName::Close)
                .xsmall()
                .text_color(cx.theme().danger)
                .into_any_element(),
        };
        let has_output = !call.output.is_empty();
        let toggle = key.clone();
        v_flex()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .child(
                h_flex()
                    .id(SharedString::from(key))
                    .gap_2()
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(&toggle, cx)))
                    .child(status)
                    .child(div().text_sm().font_medium().child(call.name.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(call.summary.clone()),
                    )
                    .when(has_output, |this| {
                        this.child(
                            Icon::new(match open {
                                true => IconName::ChevronUp,
                                false => IconName::ChevronDown,
                            })
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                        )
                    }),
            )
            .when(
                has_output && (open || call.status == ToolStatus::Failed),
                |this| {
                    this.child(
                        div()
                            .px_3()
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().muted)
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .whitespace_normal()
                            .child(match open {
                                true => call.output.clone(),
                                false => preview(&call.output, TOOL_PREVIEW_LINES),
                            }),
                    )
                },
            )
    }

    fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_owned());
        }
        cx.notify();
    }

    fn render_main(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        match &self.phase {
            Phase::Idle => start_screen(cx).into_any_element(),
            Phase::Loading => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Spinner::new())
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Loading plugins and starting a session…"),
                )
                .into_any_element(),
            Phase::Failed(error) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .child(div().font_semibold().child("Could not start a session"))
                .child(
                    div()
                        .max_w(px(TRANSCRIPT_WIDTH))
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                )
                .child(
                    Button::new("retry")
                        .outline()
                        .label("Try again")
                        .on_click(cx.listener(|this, _, window, cx| this.start(None, window, cx))),
                )
                .into_any_element(),
            Phase::Ready { boot, .. } => {
                let empty = self.transcript.items.is_empty();
                let name = self.project_name().unwrap_or_default();
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .when(empty, |this| {
                        this.child(
                            v_flex()
                                .flex_1()
                                .w(px(TRANSCRIPT_WIDTH))
                                .max_w_full()
                                .mx_auto()
                                .px_6()
                                .gap_4()
                                .justify_center()
                                .when(!boot.trusted, |this| this.child(trust_banner(cx)))
                                .child(welcome(&name, cx)),
                        )
                    })
                    .when(!empty, |this| {
                        self.sync_list();
                        this.when(!boot.trusted, |this| {
                            this.child(
                                div()
                                    .w(px(TRANSCRIPT_WIDTH))
                                    .max_w_full()
                                    .mx_auto()
                                    .px_6()
                                    .pt_4()
                                    .child(trust_banner(cx)),
                            )
                        })
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_h_0()
                                .child(
                                    list(
                                        self.list.clone(),
                                        cx.processor(|this, index: usize, _, cx| {
                                            this.render_row(index, cx)
                                        }),
                                    )
                                    .size_full(),
                                )
                                .child(self.wheel_catcher())
                                .child(self.render_turn_rail(cx)),
                        )
                    })
                    .child(self.render_composer(boot, cx))
                    .into_any_element()
            }
        }
    }

    /// The input box and, under it, everything about the session: mode on
    /// the left; plugin status, cost, model and context fill on the right.
    fn render_composer(&self, boot: &Boot, cx: &mut Context<Self>) -> impl IntoElement {
        let working = self.transcript.working;
        let empty = self.composer.read(cx).value().trim().is_empty() && self.attachments.is_empty();
        let suggestions = self.suggestions(cx);
        div().px_6().pb_3().child(
            v_flex()
                .w(px(TRANSCRIPT_WIDTH))
                .max_w_full()
                .mx_auto()
                .gap_1p5()
                .when_some(self.aside.as_ref(), |this, aside| {
                    this.child(fade_in(
                        div().child(self.render_aside(aside, cx)),
                        "aside-in",
                    ))
                })
                .child(
                    div()
                        .relative()
                        .when(!suggestions.is_empty(), |this| {
                            this.child(fade_in(
                                div().child(self.render_suggestions(&suggestions, cx)),
                                "suggestions-in",
                            ))
                        })
                        .child(
                            v_flex()
                                .id("composer")
                                .gap_1()
                                .px_2()
                                .py_1p5()
                                .rounded(px(16.))
                                .border_1()
                                .border_color(cx.theme().input)
                                .bg(cx.theme().background)
                                .shadow_sm()
                                .drag_over::<ExternalPaths>(|style, _, _, cx| {
                                    style.border_color(cx.theme().ring)
                                })
                                .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                                    this.attach_paths(paths.paths(), window, cx)
                                }))
                                .capture_key_down(cx.listener(
                                    |this, event: &KeyDownEvent, window, cx| {
                                        this.on_composer_key(event, window, cx)
                                    },
                                ))
                                .when(!self.attachments.is_empty(), |this| {
                                    this.child(self.render_attachments(cx))
                                })
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_1()
                                        .child(self.render_add_menu(boot, cx))
                                        .child(
                                            div().flex_1().min_w_0().child(
                                                Textarea::new(&self.composer).bordered(false),
                                            ),
                                        )
                                        .child(match working {
                                            true => Button::new("stop")
                                                .primary()
                                                .small()
                                                .rounded(px(999.))
                                                .icon(Icon::new(Lucide::Square))
                                                .tooltip("Stop")
                                                .on_click(cx.listener(|this, _, _, _| {
                                                    this.send(Command::Cancel)
                                                })),
                                            false => Button::new("send")
                                                .primary()
                                                .small()
                                                .rounded(px(999.))
                                                .icon(Icon::new(Lucide::ArrowUp))
                                                .tooltip("Send")
                                                .disabled(empty)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.submit(window, cx)
                                                })),
                                        }),
                                ),
                        ),
                )
                .child(self.render_session_bar(boot, cx)),
        )
    }

    fn render_attachments(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().gap_2().px_1().pt_1().flex_wrap().children(
            self.attachments.iter().enumerate().map(|(i, attachment)| {
                div()
                    .relative()
                    .group(SharedString::from(format!("attachment-{i}")))
                    .child(thumbnail(&attachment.image, THUMB_SIZE, cx))
                    .child(
                        div().absolute().top_0p5().right_0p5().child(
                            Button::new(("remove-attachment", i))
                                .xsmall()
                                .rounded(px(999.))
                                .icon(IconName::Close)
                                .tooltip("Remove")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if i < this.attachments.len() {
                                        this.attachments.remove(i);
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
            }),
        )
    }

    fn render_suggestions(
        &self,
        suggestions: &[Suggestion],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.suggestion.min(suggestions.len().saturating_sub(1));
        div()
            .absolute()
            .bottom_full()
            .left_0()
            .right_0()
            .pb_1p5()
            .child(
                v_flex()
                    .id("suggestions")
                    .max_h(px(SUGGESTIONS_HEIGHT))
                    .overflow_y_scrollbar()
                    .p_1()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().popover)
                    .text_color(cx.theme().popover_foreground)
                    .shadow_lg()
                    .children(suggestions.iter().enumerate().map(|(i, suggestion)| {
                        let chosen = suggestion.clone();
                        h_flex()
                            .id(("suggestion", i))
                            .gap_3()
                            .px_2()
                            .py_1p5()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .text_sm()
                            .when(i == selected, |this| this.bg(cx.theme().accent))
                            .hover(|style| style.bg(cx.theme().accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.complete(&chosen, window, cx);
                                this.composer.read(cx).focus_handle(cx).focus(window, cx);
                            }))
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap_1()
                                    .child(
                                        div().font_medium().child(format!("/{}", suggestion.name)),
                                    )
                                    .child(
                                        div()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(suggestion.hint.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(suggestion.description.clone()),
                            )
                    })),
            )
    }

    fn render_aside(&self, aside: &Aside, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .p_3()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(Lucide::MessageCircleQuestionMark)
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_medium()
                            .child(aside.question.clone()),
                    )
                    .when(!aside.done, |this| this.child(Spinner::new().xsmall()))
                    .child(
                        Button::new("close-aside")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.aside = None;
                                cx.notify();
                            })),
                    ),
            )
            .when(!aside.answer.is_empty(), |this| {
                this.child(
                    div()
                        .id("aside-answer")
                        .max_h(px(ASIDE_HEIGHT))
                        .overflow_y_scrollbar()
                        .text_sm()
                        .child(TextView::markdown("aside-text", aside.answer.clone())),
                )
            })
            .when_some(aside.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
    }

    fn render_add_menu(&self, boot: &Boot, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let slash: Vec<(String, String)> = boot
            .slash_commands
            .iter()
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect();
        Button::new("composer-add")
            .ghost()
            .small()
            .rounded(px(999.))
            .icon(Icon::new(Lucide::Plus))
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                let mut menu = menu
                    .min_w(px(MENU_WIDTH))
                    .max_h(px(MENU_HEIGHT))
                    .scrollable(true)
                    .item(
                        action_item(&workspace, "Attach images…", PaletteAction::AttachImages)
                            .icon(Icon::new(Lucide::ImagePlus)),
                    )
                    .item(
                        action_item(&workspace, "All commands…", PaletteAction::Palette)
                            .icon(Icon::new(Lucide::Command)),
                    )
                    .separator()
                    .label("Commands");
                for (name, _) in &slash {
                    menu = menu.item(action_item(
                        &workspace,
                        format!("/{name}"),
                        PaletteAction::Insert(format!("/{name} ")),
                    ));
                }
                menu
            })
    }

    fn render_session_bar(&self, boot: &Boot, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let mode_workspace = workspace.clone();
        let mode = self.mode;
        h_flex()
            .px_2()
            .gap_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                Button::new("mode")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(self.mode.icon()))
                    .label(self.mode.label())
                    .when(self.mode == Mode::Bypass, |this| {
                        this.text_color(cx.theme().danger)
                    })
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                        Mode::ALL
                            .into_iter()
                            .fold(menu.min_w(px(MENU_WIDTH)), |menu, option| {
                                menu.item(
                                    action_item(
                                        &mode_workspace,
                                        option.label(),
                                        PaletteAction::Mode(option),
                                    )
                                    .icon(Icon::new(option.icon()))
                                    .checked(option == mode),
                                )
                            })
                    }),
            )
            .child(self.render_context(boot, cx))
            .when_some(self.goal.as_ref(), |this, goal| {
                this.child(
                    Button::new("goal")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(Lucide::Target))
                        .label(format!("Goal {}/{}", goal.runs, goal::MAX_RUNS))
                        .tooltip(format!("{} — click to clear", goal.objective))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.goal = None;
                            cx.notify();
                        })),
                )
            })
            .when(self.transcript.working, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(Spinner::new().xsmall())
                        .child("Working…"),
                )
            })
            .child(div().flex_1())
            .children(self.ext.status.iter().map(|s| div().child(s.text.clone())))
            .children(
                self.transcript
                    .totals
                    .cost
                    .map(|cost| format!("${cost:.2}")),
            )
            .child(self.render_effort(boot, &workspace))
            .when_some(self.model_picker.clone(), |this, picker| {
                this.child(render_model_picker(short_model(&boot.model), picker, cx))
            })
            .child(self.render_usage(cx))
    }

    /// How full the context window is, as a row of ticks.
    fn render_context(&self, boot: &Boot, cx: &mut Context<Self>) -> impl IntoElement {
        let totals = &self.transcript.totals;
        let window = match totals.context_window {
            0 => boot.context_window,
            window => window,
        };
        let used = totals.context_size;
        let filled = match window {
            0 => 0,
            window => (u64::from(used) * u64::from(CONTEXT_TICKS)).div_ceil(u64::from(window)),
        };
        let details: SharedString = format!(
            "Context {used} of {window} tokens · ↑{} ↓{}",
            totals.usage.input + totals.usage.cache_read + totals.usage.cache_creation,
            totals.usage.output
        )
        .into();
        let (on, off) = (cx.theme().foreground, cx.theme().border);
        h_flex()
            .id("context")
            .gap_2()
            .tooltip(move |window, cx| Tooltip::new(details.clone()).build(window, cx))
            .child(
                h_flex()
                    .gap(px(CONTEXT_TICK_GAP))
                    .children((0..CONTEXT_TICKS).map(|tick| {
                        div()
                            .w(px(CONTEXT_TICK_WIDTH))
                            .h(px(CONTEXT_TICK_HEIGHT))
                            .bg(if u64::from(tick) < filled { on } else { off })
                    })),
            )
            .child(format!(
                "{} / {}",
                compact_tokens(used),
                compact_tokens(window)
            ))
    }

    fn render_effort(&self, boot: &Boot, workspace: &WeakEntity<Self>) -> impl IntoElement {
        let current = self.thinking.unwrap_or(boot.thinking);
        let workspace = workspace.clone();
        Button::new("effort")
            .ghost()
            .xsmall()
            .label(thinking_label(current))
            .tooltip("Thinking effort")
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                thinking_choices().fold(menu.min_w(px(MENU_WIDTH)), |menu, choice| {
                    menu.item(
                        action_item(
                            &workspace,
                            thinking_label(choice),
                            PaletteAction::Thinking(choice),
                        )
                        .checked(choice == current),
                    )
                })
            })
    }

    /// A ring as full as the tightest usage limit, opening onto all of them.
    fn render_usage(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let percent = self.usage.as_ref().and_then(tightest_limit).unwrap_or(0);
        let color = match percent {
            p if p >= USAGE_DANGER_PERCENT => cx.theme().danger,
            p if p >= USAGE_WARNING_PERCENT => cx.theme().warning,
            _ => cx.theme().muted_foreground,
        };
        let report = self.usage.clone();
        Popover::new("usage")
            .anchor(Anchor::BottomRight)
            .trigger(
                Button::new("usage-ring").ghost().xsmall().child(
                    ProgressCircle::new("usage-ring-progress")
                        .size(px(USAGE_RING_SIZE))
                        .color(color)
                        .value(percent as f32),
                ),
            )
            .content(move |_, _, cx| usage_card(report.as_ref(), cx))
    }
}

fn render_model_picker(label: String, picker: Entity<ModelPicker>, cx: &App) -> impl IntoElement {
    let search = picker.read(cx).search_focus(cx);
    Popover::new("model")
        .anchor(Anchor::BottomRight)
        .track_focus(&search)
        .trigger(Button::new("model-button").ghost().xsmall().label(label))
        .content(move |_, _, cx| {
            let popover = cx.entity().downgrade();
            picker.update(cx, |picker, _| picker.shown_in(popover));
            picker.clone()
        })
}

fn usage_card(report: Option<&UsageReport>, cx: &App) -> gpui_kit::AnyElement {
    let muted = cx.theme().muted_foreground;
    let note = |text: String| {
        div()
            .text_sm()
            .text_color(muted)
            .child(text)
            .into_any_element()
    };
    let (plan, rows): (Option<String>, Vec<gpui_kit::AnyElement>) = match report {
        None => (
            None,
            vec![
                h_flex()
                    .gap_2()
                    .child(Spinner::new().xsmall())
                    .child(note("Loading…".into()))
                    .into_any_element(),
            ],
        ),
        Some(UsageReport::Unsupported) => (
            None,
            vec![note("This provider does not report usage limits.".into())],
        ),
        Some(UsageReport::Failed(e)) => (None, vec![note(e.clone())]),
        Some(UsageReport::Ready(usage)) if usage.limits.is_empty() => {
            (usage.plan.clone(), vec![note("No limits reported.".into())])
        }
        Some(UsageReport::Ready(usage)) => {
            let now = now_epoch();
            let rows = usage
                .limits
                .iter()
                .enumerate()
                .map(|(i, limit)| {
                    let percent = limit.percentage.unwrap_or(0);
                    let detail = [
                        limit.reset_at.and_then(|at| resets_in(now, at)),
                        limit.detail.clone(),
                        limit.percentage.map(|p| format!("{p}%")),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("  ");
                    v_flex()
                        .gap_1p5()
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_3()
                                .child(div().text_sm().child(limit.label.clone()))
                                .child(div().text_xs().text_color(muted).child(detail)),
                        )
                        .child(
                            Progress::new(("usage-limit", i))
                                .value(percent as f32)
                                .color(match percent {
                                    p if p >= USAGE_DANGER_PERCENT => cx.theme().danger,
                                    p if p >= USAGE_WARNING_PERCENT => cx.theme().warning,
                                    _ => cx.theme().primary,
                                }),
                        )
                        .into_any_element()
                })
                .collect();
            (usage.plan.clone(), rows)
        }
    };
    let title = match plan {
        Some(plan) => format!("Usage limits · {plan}"),
        None => "Usage limits".to_owned(),
    };
    v_flex()
        .w(px(USAGE_CARD_WIDTH))
        .gap_3()
        .child(div().text_sm().text_color(muted).child(title))
        .children(rows)
        .into_any_element()
}

fn tightest_limit(report: &UsageReport) -> Option<u32> {
    match report {
        UsageReport::Ready(usage) => usage.limits.iter().filter_map(|l| l.percentage).max(),
        UsageReport::Unsupported | UsageReport::Failed(_) => None,
    }
}

/// "Resets in 2h 50m", or `None` once `reset_at_ms` has passed.
fn resets_in(now: u64, reset_at_ms: u64) -> Option<String> {
    let left = (reset_at_ms / MILLIS_PER_SECOND).checked_sub(now)?;
    let (days, hours, minutes) = (
        left / SECONDS_PER_DAY,
        left % SECONDS_PER_DAY / SECONDS_PER_HOUR,
        left % SECONDS_PER_HOUR / SECONDS_PER_MINUTE,
    );
    Some(match (days, hours) {
        (0, 0) => format!("Resets in {minutes}m"),
        (0, _) => format!("Resets in {hours}h {minutes}m"),
        _ => format!("Resets in {days}d {hours}h"),
    })
}

fn compact_tokens(tokens: u32) -> String {
    const THOUSAND: u32 = 1_000;
    const MILLION: u32 = 1_000_000;
    match tokens {
        t if t >= MILLION && t % MILLION == 0 => format!("{}M", t / MILLION),
        t if t >= MILLION => format!("{:.1}M", f64::from(t) / f64::from(MILLION)),
        t if t >= THOUSAND => format!("{}k", t / THOUSAND),
        t => t.to_string(),
    }
}

fn thinking_choices() -> impl Iterator<Item = StoredThinking> {
    [StoredThinking::Off, StoredThinking::Adaptive]
        .into_iter()
        .chain(Effort::ALL.map(|level| StoredThinking::Effort { level }))
}

fn thinking_label(thinking: StoredThinking) -> String {
    match thinking {
        StoredThinking::Off => "Thinking off".into(),
        StoredThinking::Adaptive => "Auto".into(),
        StoredThinking::Effort { level } => match level {
            Effort::Minimal => "Minimal",
            Effort::Low => "Low",
            Effort::Medium => "Medium",
            Effort::High => "High",
            Effort::XHigh => "Extra high",
            Effort::Max => "Max",
        }
        .into(),
        StoredThinking::Budget { tokens } => format!("{} thinking", compact_tokens(tokens)),
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ease_scroll(window);
        v_flex()
            .size_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(|this, _: &NewSession, window, cx| this.new_session(window, cx)))
            .on_action(cx.listener(|this, _: &OpenFolder, window, cx| this.open_folder(window, cx)))
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)),
            )
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child({
                let workspace = cx.entity().downgrade();
                let settings: Handler = Rc::new(move |window, cx| {
                    let _ = workspace.update(cx, |this, cx| this.open_settings(window, cx));
                });
                let crumbs = self
                    .project_name()
                    .map(SharedString::from)
                    .into_iter()
                    .collect();
                let notice = self.render_update(cx);
                title_bar(crumbs, self.gui_dir.clone(), Some(settings), notice, cx)
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(self.render_main(cx)),
                    ),
            )
    }
}

fn action_item(
    workspace: &WeakEntity<WorkspaceView>,
    label: impl Into<SharedString>,
    action: PaletteAction,
) -> PopupMenuItem {
    let workspace = workspace.clone();
    PopupMenuItem::new(label).on_click(move |_, window, cx| {
        let _ = workspace.update(cx, |this, cx| this.run(action.clone(), window, cx));
    })
}

fn section_label(text: &str, cx: &Context<WorkspaceView>) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .text_xs()
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_owned())
}

fn start_screen(cx: &mut Context<WorkspaceView>) -> impl IntoElement {
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_3()
        .child(
            Icon::new(Lucide::FolderOpen)
                .large()
                .text_color(cx.theme().muted_foreground),
        )
        .child(div().text_xl().font_semibold().child("Start in a project"))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("maki works inside a folder. Open one, or create a new project."),
        )
        .child(
            h_flex()
                .gap_2()
                .pt_2()
                .child(
                    Button::new("start-open")
                        .outline()
                        .icon(Icon::new(Lucide::FolderOpen))
                        .label("Open folder")
                        .on_click(cx.listener(|this, _, window, cx| this.open_folder(window, cx))),
                )
                .child(
                    Button::new("start-new")
                        .primary()
                        .icon(Icon::new(Lucide::FolderPlus))
                        .label("New project")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.create_project(window, cx)),
                        ),
                ),
        )
}

fn trust_banner(cx: &mut Context<WorkspaceView>) -> impl IntoElement {
    h_flex()
        .gap_3()
        .p_3()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().warning)
        .child(Icon::new(IconName::TriangleAlert).text_color(cx.theme().warning))
        .child(div().flex_1().text_sm().child(
            "This project's .maki config, plugins and env files are not loaded until you trust it.",
        ))
        .child(
            Button::new("trust")
                .small()
                .outline()
                .label("Trust project")
                .on_click(
                    cx.listener(|this, _, window, cx| this.run(PaletteAction::Trust, window, cx)),
                ),
        )
}

fn welcome(name: &str, cx: &mut Context<WorkspaceView>) -> impl IntoElement {
    v_flex()
        .items_center()
        .gap_2()
        .child(div().text_2xl().font_semibold().child(name.to_owned()))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Describe a task to start. Type / for commands, or drop an image in."),
        )
}

fn thumbnail(image: &Arc<Image>, size: f32, cx: &App) -> impl IntoElement {
    img(image.clone())
        .size(px(size))
        .object_fit(ObjectFit::Cover)
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `provider/model` reads as just the model next to the input.
fn short_model(spec: &str) -> String {
    spec.rsplit_once('/')
        .map_or(spec, |(_, model)| model)
        .to_owned()
}

fn title_of(title: &str) -> String {
    match title.trim() {
        "" => UNTITLED.to_owned(),
        title => title.to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Turn {
    index: usize,
    reply: Option<usize>,
}

impl Turn {
    /// The prompt's first line and the start of the reply. Built on hover
    /// only, not on every frame the rail is drawn.
    fn texts(self, items: &[Item]) -> (SharedString, SharedString) {
        let prompt = match items.get(self.index) {
            Some(Item::User { text, .. }) => text.lines().next().unwrap_or_default().to_owned(),
            _ => String::new(),
        };
        let reply = match self.reply.and_then(|i| items.get(i)) {
            Some(Item::Assistant(text)) => preview(text, TURN_PREVIEW_LINES),
            _ => String::new(),
        };
        (prompt.into(), reply.into())
    }
}

/// Each user message, with the first reply under it.
fn turns(items: &[Item]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::User { .. } => turns.push(Turn { index, reply: None }),
            Item::Assistant(_) => {
                if let Some(turn) = turns.last_mut().filter(|t| t.reply.is_none()) {
                    turn.reply = Some(index);
                }
            }
            _ => {}
        }
    }
    turns
}

fn fade_in<E: gpui_kit::Styled + IntoElement + 'static>(
    element: E,
    id: impl Into<gpui_kit::ElementId>,
) -> impl IntoElement {
    element.with_animation(
        id,
        Animation::new(FADE_IN).with_easing(ease_out_quint()),
        |element, delta| element.opacity(delta).mt(px((1. - delta) * RISE_IN)),
    )
}

fn turn_card(prompt: &SharedString, reply: &SharedString, cx: &App) -> gpui_kit::AnyElement {
    v_flex()
        .w(px(TURN_CARD_WIDTH))
        .gap_1()
        .child(
            div()
                .text_sm()
                .font_semibold()
                .truncate()
                .child(prompt.clone()),
        )
        .when(!reply.is_empty(), |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(reply.clone()),
            )
        })
        .into_any_element()
}

fn preview(text: &str, lines: usize) -> String {
    let mut out: String = text.lines().take(lines).collect::<Vec<_>>().join("\n");
    if text.lines().nth(lines).is_some() {
        out.push_str("\n…");
    }
    out
}

fn short_ago(now: u64, then: u64) -> String {
    let elapsed = now.saturating_sub(then);
    match elapsed {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", elapsed / 60),
        3_600..86_400 => format!("{}h", elapsed / 3_600),
        _ => format!("{}d", elapsed / 86_400),
    }
}

/// A `/btw` side question and its answer as it streams in.
struct Aside {
    question: String,
    answer: String,
    done: bool,
    error: Option<String>,
}

impl Aside {
    fn new(question: &str) -> Self {
        Self {
            question: question.to_owned(),
            answer: String::new(),
            done: false,
            error: None,
        }
    }
}

enum UpdateState {
    Idle,
    Available(Release),
    Installing,
}

/// How much the agent may do on its own. Plan is maki's plan mode, sent with
/// each prompt; Bypass is yolo, held by the session's permission manager.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ask,
    Plan,
    Bypass,
}

impl Mode {
    const ALL: [Self; 3] = [Self::Ask, Self::Plan, Self::Bypass];

    fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask before edits",
            Self::Plan => "Plan mode",
            Self::Bypass => "Bypass permissions",
        }
    }

    fn icon(self) -> Lucide {
        match self {
            Self::Ask => Lucide::Shield,
            Self::Plan => Lucide::ListChecks,
            Self::Bypass => Lucide::ShieldOff,
        }
    }
}

#[derive(Clone)]
enum PaletteAction {
    NewSession,
    OpenFolder,
    NewProject,
    Project(PathBuf),
    Palette,
    Settings,
    AttachImages,
    ToggleDark,
    Mode(Mode),
    Trust,
    Insert(String),
    Model(String),
    Thinking(StoredThinking),
    Lua(String),
}

#[derive(Clone)]
struct PaletteEntry {
    title: SharedString,
    detail: SharedString,
    action: PaletteAction,
}

impl PaletteEntry {
    fn new(title: impl Into<SharedString>, action: PaletteAction) -> Self {
        Self {
            title: title.into(),
            detail: SharedString::default(),
            action,
        }
    }

    fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = detail.into();
        self
    }
}

struct Palette {
    entries: Vec<PaletteEntry>,
    query: Entity<gpui_kit::component::input::InputState>,
    workspace: WeakEntity<WorkspaceView>,
    _subscription: Subscription,
}

impl Palette {
    fn new(
        entries: Vec<PaletteEntry>,
        initial: &str,
        workspace: WeakEntity<WorkspaceView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            gpui_kit::component::input::InputState::new(window, cx)
                .placeholder("Type a command…")
                .default_value(initial.to_owned())
        });
        query.read(cx).focus_handle(cx).focus(window, cx);
        let subscription =
            cx.subscribe_in(&query, window, |this, _, event, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => {
                    if let Some(entry) = this.matches(cx).into_iter().next() {
                        this.choose(entry.action, window, cx);
                    }
                }
                _ => {}
            });
        Self {
            entries,
            query,
            workspace,
            _subscription: subscription,
        }
    }

    fn matches(&self, cx: &App) -> Vec<PaletteEntry> {
        let query = self.query.read(cx).value().to_lowercase();
        self.entries
            .iter()
            .filter(|e| query.is_empty() || e.title.to_lowercase().contains(&query))
            .cloned()
            .collect()
    }

    fn choose(&mut self, action: PaletteAction, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let _ = self
            .workspace
            .update(cx, |workspace, cx| workspace.run(action, window, cx));
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let matches = self.matches(cx);
        v_flex()
            .gap_2()
            .child(gpui_kit::component::input::Input::new(&self.query).appearance(false))
            .child(
                div()
                    .id("palette-list")
                    .max_h(px(360.))
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .children(matches.into_iter().enumerate().map(|(i, entry)| {
                                let action = entry.action.clone();
                                h_flex()
                                    .id(("palette-entry", i))
                                    .justify_between()
                                    .gap_3()
                                    .px_2()
                                    .py_1p5()
                                    .rounded(cx.theme().radius)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(cx.theme().accent))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.choose(action.clone(), window, cx)
                                    }))
                                    .child(div().text_sm().child(entry.title))
                                    .child(
                                        div()
                                            .text_xs()
                                            .truncate()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(entry.detail),
                                    )
                            })),
                    ),
            )
    }
}

/// The `question` tool's form: each question's options, plus a free answer.
struct QuestionForm {
    questions: Vec<Question>,
}

struct Question {
    text: String,
    options: Vec<String>,
    multiple: bool,
    picked: HashSet<usize>,
    other: Entity<gpui_kit::component::input::InputState>,
}

impl QuestionForm {
    fn new(input: &Value, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let questions = input["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|q| Question {
                text: q["question"].as_str().unwrap_or_default().to_owned(),
                options: q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o["label"].as_str().map(String::from))
                    .collect(),
                multiple: q["multiSelect"].as_bool().unwrap_or(false),
                picked: HashSet::new(),
                other: cx.new(|cx| {
                    gpui_kit::component::input::InputState::new(window, cx)
                        .placeholder("Or type your own answer")
                }),
            })
            .collect();
        Self { questions }
    }

    fn pick(&mut self, question: usize, option: usize, cx: &mut Context<Self>) {
        let q = &mut self.questions[question];
        if !q.multiple {
            q.picked.clear();
        }
        if !q.picked.remove(&option) {
            q.picked.insert(option);
        }
        cx.notify();
    }

    fn answer(&self, cx: &App) -> String {
        self.questions
            .iter()
            .map(|q| {
                let mut answers: Vec<String> = q
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| q.picked.contains(i))
                    .map(|(_, o)| o.clone())
                    .collect();
                let other = q.other.read(cx).value().trim().to_owned();
                if !other.is_empty() {
                    answers.push(other);
                }
                format!("Q: {}\nA: {}", q.text, answers.join(", "))
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

impl Render for QuestionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_4()
            .children(self.questions.iter().enumerate().map(|(qi, q)| {
                v_flex()
                    .gap_2()
                    .child(div().text_sm().font_medium().child(q.text.clone()))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .children(q.options.iter().enumerate().map(|(oi, option)| {
                                let selected = q.picked.contains(&oi);
                                Button::new(("option", qi * 1000 + oi))
                                    .small()
                                    .label(option.clone())
                                    .when(selected, |b| b.primary())
                                    .when(!selected, |b| b.outline())
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.pick(qi, oi, cx)),
                                    )
                            })),
                    )
                    .child(gpui_kit::component::input::Input::new(&q.other))
            }))
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("anthropic/claude-opus-5-5", "claude-opus-5-5" ; "provider_prefix")]
    #[test_case("local", "local" ; "bare")]
    fn short_model_drops_the_provider(spec: &str, expected: &str) {
        assert_eq!(short_model(spec), expected);
    }

    #[test_case(30, "now" ; "seconds")]
    #[test_case(120, "2m" ; "minutes")]
    #[test_case(2 * 86_400, "2d" ; "days")]
    fn ages_are_compact(elapsed: u64, expected: &str) {
        assert_eq!(short_ago(1_000_000, 1_000_000 - elapsed), expected);
    }

    #[test_case(999, "999" ; "small")]
    #[test_case(12_345, "12k" ; "thousands")]
    #[test_case(1_000_000, "1M" ; "round_million")]
    #[test_case(1_500_000, "1.5M" ; "fractional_million")]
    fn token_counts_are_compact(tokens: u32, expected: &str) {
        assert_eq!(compact_tokens(tokens), expected);
    }

    #[test_case(5 * 60, Some("Resets in 5m") ; "minutes")]
    #[test_case(2 * 3_600 + 50 * 60, Some("Resets in 2h 50m") ; "hours")]
    #[test_case(3 * 86_400 + 4 * 3_600, Some("Resets in 3d 4h") ; "days")]
    fn reset_times_are_relative(left: u64, expected: Option<&str>) {
        let now = 1_000_000;
        assert_eq!(resets_in(now, (now + left) * 1_000).as_deref(), expected);
    }

    #[test]
    fn turns_pair_each_prompt_with_its_first_reply() {
        let user = |text: &str| Item::User {
            text: text.into(),
            images: Vec::new(),
        };
        let items = [
            user(
                "fix the bug
in detail",
            ),
            Item::Assistant("Done.".into()),
            Item::Assistant("Anything else?".into()),
            user("thanks"),
        ];
        let turns = turns(&items);
        assert_eq!(
            turns,
            [
                Turn {
                    index: 0,
                    reply: Some(1)
                },
                Turn {
                    index: 3,
                    reply: None
                }
            ]
        );
        let (prompt, reply) = turns[0].texts(&items);
        assert_eq!((prompt.as_ref(), reply.as_ref()), ("fix the bug", "Done."));
    }

    #[test]
    fn past_reset_is_hidden() {
        assert_eq!(resets_in(1_000_000, 1_000), None);
    }
}
