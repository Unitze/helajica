//! The one window: projects and their sessions down the side, the
//! conversation in the middle, the composer under it, and whatever Lua
//! plugins add around them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use flume::Sender;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Anchor, App, AppContext as _, AsyncWindowContext, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity,
    Window, actions, div, prelude::FluentBuilder as _, px,
};
use maki_agent::AgentEvent;
use maki_agent::permissions::PermissionAnswer;
use maki_storage::now_epoch;
use maki_storage::sessions::SessionSummary;
use serde_json::Value;

use crate::app::{apply_theme_message, toggle_dark};
use crate::engine::{Boot, Command, Engine, Update};
use crate::ext::{COMMAND_TOPIC_PREFIX, Effect, Extensions, NotifyLevel};
use crate::new_project::{self, OnProject};
use crate::nodes;
use crate::projects::ProjectStore;
use crate::titlebar::{PaletteHandler, title_bar};
use crate::transcript::{Change, Item, Level, ToolCall, ToolStatus, Transcript};
use crate::update::{self, Release};

actions!(maki_gui, [OpenPalette, NewSession, OpenFolder]);

const KEY_CONTEXT: &str = "Workspace";
const SIDEBAR_WIDTH: f32 = 264.;
const TRANSCRIPT_WIDTH: f32 = 820.;
const TOOL_PREVIEW_LINES: usize = 12;
const THINKING_PREVIEW_LINES: usize = 3;
const FOLLOW_TAIL_SLACK: f32 = 64.;
const SESSIONS_PER_PROJECT: usize = 30;
const CONTEXT_RING_SIZE: f32 = 14.;
const MENU_WIDTH: f32 = 240.;
const MENU_HEIGHT: f32 = 360.;
const UNTITLED: &str = "Untitled session";
const COMPOSER_PLACEHOLDER: &str = "Enter to send, Shift+Enter for a newline";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-k", OpenPalette, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-n", NewSession, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-o", OpenFolder, Some(KEY_CONTEXT)),
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
    mode: Mode,
    update: UpdateState,
    expanded: HashSet<String>,
    scroll: ScrollHandle,
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
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            );
        let store = ProjectStore::load(gui_dir.as_deref());
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
            mode: Mode::Ask,
            update: UpdateState::Idle,
            expanded: HashSet::new(),
            scroll: ScrollHandle::new(),
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
        self.ext = Extensions::default();
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
                for warning in &session.boot.warnings {
                    this.transcript.notice(warning.clone(), Level::Warning);
                }
                this.refresh_sessions(&project);
                if session.boot.yolo {
                    this.mode = Mode::Bypass;
                } else if this.mode == Mode::Bypass {
                    let _ = session.commands.send(Command::Yolo(true));
                }
                this.phase = Phase::Ready {
                    boot: session.boot,
                    commands: session.commands,
                };
                this.scroll.scroll_to_bottom();
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
        })
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
        let following = self.following_tail();
        if self.transcript.apply(envelope) != Change::None {
            if following {
                self.scroll.scroll_to_bottom();
            }
            cx.notify();
        }
        if matches!(envelope.event, AgentEvent::Done { .. })
            && envelope.subagent.is_none()
            && let Some(project) = self.project.clone()
        {
            self.refresh_sessions(&project);
        }
    }

    fn following_tail(&self) -> bool {
        let offset = -self.scroll.offset().y;
        offset + px(FOLLOW_TAIL_SLACK) >= self.scroll.max_offset().y
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
        if text.is_empty() || self.commands().is_none() {
            return;
        }
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.transcript.push_user(text.clone());
        self.send(Command::Prompt {
            text,
            plan: self.mode == Mode::Plan,
        });
        self.scroll.scroll_to_bottom();
        cx.notify();
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
        let entries = self.palette_entries();
        let workspace = cx.entity().downgrade();
        let palette = cx.new(|cx| Palette::new(entries, workspace, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.w(px(560.)).p_0().child(palette.clone())
        });
    }

    fn palette_entries(&self) -> Vec<PaletteEntry> {
        let mut entries = vec![
            PaletteEntry::new("New session", PaletteAction::NewSession),
            PaletteEntry::new("Open folder…", PaletteAction::OpenFolder),
            PaletteEntry::new("New project…", PaletteAction::NewProject),
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
                self.send(Command::Model(spec));
            }
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
            rows.push(
                h_flex()
                    .id(SharedString::from(format!("session-{id}")))
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

    fn render_item(
        &self,
        index: usize,
        item: &Item,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        match item {
            Item::User(text) => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(px(TRANSCRIPT_WIDTH * 0.8))
                        .px_4()
                        .py_2p5()
                        .rounded(cx.theme().radius_lg)
                        .bg(cx.theme().secondary)
                        .text_sm()
                        .child(text.clone()),
                )
                .into_any_element(),
            Item::Assistant(text) => {
                TextView::markdown(SharedString::from(format!("item-{index}")), text.clone())
                    .text_sm()
                    .into_any_element()
            }
            Item::Thinking(text) => {
                let key = format!("thinking-{index}");
                let open = self.expanded.contains(&key);
                div()
                    .id(SharedString::from(key.clone()))
                    .pl_3()
                    .border_l_2()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .italic()
                    .text_color(cx.theme().muted_foreground)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(&key, cx)))
                    .child(match open {
                        true => text.clone(),
                        false => preview(text, THINKING_PREVIEW_LINES),
                    })
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
                    .child(text.clone())
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
                let items: Vec<_> = self
                    .transcript
                    .items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| self.render_item(i, item, cx))
                    .collect();
                let empty = items.is_empty();
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
                        this.child(
                            div()
                                .id("transcript")
                                .flex_1()
                                .overflow_y_scroll()
                                .track_scroll(&self.scroll)
                                .child(
                                    v_flex()
                                        .w(px(TRANSCRIPT_WIDTH))
                                        .max_w_full()
                                        .mx_auto()
                                        .px_6()
                                        .py_6()
                                        .gap_4()
                                        .when(!boot.trusted, |this| this.child(trust_banner(cx)))
                                        .children(items),
                                ),
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
        let empty = self.composer.read(cx).value().trim().is_empty();
        div().px_6().pb_3().child(
            v_flex()
                .w(px(TRANSCRIPT_WIDTH))
                .max_w_full()
                .mx_auto()
                .gap_1p5()
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .py_1p5()
                        .rounded(px(16.))
                        .border_1()
                        .border_color(cx.theme().input)
                        .bg(cx.theme().background)
                        .shadow_sm()
                        .child(self.render_add_menu(boot, cx))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Textarea::new(&self.composer).bordered(false)),
                        )
                        .child(match working {
                            true => Button::new("stop")
                                .primary()
                                .small()
                                .rounded(px(999.))
                                .icon(Icon::new(Lucide::Square))
                                .tooltip("Stop")
                                .on_click(cx.listener(|this, _, _, _| this.send(Command::Cancel))),
                            false => Button::new("send")
                                .primary()
                                .small()
                                .rounded(px(999.))
                                .icon(Icon::new(Lucide::ArrowUp))
                                .tooltip("Send")
                                .disabled(empty)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                ),
                        }),
                )
                .child(self.render_session_bar(boot, cx)),
        )
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
                    .item(action_item(
                        &workspace,
                        "All commands…",
                        PaletteAction::Palette,
                    ))
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
        let totals = &self.transcript.totals;
        let percent = match totals.context_window {
            0 => 0.,
            window => (u64::from(totals.context_size) * 100) as f32 / window as f32,
        };
        let tokens = format!(
            "{:.0}% of context · ↑{} ↓{}",
            percent,
            totals.usage.input + totals.usage.cache_read + totals.usage.cache_creation,
            totals.usage.output
        );
        let workspace = cx.entity().downgrade();
        let mode_workspace = workspace.clone();
        let mode = self.mode;
        let models = boot.models.clone();
        let current = boot.model.clone();
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
            .children(totals.cost.map(|cost| format!("${cost:.2}")))
            .child(
                Button::new("model")
                    .ghost()
                    .xsmall()
                    .label(short_model(&boot.model))
                    .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                        let mut menu = menu
                            .min_w(px(MENU_WIDTH))
                            .max_h(px(MENU_HEIGHT))
                            .scrollable(true);
                        for spec in &models {
                            menu = menu.item(
                                action_item(
                                    &workspace,
                                    spec.clone(),
                                    PaletteAction::Model(spec.clone()),
                                )
                                .checked(*spec == current),
                            );
                        }
                        menu
                    }),
            )
            .child(
                Button::new("context")
                    .ghost()
                    .xsmall()
                    .tooltip(tokens)
                    .child(
                        ProgressCircle::new("context-ring")
                            .size(px(CONTEXT_RING_SIZE))
                            .color(cx.theme().muted_foreground)
                            .value(percent),
                    ),
            )
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(|this, _: &NewSession, window, cx| this.new_session(window, cx)))
            .on_action(cx.listener(|this, _: &OpenFolder, window, cx| this.open_folder(window, cx)))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child({
                let workspace = cx.entity().downgrade();
                let palette: PaletteHandler = Rc::new(move |window, cx| {
                    let _ = workspace
                        .update(cx, |this, cx| this.open_palette(&OpenPalette, window, cx));
                });
                let crumbs = self
                    .project_name()
                    .map(SharedString::from)
                    .into_iter()
                    .collect();
                let notice = self.render_update(cx);
                title_bar(crumbs, self.gui_dir.clone(), Some(palette), notice, cx)
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
                .child("Describe a task to start. Press Ctrl+K for commands."),
        )
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
    ToggleDark,
    Mode(Mode),
    Trust,
    Insert(String),
    Model(String),
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
        workspace: WeakEntity<WorkspaceView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            gpui_kit::component::input::InputState::new(window, cx).placeholder("Type a command…")
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
}
