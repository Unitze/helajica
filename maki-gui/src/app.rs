use std::path::Path;
use std::rc::Rc;

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeConfig, ThemeMode, ThemeRegistry};
use gpui_kit::{App, SharedString, Window};
use serde_json::Value;
use tracing::warn;

use crate::settings::Settings;

const THEMES_DIR: &str = "themes";

pub fn init(gui_dir: Option<&Path>, cx: &mut App) {
    crate::workspace::bind_keys(cx);
    let settings = Settings::load(gui_dir);
    set_dark(settings.dark, None, cx);
    let Some(dir) = gui_dir else { return };
    let themes = dir.join(THEMES_DIR);
    if let Err(e) = std::fs::create_dir_all(&themes) {
        return warn!(error = %e, "create themes dir");
    }
    let wanted = settings.theme.map(SharedString::from);
    let watched = ThemeRegistry::watch_dir(themes, cx, move |cx| {
        if let Some(name) = &wanted {
            apply_named_theme(name, cx);
        }
    });
    if let Err(e) = watched {
        warn!(error = %e, "watch themes dir");
    }
}

pub fn set_dark(dark: bool, window: Option<&mut Window>, cx: &mut App) {
    let mode = match dark {
        true => ThemeMode::Dark,
        false => ThemeMode::Light,
    };
    Theme::change(mode, window, cx);
}

/// Flips light and dark, and remembers the choice for the next launch.
pub fn toggle_dark(gui_dir: Option<&Path>, window: &mut Window, cx: &mut App) {
    let mut settings = Settings::load(gui_dir);
    settings.dark = !cx.theme().is_dark();
    set_dark(settings.dark, Some(window), cx);
    settings.save(gui_dir);
    window.refresh();
}

fn apply_named_theme(name: &SharedString, cx: &mut App) -> bool {
    let Some(config) = ThemeRegistry::global(cx).themes().get(name).cloned() else {
        return false;
    };
    let mode = config.mode;
    Theme::update(cx, |theme| theme.apply_config(&config));
    Theme::change(mode, None, cx);
    true
}

/// A plugin's `ui.theme` message: `{ name = "..." }` picks a registered theme,
/// `{ config = {...} }` applies a theme document inline, `{ dark = bool }`
/// flips the mode.
pub fn apply_theme_message(data: &Value, window: &mut Window, cx: &mut App) {
    if let Some(name) = data.get("name").and_then(Value::as_str)
        && !apply_named_theme(&SharedString::from(name.to_owned()), cx)
    {
        warn!(theme = name, "unknown theme");
    }
    if let Some(config) = data.get("config") {
        match serde_json::from_value::<ThemeConfig>(config.clone()) {
            Ok(config) => {
                let mode = config.mode;
                Theme::update(cx, |theme| theme.apply_config(&Rc::new(config)));
                Theme::change(mode, Some(window), cx);
            }
            Err(e) => warn!(error = %e, "malformed theme config"),
        }
    }
    if let Some(dark) = data.get("dark").and_then(Value::as_bool) {
        set_dark(dark, Some(window), cx);
    }
}
