//! Native desktop app for maki, built on GPUI.
//!
//! The core is one view over [`Engine`]: projects and sessions in a sidebar,
//! the transcript and composer beside them. Everything else plugs in from outside: Lua
//! plugins draw panels, status items and palette commands through `maki.gui`
//! (see [`ext`]), and themes are JSON files in the user's theme directory.

mod addons;
mod app;
mod attachments;
mod commands;
mod custom_providers;
mod customize;
pub mod engine;
mod ext;
mod goal;
mod media;
mod model_picker;
mod new_project;
mod nodes;
mod projects;
mod scroller;
mod settings;
mod settings_page;
mod titlebar;
mod transcript;
mod update;
mod workspace;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
use maki_config::PluginOverlay;

pub use engine::{
    Boot, BtwEvent, Command, Engine, PluginInfo, PluginKind, Session, SlashCommand, Update,
    UsageReport,
};

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
            addons::apply_all(
                &settings::Settings::load(gui_dir.as_deref()),
                maki_storage::StateDir::resolve().ok().as_ref(),
            );
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

/// What the window changed about plugins, which a session opened from it
/// applies over the config: the switches in Customize and Settings, and the
/// options the add-ons switched on set.
pub fn plugin_overlay(gui_dir: Option<&Path>) -> PluginOverlay {
    let settings = settings::Settings::load(gui_dir);
    let storage = maki_storage::StateDir::resolve().ok();
    let options = addons::Addon::ALL
        .into_iter()
        .filter(|addon| addon.enabled(&settings, storage.as_ref()))
        .filter_map(addons::Addon::plugin_options)
        .collect();
    PluginOverlay {
        switches: settings.plugins,
        options,
    }
}
