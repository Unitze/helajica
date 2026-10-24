//! Adding a project: picking an existing folder, or creating a fresh one.

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, PathPromptOptions,
    Render, SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use maki_storage::process::hide_console;

const TITLE: &str = "New project";
const PICK_FOLDER_PROMPT: &str = "Open";
const DIALOG_WIDTH: f32 = 520.;
const GIT: &str = "git";

pub type OnProject = Rc<dyn Fn(PathBuf, &mut Window, &mut App)>;

/// Asks the OS for a folder and hands it to `done`.
pub fn pick_folder(done: OnProject, window: &mut Window, cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(PICK_FOLDER_PROMPT.into()),
    });
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = cx.update(|window, cx| done(path, window, cx));
        })
        .detach();
}

/// Opens the "New project" dialog; `done` gets the created folder.
pub fn open_dialog(parent: PathBuf, done: OnProject, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| NewProjectForm::new(parent, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        let form = form.clone();
        let done = done.clone();
        dialog
            .title(TITLE)
            .w(px(DIALOG_WIDTH))
            .child(form.clone())
            .footer(
                DialogFooter::new().children([
                    Button::new("cancel")
                        .label("Cancel")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                    Button::new("create").primary().label("Create").on_click(
                        move |_, window, cx| match form.update(cx, |form, cx| form.create(cx)) {
                            Ok(path) => {
                                window.close_dialog(cx);
                                done(path, window, cx);
                            }
                            Err(e) => form.update(cx, |form, cx| {
                                form.error = Some(e.into());
                                cx.notify();
                            }),
                        },
                    ),
                ]),
            )
    });
}

struct NewProjectForm {
    name: Entity<InputState>,
    parent: Entity<InputState>,
    git: bool,
    error: Option<SharedString>,
}

impl NewProjectForm {
    fn new(parent: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("my-project")),
            parent: cx
                .new(|cx| InputState::new(window, cx).default_value(parent.display().to_string())),
            git: true,
            error: None,
        }
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let parent = self.parent.clone();
        let done: OnProject = Rc::new(move |path, window, cx| {
            parent.update(cx, |state, cx| {
                state.set_value(path.display().to_string(), window, cx)
            });
        });
        pick_folder(done, window, cx);
    }

    fn create(&mut self, cx: &mut Context<Self>) -> Result<PathBuf, String> {
        let name = self.name.read(cx).value().trim().to_owned();
        let parent = PathBuf::from(self.parent.read(cx).value().trim());
        let path = new_project_path(&parent, &name)?;
        std::fs::create_dir_all(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        if self.git {
            // Best effort: a missing git still leaves a usable folder.
            let _ = hide_console(&mut Process::new(GIT))
                .arg("init")
                .current_dir(&path)
                .output();
        }
        Ok(path)
    }
}

impl Render for NewProjectForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(field("Name", Input::new(&self.name), cx))
            .child(field(
                "Location",
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.parent)))
                    .child(
                        Button::new("browse")
                            .outline()
                            .label("Browse…")
                            .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                    ),
                cx,
            ))
            .child(
                Checkbox::new("git-init")
                    .label("Initialize a git repository")
                    .checked(self.git)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.git = *checked;
                        cx.notify();
                    })),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
    }
}

fn field(
    label: &'static str,
    control: impl IntoElement,
    cx: &Context<NewProjectForm>,
) -> impl IntoElement {
    v_flex()
        .gap_1p5()
        .child(
            div()
                .text_sm()
                .font_medium()
                .text_color(cx.theme().foreground)
                .child(label),
        )
        .child(control)
}

fn new_project_path(parent: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err("Give the project a name".into());
    }
    if name.contains(['/', '\\']) {
        return Err("The name cannot contain path separators".into());
    }
    if !parent.is_dir() {
        return Err(format!("{} is not a folder", parent.display()));
    }
    let path = parent.join(name);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("", false ; "empty_name")]
    #[test_case("a/b", false ; "separator")]
    #[test_case("fresh", true ; "valid")]
    fn validates_new_project_names(name: &str, ok: bool) {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(new_project_path(dir.path(), name).is_ok(), ok);
    }
}
