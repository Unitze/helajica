//! Native desktop app for maki, built on GPUI.
//!
//! The core is one view over [`Engine`]: projects and sessions in a sidebar,
//! the transcript and composer beside them. Everything else plugs in from outside: Lua
//! plugins draw panels, status items and palette commands through `maki.gui`
//! (see [`ext`]), and themes are JSON files in the user's theme directory.

mod app;
pub mod engine;
mod ext;
mod new_project;
mod nodes;
mod projects;
mod settings;
mod titlebar;
mod transcript;
mod update;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};

pub use engine::{Boot, Command, Engine, Session, SlashCommand, Update};

const WINDOW_WIDTH: f32 = 1240.;
const WINDOW_HEIGHT: f32 = 820.;

/// Opens the app and blocks until it quits. `gui_dir` holds the project list,
/// settings and themes; `None` keeps everything in memory. `project` opens
/// that folder instead of the most recent one.
pub fn run(engine: Arc<dyn Engine>, gui_dir: Option<PathBuf>, project: Option<PathBuf>) {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            app::init(gui_dir.as_deref(), cx);
            let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| workspace::WorkspaceView::new(engine, gui_dir, project, window, cx))
            });
            if let Err(e) = opened {
                tracing::error!(error = %e, "open main window");
                cx.quit();
            }
        });
}
