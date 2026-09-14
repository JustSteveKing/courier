//! The request editor on the right: edit, save, send, and show the response.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{EditorState, InputEvent, InputState};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, IndexPath, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use indexmap::IndexMap;
use rust_i18n::t;

use crate::credentials::is_literal_credential;
use crate::http::{self, Request};
use crate::model::{Body, BodyKind, RequestFile, Variables, headers_from_text, headers_to_text};
use crate::response_cache::{self, CacheKey, Outcome, ResponseCache, StoredResponse};
use crate::secret_store::{SecretRef, SecretStore};
use crate::settings::AppSettings;
use crate::storage::write_yaml;
use crate::ui::{code_editor, readonly_editor, text_input};

pub const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
const CONTEXT: &str = "RequestEditor";

gpui_kit::actions!(request_editor, [SaveRequest, SendRequest]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-s", SaveRequest, Some(CONTEXT)),
        KeyBinding::new("ctrl-enter", SendRequest, Some(CONTEXT)),
        // Text inputs bind their own "secondary enter"; this deeper binding wins inside the
        // editor's fields, so Ctrl+Enter sends from any of them.
        KeyBinding::new("secondary-enter", SendRequest, Some("RequestEditor > Input")),
    ]);
}

pub enum RequestEditorEvent {
    Saved(PathBuf),
    /// The user asked to move the literal credential in header `index` into a secret.
    /// The request has already been saved.
    MoveHeaderToSecret {
        path: PathBuf,
        index: usize,
    },
    Error(String),
}

impl EventEmitter<RequestEditorEvent> for RequestEditor {}

/// Response and send state for one request, kept while the app runs so switching between
/// requests never loses (or misplaces) a response.
#[derive(Default)]
struct ResponseState {
    response: Option<StoredResponse>,
    /// Loaded from the cache rather than received in this session.
    restored: bool,
    sending: bool,
    missing_variables: Vec<String>,
    cache_key: Option<CacheKey>,
}

impl ResponseState {
    fn outcome(&self) -> Option<&Outcome> {
        self.response.as_ref().map(|r| &r.outcome)
    }

    fn clear(&mut self) {
        self.response = None;
        self.restored = false;
        self.missing_variables.clear();
    }

    /// Loads the saved response, if this request has none yet and isn't mid-send.
    fn restore_from(&mut self, cache: &ResponseCache) {
        if self.response.is_none()
            && !self.sending
            && let Some(key) = &self.cache_key
            && let Some(response) = cache.load(key)
        {
            self.response = Some(response);
            self.restored = true;
        }
    }

    fn finish(&mut self, response: StoredResponse, missing_variables: Vec<String>) {
        self.response = Some(response);
        self.restored = false;
        self.sending = false;
        self.missing_variables = missing_variables;
    }
}

pub struct RequestEditor {
    focus_handle: FocusHandle,
    /// The file being edited, and its contents as last loaded or saved.
    path: Option<PathBuf>,
    saved: Option<RequestFile>,
    dirty: bool,
    variables: Variables,
    secrets: IndexMap<String, SecretRef>,
    secret_store: Option<SecretStore>,

    name: Entity<InputState>,
    method: Entity<SelectState<SearchableVec<&'static str>>>,
    url: Entity<InputState>,
    headers: Entity<EditorState>,
    body: Entity<EditorState>,

    response_tab: usize,
    response_body: Entity<EditorState>,
    response_headers: Entity<EditorState>,
    responses: HashMap<PathBuf, ResponseState>,
    response_cache: ResponseCache,
}

impl RequestEditor {
    pub fn new(response_cache: ResponseCache, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(t!("request.name_placeholder").to_string()));
        let method = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(METHODS.to_vec()),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });
        let url = cx.new(|cx| InputState::new(window, cx).placeholder("{{base_url}}/path"));
        let headers = cx.new(|cx| EditorState::new(window, cx).language("text"));
        let body = cx.new(|cx| EditorState::new(window, cx).language("json"));
        let response_body = cx.new(|cx| EditorState::new(window, cx).language("json"));
        let response_headers = cx.new(|cx| EditorState::new(window, cx).language("text"));

        cx.subscribe_in(&url, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { secondary: false, .. } => this.send(window, cx),
            InputEvent::Change => this.update_dirty(cx),
            _ => {}
        })
        .detach();
        cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.update_dirty(cx);
            }
        })
        .detach();
        for editor in [&headers, &body] {
            cx.subscribe(editor, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.update_dirty(cx);
                }
            })
            .detach();
        }
        cx.subscribe(&method, |this, _, _: &SelectEvent<SearchableVec<&'static str>>, cx| {
            this.update_dirty(cx);
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            path: None,
            saved: None,
            dirty: false,
            variables: Variables::new(),
            secrets: IndexMap::new(),
            secret_store: None,
            name,
            method,
            url,
            headers,
            body,
            response_tab: 0,
            response_body,
            response_headers,
            responses: HashMap::new(),
            response_cache,
        }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    #[cfg(test)]
    pub fn secret_names(&self) -> Vec<String> {
        self.secrets.keys().cloned().collect()
    }

    #[cfg(test)]
    pub fn headers_entity(&self) -> Entity<EditorState> {
        self.headers.clone()
    }

    /// Re-applies strings set at construction after the interface language changes. Strings
    /// built during render update on their own.
    pub fn relocalize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.name.update(cx, |s, cx| {
            s.set_placeholder(t!("request.name_placeholder").to_string(), window, cx)
        });
        cx.notify();
    }

    /// The response cache, unless saving responses is turned off.
    fn cache(&self, cx: &App) -> Option<&ResponseCache> {
        AppSettings::get(cx).remember_responses.then_some(&self.response_cache)
    }

    #[cfg(test)]
    pub fn shown_response(&self) -> Option<(&StoredResponse, bool)> {
        self.state().and_then(|s| s.response.as_ref().map(|r| (r, s.restored)))
    }

    #[cfg(test)]
    pub fn is_sending(&self) -> bool {
        self.state().is_some_and(|s| s.sending)
    }

    #[cfg(test)]
    pub fn response_for(&self, path: &std::path::Path) -> Option<&StoredResponse> {
        self.responses.get(path).and_then(|s| s.response.as_ref())
    }

    /// Forgets every response (in memory and on disk), except requests still in flight.
    pub fn clear_responses(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let removed = self.response_cache.clear().unwrap_or(0);
        self.responses.values_mut().for_each(ResponseState::clear);
        self.show_response(window, cx);
        removed
    }

    fn state(&self) -> Option<&ResponseState> {
        self.path.as_ref().and_then(|p| self.responses.get(p))
    }

    /// Puts the current request's response (if any) into the response panes.
    fn show_response(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (body, headers) = match self.state().and_then(ResponseState::outcome) {
            Some(Outcome::Response { headers, body, .. }) => (
                http::pretty_body(body),
                headers
                    .iter()
                    .map(|(n, v)| format!("{n}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Some(Outcome::Error { message }) => (message.clone(), String::new()),
            None => (String::new(), String::new()),
        };
        self.response_body.update(cx, |s, cx| s.set_value(body, window, cx));
        self.response_headers
            .update(cx, |s, cx| s.set_value(headers, window, cx));
        cx.notify();
    }

    pub fn set_secret_store(&mut self, store: SecretStore) {
        self.secret_store = Some(store);
    }

    /// Variables and secret references in effect for the active environment.
    pub fn set_variables(&mut self, variables: Variables, secrets: IndexMap<String, SecretRef>) {
        self.variables = variables;
        self.secrets = secrets;
    }

    /// Shows `request`. `cache_key` identifies it in the response cache (see
    /// [`response_cache::cache_key`]).
    pub fn load(
        &mut self,
        path: PathBuf,
        request: RequestFile,
        cache_key: Option<CacheKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let method_index = METHODS
            .iter()
            .position(|m| m.eq_ignore_ascii_case(&request.method))
            .unwrap_or(0);
        self.name
            .update(cx, |s, cx| s.set_value(request.name.clone(), window, cx));
        self.method.update(cx, |s, cx| {
            s.set_selected_index(Some(IndexPath::new(method_index)), window, cx)
        });
        self.url
            .update(cx, |s, cx| s.set_value(request.url.clone(), window, cx));
        self.headers
            .update(cx, |s, cx| s.set_value(headers_to_text(&request.headers), window, cx));
        let body = request.body.as_ref().map(|b| b.content.clone()).unwrap_or_default();
        self.body.update(cx, |s, cx| s.set_value(body, window, cx));

        let cache = self.cache(cx).cloned();
        let state = self.responses.entry(path.clone()).or_default();
        state.cache_key = cache_key;
        if let Some(cache) = cache {
            state.restore_from(&cache);
        }

        self.path = Some(path);
        self.saved = Some(request);
        self.dirty = false;
        self.show_response(window, cx);
    }

    /// Clears the editor, e.g. after the open request's collection was closed.
    pub fn unload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path = None;
        self.saved = None;
        self.dirty = false;
        for input in [&self.name, &self.url] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        for editor in [&self.headers, &self.body, &self.response_body, &self.response_headers] {
            editor.update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    /// The request as currently shown in the editor.
    fn current(&self, cx: &App) -> RequestFile {
        let saved = self.saved.clone().unwrap_or_else(|| RequestFile::new(""));
        let headers = headers_from_text(&self.headers.read(cx).value());
        let content = self.body.read(cx).value().to_string();
        let body = if content.trim().is_empty() {
            None
        } else {
            let kind = saved.body.as_ref().map(|b| b.kind).unwrap_or_else(|| {
                let content_type = headers.iter().find(|h| h.name.eq_ignore_ascii_case("content-type"));
                match content_type {
                    Some(h) => BodyKind::from_content_type(&h.value),
                    None if serde_json::from_str::<serde_json::Value>(&content).is_ok() => BodyKind::Json,
                    None => BodyKind::Text,
                }
            });
            Some(Body { kind, content })
        };
        RequestFile {
            name: self.name.read(cx).value().trim().to_string(),
            method: self
                .method
                .read(cx)
                .selected_value()
                .copied()
                .unwrap_or("GET")
                .to_string(),
            url: self.url.read(cx).value().to_string(),
            headers,
            body,
            order: saved.order,
        }
    }

    /// Computed from the inputs rather than cached, so a save right after a keystroke
    /// never misses the edit.
    fn is_modified(&self, cx: &App) -> bool {
        self.saved.is_some() && self.saved.as_ref() != Some(&self.current(cx))
    }

    fn update_dirty(&mut self, cx: &mut Context<Self>) {
        let dirty = self.is_modified(cx);
        if dirty != self.dirty {
            self.dirty = dirty;
            cx.notify();
        }
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if !self.is_modified(cx) {
            return;
        }
        let request = self.current(cx);
        match write_yaml(&path, &request) {
            Ok(()) => {
                self.saved = Some(request);
                self.dirty = false;
                cx.emit(RequestEditorEvent::Saved(path));
            }
            Err(e) => cx.emit(RequestEditorEvent::Error(
                t!("request.could_not_save", error = format!("{e:#}")).to_string(),
            )),
        }
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let state = self.responses.entry(path.clone()).or_default();
        if state.sending {
            return;
        }
        state.sending = true;
        state.missing_variables.clear();
        let file = self.current(cx);
        let mut variables = self.variables.clone();
        let secrets = self.secrets.clone();
        let store = self.secret_store.clone();
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let (result, missing) = cx
                .background_executor()
                .spawn(async move {
                    // Secret values are fetched only now, used for this one request, and dropped.
                    if !secrets.is_empty() {
                        match store {
                            Some(store) => match store.get_all(&secrets).await {
                                Ok(found) => variables.extend(found),
                                Err(e) => {
                                    let error = format!("{e:#}");
                                    return (
                                        Err(t!("request.could_not_read_secrets", error = error).to_string()),
                                        Vec::new(),
                                    );
                                }
                            },
                            None => return (Err(t!("secrets.store_unavailable").to_string()), Vec::new()),
                        }
                    }
                    let (request, missing) = Request::resolve(&file, &variables);
                    (http::send(&request), missing)
                })
                .await;
            let stored = StoredResponse::from_result(&result, response_cache::now());
            this.update_in(cx, |this, window, cx| {
                // The response belongs to the request that sent it, whichever one is open now.
                let cache = this.cache(cx).cloned();
                let state = this.responses.entry(path.clone()).or_default();
                state.finish(stored.clone(), missing);
                if let (Some(cache), Some(key)) = (cache, state.cache_key.clone()) {
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(e) = cache.save(&key, &stored) {
                                eprintln!("could not cache response: {e:#}");
                            }
                        })
                        .detach();
                }
                if this.path.as_ref() == Some(&path) {
                    this.show_response(window, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn describe_missing(&self, missing: &[String]) -> String {
        let (secrets, plain): (Vec<_>, Vec<_>) = missing
            .iter()
            .cloned()
            .partition(|name| self.secrets.contains_key(name));
        let mut parts = Vec::new();
        if !plain.is_empty() {
            parts.push(t!("request.undefined", names = plain.join(", ")).to_string());
        }
        if !secrets.is_empty() {
            parts.push(t!("request.secret_not_set", names = secrets.join(", ")).to_string());
        }
        parts.join("  ·  ")
    }

    fn move_header_to_secret(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.save(cx);
        cx.emit(RequestEditorEvent::MoveHeaderToSecret { path, index });
    }

    fn render_credential_warning(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let theme = cx.theme().clone();
        let headers = headers_from_text(&self.headers.read(cx).value());
        let buttons: Vec<_> = headers
            .iter()
            .enumerate()
            .filter(|(_, header)| is_literal_credential(header))
            .map(|(index, header)| {
                Button::new(("move-header-to-secret", index))
                    .xsmall()
                    .warning()
                    .label(t!("request.move_header_to_secret", header = header.name).to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.move_header_to_secret(index, cx)))
            })
            .collect();
        (!buttons.is_empty()).then(|| {
            h_flex()
                .flex_wrap()
                .gap_2()
                .text_xs()
                .text_color(theme.warning)
                .child(t!("request.literal_credential").to_string())
                .children(buttons)
        })
    }

    fn render_status(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let state = self.state();
        let sending = state.is_some_and(|s| s.sending);
        let response = state.and_then(|s| s.response.as_ref());
        let (line, color) = match response.map(|r| (r, &r.outcome)) {
            _ if sending => (t!("request.status_sending").to_string(), theme.muted_foreground),
            None => (t!("request.status_none").to_string(), theme.muted_foreground),
            Some((_, Outcome::Error { .. })) => (t!("request.status_failed").to_string(), theme.danger),
            Some((
                r,
                Outcome::Response {
                    status,
                    reason,
                    body_size,
                    ..
                },
            )) => (
                format!(
                    "{status} {reason}  ·  {}  ·  {}",
                    format_duration(r.elapsed_ms),
                    format_size(*body_size)
                ),
                if *status < 400 { theme.success } else { theme.danger },
            ),
        };
        let restored = state.filter(|s| s.restored && !sending).and(response);
        let truncated = matches!(
            response.map(|r| &r.outcome),
            Some(Outcome::Response { truncated: true, .. })
        );
        let missing = state.map(|s| s.missing_variables.clone()).unwrap_or_default();
        h_flex()
            .flex_1()
            .min_w_0()
            .flex_wrap()
            .gap_x_3()
            .text_sm()
            .child(div().text_color(color).child(line))
            .when_some(restored, |this, response| {
                this.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(saved_age(response_cache::now().saturating_sub(response.received_at))),
                )
            })
            .when(truncated, |this| {
                this.child(
                    div()
                        .text_color(theme.warning)
                        .child(t!("request.truncated").to_string()),
                )
            })
            .when(!missing.is_empty(), |this| {
                this.child(div().text_color(theme.warning).child(self.describe_missing(&missing)))
            })
    }
}

impl Focusable for RequestEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RequestEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        if self.path.is_none() {
            return v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .text_color(theme.muted_foreground)
                .child(t!("request.empty_state").to_string())
                .into_any_element();
        }

        let label = |text: String| div().text_xs().text_color(theme.muted_foreground).child(text);
        let response_tab = self.response_tab;
        let header_count = match self.state().and_then(ResponseState::outcome) {
            Some(Outcome::Response { headers, .. }) => {
                t!("request.headers_tab_count", count = headers.len()).to_string()
            }
            _ => t!("request.headers_tab").to_string(),
        };
        let sending = self.state().is_some_and(|s| s.sending);

        v_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &SaveRequest, _, cx| this.save(cx)))
            .on_action(cx.listener(|this, _: &SendRequest, window, cx| this.send(window, cx)))
            .size_full()
            .p_3()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(text_input(&self.name)))
                    .child(
                        div().text_xs().text_color(theme.muted_foreground).child(
                            if self.dirty {
                                t!("request.unsaved")
                            } else {
                                t!("request.saved")
                            }
                            .to_string(),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w_32().child(Select::new(&self.method)))
                    .child(div().flex_1().child(text_input(&self.url)))
                    .child(
                        Button::new("send")
                            .primary()
                            .label(t!("request.send").to_string())
                            .tooltip(t!("request.send_shortcut").to_string())
                            .loading(sending)
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(label(t!("request.headers_label").to_string()))
                            .child(code_editor(&self.headers).h_32())
                            .children(self.render_credential_warning(cx))
                            .child(label(t!("request.body").to_string()))
                            .child(code_editor(&self.body).flex_1().min_h_0()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex().gap_2().child(self.render_status(cx)).child(
                                    div().flex_none().child(
                                        TabBar::new("response-tabs")
                                            .segmented()
                                            .small()
                                            .selected_index(response_tab)
                                            .child(Tab::new().label(t!("request.body").to_string()))
                                            .child(Tab::new().label(header_count))
                                            .on_click(cx.listener(|this, index: &usize, _, cx| {
                                                this.response_tab = *index;
                                                cx.notify();
                                            })),
                                    ),
                                ),
                            )
                            .child(
                                readonly_editor(if response_tab == 0 {
                                    &self.response_body
                                } else {
                                    &self.response_headers
                                })
                                .flex_1()
                                .min_h_0(),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn format_duration(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.2} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

/// "saved 5 min ago" and similar, for a response restored from the cache.
fn saved_age(seconds: u64) -> String {
    match seconds {
        0..60 => t!("request.saved_just_now").to_string(),
        60..3600 => t!("request.saved_minutes_ago", count = seconds / 60).to_string(),
        3600..86400 => t!("request.saved_hours_ago", count = seconds / 3600).to_string(),
        _ => t!("request.saved_days_ago", count = seconds / 86400).to_string(),
    }
}

fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
