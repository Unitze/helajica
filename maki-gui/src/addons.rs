//! Optional pieces the core runs without, switched on in Settings › Add-ons.
//! Each one is applied at startup and again whenever it is toggled.

use maki_providers::openai_auth;
use maki_storage::StateDir;
use serde_json::{Map, Value};

use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addon {
    /// Sign in to OpenAI with a ChatGPT plan instead of an API key.
    ChatGptSignIn,
    /// Subagents that run while the agent keeps working and report back.
    BackgroundSubagents,
}

const TASK_PLUGIN: &str = "task";
const ALLOW_BACKGROUND: &str = "allow_background";

impl Addon {
    pub const ALL: [Self; 2] = [Self::ChatGptSignIn, Self::BackgroundSubagents];

    pub fn id(self) -> &'static str {
        match self {
            Self::ChatGptSignIn => "openai-oauth",
            Self::BackgroundSubagents => "background-subagents",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ChatGptSignIn => "ChatGPT sign-in for OpenAI",
            Self::BackgroundSubagents => "Background subagents",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::ChatGptSignIn => {
                "Use your ChatGPT plan for OpenAI models instead of an API key. \
                 Signing in opens your browser."
            }
            Self::BackgroundSubagents => {
                "Let the agent hand work to subagents that run while it keeps going, \
                 and pick up their results when they finish. Applies to the next session."
            }
        }
    }

    /// Off unless the user turned it on, except that someone already signed
    /// in keeps what they had.
    pub fn enabled(self, settings: &Settings, storage: Option<&StateDir>) -> bool {
        settings
            .addons
            .get(self.id())
            .copied()
            .unwrap_or_else(|| match self {
                Self::ChatGptSignIn => storage.is_some_and(openai_auth::signed_in),
                Self::BackgroundSubagents => false,
            })
    }

    pub fn apply(self, enabled: bool) {
        match self {
            Self::ChatGptSignIn => openai_auth::set_oauth_enabled(enabled),
            Self::BackgroundSubagents => {}
        }
    }

    /// The plugin options this add-on sets while on, laid over the config.
    pub fn plugin_options(self) -> Option<(String, Map<String, Value>)> {
        match self {
            Self::ChatGptSignIn => None,
            Self::BackgroundSubagents => Some((
                TASK_PLUGIN.to_owned(),
                Map::from_iter([(ALLOW_BACKGROUND.to_owned(), Value::Bool(true))]),
            )),
        }
    }
}

pub fn apply_all(settings: &Settings, storage: Option<&StateDir>) {
    for addon in Addon::ALL {
        addon.apply(addon.enabled(settings, storage));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn an_explicit_choice_wins() {
        let settings = Settings {
            addons: BTreeMap::from([(Addon::ChatGptSignIn.id().into(), true)]),
            ..Settings::default()
        };
        assert!(Addon::ChatGptSignIn.enabled(&settings, None));
    }

    #[test]
    fn off_by_default_without_a_sign_in() {
        assert!(!Addon::ChatGptSignIn.enabled(&Settings::default(), None));
    }
}
