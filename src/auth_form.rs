//! The auth editor shared by requests, folders and collections: a kind picker and the fields
//! for that kind, with a nudge to move literal credentials into secrets.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, IconName, IndexPath, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::model::Auth;
use crate::ui::text_input;

pub enum AuthFormEvent {
    Changed,
    /// The user asked to move the literal credential into a secret.
    MoveToSecret,
}

impl EventEmitter<AuthFormEvent> for AuthForm {}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Inherit,
    None,
    Basic,
    Bearer,
    ApiKey,
}

pub struct AuthForm {
    allow_inherit: bool,
    kind: Entity<SelectState<SearchableVec<SharedString>>>,
    kinds: Vec<Kind>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    token: Entity<InputState>,
    key_name: Entity<InputState>,
    key_value: Entity<InputState>,
    key_in_query: bool,
    /// What `Inherit` resolves to, and where it comes from, for display.
    inherited: Option<(Auth, String)>,
}

fn kind_label(kind: Kind) -> SharedString {
    match kind {
        Kind::Inherit => t!("auth.inherit"),
        Kind::None => t!("auth.none"),
        Kind::Basic => t!("auth.basic"),
        Kind::Bearer => t!("auth.bearer"),
        Kind::ApiKey => t!("auth.api_key"),
    }
    .to_string()
    .into()
}

fn kind_of(auth: &Auth) -> Kind {
    match auth {
        Auth::Inherit => Kind::Inherit,
        Auth::None => Kind::None,
        Auth::Basic { .. } => Kind::Basic,
        Auth::Bearer { .. } => Kind::Bearer,
        Auth::ApiKey { .. } => Kind::ApiKey,
    }
}

impl AuthForm {
    /// `allow_inherit` is false for a collection, which has nothing to inherit from.
    pub fn new(allow_inherit: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kinds: Vec<Kind> = [Kind::Inherit, Kind::None, Kind::Basic, Kind::Bearer, Kind::ApiKey]
            .into_iter()
            .filter(|k| allow_inherit || *k != Kind::Inherit)
            .collect();
        let labels: Vec<SharedString> = kinds.iter().map(|k| kind_label(*k)).collect();
        let kind = cx.new(|cx| SelectState::new(SearchableVec::new(labels), Some(IndexPath::default()), window, cx));
        cx.subscribe(&kind, |_, _, _: &SelectEvent<SearchableVec<SharedString>>, cx| {
            cx.emit(AuthFormEvent::Changed);
            cx.notify();
        })
        .detach();
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_string();
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            cx.subscribe(&state, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.emit(AuthFormEvent::Changed);
                    cx.notify();
                }
            })
            .detach();
            state
        };
        Self {
            allow_inherit,
            kind,
            kinds,
            username: input(&t!("auth.username"), window, cx),
            password: input("{{password}}", window, cx),
            token: input("{{token}}", window, cx),
            key_name: input("X-API-Key", window, cx),
            key_value: input("{{api_key}}", window, cx),
            key_in_query: false,
            inherited: None,
        }
    }

    fn selected_kind(&self, cx: &App) -> Kind {
        self.kind
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| self.kinds.get(ix.row).copied())
            .unwrap_or(if self.allow_inherit { Kind::Inherit } else { Kind::None })
    }

    pub fn value(&self, cx: &App) -> Auth {
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        match self.selected_kind(cx) {
            Kind::Inherit => Auth::Inherit,
            Kind::None => Auth::None,
            Kind::Basic => Auth::Basic {
                username: text(&self.username),
                password: text(&self.password),
            },
            Kind::Bearer => Auth::Bearer {
                token: text(&self.token),
            },
            Kind::ApiKey => Auth::ApiKey {
                name: text(&self.key_name),
                value: text(&self.key_value),
                in_query: self.key_in_query,
            },
        }
    }

    /// Shows `auth` without emitting changes.
    pub fn set(&mut self, auth: &Auth, window: &mut Window, cx: &mut Context<Self>) {
        let auth = if !self.allow_inherit && auth.is_inherit() {
            &Auth::None
        } else {
            auth
        };
        let index = self.kinds.iter().position(|k| *k == kind_of(auth)).unwrap_or(0);
        self.kind.update(cx, |s, cx| {
            s.set_selected_index(Some(IndexPath::new(index)), window, cx)
        });
        let empty = String::new();
        let (username, password, token, name, value) = match auth {
            Auth::Basic { username, password } => (username, password, &empty, &empty, &empty),
            Auth::Bearer { token } => (&empty, &empty, token, &empty, &empty),
            Auth::ApiKey { name, value, .. } => (&empty, &empty, &empty, name, value),
            Auth::Inherit | Auth::None => (&empty, &empty, &empty, &empty, &empty),
        };
        for (input, text) in [
            (&self.username, username),
            (&self.password, password),
            (&self.token, token),
            (&self.key_name, name),
            (&self.key_value, value),
        ] {
            input.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
        }
        self.key_in_query = matches!(auth, Auth::ApiKey { in_query: true, .. });
        cx.notify();
    }

    /// Replaces the credential field's text, e.g. with `{{token}}` after moving it to a secret.
    pub fn set_credential(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let mut auth = self.value(cx);
        auth.set_credential(text);
        self.set(&auth, window, cx);
        cx.emit(AuthFormEvent::Changed);
    }

    pub fn set_inherited(&mut self, inherited: Option<(Auth, String)>, cx: &mut Context<Self>) {
        self.inherited = inherited;
        cx.notify();
    }
}

impl Render for AuthForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let kind = self.selected_kind(cx);
        let muted = |text: String| div().text_xs().text_color(theme.muted_foreground).child(text);
        let fields = h_flex().flex_1().min_w_0().gap_2();
        let fields = match kind {
            Kind::Inherit => {
                let description = match &self.inherited {
                    Some((auth, source)) if !auth.is_unset() => {
                        t!("auth.inherited_from", kind = kind_label(kind_of(auth)), source = source).to_string()
                    }
                    Some((_, source)) => t!("auth.inherited_none", source = source).to_string(),
                    None => t!("auth.inherit_hint").to_string(),
                };
                fields.child(muted(description))
            }
            Kind::None => fields.child(muted(t!("auth.none_hint").to_string())),
            Kind::Basic => fields
                .child(div().flex_1().child(text_input(&self.username).small()))
                .child(div().flex_1().child(text_input(&self.password).small())),
            Kind::Bearer => fields.child(div().flex_1().child(text_input(&self.token).small())),
            Kind::ApiKey => fields
                .child(div().w_32().child(text_input(&self.key_name).small()))
                .child(div().flex_1().child(text_input(&self.key_value).small()))
                .child(
                    h_flex()
                        .child(
                            Button::new("auth-key-header")
                                .xsmall()
                                .label(t!("auth.in_header").to_string())
                                .selected(!self.key_in_query)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.key_in_query = false;
                                    cx.emit(AuthFormEvent::Changed);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("auth-key-query")
                                .xsmall()
                                .label(t!("auth.in_query").to_string())
                                .selected(self.key_in_query)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.key_in_query = true;
                                    cx.emit(AuthFormEvent::Changed);
                                    cx.notify();
                                })),
                        ),
                ),
        };
        let literal = self.value(cx).literal_credential().is_some();
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w_32().flex_none().child(Select::new(&self.kind).small()))
                    .child(fields),
            )
            .when(literal, |form| {
                form.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(t!("auth.literal_warning").to_string())
                        .child(
                            Button::new("auth-move-to-secret")
                                .xsmall()
                                .ghost()
                                .icon(IconName::ArrowRight)
                                .label(t!("auth.move_to_secret").to_string())
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(AuthFormEvent::MoveToSecret))),
                        ),
                )
            })
    }
}
