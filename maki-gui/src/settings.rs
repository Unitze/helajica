use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub dark: bool,
    /// A theme from the bundled set or the user's `themes/` dir, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Add-ons switched on or off, by id. One missing here takes its default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub addons: BTreeMap<String, bool>,
    /// Plugins switched on or off in Customize, by name. These win over the
    /// config; one missing here follows it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, bool>,
    /// The thinking level picked last, as maki's `/thinking` takes it: what a
    /// model gets until one is picked for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// The thinking level picked for each model, by model spec.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_thinking: BTreeMap<String, String>,
}

impl Settings {
    pub fn load(dir: Option<&Path>) -> Self {
        dir.and_then(|d| fs::read(d.join(SETTINGS_FILE)).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Records a plugin switch and returns them all. One set back to the
    /// plugin's default is forgotten, so the config decides again.
    pub fn switch_plugin(
        dir: Option<&Path>,
        name: &str,
        default: bool,
        enabled: bool,
    ) -> BTreeMap<String, bool> {
        let mut settings = Self::load(dir);
        match enabled == default {
            true => settings.plugins.remove(name),
            false => settings.plugins.insert(name.to_owned(), enabled),
        };
        settings.save(dir);
        settings.plugins
    }

    /// The thinking level for `model`: the one picked for it, else the last
    /// one picked.
    pub fn thinking_for(&self, model: &str) -> Option<&str> {
        self.model_thinking
            .get(model)
            .or(self.thinking.as_ref())
            .map(String::as_str)
    }

    pub fn save(&self, dir: Option<&Path>) {
        let Some(dir) = dir else { return };
        let file: PathBuf = dir.join(SETTINGS_FILE);
        let written = fs::create_dir_all(dir)
            .and_then(|()| fs::write(&file, serde_json::to_vec_pretty(self)?));
        if let Err(e) = written {
            warn!(file = %file.display(), error = %e, "save gui settings");
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const THEME: &str = "Ayu Light";
    const ADDON: &str = "openai-oauth";
    const THINKING: &str = "high";
    const PLUGIN: &str = "websearch";
    const MODEL: &str = "openai/gpt-6-luna";
    const OTHER_MODEL: &str = "anthropic/claude-opus-5-5";
    const MODEL_THINKING: &str = "low";

    #[test_case(false, Some(false) ; "off_is_remembered")]
    #[test_case(true, None ; "back_to_default_is_forgotten")]
    fn switching_a_plugin(enabled: bool, expected: Option<bool>) {
        let dir = tempfile::tempdir().unwrap();
        Settings::switch_plugin(Some(dir.path()), PLUGIN, true, false);
        let switches = Settings::switch_plugin(Some(dir.path()), PLUGIN, true, enabled);
        assert_eq!(switches.get(PLUGIN).copied(), expected);
    }

    #[test_case(MODEL, Some(MODEL_THINKING) ; "picked_for_the_model")]
    #[test_case(OTHER_MODEL, Some(THINKING) ; "falls_back_to_the_last_pick")]
    fn thinking_follows_the_model(model: &str, expected: Option<&str>) {
        let settings = Settings {
            thinking: Some(THINKING.into()),
            model_thinking: BTreeMap::from([(MODEL.into(), MODEL_THINKING.into())]),
            ..Settings::default()
        };
        assert_eq!(settings.thinking_for(model), expected);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            dark: true,
            theme: Some(THEME.into()),
            addons: BTreeMap::from([(ADDON.into(), true)]),
            plugins: BTreeMap::from([(PLUGIN.into(), false)]),
            thinking: Some(THINKING.into()),
            model_thinking: BTreeMap::from([(MODEL.into(), MODEL_THINKING.into())]),
        };
        settings.save(Some(dir.path()));
        assert_eq!(Settings::load(Some(dir.path())), settings);
    }
}
