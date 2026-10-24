//! The Customize page: the skills a session in the open project can load, and
//! the plugins switched on or off for every session the window opens. Skills
//! are found where the bundled `skill` plugin looks, in the same order, so the
//! list matches what the agent is offered.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, Context, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use serde::Deserialize;

use crate::engine::{Engine, PluginInfo, PluginKind};
use crate::scroller::{Scroller, SmoothScroll as _};
use crate::settings::Settings;
use crate::settings_page::{card, divider, heading};

const SKILL_FILE: &str = "SKILL.md";
const FENCE: &str = "---";
const CONFIG_SKILL_DIR: &str = "skills";
const GIT_DIR: &str = ".git";
const PROJECT_SKILL_DIRS: [&str; 4] = [
    ".maki/skills",
    ".claude/skills",
    ".opencode/skills",
    ".agents/skills",
];
const GLOBAL_SKILL_DIRS: [&str; 3] = [
    ".claude/skills",
    ".config/opencode/skills",
    ".agents/skills",
];
const PAGE_WIDTH: f32 = 680.;
const NO_SKILLS: &str = "No skills yet. Put a folder with a SKILL.md in .claude/skills, \
                         .maki/skills or .agents/skills, here or in your home folder.";
const PLUGIN_GROUPS: [(PluginKind, &str); 4] = [
    (PluginKind::Tool, "Tools"),
    (PluginKind::Provider, "Providers"),
    (PluginKind::Optional, "Optional"),
    (PluginKind::Package, "Installed packages"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Project,
    Global,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Global => "Global",
        }
    }
}

#[derive(Debug, PartialEq)]
struct Skill {
    name: String,
    description: String,
    path: PathBuf,
    scope: Scope,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// A plugin was switched; the open session reopens to pick it up.
pub struct PluginsChanged;

pub struct CustomizeView {
    gui_dir: Option<PathBuf>,
    skills: Vec<Skill>,
    plugins: Vec<PluginInfo>,
    switches: BTreeMap<String, bool>,
    /// Skills whose whole description is showing, by index.
    unfolded: HashSet<usize>,
    scroller: Scroller,
}

impl EventEmitter<PluginsChanged> for CustomizeView {}

impl CustomizeView {
    pub fn new(engine: &dyn Engine, gui_dir: Option<PathBuf>, project: Option<&Path>) -> Self {
        let switches = Settings::load(gui_dir.as_deref()).plugins;
        Self {
            gui_dir,
            skills: discover_skills(&skill_roots(project)),
            plugins: engine.plugins(),
            switches,
            unfolded: HashSet::new(),
            scroller: Scroller::area(),
        }
    }

    fn enabled(&self, plugin: &PluginInfo) -> bool {
        self.switches
            .get(&plugin.name)
            .copied()
            .unwrap_or_else(|| plugin.kind.on_by_default())
    }

    /// A switch back to the default is forgotten, so the config decides again.
    fn switch(&mut self, name: String, kind: PluginKind, enabled: bool, cx: &mut Context<Self>) {
        let mut settings = Settings::load(self.gui_dir.as_deref());
        match enabled == kind.on_by_default() {
            true => settings.plugins.remove(&name),
            false => settings.plugins.insert(name, enabled),
        };
        settings.save(self.gui_dir.as_deref());
        self.switches = settings.plugins;
        cx.emit(PluginsChanged);
        cx.notify();
    }

    fn toggle_skill(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.unfolded.remove(&index) {
            self.unfolded.insert(index);
        }
        cx.notify();
    }

    /// One line of description until the row is clicked open.
    fn render_skills(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.skills.iter().enumerate().map(|(index, skill)| {
            let path = skill.path.clone();
            let open = self.unfolded.contains(&index);
            let description = div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(skill.description.clone());
            v_flex()
                .when(index > 0, |this| this.child(divider(cx)))
                .child(
                    v_flex()
                        .id(("skill", index))
                        .px_4()
                        .py_2p5()
                        .gap_0p5()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().muted))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_skill(index, cx)))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Icon::new(match open {
                                        true => Lucide::ChevronDown,
                                        false => Lucide::ChevronRight,
                                    })
                                    .xsmall()
                                    .text_color(cx.theme().muted_foreground),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_sm()
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .child(skill.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(skill.scope.label()),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "reveal-skill-{index}"
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(Lucide::FolderOpen))
                                    .tooltip("Show in folder")
                                    .on_click(
                                        move |_, _, cx| {
                                            cx.stop_propagation();
                                            cx.reveal_path(&path)
                                        },
                                    ),
                                ),
                        )
                        .when(!skill.description.is_empty(), |this| {
                            this.child(div().pl_5().child(match open {
                                true => description,
                                false => description.truncate(),
                            }))
                        }),
                )
        });
        v_flex()
            .gap_2()
            .child(group_label("Skills", cx))
            .child(match self.skills.is_empty() {
                true => card(cx)
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(NO_SKILLS)
                    .into_any_element(),
                false => card(cx).children(rows).into_any_element(),
            })
    }

    fn render_plugins(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = PLUGIN_GROUPS.into_iter().filter_map(|(kind, title)| {
            let members: Vec<&PluginInfo> =
                self.plugins.iter().filter(|p| p.kind == kind).collect();
            if members.is_empty() {
                return None;
            }
            let rows = members.into_iter().enumerate().map(|(index, plugin)| {
                let name = plugin.name.clone();
                h_flex()
                    .when(index > 0, |this| {
                        this.border_t_1().border_color(cx.theme().border)
                    })
                    .px_4()
                    .py_2p5()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .child(plugin.name.clone()),
                    )
                    .child(
                        Switch::new(SharedString::from(format!("plugin-{}", plugin.name)))
                            .checked(self.enabled(plugin))
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.switch(name.clone(), kind, *checked, cx)
                            })),
                    )
            });
            Some(
                v_flex()
                    .gap_2()
                    .child(group_label(title, cx))
                    .child(card(cx).children(rows)),
            )
        });
        v_flex().gap_6().children(groups)
    }
}

impl Render for CustomizeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("customize")
            .flex_1()
            .min_h_0()
            .smooth_scroll(&self.scroller, cx)
            .child(
                v_flex()
                    .w(px(PAGE_WIDTH))
                    .max_w_full()
                    .mx_auto()
                    .px_6()
                    .py_8()
                    .gap_8()
                    .child(heading(
                        "Customize",
                        "Skills the agent can load in this project, and the plugins every \
                         session runs with. A switch reopens the open session to apply it.",
                        cx,
                    ))
                    .child(self.render_skills(cx))
                    .child(self.render_plugins(cx)),
            )
    }
}

fn group_label(text: &str, cx: &App) -> impl IntoElement {
    div()
        .text_sm()
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(text.to_owned())
}

/// Where skills live, in the order the `skill` plugin reads them: a later
/// root's skill replaces an earlier one of the same name.
fn skill_roots(project: Option<&Path>) -> Vec<(PathBuf, Scope)> {
    let config = maki_storage::paths::config_dir()
        .ok()
        .map(|dir| dir.join(CONFIG_SKILL_DIR));
    let home = maki_storage::paths::home()
        .into_iter()
        .flat_map(|home| GLOBAL_SKILL_DIRS.iter().map(move |rel| home.join(rel)));
    let global: Vec<PathBuf> = config.into_iter().chain(home).collect();
    // Compared resolved, since the project path may carry the verbatim prefix
    // the home folder's does not.
    let seen: Vec<PathBuf> = global
        .iter()
        .filter_map(|dir| fs::canonicalize(dir).ok())
        .collect();
    let project: Vec<PathBuf> = project
        .map(project_ancestors)
        .unwrap_or_default()
        .into_iter()
        .flat_map(|dir| PROJECT_SKILL_DIRS.iter().map(move |rel| dir.join(rel)))
        .filter(|dir| fs::canonicalize(dir).is_ok_and(|dir| !seen.contains(&dir)))
        .collect();
    let global = global.into_iter().map(|dir| (dir, Scope::Global));
    global
        .chain(project.into_iter().map(|dir| (dir, Scope::Project)))
        .collect()
}

/// The project folder and its parents up to the repository root; just the
/// folder when it is the root or there is none.
fn project_ancestors(project: &Path) -> Vec<PathBuf> {
    if project.join(GIT_DIR).exists() {
        return vec![project.to_path_buf()];
    }
    let mut dirs = Vec::new();
    for dir in project.ancestors() {
        dirs.push(dir.to_path_buf());
        if dir != project && dir.join(GIT_DIR).exists() {
            break;
        }
    }
    dirs
}

fn discover_skills(roots: &[(PathBuf, Scope)]) -> Vec<Skill> {
    let mut skills = BTreeMap::new();
    for (root, scope) in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let path = entry.path().join(SKILL_FILE);
            let Some((frontmatter, body)) = fs::read_to_string(&path)
                .ok()
                .and_then(|content| split_frontmatter(&content))
            else {
                continue;
            };
            if body.is_empty() {
                continue;
            }
            let name = frontmatter
                .name
                .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
            skills.insert(
                name.clone(),
                Skill {
                    name,
                    description: frontmatter.description.unwrap_or_default(),
                    path,
                    scope: *scope,
                },
            );
        }
    }
    skills.into_values().collect()
}

/// The frontmatter and trimmed body of a `SKILL.md`, read as the `skill`
/// plugin reads it: no fence, or a broken one, leaves the whole file as body.
fn split_frontmatter(content: &str) -> Option<(Frontmatter, String)> {
    let Some(rest) = content.trim_start().strip_prefix(FENCE).and_then(|rest| {
        rest.strip_prefix('\n')
            .or_else(|| rest.strip_prefix("\r\n"))
    }) else {
        return Some((Frontmatter::default(), content.trim().to_owned()));
    };
    let Some(end) = rest.find(&format!("\n{FENCE}")) else {
        return Some((Frontmatter::default(), content.trim().to_owned()));
    };
    let frontmatter = serde_yaml::from_str(&rest[..end]).unwrap_or_default();
    let body = rest[end + 1 + FENCE.len()..].trim().to_owned();
    Some((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const BODY: &str = "Do the thing.";
    const DESCRIPTION: &str = "Explains things";
    const DIR_NAME: &str = "explain";
    const NAMED: &str = "explainer";

    #[test_case("---\nname: explainer\ndescription: Explains things\n---\nDo the thing.\n", Some(NAMED), DESCRIPTION, BODY ; "full_frontmatter")]
    #[test_case("---\r\ndescription: \"Explains things\"\r\n---\r\nDo the thing.", None, DESCRIPTION, BODY ; "crlf_and_quotes")]
    #[test_case("Do the thing.\n", None, "", BODY ; "no_frontmatter")]
    #[test_case("---\nname: explainer\nDo the thing.", None, "", "---\nname: explainer\nDo the thing." ; "unclosed_fence")]
    fn reads_frontmatter(content: &str, name: Option<&str>, description: &str, body: &str) {
        let (frontmatter, parsed_body) = split_frontmatter(content).unwrap();
        assert_eq!(frontmatter.name.as_deref(), name);
        assert_eq!(frontmatter.description.unwrap_or_default(), description);
        assert_eq!(parsed_body, body);
    }

    fn write_skill(root: &Path, dir: &str, content: &str) -> PathBuf {
        let skill_dir = root.join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join(SKILL_FILE);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn a_later_root_replaces_a_skill_of_the_same_name() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill(global.path(), DIR_NAME, BODY);
        let local = write_skill(project.path(), DIR_NAME, BODY);
        let skills = discover_skills(&[
            (global.path().to_path_buf(), Scope::Global),
            (project.path().to_path_buf(), Scope::Project),
        ]);
        assert_eq!(
            skills,
            vec![Skill {
                name: DIR_NAME.into(),
                description: String::new(),
                path: local,
                scope: Scope::Project,
            }]
        );
    }

    #[test]
    fn skips_skills_without_a_body() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), DIR_NAME, "---\nname: explainer\n---\n");
        assert!(discover_skills(&[(root.path().to_path_buf(), Scope::Global)]).is_empty());
    }

    #[test]
    fn walks_up_to_the_repository_root() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir(repo.path().join(GIT_DIR)).unwrap();
        let nested = repo.path().join(DIR_NAME);
        fs::create_dir(&nested).unwrap();
        assert_eq!(
            project_ancestors(&nested),
            vec![nested.clone(), repo.path().to_path_buf()]
        );
        assert_eq!(
            project_ancestors(repo.path()),
            vec![repo.path().to_path_buf()]
        );
    }
}
