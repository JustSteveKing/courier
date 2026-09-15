//! The request settings editor shared by collections, folders and requests. Every field can
//! be left to inherit; the form shows what inheriting would give.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, IconName, IndexPath, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::model::{EffectiveSettings, ProxySetting, RequestSettings};
use crate::ui::text_input;

/// A file path field the form can ask its owner to browse for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathField {
    CaCertificate,
    ClientCertificate,
    ClientKey,
    UnixSocket,
}

pub enum SettingsFormEvent {
    Browse(PathField),
}

impl EventEmitter<SettingsFormEvent> for SettingsForm {}

type Choice = Entity<SelectState<SearchableVec<SharedString>>>;

pub struct SettingsForm {
    redirects: Choice,
    max_redirects: Entity<InputState>,
    timeout: Entity<InputState>,
    tls: Choice,
    proxy: Choice,
    proxy_url: Entity<InputState>,
    ca_certificate: Entity<InputState>,
    client_certificate: Entity<InputState>,
    client_key: Entity<InputState>,
    unix_socket: Entity<InputState>,
    /// What inheriting gives, for labels and placeholders.
    inherited: EffectiveSettings,
}

fn on_off(value: bool, on: &str, off: &str) -> String {
    if value { t!(on) } else { t!(off) }.to_string()
}

fn proxy_label(proxy: &ProxySetting) -> String {
    match proxy {
        ProxySetting::System => t!("settings_form.proxy_system").to_string(),
        ProxySetting::None => t!("settings_form.proxy_none").to_string(),
        ProxySetting::Url(url) => url.clone(),
    }
}

impl SettingsForm {
    pub fn new(
        settings: &RequestSettings,
        inherited: EffectiveSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inherit = |value: String| t!("settings_form.inherit", value = value).to_string();
        let choice = |labels: Vec<String>, selected: usize, window: &mut Window, cx: &mut Context<Self>| {
            let labels: Vec<SharedString> = labels.into_iter().map(SharedString::from).collect();
            let state =
                cx.new(|cx| SelectState::new(SearchableVec::new(labels), Some(IndexPath::new(selected)), window, cx));
            cx.subscribe(&state, |_, _, _: &SelectEvent<SearchableVec<SharedString>>, cx| {
                cx.notify()
            })
            .detach();
            state
        };
        let input = |value: Option<String>, placeholder: String, window: &mut Window, cx: &mut Context<Self>| {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value.unwrap_or_default())
            });
            cx.subscribe(&state, |_, _, _: &InputEvent, cx| cx.notify()).detach();
            state
        };
        let index = |value: Option<bool>| match value {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        let redirects = choice(
            vec![
                inherit(on_off(
                    inherited.follow_redirects,
                    "settings_form.follow",
                    "settings_form.dont_follow",
                )),
                t!("settings_form.follow").to_string(),
                t!("settings_form.dont_follow").to_string(),
            ],
            index(settings.follow_redirects),
            window,
            cx,
        );
        let tls = choice(
            vec![
                inherit(on_off(
                    inherited.verify_tls,
                    "settings_form.verify",
                    "settings_form.dont_verify",
                )),
                t!("settings_form.verify").to_string(),
                t!("settings_form.dont_verify").to_string(),
            ],
            index(settings.verify_tls),
            window,
            cx,
        );
        let proxy_index = match &settings.proxy {
            None => 0,
            Some(ProxySetting::System) => 1,
            Some(ProxySetting::None) => 2,
            Some(ProxySetting::Url(_)) => 3,
        };
        let proxy = choice(
            vec![
                inherit(proxy_label(&inherited.proxy)),
                t!("settings_form.proxy_system").to_string(),
                t!("settings_form.proxy_none").to_string(),
                t!("settings_form.proxy_custom").to_string(),
            ],
            proxy_index,
            window,
            cx,
        );
        let proxy_url = match &settings.proxy {
            Some(ProxySetting::Url(url)) => Some(url.clone()),
            _ => None,
        };
        let path_placeholder = |inherited: &Option<std::path::PathBuf>, example: &str| {
            inherited
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| example.to_string())
        };
        Self {
            redirects,
            max_redirects: input(
                settings.max_redirects.map(|n| n.to_string()),
                inherited.max_redirects.to_string(),
                window,
                cx,
            ),
            timeout: input(
                settings.timeout_secs.map(|n| n.to_string()),
                inherited.timeout_secs.to_string(),
                window,
                cx,
            ),
            tls,
            proxy,
            proxy_url: input(proxy_url, "http://proxy.example:3128".into(), window, cx),
            ca_certificate: input(
                settings.ca_certificate.clone(),
                path_placeholder(&inherited.ca_certificate, "certs/dev-ca.pem"),
                window,
                cx,
            ),
            client_certificate: input(
                settings.client_certificate.clone(),
                path_placeholder(&inherited.client_certificate, "certs/client.pem"),
                window,
                cx,
            ),
            client_key: input(
                settings.client_key.clone(),
                path_placeholder(&inherited.client_key, "certs/client-key.pem"),
                window,
                cx,
            ),
            unix_socket: input(
                settings.unix_socket.clone(),
                path_placeholder(&inherited.unix_socket, "/run/docker.sock"),
                window,
                cx,
            ),
            inherited,
        }
    }

    fn selected(choice: &Choice, cx: &App) -> usize {
        choice.read(cx).selected_index(cx).map_or(0, |ix| ix.row)
    }

    /// The settings as entered, or what's wrong with them.
    pub fn value(&self, cx: &App) -> Result<RequestSettings, String> {
        let text = |input: &Entity<InputState>| {
            let value = input.read(cx).value().trim().to_string();
            (!value.is_empty()).then_some(value)
        };
        let tri = |choice: &Choice| match Self::selected(choice, cx) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        };
        let number = |input: &Entity<InputState>, label: &str| -> Result<Option<u64>, String> {
            text(input)
                .map(|v| {
                    v.parse::<u64>()
                        .map_err(|_| t!("settings_form.not_a_number", field = t!(label)).to_string())
                })
                .transpose()
        };
        let proxy = match Self::selected(&self.proxy, cx) {
            1 => Some(ProxySetting::System),
            2 => Some(ProxySetting::None),
            3 => Some(ProxySetting::Url(
                text(&self.proxy_url).ok_or_else(|| t!("settings_form.proxy_url_missing").to_string())?,
            )),
            _ => None,
        };
        Ok(RequestSettings {
            follow_redirects: tri(&self.redirects),
            max_redirects: number(&self.max_redirects, "settings_form.max_redirects")?.map(|n| n as u32),
            timeout_secs: number(&self.timeout, "settings_form.timeout")?,
            verify_tls: tri(&self.tls),
            proxy,
            ca_certificate: text(&self.ca_certificate),
            client_certificate: text(&self.client_certificate),
            client_key: text(&self.client_key),
            unix_socket: text(&self.unix_socket),
        })
    }

    /// Fills a path field, e.g. after browsing.
    pub fn set_path(&mut self, field: PathField, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = match field {
            PathField::CaCertificate => &self.ca_certificate,
            PathField::ClientCertificate => &self.client_certificate,
            PathField::ClientKey => &self.client_key,
            PathField::UnixSocket => &self.unix_socket,
        };
        input.update(cx, |s, cx| s.set_value(path, window, cx));
        cx.notify();
    }
}

impl Render for SettingsForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let heading = |text: String| {
            div()
                .pt_2()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let row = |label: String, control: AnyElement| {
            h_flex()
                .gap_3()
                .child(div().w_40().flex_none().text_sm().child(label))
                .child(div().flex_1().min_w_0().child(control))
        };
        let path_row =
            |label: String, input: &Entity<InputState>, field: PathField, id: &'static str, cx: &mut Context<Self>| {
                row(
                    label,
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(text_input(input).small()))
                        .child(
                            Button::new(id)
                                .small()
                                .ghost()
                                .icon(IconName::FolderOpen)
                                .tooltip(t!("settings_form.browse").to_string())
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(SettingsFormEvent::Browse(field)))),
                        )
                        .into_any_element(),
                )
            };
        let custom_proxy = Self::selected(&self.proxy, cx) == 3;
        let error = self.value(cx).err();
        let inherited_timeout = self.inherited.timeout_secs;

        v_flex()
            .gap_2()
            .child(heading(t!("settings_form.heading_sending").to_string()))
            .child(row(
                t!("settings_form.redirects").to_string(),
                Select::new(&self.redirects).small().into_any_element(),
            ))
            .child(row(
                t!("settings_form.max_redirects").to_string(),
                text_input(&self.max_redirects).small().into_any_element(),
            ))
            .child(row(
                t!("settings_form.timeout").to_string(),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(text_input(&self.timeout).small()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("settings_form.seconds", default = inherited_timeout).to_string()),
                    )
                    .into_any_element(),
            ))
            .child(heading(t!("settings_form.heading_tls").to_string()))
            .child(row(
                t!("settings_form.certificates").to_string(),
                Select::new(&self.tls).small().into_any_element(),
            ))
            .child(path_row(
                t!("settings_form.ca_certificate").to_string(),
                &self.ca_certificate,
                PathField::CaCertificate,
                "browse-ca",
                cx,
            ))
            .child(path_row(
                t!("settings_form.client_certificate").to_string(),
                &self.client_certificate,
                PathField::ClientCertificate,
                "browse-client-cert",
                cx,
            ))
            .child(path_row(
                t!("settings_form.client_key").to_string(),
                &self.client_key,
                PathField::ClientKey,
                "browse-client-key",
                cx,
            ))
            .child(heading(t!("settings_form.heading_connection").to_string()))
            .child(row(
                t!("settings_form.proxy").to_string(),
                Select::new(&self.proxy).small().into_any_element(),
            ))
            .when(custom_proxy, |form| {
                form.child(row(
                    String::new(),
                    text_input(&self.proxy_url).small().into_any_element(),
                ))
            })
            .child(path_row(
                t!("settings_form.unix_socket").to_string(),
                &self.unix_socket,
                PathField::UnixSocket,
                "browse-socket",
                cx,
            ))
            .child(
                div()
                    .pt_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("settings_form.paths_hint").to_string()),
            )
            .when_some(error, |form, error| {
                form.child(div().text_sm().text_color(theme.danger).child(error))
            })
    }
}
