//! The projects in the sidebar: folders the user opened or created,
//! in the order they were added unless the user drags them elsewhere,
//! persisted next to the rest of the GUI config.

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

    /// Records that `path` was opened, adding it at the end if new. Opening
    /// never moves a project: only [`Self::reorder`] does.
    pub fn touch(&mut self, path: &Path, now: u64) {
        match self.projects.iter_mut().find(|p| p.path == path) {
            Some(project) => project.last_opened = now,
            None => self.projects.push(Project::new(path.to_path_buf(), now)),
        }
        self.save();
    }

    /// The project opened most recently, which is where the app starts.
    pub fn last_opened(&self) -> Option<&Project> {
        self.projects
            .iter()
            .filter(|p| p.path.is_dir())
            .max_by_key(|p| p.last_opened)
    }

    pub fn reorder(&mut self, from: usize, to: usize) {
        if from == to || from >= self.projects.len() || to >= self.projects.len() {
            return;
        }
        let project = self.projects.remove(from);
        self.projects.insert(to, project);
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
    use test_case::test_case;

    use super::*;

    const NAMES: [&str; 3] = ["alpha", "beta", "gamma"];

    fn names(store: &ProjectStore) -> Vec<&str> {
        store.projects().iter().map(|p| p.name.as_str()).collect()
    }

    fn store_with_all(dir: &Path) -> ProjectStore {
        let mut store = ProjectStore::load(Some(dir));
        for (opened, name) in NAMES.iter().enumerate() {
            store.touch(&dir.join(name), opened as u64);
        }
        store
    }

    #[test]
    fn opening_keeps_the_order_projects_were_added_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_with_all(dir.path());
        store.touch(&dir.path().join(NAMES[0]), 10);

        let reloaded = ProjectStore::load(Some(dir.path()));
        assert_eq!(names(&reloaded), NAMES);
        assert_eq!(reloaded.projects()[0].last_opened, 10);
    }

    #[test_case(0, 2, ["beta", "gamma", "alpha"] ; "down_to_the_end")]
    #[test_case(2, 0, ["gamma", "alpha", "beta"] ; "up_to_the_top")]
    #[test_case(1, 1, NAMES ; "onto_itself")]
    #[test_case(0, 5, NAMES ; "out_of_range")]
    fn reorder_moves_one_project_and_persists(from: usize, to: usize, expected: [&str; 3]) {
        let dir = tempfile::tempdir().unwrap();
        store_with_all(dir.path()).reorder(from, to);
        assert_eq!(names(&ProjectStore::load(Some(dir.path()))), expected);
    }
}
