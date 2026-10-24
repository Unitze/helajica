//! The model menu by the composer: a search box over the models, grouped
//! under the provider that serves them, folded until opened or searched.

use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::PopoverState;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};

const PROVIDER_SEPARATOR: char = '/';
const LIST_HEIGHT: f32 = 360.;
pub const PICKER_WIDTH: f32 = 320.;

pub type OnPick = Rc<dyn Fn(String, &mut Window, &mut App)>;

pub struct ModelPicker {
    models: Vec<String>,
    current: String,
    /// Providers unfolded; the current model's starts out open.
    unfolded: HashSet<String>,
    query: Entity<InputState>,
    on_pick: OnPick,
    /// The popover showing this picker, closed once a model is picked.
    popover: Option<WeakEntity<PopoverState>>,
    _subscription: Subscription,
}

impl ModelPicker {
    pub fn new(
        models: Vec<String>,
        current: String,
        on_pick: OnPick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search models…"));
        let subscription =
            cx.subscribe_in(&query, window, |this, _, event, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => {
                    let first = grouped(&this.models, &this.query.read(cx).value())
                        .into_iter()
                        .find_map(|(_, specs)| specs.first().map(|s| (*s).to_owned()));
                    if let Some(spec) = first {
                        this.pick(spec, window, cx);
                    }
                }
                _ => {}
            });
        let unfolded = HashSet::from([provider_of(&current).to_owned()]);
        Self {
            models,
            current,
            unfolded,
            query,
            on_pick,
            popover: None,
            _subscription: subscription,
        }
    }

    pub fn search_focus(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }

    pub fn shown_in(&mut self, popover: WeakEntity<PopoverState>) {
        self.popover = Some(popover);
    }

    pub fn set_current(&mut self, spec: &str) {
        spec.clone_into(&mut self.current);
    }

    fn toggle(&mut self, provider: &str, cx: &mut Context<Self>) {
        if !self.unfolded.remove(provider) {
            self.unfolded.insert(provider.to_owned());
        }
        cx.notify();
    }

    fn pick(&mut self, spec: String, window: &mut Window, cx: &mut Context<Self>) {
        self.current.clone_from(&spec);
        self.query
            .update(cx, |query, cx| query.set_value("", window, cx));
        if let Some(popover) = self.popover.as_ref().and_then(WeakEntity::upgrade) {
            popover.update(cx, |popover, cx| popover.dismiss(window, cx));
        }
        (self.on_pick)(spec, window, cx);
    }
}

impl Render for ModelPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query.read(cx).value();
        let searching = !query.trim().is_empty();
        let groups = grouped(&self.models, &query);
        let empty = groups.is_empty();
        let rows = groups.into_iter().flat_map(|(provider, specs)| {
            let open = searching || self.unfolded.contains(provider);
            let toggled = provider.to_owned();
            let header = h_flex()
                .id(SharedString::from(format!("provider-{provider}")))
                .gap_1()
                .px_2()
                .py_1p5()
                .rounded(cx.theme().radius)
                .text_xs()
                .font_semibold()
                .text_color(cx.theme().muted_foreground)
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().accent))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(&toggled, cx)))
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall(),
                )
                .child(div().flex_1().child(provider.to_owned()))
                .child(specs.len().to_string())
                .into_any_element();
            let shown = if open { specs } else { Vec::new() };
            let models =
                shown.into_iter().map(|spec| {
                    let checked = spec == self.current;
                    let picked = spec.to_owned();
                    h_flex()
                        .id(SharedString::from(format!("model-{spec}")))
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .text_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().accent))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.pick(picked.clone(), window, cx)
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(model_name(spec).to_owned()),
                        )
                        .when(checked, |this| {
                            this.child(Icon::new(IconName::Check).small())
                        })
                        .into_any_element()
                });
            std::iter::once(header).chain(models).collect::<Vec<_>>()
        });
        v_flex()
            .w(px(PICKER_WIDTH))
            .gap_1()
            .child(Input::new(&self.query).small().cleanable(true))
            .child(
                div()
                    .id("model-list")
                    .max_h(px(LIST_HEIGHT))
                    .overflow_y_scrollbar()
                    .child(v_flex().children(rows))
                    .when(empty, |this| {
                        this.child(
                            div()
                                .p_2()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("No matching models"),
                        )
                    }),
            )
    }
}

fn provider_of(spec: &str) -> &str {
    spec.split_once(PROVIDER_SEPARATOR).map_or("", |(p, _)| p)
}

fn model_name(spec: &str) -> &str {
    spec.split_once(PROVIDER_SEPARATOR).map_or(spec, |(_, m)| m)
}

/// Models matching `query`, under their provider, providers in the order
/// they first appear.
fn grouped<'a>(models: &'a [String], query: &str) -> Vec<(&'a str, Vec<&'a str>)> {
    let query = query.trim().to_lowercase();
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for spec in models
        .iter()
        .filter(|spec| query.is_empty() || spec.to_lowercase().contains(&query))
    {
        let provider = provider_of(spec);
        match groups.iter_mut().find(|(p, _)| *p == provider) {
            Some((_, specs)) => specs.push(spec),
            None => groups.push((provider, vec![spec])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const MODELS: [&str; 4] = [
        "anthropic/claude-opus",
        "openai/gpt-5",
        "anthropic/claude-haiku",
        "openai/o3",
    ];

    #[test_case("", &[("anthropic", 2), ("openai", 2)] ; "everything_grouped_by_provider")]
    #[test_case("HAIKU", &[("anthropic", 1)] ; "search_ignores_case")]
    #[test_case("openai", &[("openai", 2)] ; "search_matches_the_provider")]
    #[test_case("nothing", &[] ; "no_match_no_groups")]
    fn models_are_grouped(query: &str, expected: &[(&str, usize)]) {
        let models: Vec<String> = MODELS.iter().map(|m| (*m).to_owned()).collect();
        let groups: Vec<(&str, usize)> = grouped(&models, query)
            .into_iter()
            .map(|(p, specs)| (p, specs.len()))
            .collect();
        assert_eq!(groups, expected);
    }
}
