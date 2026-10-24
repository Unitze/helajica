//! Draws the element trees Lua plugins send (see [`crate::ext::Node`]).

use flume::Sender;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{AnyElement, App, IntoElement, ParentElement as _, SharedString, Styled as _, div};

use crate::engine::Command;
use crate::ext::{ButtonVariant, Node};

/// `path` keys element ids, so two buttons with one label in different
/// panels keep their own hover and press state.
pub fn render(node: &Node, path: &str, commands: &Sender<Command>, cx: &App) -> AnyElement {
    match node {
        Node::Text { text, muted } => div()
            .text_sm()
            .when_muted(*muted, cx)
            .child(text.clone())
            .into_any_element(),
        Node::Markdown { text } => {
            TextView::markdown(SharedString::from(format!("{path}/md")), text.clone())
                .into_any_element()
        }
        Node::Code { text } => div()
            .p_2()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .font_family(cx.theme().mono_font_family.clone())
            .text_xs()
            .child(text.clone())
            .into_any_element(),
        Node::Badge { text } => Tag::secondary()
            .small()
            .child(text.clone())
            .into_any_element(),
        Node::Separator => Separator::horizontal().into_any_element(),
        Node::Progress { value } => Progress::new(SharedString::from(format!("{path}/progress")))
            .value(*value)
            .into_any_element(),
        Node::Button {
            label,
            topic,
            data,
            variant,
        } => {
            let commands = commands.clone();
            let topic = topic.clone();
            let data = data.clone();
            let button = Button::new(SharedString::from(format!("{path}/{label}")))
                .small()
                .label(label.clone())
                .on_click(move |_, _, _| {
                    let _ = commands.send(Command::Lua {
                        topic: topic.clone(),
                        data: data.clone(),
                    });
                });
            match variant {
                ButtonVariant::Outline => button.outline(),
                ButtonVariant::Primary => button.primary(),
                ButtonVariant::Ghost => button.ghost(),
                ButtonVariant::Danger => button.danger(),
            }
            .into_any_element()
        }
        Node::Row { children } => h_flex()
            .gap_2()
            .flex_wrap()
            .children(render_all(children, path, commands, cx))
            .into_any_element(),
        Node::Col { children } => v_flex()
            .gap_2()
            .children(render_all(children, path, commands, cx))
            .into_any_element(),
    }
}

pub fn render_all(
    nodes: &[Node],
    path: &str,
    commands: &Sender<Command>,
    cx: &App,
) -> Vec<AnyElement> {
    nodes
        .iter()
        .enumerate()
        .map(|(i, node)| render(node, &format!("{path}/{i}"), commands, cx))
        .collect()
}

trait MutedExt {
    fn when_muted(self, muted: bool, cx: &App) -> Self;
}

impl MutedExt for gpui_kit::Div {
    fn when_muted(self, muted: bool, cx: &App) -> Self {
        match muted {
            true => self.text_color(cx.theme().muted_foreground),
            false => self,
        }
    }
}
