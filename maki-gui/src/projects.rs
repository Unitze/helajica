//! The projects in the sidebar: folders the user opened or created,
//! most recent first, persisted next to the rest of the GUI config.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

const PROJECTS_FILE: &str = "projects.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub path: PathBuf,
    pub name: String,
    pub last_opened: u64,
}

impl Project {
    pub fn new(path: PathBuf, now: u64) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            path,
            name,
            last_opened: now,
        }
    }
}

pub struct ProjectStore {
    file: Option<PathBuf>,
    projects: Vec<Project>,
}

impl ProjectStore {
    pub fn load(dir: Option<&Path>) -> Self {
        let file = dir.map(|d| d.join(PROJECTS_FILE));
        let projects = file
            .as_deref()
            .and_then(|f| fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { file, projects }
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    /// Moves `path` to the front, adding it if new.
    pub fn touch(&mut self, path: &Path, now: u64) {
        self.projects.retain(|p| p.path != path);
        self.projects
            .insert(0, Project::new(path.to_path_buf(), now));
        self.save();
    }

    pub fn remove(&mut self, path: &Path) {
        self.projects.retain(|p| p.path != path);
        self.save();
    }

    fn save(&self) {
        let Some(file) = &self.file else { return };
        let written = file
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(file, serde_json::to_vec_pretty(&self.projects)?));
        if let Err(e) = written {
            warn!(file = %file.display(), error = %e, "save project list");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "alpha";
    const SECOND: &str = "beta";

    #[test]
    fn touch_moves_to_front_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ProjectStore::load(Some(dir.path()));
        store.touch(&dir.path().join(FIRST), 1);
        store.touch(&dir.path().join(SECOND), 2);
        store.touch(&dir.path().join(FIRST), 3);

        let reloaded = ProjectStore::load(Some(dir.path()));
        let names: Vec<_> = reloaded
            .projects()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, [FIRST, SECOND]);
        assert_eq!(reloaded.projects()[0].last_opened, 3);
    }
}
