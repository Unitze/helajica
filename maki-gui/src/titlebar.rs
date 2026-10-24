//! The window's own title bar, drawn by the app on every screen: borderless,
//! on the page background, so the window reads as one surface.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, TitleBar, h_flex};
use gpui_kit::{
    AnyElement, App, InteractiveElement as _, MouseButton, ParentElement as _, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};

use crate::app::toggle_dark;

pub const APP_NAME: &str = "Helajica";
const HEIGHT: f32 = 40.;

pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

pub fn title_bar(
    crumbs: Vec<SharedString>,
    gui_dir: Option<PathBuf>,
    settings: Option<Handler>,
    notice: Option<AnyElement>,
    cx: &App,
) -> TitleBar {
    let dark = cx.theme().is_dark();
    TitleBar::new()
        .h(px(HEIGHT))
        .border_b_0()
        .bg(cx.theme().background)
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .text_sm()
                .child(div().pl_1().font_semibold().child(APP_NAME))
                .children(crumbs.into_iter().map(|crumb| {
                    h_flex()
                        .gap_2()
                        .min_w_0()
                        .child(
                            Icon::new(Lucide::ChevronRight)
                                .xsmall()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_color(cx.theme().muted_foreground)
                                .child(crumb),
                        )
                })),
        )
        .child(
            h_flex()
                .gap_1()
                .pr_2()
                // The bar under these is a window drag area, and the title
                // bar starts a window move on a press it sees: without both,
                // a click here becomes a grab of the window.
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(notice)
                .child(
                    Button::new("titlebar-theme")
                        .ghost()
                        .small()
                        .icon(Icon::new(match dark {
                            true => Lucide::Sun,
                            false => Lucide::Moon,
                        }))
                        .tooltip(match dark {
                            true => "Light mode",
                            false => "Dark mode",
                        })
                        .on_click(move |_, window, cx| toggle_dark(gui_dir.as_deref(), window, cx)),
                )
                .when_some(settings, |this, settings| {
                    this.child(
                        Button::new("titlebar-settings")
                            .ghost()
                            .small()
                            .icon(Icon::new(Lucide::Settings))
                            .tooltip("Settings")
                            .on_click(move |_, window, cx| settings(window, cx)),
                    )
                }),
        )
}
