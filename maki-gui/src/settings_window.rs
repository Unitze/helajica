//! The Settings window: appearance, the providers the user connected, and
//! add-ons. Changes that affect sessions are reported through `on_change`, so
//! the main window can pick them up.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::{
    Anchor, AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, WindowBounds,
    WindowOptions, div, prelude::FluentBuilder as _, px, size,
};
use maki_config::providers::Protocol;
use maki_providers::openai_auth;
use maki_storage::StateDir;

use crate::addons::Addon;
use crate::app::set_dark;
use crate::custom_providers::{
    self, Connected, Draft, PROTOCOLS, ProviderModel, parse_tokens, protocol_label,
};
use crate::settings::Settings;
use crate::titlebar::APP_NAME;
use crate::update;

const WIDTH: f32 = 820.;
const HEIGHT: f32 = 620.;
const NAV_WIDTH: f32 = 200.;
const CONTENT_WIDTH: f32 = 560.;
const TITLE_BAR_HEIGHT: f32 = 40.;
const MENU_WIDTH: f32 = 220.;
const MODEL_LIST_HEIGHT: f32 = 280.;
const CONTEXT_INPUT_WIDTH: f32 = 110.;
const CONTEXT_PLACEHOLDER: &str = "Context";

pub type OnChange = Rc<dyn Fn(&mut App)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Providers,
    Addons,
}

impl Page {
    const ALL: [Self; 3] = [Self::General, Self::Providers, Self::Addons];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Providers => "Providers",
            Self::Addons => "Add-ons",
        }
    }

    fn icon(self) -> Lucide {
        match self {
            Self::General => Lucide::Settings,
            Self::Providers => Lucide::Plug,
            Self::Addons => Lucide::Puzzle,
        }
    }
}

/// Brings the window forward if it is open, otherwise opens it.
pub fn open(
    existing: Option<AnyWindowHandle>,
    gui_dir: Option<PathBuf>,
    on_change: OnChange,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    if let Some(handle) = existing
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return Some(handle);
    }
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..TitleBar::window_options()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| SettingsView::new(gui_dir, on_change, window, cx))
    }) {
        Ok((handle, _)) => Some(handle),
        Err(e) => {
            tracing::error!(error = %e, "open settings window");
            None
        }
    }
}

struct ModelRow {
    id: String,
    /// Saved with the provider; fetched models start out unticked.
    enabled: bool,
    context: Entity<InputState>,
}

struct ProviderForm {
    name: Entity<InputState>,
    base_url: Entity<InputState>,
    api_key: Entity<InputState>,
    models: Vec<ModelRow>,
    new_model: Entity<InputState>,
    fetching: bool,
    protocol: Option<Protocol>,
    /// The slug being edited, `None` while connecting a new one.
    editing: Option<String>,
}

impl ProviderForm {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("My gateway")),
            base_url: cx
                .new(|cx| InputState::new(window, cx).placeholder("https://llm.example.com/v1")),
            api_key: cx.new(|cx| InputState::new(window, cx).masked(true)),
            models: Vec::new(),
            new_model: cx.new(|cx| InputState::new(window, cx).placeholder("Model id")),
            fetching: false,
            protocol: Some(Protocol::Openai),
            editing: None,
        }
    }

    fn draft(&self, cx: &App) -> Draft {
        Draft {
            name: self.name.read(cx).value().to_string(),
            protocol: self.protocol,
            base_url: self.base_url.read(cx).value().to_string(),
            api_key: self.api_key.read(cx).value().to_string(),
            models: self
                .models
                .iter()
                .filter(|row| row.enabled)
                .map(|row| ProviderModel {
                    id: row.id.clone(),
                    context_window: parse_tokens(&row.context.read(cx).value()),
                })
                .collect(),
        }
    }

    /// Adds models not listed yet, keeping the ticks and windows of those
    /// that are.
    fn add_models(
        &mut self,
        models: impl IntoIterator<Item = ProviderModel>,
        enabled: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        for model in models {
            if self.models.iter().any(|row| row.id == model.id) {
                continue;
            }
            let value = model
                .context_window
                .map(|n| n.to_string())
                .unwrap_or_default();
            let context = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(CONTEXT_PLACEHOLDER)
                    .default_value(value)
            });
            self.models.push(ModelRow {
                id: model.id,
                enabled,
                context,
            });
        }
    }

    fn fill(&mut self, provider: Option<&Connected>, window: &mut Window, cx: &mut App) {
        let set = |input: &Entity<InputState>, value: String, window: &mut Window, cx: &mut App| {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        };
        set(
            &self.name,
            provider.map(|p| p.name.clone()).unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.base_url,
            provider.map(|p| p.base_url.clone()).unwrap_or_default(),
            window,
            cx,
        );
        set(&self.api_key, String::new(), window, cx);
        set(&self.new_model, String::new(), window, cx);
        self.models.clear();
        self.fetching = false;
        let saved = provider.map(|p| p.models.clone()).unwrap_or_default();
        self.add_models(saved, true, window, cx);
        self.protocol = Some(provider.map_or(Protocol::Openai, |p| p.protocol));
        self.editing = provider.map(|p| p.slug.clone());
    }
}

struct SettingsView {
    gui_dir: Option<PathBuf>,
    storage: Option<StateDir>,
    on_change: OnChange,
    page: Page,
    providers: Vec<Connected>,
    form: ProviderForm,
    error: Option<SharedString>,
    signing_in: bool,
    _subscription: Subscription,
}

impl SettingsView {
    fn new(
        gui_dir: Option<PathBuf>,
        on_change: OnChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let storage = StateDir::resolve().ok();
        let form = ProviderForm::new(window, cx);
        // The model box filters the list as it is typed in; Enter adds it.
        let subscription = cx.subscribe_in(
            &form.new_model,
            window,
            |this, _, event, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => this.add_typed_model(window, cx),
                _ => {}
            },
        );
        Self {
            providers: custom_providers::list(storage.as_ref()),
            gui_dir,
            storage,
            on_change,
            page: Page::General,
            form,
            error: None,
            signing_in: false,
            _subscription: subscription,
        }
    }

    fn settings(&self) -> Settings {
        Settings::load(self.gui_dir.as_deref())
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.providers = custom_providers::list(self.storage.as_ref());
        (self.on_change)(cx);
        cx.notify();
    }

    fn save_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.form.draft(cx);
        let renamed = self
            .form
            .editing
            .clone()
            .filter(|old| *old != maki_config::providers::slugify(&draft.name));
        let saved =
            custom_providers::connect(&draft, self.storage.as_ref()).and_then(
                |slug| match renamed {
                    Some(old) => {
                        custom_providers::disconnect(&old, self.storage.as_ref()).map(|()| slug)
                    }
                    None => Ok(slug),
                },
            );
        match saved {
            Ok(_) => {
                self.error = None;
                self.form.fill(None, window, cx);
                self.changed(cx);
            }
            Err(e) => {
                self.error = Some(e.into());
                cx.notify();
            }
        }
    }

    fn fetch_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.form.draft(cx);
        let storage = self.storage.clone();
        self.form.fetching = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let fetched = cx
                .background_spawn(async move {
                    custom_providers::fetch_models(&draft, storage.as_ref())
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.form.fetching = false;
                match fetched {
                    Ok(models) if models.is_empty() => {
                        this.error = Some("The provider listed no models.".into());
                    }
                    Ok(models) => this.form.add_models(models, false, window, cx),
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn add_typed_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.form.new_model.read(cx).value().trim().to_owned();
        if id.is_empty() {
            return;
        }
        let model = ProviderModel {
            id,
            context_window: None,
        };
        self.form.add_models([model], true, window, cx);
        self.form
            .new_model
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    fn render_models(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let filter = self.form.new_model.read(cx).value().trim().to_lowercase();
        let shown: Vec<usize> = (0..self.form.models.len())
            .filter(|&i| {
                filter.is_empty() || self.form.models[i].id.to_lowercase().contains(&filter)
            })
            .collect();
        let all = !shown.is_empty() && shown.iter().all(|&i| self.form.models[i].enabled);
        let toggled = shown.clone();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.form.new_model)))
                    .child(Button::new("add-model").label("Add").on_click(
                        cx.listener(|this, _, window, cx| this.add_typed_model(window, cx)),
                    ))
                    .child(
                        Button::new("fetch-models")
                            .label("Fetch /models")
                            .loading(self.form.fetching)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.fetch_models(window, cx)),
                            ),
                    ),
            )
            .when(self.form.models.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("None picked: maki lists them from the provider at startup."),
                )
            })
            .when(!self.form.models.is_empty(), move |this| {
                this.child(
                    v_flex()
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded(cx.theme().radius)
                        .child(
                            h_flex()
                                .gap_3()
                                .px_3()
                                .py_1p5()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(Checkbox::new("all-models").checked(all).on_click(
                                    cx.listener(move |this, checked: &bool, _, cx| {
                                        for &i in &toggled {
                                            if let Some(row) = this.form.models.get_mut(i) {
                                                row.enabled = *checked;
                                            }
                                        }
                                        cx.notify();
                                    }),
                                ))
                                .child(div().flex_1().child(format!(
                                    "{} of {} models",
                                    self.form.models.iter().filter(|m| m.enabled).count(),
                                    self.form.models.len()
                                )))
                                .child(div().w(px(CONTEXT_INPUT_WIDTH)).child("Context window")),
                        )
                        .child(divider(cx))
                        .child(
                            div()
                                .id("provider-models")
                                .max_h(px(MODEL_LIST_HEIGHT))
                                .overflow_y_scrollbar()
                                .child(v_flex().children(shown.iter().map(|&i| {
                                    let row = &self.form.models[i];
                                    h_flex()
                                        .gap_3()
                                        .px_3()
                                        .py_1()
                                        .child(
                                            Checkbox::new(("model-enabled", i))
                                                .checked(row.enabled)
                                                .on_click(cx.listener(
                                                    move |this, checked: &bool, _, cx| {
                                                        if let Some(row) =
                                                            this.form.models.get_mut(i)
                                                        {
                                                            row.enabled = *checked;
                                                        }
                                                        cx.notify();
                                                    },
                                                )),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_sm()
                                                .child(row.id.clone()),
                                        )
                                        .child(
                                            div()
                                                .w(px(CONTEXT_INPUT_WIDTH))
                                                .child(Input::new(&row.context).small()),
                                        )
                                }))),
                        ),
                )
            })
    }

    fn remove_provider(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        match custom_providers::disconnect(slug, self.storage.as_ref()) {
            Ok(()) => {
                if self.form.editing.as_deref() == Some(slug) {
                    self.form.fill(None, window, cx);
                }
                self.error = None;
                self.changed(cx);
            }
            Err(e) => {
                self.error = Some(e.into());
                cx.notify();
            }
        }
    }

    fn set_addon(&mut self, addon: Addon, enabled: bool, cx: &mut Context<Self>) {
        let mut settings = self.settings();
        settings.addons.insert(addon.id().to_owned(), enabled);
        settings.save(self.gui_dir.as_deref());
        addon.apply(enabled);
        self.changed(cx);
    }

    fn sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(storage) = self.storage.clone() else {
            return;
        };
        self.signing_in = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { openai_auth::browser_sign_in(&storage) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.signing_in = false;
                match result {
                    Ok(()) => this.changed(cx),
                    Err(e) => {
                        this.error = Some(format!("Sign-in failed: {e}").into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        if let Some(storage) = &self.storage
            && let Err(e) = openai_auth::sign_out(storage)
        {
            self.error = Some(format!("Sign-out failed: {e}").into());
        }
        self.changed(cx);
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(NAV_WIDTH))
            .h_full()
            .flex_none()
            .p_2()
            .gap_0p5()
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .bg(cx.theme().sidebar)
            .children(Page::ALL.into_iter().map(|page| {
                let current = page == self.page;
                h_flex()
                    .id(page.label())
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .text_sm()
                    .when(current, |this| {
                        this.bg(cx.theme().sidebar_accent)
                            .font_medium()
                            .text_color(cx.theme().sidebar_accent_foreground)
                    })
                    .hover(|style| style.bg(cx.theme().sidebar_accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page = page;
                        this.error = None;
                        cx.notify();
                    }))
                    .child(Icon::new(page.icon()).small())
                    .child(page.label())
            }))
    }

    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = cx.theme().is_dark();
        let gui_dir = self.gui_dir.clone();
        v_flex()
            .gap_6()
            .child(heading(
                "General",
                "How Helajica looks, and which version you are on.",
                cx,
            ))
            .child(
                card(cx)
                    .child(row(
                        "Dark mode",
                        "Follows you to every window.",
                        Switch::new("dark")
                            .checked(dark)
                            .on_click(move |checked, window, cx| {
                                let mut settings = Settings::load(gui_dir.as_deref());
                                settings.dark = *checked;
                                settings.save(gui_dir.as_deref());
                                set_dark(*checked, Some(window), cx);
                                cx.refresh_windows();
                            }),
                        cx,
                    ))
                    .child(divider(cx))
                    .child(row(
                        "Version",
                        "Updates are offered in the title bar when one is out.",
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{APP_NAME} v{}", update::CURRENT)),
                        cx,
                    )),
            )
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.form.editing.is_some();
        let view = cx.entity().downgrade();
        let protocol = self.form.protocol.map_or("Pick one", protocol_label);
        v_flex()
            .gap_6()
            .child(heading(
                "Providers",
                "Connect any API that speaks the OpenAI, Anthropic or Gemini protocol. \
                 Saved to maki's providers.toml, so the terminal maki sees them too.",
                cx,
            ))
            .when(!self.providers.is_empty(), |this| {
                this.child(card(cx).children(self.providers.iter().enumerate().map(
                    |(i, provider)| {
                        let edit = provider.clone();
                        let remove = provider.slug.clone();
                        v_flex().when(i > 0, |this| this.child(divider(cx))).child(
                            h_flex()
                                .gap_3()
                                .px_4()
                                .py_3()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_0p5()
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .font_medium()
                                                        .child(provider.name.clone()),
                                                )
                                                .child(badge(protocol_label(provider.protocol), cx))
                                                .when(!provider.has_key, |this| {
                                                    this.child(badge("No key", cx))
                                                }),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .truncate()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!(
                                                    "{}  ·  {}",
                                                    provider.slug, provider.base_url
                                                )),
                                        ),
                                )
                                .child(
                                    Button::new(("edit-provider", i))
                                        .ghost()
                                        .small()
                                        .label("Edit")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.form.fill(Some(&edit), window, cx);
                                            this.error = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(("remove-provider", i))
                                        .ghost()
                                        .small()
                                        .icon(Icon::new(Lucide::Trash))
                                        .tooltip("Remove")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.remove_provider(&remove, window, cx)
                                        })),
                                ),
                        )
                    },
                )))
            })
            .child(
                card(cx).child(
                    v_flex()
                        .p_4()
                        .gap_4()
                        .child(div().text_sm().font_semibold().child(match editing {
                            true => "Edit provider",
                            false => "Connect a provider",
                        }))
                        .child(field("Name", Input::new(&self.form.name), cx))
                        .child(field(
                            "API",
                            Button::new("protocol")
                                .outline()
                                .w_full()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .child(protocol)
                                        .child(Icon::new(Lucide::ChevronsUpDown).xsmall()),
                                )
                                .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
                                    PROTOCOLS.into_iter().fold(
                                        menu.min_w(px(MENU_WIDTH)),
                                        |menu, (protocol, label)| {
                                            let view = view.clone();
                                            menu.item(PopupMenuItem::new(label).on_click(
                                                move |_, _, cx| {
                                                    let _ = view.update(cx, |this, cx| {
                                                        this.form.protocol = Some(protocol);
                                                        cx.notify();
                                                    });
                                                },
                                            ))
                                        },
                                    )
                                }),
                            cx,
                        ))
                        .child(field("Base URL", Input::new(&self.form.base_url), cx))
                        .child(field(
                            match editing {
                                true => "API key (blank keeps the saved one)",
                                false => "API key",
                            },
                            Input::new(&self.form.api_key).mask_toggle(),
                            cx,
                        ))
                        .child(field("Models", self.render_models(cx), cx))
                        .when_some(self.error.clone(), |this, error| {
                            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                        })
                        .child(
                            h_flex()
                                .justify_end()
                                .gap_2()
                                .when(editing, |this| {
                                    this.child(Button::new("cancel-edit").label("Cancel").on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.form.fill(None, window, cx);
                                            this.error = None;
                                            cx.notify();
                                        }),
                                    ))
                                })
                                .child(
                                    Button::new("save-provider")
                                        .primary()
                                        .label(match editing {
                                            true => "Save",
                                            false => "Connect",
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.save_provider(window, cx)
                                        })),
                                ),
                        ),
                ),
            )
    }

    fn render_addons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings();
        let storage = self.storage.as_ref();
        v_flex()
            .gap_6()
            .child(heading(
                "Add-ons",
                "Optional pieces the core runs without. Switch them on when you want them.",
                cx,
            ))
            .children(Addon::ALL.into_iter().map(|addon| {
                let enabled = addon.enabled(&settings, storage);
                card(cx)
                    .child(row(
                        addon.name(),
                        addon.description(),
                        Switch::new(addon.id())
                            .checked(enabled)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.set_addon(addon, *checked, cx)
                            })),
                        cx,
                    ))
                    .when(enabled && addon == Addon::ChatGptSignIn, |this| {
                        this.child(divider(cx))
                            .child(self.render_chatgpt_account(cx))
                    })
            }))
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
    }

    fn render_chatgpt_account(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let signed_in = self.storage.as_ref().is_some_and(openai_auth::signed_in);
        let (status, control) = match (self.signing_in, signed_in) {
            (true, _) => (
                "Finish signing in in your browser…",
                h_flex()
                    .gap_2()
                    .child(Spinner::new().small())
                    .into_any_element(),
            ),
            (false, true) => (
                "Signed in. OpenAI models use your ChatGPT plan.",
                Button::new("chatgpt-sign-out")
                    .outline()
                    .small()
                    .label("Sign out")
                    .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                    .into_any_element(),
            ),
            (false, false) => (
                "Not signed in.",
                Button::new("chatgpt-sign-in")
                    .primary()
                    .small()
                    .icon(Icon::new(Lucide::LogIn))
                    .label("Sign in with ChatGPT")
                    .on_click(cx.listener(|this, _, _, cx| this.sign_in(cx)))
                    .into_any_element(),
            ),
        };
        h_flex()
            .px_4()
            .py_3()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            )
            .child(control)
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match self.page {
            Page::General => self.render_general(cx).into_any_element(),
            Page::Providers => self.render_providers(cx).into_any_element(),
            Page::Addons => self.render_addons(cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new()
                    .h(px(TITLE_BAR_HEIGHT))
                    .border_b_0()
                    .bg(cx.theme().background)
                    .child(div().pl_1().text_sm().font_semibold().child("Settings")),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_nav(cx))
                    .child(
                        div()
                            .id("settings-page")
                            .flex_1()
                            .h_full()
                            .overflow_y_scrollbar()
                            .child(
                                div()
                                    .w(px(CONTENT_WIDTH))
                                    .max_w_full()
                                    .mx_auto()
                                    .px_6()
                                    .py_8()
                                    .child(page),
                            ),
                    ),
            )
    }
}

fn heading(title: &str, description: &str, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_xl().font_semibold().child(title.to_owned()))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(description.to_owned()),
        )
}

fn card(cx: &App) -> gpui_kit::Div {
    v_flex()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
}

fn divider(cx: &App) -> impl IntoElement {
    div().h(px(1.)).w_full().bg(cx.theme().border)
}

fn row(title: &str, description: &str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_4()
        .px_4()
        .py_3()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_medium().child(title.to_owned()))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(description.to_owned()),
                ),
        )
        .child(div().flex_none().child(control))
}

fn field(label: &str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1p5()
        .child(
            div()
                .text_sm()
                .font_medium()
                .text_color(cx.theme().foreground)
                .child(label.to_owned()),
        )
        .child(control)
}

fn badge(text: &str, cx: &App) -> impl IntoElement {
    div()
        .px_1p5()
        .rounded(cx.theme().radius)
        .bg(cx.theme().secondary)
        .text_xs()
        .text_color(cx.theme().secondary_foreground)
        .child(text.to_owned())
}
