//! The auth editor shared by requests, folders and collections: a kind picker and the fields
//! for that kind, with a nudge to move literal credentials into secrets.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, IconName, IndexPath, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::model::{Auth, OAuthGrant};
use crate::ui::text_input;

pub enum AuthFormEvent {
    Changed,
    /// The user asked to move the literal credential into a secret.
    MoveToSecret,
    /// The user asked to fetch a fresh OAuth token.
    GetToken,
    /// The user asked to forget the OAuth token held for these settings.
    ForgetToken,
}

impl EventEmitter<AuthFormEvent> for AuthForm {}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Inherit,
    None,
    Basic,
    Bearer,
    ApiKey,
    OAuth2,
    Jwt,
    Aws,
    Digest,
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
    grant: OAuthGrant,
    token_url: Entity<InputState>,
    auth_url: Entity<InputState>,
    client_id: Entity<InputState>,
    client_secret: Entity<InputState>,
    scope: Entity<InputState>,
    audience: Entity<InputState>,
    /// What the token store holds for these settings, for the status line.
    token_status: Option<String>,
    jwt_algorithm: crate::jwt::Algorithm,
    jwt_key: Entity<InputState>,
    jwt_claims: Entity<InputState>,
    jwt_header: Entity<InputState>,
    jwt_prefix: Entity<InputState>,
    aws_key_id: Entity<InputState>,
    aws_secret: Entity<InputState>,
    aws_session: Entity<InputState>,
    aws_region: Entity<InputState>,
    aws_service: Entity<InputState>,
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
        Kind::OAuth2 => t!("auth.oauth2"),
        Kind::Jwt => t!("auth.jwt"),
        Kind::Aws => t!("auth.aws"),
        Kind::Digest => t!("auth.digest"),
    }
    .to_string()
    .into()
}

fn grant_label(grant: OAuthGrant) -> String {
    match grant {
        OAuthGrant::ClientCredentials => t!("auth.grant_client_credentials"),
        OAuthGrant::AuthorizationCode => t!("auth.grant_authorization_code"),
        OAuthGrant::DeviceCode => t!("auth.grant_device_code"),
    }
    .to_string()
}

fn kind_of(auth: &Auth) -> Kind {
    match auth {
        Auth::Inherit => Kind::Inherit,
        Auth::None => Kind::None,
        Auth::Basic { .. } => Kind::Basic,
        Auth::Bearer { .. } => Kind::Bearer,
        Auth::ApiKey { .. } => Kind::ApiKey,
        Auth::OAuth2 { .. } => Kind::OAuth2,
        Auth::Jwt { .. } => Kind::Jwt,
        Auth::AwsSigV4 { .. } => Kind::Aws,
        Auth::Digest { .. } => Kind::Digest,
    }
}

impl AuthForm {
    /// `allow_inherit` is false for a collection, which has nothing to inherit from.
    pub fn new(allow_inherit: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kinds: Vec<Kind> = [
            Kind::Inherit,
            Kind::None,
            Kind::Basic,
            Kind::Bearer,
            Kind::ApiKey,
            Kind::OAuth2,
            Kind::Jwt,
            Kind::Aws,
            Kind::Digest,
        ]
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
            grant: OAuthGrant::default(),
            token_url: input("https://id.example/oauth2/token", window, cx),
            auth_url: input("https://id.example/oauth2/authorize", window, cx),
            client_id: input(&t!("auth.client_id"), window, cx),
            client_secret: input("{{client_secret}}", window, cx),
            scope: input(&t!("auth.scope"), window, cx),
            audience: input(&t!("auth.audience"), window, cx),
            token_status: None,
            jwt_algorithm: crate::jwt::Algorithm::default(),
            jwt_key: input("{{jwt_key}}", window, cx),
            jwt_claims: input(r#"{"sub": "{{user_id}}"}"#, window, cx),
            jwt_header: input(r#"{"kid": "…"}"#, window, cx),
            jwt_prefix: input("Bearer", window, cx),
            aws_key_id: input("AKIA…", window, cx),
            aws_secret: input("{{aws_secret_access_key}}", window, cx),
            aws_session: input(&t!("auth.aws_session"), window, cx),
            aws_region: input("eu-west-2", window, cx),
            aws_service: input("s3", window, cx),
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
            Kind::Digest => Auth::Digest {
                username: text(&self.username),
                password: text(&self.password),
            },
            Kind::Jwt => Auth::Jwt {
                algorithm: self.jwt_algorithm,
                key: text(&self.jwt_key),
                claims: text(&self.jwt_claims),
                header: text(&self.jwt_header),
                prefix: text(&self.jwt_prefix),
            },
            Kind::Aws => Auth::AwsSigV4 {
                access_key_id: text(&self.aws_key_id),
                secret_access_key: text(&self.aws_secret),
                session_token: text(&self.aws_session),
                region: text(&self.aws_region),
                service: text(&self.aws_service),
            },
            Kind::OAuth2 => Auth::OAuth2 {
                grant: self.grant,
                token_url: text(&self.token_url),
                auth_url: text(&self.auth_url),
                client_id: text(&self.client_id),
                client_secret: text(&self.client_secret),
                scope: text(&self.scope),
                audience: text(&self.audience),
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
            Auth::Basic { username, password } | Auth::Digest { username, password } => {
                (username, password, &empty, &empty, &empty)
            }
            Auth::Bearer { token } => (&empty, &empty, token, &empty, &empty),
            Auth::ApiKey { name, value, .. } => (&empty, &empty, &empty, name, value),
            Auth::OAuth2 { .. } | Auth::Jwt { .. } | Auth::AwsSigV4 { .. } | Auth::Inherit | Auth::None => {
                (&empty, &empty, &empty, &empty, &empty)
            }
        };
        if let Auth::Jwt {
            algorithm,
            key,
            claims,
            header,
            prefix,
        } = auth
        {
            self.jwt_algorithm = *algorithm;
            for (input, text) in [
                (&self.jwt_key, key),
                (&self.jwt_claims, claims),
                (&self.jwt_header, header),
                (&self.jwt_prefix, prefix),
            ] {
                input.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
            }
        }
        if let Auth::AwsSigV4 {
            access_key_id,
            secret_access_key,
            session_token,
            region,
            service,
        } = auth
        {
            for (input, text) in [
                (&self.aws_key_id, access_key_id),
                (&self.aws_secret, secret_access_key),
                (&self.aws_session, session_token),
                (&self.aws_region, region),
                (&self.aws_service, service),
            ] {
                input.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
            }
        }
        if let Auth::OAuth2 {
            grant,
            token_url,
            auth_url,
            client_id,
            client_secret,
            scope,
            audience,
        } = auth
        {
            self.grant = *grant;
            for (input, text) in [
                (&self.token_url, token_url),
                (&self.auth_url, auth_url),
                (&self.client_id, client_id),
                (&self.client_secret, client_secret),
                (&self.scope, scope),
                (&self.audience, audience),
            ] {
                input.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
            }
        }
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

    /// What the token store holds for these settings ("expires in 12 min", an error, …).
    pub fn set_token_status(&mut self, status: Option<String>, cx: &mut Context<Self>) {
        self.token_status = status;
        cx.notify();
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
            Kind::OAuth2 => fields.child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .children(OAuthGrant::ALL.iter().map(|grant| {
                                let grant = *grant;
                                Button::new(("auth-grant", grant as usize))
                                    .xsmall()
                                    .label(grant_label(grant))
                                    .selected(self.grant == grant)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.grant = grant;
                                        cx.emit(AuthFormEvent::Changed);
                                        cx.notify();
                                    }))
                            }))
                            .child(div().flex_1())
                            .child(
                                Button::new("auth-get-token")
                                    .xsmall()
                                    .primary()
                                    .label(t!("auth.get_token").to_string())
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(AuthFormEvent::GetToken))),
                            )
                            .child(
                                Button::new("auth-forget-token")
                                    .xsmall()
                                    .ghost()
                                    .label(t!("auth.forget_token").to_string())
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(AuthFormEvent::ForgetToken))),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(text_input(&self.token_url).small()))
                            .when(self.grant != OAuthGrant::ClientCredentials, |row| {
                                row.child(div().flex_1().child(text_input(&self.auth_url).small()))
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(text_input(&self.client_id).small()))
                            .child(div().flex_1().child(text_input(&self.client_secret).small())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(text_input(&self.scope).small()))
                            .child(div().flex_1().child(text_input(&self.audience).small())),
                    )
                    .children(self.token_status.clone().map(|status| {
                        div()
                            .id("oauth-token-status")
                            .test_support()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(status)
                    })),
            ),
            Kind::Digest => fields
                .child(div().flex_1().child(text_input(&self.username).small()))
                .child(div().flex_1().child(text_input(&self.password).small()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("auth.digest_hint").to_string()),
                ),
            Kind::Jwt => fields.child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(crate::jwt::Algorithm::ALL.iter().map(|algorithm| {
                                let algorithm = *algorithm;
                                Button::new(("jwt-algorithm", algorithm as usize))
                                    .xsmall()
                                    .label(algorithm.name())
                                    .selected(self.jwt_algorithm == algorithm)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.jwt_algorithm = algorithm;
                                        cx.emit(AuthFormEvent::Changed);
                                        cx.notify();
                                    }))
                            }))
                            .child(div().flex_1())
                            .child(div().w_24().child(text_input(&self.jwt_prefix).small())),
                    )
                    .child(text_input(&self.jwt_key).small())
                    .child(text_input(&self.jwt_claims).small())
                    .child(text_input(&self.jwt_header).small())
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if self.jwt_algorithm.is_shared_secret() {
                            t!("auth.jwt_secret_hint").to_string()
                        } else {
                            t!("auth.jwt_key_hint").to_string()
                        },
                    )),
            ),
            Kind::Aws => fields.child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(text_input(&self.aws_key_id).small()))
                            .child(div().flex_1().child(text_input(&self.aws_secret).small())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().w_32().child(text_input(&self.aws_region).small()))
                            .child(div().w_24().child(text_input(&self.aws_service).small()))
                            .child(div().flex_1().child(text_input(&self.aws_session).small())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("auth.aws_hint").to_string()),
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
