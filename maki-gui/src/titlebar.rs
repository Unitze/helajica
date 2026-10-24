//! The window's own title bar, drawn by the app on every screen: borderless,
//! on the page background, so the window reads as one surface.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, TitleBar, h_flex};
use gpui_kit::{
    AnyElement, App, Keystroke, ParentElement as _, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};

use crate::app::toggle_dark;

pub const APP_NAME: &str = "Helajica";
const HEIGHT: f32 = 40.;
const PALETTE_WIDTH: f32 = 320.;
const PALETTE_KEY: &str = "secondary-k";

pub type PaletteHandler = Rc<dyn Fn(&mut Window, &mut App)>;

pub fn title_bar(
    crumbs: Vec<SharedString>,
    gui_dir: Option<PathBuf>,
    palette: Option<PaletteHandler>,
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
                .children(notice)
                .when_some(palette, |this, palette| {
                    this.child(
                        Button::new("titlebar-palette")
                            .outline()
                            .small()
                            .w(px(PALETTE_WIDTH))
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(Icon::new(Lucide::Search).xsmall())
                                            .child("Search or run a command"),
                                    )
                                    .children(Keystroke::parse(PALETTE_KEY).ok().map(Kbd::new)),
                            )
                            .on_click(move |_, window, cx| palette(window, cx)),
                    )
                })
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
                ),
        )
}
