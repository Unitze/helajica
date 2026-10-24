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
}

impl Settings {
    pub fn load(dir: Option<&Path>) -> Self {
        dir.and_then(|d| fs::read(d.join(SETTINGS_FILE)).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
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
    use super::*;

    const THEME: &str = "Ayu Light";

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            dark: true,
            theme: Some(THEME.into()),
        };
        settings.save(Some(dir.path()));
        assert_eq!(Settings::load(Some(dir.path())), settings);
    }
}
