//! The request editor on the right: edit, save, send, and show the response.

mod highlight;
mod json_filter;
mod schema;
mod sse;
mod ws;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
use crate::graphql::SchemaCache;
use crate::http::{self, Request};
use crate::model::{Body, BodyKind, Graphql, RequestFile, Variables, headers_from_text, headers_to_text};
use crate::response_cache::{self, CacheKey, Outcome, ResponseCache, StoredResponse};
use crate::secret_store::{SecretRef, SecretStore};
use crate::settings::AppSettings;
use crate::storage::write_yaml;
use crate::transport;
use crate::ui::{code_editor, readonly_editor, single_line_editor, text_input};

pub const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
/// The last entry in the method menu: a POST whose body is a GraphQL query and variables.
const GRAPHQL: &str = "GraphQL";
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

/// Bodies beyond this are not kept in memory (the size is still counted).
const MAX_BODY_IN_MEMORY: usize = 50 * 1024 * 1024;
/// While a body streams in, repaint at most this often.
const PROGRESS_REPAINT: Duration = Duration::from_millis(100);

/// Response and send state for one request, kept while the app runs so switching between
/// requests never loses (or misplaces) a response.
#[derive(Default)]
struct ResponseState {
    response: Option<StoredResponse>,
    /// Loaded from the cache rather than received in this session.
    restored: bool,
    missing_variables: Vec<String>,
    cache_key: Option<CacheKey>,
    /// The exchange in flight, if any. Dropping it cancels the request.
    live: Option<Live>,
    /// The Server-Sent Events log, when the last response was an event stream.
    sse: Option<sse::SseLog>,
    /// The message timeline of a WebSocket request.
    ws: Option<ws::WsLog>,
}

/// A request ready to send, the variables it lacked, and the variables (with secrets) used.
type Resolved = (Request, Vec<String>, Variables);

/// Status code, reason phrase and headers of a response.
type Head = (u16, String, Vec<(String, String)>);

/// A request that is still being sent or received.
struct Live {
    /// Distinguishes this send from a later one, so late events from a cancelled send are ignored.
    id: u64,
    handle: Option<transport::Handle>,
    started: Instant,
    head: Option<Head>,
    body: Vec<u8>,
    bytes: usize,
    truncated: bool,
    last_repaint: Instant,
    /// Reconnecting to an event stream: keep the existing log and send `Last-Event-ID`.
    resume: bool,
    websocket: bool,
    /// Resolved variables (including secret values), for messages sent on a WebSocket. Kept
    /// in memory only while the connection is open.
    variables: Variables,
}

impl Live {
    fn new(id: u64, resume: bool) -> Self {
        let now = Instant::now();
        Self {
            id,
            handle: None,
            started: now,
            head: None,
            body: Vec::new(),
            bytes: 0,
            truncated: false,
            last_repaint: now,
            resume,
            websocket: false,
            variables: Variables::new(),
        }
    }

    /// Whether enough time has passed to repaint progress; resets the clock if so.
    fn should_repaint(&mut self) -> bool {
        if self.last_repaint.elapsed() < PROGRESS_REPAINT {
            return false;
        }
        self.last_repaint = Instant::now();
        true
    }

    fn append(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len();
        if self.body.len() + chunk.len() <= MAX_BODY_IN_MEMORY {
            self.body.extend_from_slice(chunk);
        } else {
            self.truncated = true;
        }
    }
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
            && self.live.is_none()
            && let Some(key) = &self.cache_key
            && let Some(response) = cache.load(key)
        {
            self.response = Some(response);
            self.restored = true;
        }
    }

    fn finish(&mut self, response: StoredResponse) {
        self.response = Some(response);
        self.restored = false;
        self.live = None;
    }

    fn live(&mut self, id: u64) -> Option<&mut Live> {
        self.live.as_mut().filter(|live| live.id == id)
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
    /// A one-line code editor rather than a plain input, so `{{variables}}` can be coloured.
    url: Entity<EditorState>,
    headers: Entity<EditorState>,
    body: Entity<EditorState>,
    graphql_query: Entity<EditorState>,
    graphql_variables: Entity<EditorState>,
    operation_name: Entity<InputState>,

    response_tab: usize,
    response_body: Entity<EditorState>,
    /// A JSONPath expression narrowing the response body, remembered per request.
    response_filter: Entity<InputState>,
    response_filters: HashMap<PathBuf, String>,
    /// The shown response parsed as JSON, keyed by request and receive time, so typing a
    /// filter doesn't reparse a large body on every keystroke.
    parsed_body: Option<(PathBuf, u64, Option<std::rc::Rc<serde_json::Value>>)>,
    filter_status: Option<json_filter::Filtered>,
    response_headers: Entity<EditorState>,
    stream_filter: Entity<InputState>,
    stream_detail: Entity<EditorState>,
    responses: HashMap<PathBuf, ResponseState>,
    response_cache: ResponseCache,
    /// GraphQL schemas by [`SchemaCache::key`], and the one the query editor uses.
    schemas: HashMap<String, schema::SchemaState>,
    schema_cache: SchemaCache,
    schema_slot: schema::SchemaSlot,
    /// Types opened in the Schema tab, most recent last.
    schema_nav: Vec<String>,
    schema_filter: Entity<InputState>,
    /// Editors whose `{{variables}}` are coloured, with their decorations and whether they
    /// hold a URL.
    highlighted: Vec<(
        Entity<EditorState>,
        gpui_kit::base::input::TextDecorationCollection,
        bool,
    )>,
    next_send_id: u64,
}

impl RequestEditor {
    pub fn new(
        response_cache: ResponseCache,
        schema_cache: SchemaCache,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(t!("request.name_placeholder").to_string()));
        let method = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(METHODS.iter().copied().chain([GRAPHQL]).collect::<Vec<_>>()),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });
        let url = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("text")
                .line_number(false)
                .folding(false)
                .soft_wrap(false)
                .scroll_beyond_last_line(None)
                .submit_on_enter(true)
                .placeholder("{{base_url}}/path")
        });
        let headers = cx.new(|cx| EditorState::new(window, cx).language("text"));
        let body = cx.new(|cx| EditorState::new(window, cx).language("json"));
        let schema_slot = schema::SchemaSlot::default();
        let graphql_query = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).language("graphql");
            let assist = std::rc::Rc::new(schema::GraphqlAssist {
                schema: schema_slot.clone(),
            });
            state.lsp_mut().completion_provider = Some(assist.clone());
            state.lsp_mut().hover_provider = Some(assist);
            state
        });
        let schema_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("request.schema_filter_placeholder").to_string()));
        let graphql_variables = cx.new(|cx| EditorState::new(window, cx).language("json"));
        let operation_name = cx
            .new(|cx| InputState::new(window, cx).placeholder(t!("request.graphql_operation_placeholder").to_string()));
        let response_body = cx.new(|cx| EditorState::new(window, cx).language("json"));
        let response_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("request.filter_placeholder").to_string()));
        cx.subscribe_in(
            &response_filter,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event
                    && let Some(path) = this.path.clone()
                {
                    let expression = input.read(cx).value().to_string();
                    if expression.trim().is_empty() {
                        this.response_filters.remove(&path);
                    } else {
                        this.response_filters.insert(path, expression);
                    }
                    this.show_response(window, cx);
                }
            },
        )
        .detach();
        let response_headers = cx.new(|cx| EditorState::new(window, cx).language("text"));
        let stream_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("request.stream_filter_placeholder").to_string()));
        for filter in [&stream_filter, &schema_filter] {
            cx.subscribe(filter, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            })
            .detach();
        }
        let stream_detail = cx.new(|cx| EditorState::new(window, cx).language("json"));

        cx.subscribe_in(&url, window, |this, url, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { secondary: false, .. } => this.send(window, cx),
            InputEvent::Change => {
                // A URL is one line: Shift+Enter or a pasted line break shouldn't split it.
                let value = url.read(cx).value();
                if value.contains(['\n', '\r']) {
                    let joined = value.replace(['\n', '\r'], "");
                    url.update(cx, |s, cx| s.replace_all(joined, window, cx));
                }
                this.update_dirty(cx);
                this.sync_schema(cx);
                this.rehighlight(url, cx);
            }
            _ => {}
        })
        .detach();
        for input in [&name, &operation_name] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.update_dirty(cx);
                }
            })
            .detach();
        }
        cx.subscribe(&graphql_query, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.check_query(cx);
            }
        })
        .detach();
        for editor in [&headers, &body, &graphql_query, &graphql_variables] {
            cx.subscribe(editor, |this, editor, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.update_dirty(cx);
                    this.rehighlight(&editor, cx);
                }
            })
            .detach();
        }
        let highlighted = [&url, &headers, &body, &graphql_query, &graphql_variables]
            .into_iter()
            .map(|editor| {
                let decorations = editor.update(cx, |state, cx| state.create_decorations_collection(Vec::new(), cx));
                (editor.clone(), decorations, editor == &url)
            })
            .collect();
        // Colours come from the theme, which follows Omarchy theme switches.
        cx.observe_global::<gpui_kit::component::Theme>(|this, cx| this.rehighlight_all(cx))
            .detach();
        cx.subscribe(&method, |this, _, _: &SelectEvent<SearchableVec<&'static str>>, cx| {
            this.update_dirty(cx);
            this.sync_schema(cx);
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
            graphql_query,
            graphql_variables,
            operation_name,
            response_tab: 0,
            response_body,
            response_filter,
            response_filters: HashMap::new(),
            parsed_body: None,
            filter_status: None,
            response_headers,
            stream_filter,
            stream_detail,
            responses: HashMap::new(),
            response_cache,
            schemas: HashMap::new(),
            schema_cache,
            schema_slot,
            schema_nav: Vec::new(),
            schema_filter,
            highlighted,
            next_send_id: 0,
        }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// After a request file or a folder of them moved on disk: keeps responses, and the open
    /// request, with their new paths.
    pub fn moved(&mut self, from: &Path, to: &Path) {
        let remap = |path: &Path| path.strip_prefix(from).ok().map(|rest| to.join(rest));
        let moved: Vec<PathBuf> = self.responses.keys().filter(|p| p.starts_with(from)).cloned().collect();
        for old in moved {
            if let (Some(state), Some(new)) = (self.responses.remove(&old), remap(&old)) {
                self.responses.insert(new, state);
            }
        }
        if let Some(new) = self.path.as_deref().and_then(remap) {
            self.path = Some(new);
        }
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
        self.stream_filter.update(cx, |s, cx| {
            s.set_placeholder(t!("request.stream_filter_placeholder").to_string(), window, cx)
        });
        self.response_filter.update(cx, |s, cx| {
            s.set_placeholder(t!("request.filter_placeholder").to_string(), window, cx)
        });
        self.operation_name.update(cx, |s, cx| {
            s.set_placeholder(t!("request.graphql_operation_placeholder").to_string(), window, cx)
        });
        self.schema_filter.update(cx, |s, cx| {
            s.set_placeholder(t!("request.schema_filter_placeholder").to_string(), window, cx)
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

    /// Events received and whether the stream has ended, when showing an event stream.
    #[cfg(test)]
    pub fn sse_summary(&self) -> Option<(usize, bool)> {
        self.state()
            .and_then(|s| s.sse.as_ref())
            .map(|log| (log.total, log.ended.is_some()))
    }

    /// Messages, whether connected, and whether the connection has ended.
    #[cfg(test)]
    pub fn ws_summary(&self) -> Option<(usize, bool, bool)> {
        self.state()
            .and_then(|s| s.ws.as_ref())
            .map(|log| (log.total, log.connected, log.ended.is_some()))
    }

    #[cfg(test)]
    pub fn add_template_for_test(&mut self, name: &str, cx: &mut Context<Self>) {
        self.add_template(name.to_string(), cx);
    }

    #[cfg(test)]
    pub fn stream_detail_text(&self, cx: &App) -> String {
        self.stream_detail.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub fn show_schema_tab_for_test(&mut self, cx: &mut Context<Self>) {
        self.response_tab = 2;
        cx.notify();
    }

    /// User-defined types in the current request's schema, once there is one.
    #[cfg(test)]
    pub fn schema_type_count(&self, cx: &App) -> Option<usize> {
        let cached = self.schema_state(cx)?.schema.as_ref()?;
        Some(cached.schema.user_types().count())
    }

    /// Suggestions the query editor would offer, from the schema it's been given.
    #[cfg(test)]
    pub fn complete_for_test(&self, text: &str, offset: usize) -> Vec<String> {
        let Some(cached) = self.schema_slot.borrow().clone() else {
            return Vec::new();
        };
        let (_, suggestions) = crate::graphql::assist::complete(&cached.schema, text, offset);
        suggestions.into_iter().map(|s| s.label).collect()
    }

    /// Underlined problems in the query editor, as (message, underlined text).
    #[cfg(test)]
    pub fn query_problems_for_test(&self, cx: &App) -> Vec<(String, String)> {
        let state = self.graphql_query.read(cx);
        let text = state.value().to_string();
        state
            .diagnostics()
            .map(|set| {
                set.range(0..text.len())
                    .map(|entry| (entry.message.to_string(), text[entry.range.clone()].to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn response_filter_for_test(&self) -> Entity<InputState> {
        self.response_filter.clone()
    }

    #[cfg(test)]
    pub fn response_body_text(&self, cx: &App) -> String {
        self.response_body.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub fn url_for_test(&self) -> Entity<EditorState> {
        self.url.clone()
    }

    /// Coloured spans in the URL and headers, as (text, kind) pairs.
    #[cfg(test)]
    pub fn highlights_for_test(&self, cx: &App) -> Vec<(String, String)> {
        self.highlighted
            .iter()
            .take(2)
            .flat_map(|(editor, decorations, _)| {
                let text = editor.read(cx).value().to_string();
                let spans = self.spans(&text, editor == &self.url);
                assert_eq!(
                    decorations.get_ranges(cx),
                    spans.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(),
                    "decorations match the text"
                );
                spans
                    .into_iter()
                    .map(move |(range, kind)| (text[range].to_string(), format!("{kind:?}")))
            })
            .collect()
    }

    #[cfg(test)]
    pub fn graphql_query_for_test(&self) -> Entity<EditorState> {
        self.graphql_query.clone()
    }

    #[cfg(test)]
    pub fn schema_nav_for_test(&self) -> Vec<String> {
        self.schema_nav.clone()
    }

    #[cfg(test)]
    pub fn is_modified_for_test(&self, cx: &App) -> bool {
        self.is_modified(cx)
    }

    #[cfg(test)]
    pub fn received_bytes(&self) -> usize {
        self.state().and_then(|s| s.live.as_ref()).map_or(0, |live| live.bytes)
    }

    #[cfg(test)]
    pub fn is_sending(&self) -> bool {
        self.state().is_some_and(|s| s.live.is_some())
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

    fn state_mut(&mut self) -> Option<&mut ResponseState> {
        self.path.as_ref().and_then(|p| self.responses.get_mut(p))
    }

    /// Puts the current request's response (if any) into the response panes.
    /// The response body narrowed by `expression`, updating the filter status. Falls back to
    /// the whole body when the expression doesn't apply.
    fn filtered_body(&mut self, expression: &str) -> String {
        let (Some(path), Some(response)) = (self.path.clone(), self.state().and_then(|s| s.response.as_ref())) else {
            return String::new();
        };
        let Outcome::Response { body, .. } = &response.outcome else {
            return String::new();
        };
        let received_at = response.received_at;
        let fresh = matches!(&self.parsed_body, Some((p, at, _)) if *p == path && *at == received_at);
        if !fresh {
            let parsed = json_filter::parse_body(body).map(std::rc::Rc::new);
            self.parsed_body = Some((path, received_at, parsed));
        }
        let parsed = self.parsed_body.as_ref().and_then(|(_, _, v)| v.clone());
        let result = json_filter::filter(parsed.as_deref(), expression);
        let text = match &result {
            json_filter::Filtered::Matches { text, .. } => text.clone(),
            _ => self
                .state()
                .and_then(ResponseState::outcome)
                .map(|outcome| match outcome {
                    Outcome::Response { body, .. } => http::pretty_body(body),
                    Outcome::Error { message } => message.clone(),
                })
                .unwrap_or_default(),
        };
        self.filter_status = Some(result);
        text
    }

    fn show_response(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_status = None;
        let expression = self.path.as_ref().and_then(|p| self.response_filters.get(p)).cloned();
        let (body, headers) = match self.state().and_then(ResponseState::outcome) {
            Some(Outcome::Response { headers, body, .. }) => (
                expression.is_none().then(|| http::pretty_body(body)),
                headers
                    .iter()
                    .map(|(n, v)| format!("{n}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Some(Outcome::Error { message }) => (Some(message.clone()), String::new()),
            None => (Some(String::new()), String::new()),
        };
        let body = match (body, expression) {
            (Some(body), _) => body,
            (None, Some(expression)) => self.filtered_body(&expression),
            (None, None) => String::new(),
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
    pub fn set_variables(&mut self, variables: Variables, secrets: IndexMap<String, SecretRef>, cx: &mut App) {
        self.variables = variables;
        self.secrets = secrets;
        self.rehighlight_all(cx);
    }

    fn rehighlight(&self, editor: &Entity<EditorState>, cx: &mut App) {
        if let Some((editor, decorations, url)) = self.highlighted.iter().find(|(e, _, _)| e == editor) {
            self.highlight(editor, decorations, *url, cx);
        }
    }

    fn rehighlight_all(&self, cx: &mut App) {
        for (editor, decorations, url) in &self.highlighted {
            self.highlight(editor, decorations, *url, cx);
        }
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
        let method_index = match &request.graphql {
            Some(_) => METHODS.len(),
            None => METHODS
                .iter()
                .position(|m| m.eq_ignore_ascii_case(&request.method))
                .unwrap_or(0),
        };
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
        let graphql = request.graphql.clone().unwrap_or_default();
        self.graphql_query
            .update(cx, |s, cx| s.set_value(graphql.query, window, cx));
        self.graphql_variables
            .update(cx, |s, cx| s.set_value(graphql.variables, window, cx));
        self.operation_name.update(cx, |s, cx| {
            s.set_value(graphql.operation_name.unwrap_or_default(), window, cx)
        });

        let cache = self.cache(cx).cloned();
        let state = self.responses.entry(path.clone()).or_default();
        state.cache_key = cache_key;
        if let Some(cache) = cache {
            state.restore_from(&cache);
        }

        let filter = self.response_filters.get(&path).cloned().unwrap_or_default();
        self.response_filter.update(cx, |s, cx| s.set_value(filter, window, cx));
        self.path = Some(path);
        self.saved = Some(request);
        self.dirty = false;
        self.schema_nav.clear();
        self.sync_schema(cx);
        self.check_query(cx);
        self.rehighlight_all(cx);
        self.show_response(window, cx);
    }

    /// Clears the editor, e.g. after the open request's collection was closed.
    pub fn unload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path = None;
        self.saved = None;
        self.dirty = false;
        for input in [&self.name, &self.operation_name] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        for editor in [
            &self.url,
            &self.headers,
            &self.body,
            &self.graphql_query,
            &self.graphql_variables,
            &self.response_body,
            &self.response_headers,
        ] {
            editor.update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Whether "GraphQL" is picked in the method menu.
    fn is_graphql(&self, cx: &App) -> bool {
        self.method.read(cx).selected_value() == Some(&GRAPHQL)
    }

    /// The request as currently shown in the editor.
    fn current(&self, cx: &App) -> RequestFile {
        let saved = self.saved.clone().unwrap_or_else(|| RequestFile::new(""));
        let headers = headers_from_text(&self.headers.read(cx).value());
        if self.is_graphql(cx) {
            let operation_name = self.operation_name.read(cx).value().trim().to_string();
            return RequestFile {
                name: self.name.read(cx).value().trim().to_string(),
                method: "POST".into(),
                url: self.url.read(cx).value().to_string(),
                headers,
                body: None,
                order: saved.order,
                messages: saved.messages,
                graphql: Some(Graphql {
                    query: self.graphql_query.read(cx).value().to_string(),
                    variables: self.graphql_variables.read(cx).value().to_string(),
                    operation_name: (!operation_name.is_empty()).then_some(operation_name),
                }),
            };
        }
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
            messages: saved.messages,
            graphql: None,
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

    fn timeout(&self, cx: &App) -> Duration {
        Duration::from_secs(AppSettings::get(cx).request_timeout_secs.max(1))
    }

    /// Resolves `file` with the active variables and secrets, off the UI thread. Secret
    /// values are fetched only now, used for this one request, and dropped. Also returns the
    /// missing variable names and the variables used.
    fn resolve_in_background(&self, file: RequestFile, cx: &App) -> Task<Result<Resolved, String>> {
        let mut variables = self.variables.clone();
        let secrets = self.secrets.clone();
        let store = self.secret_store.clone();
        cx.background_executor().spawn(async move {
            if !secrets.is_empty() {
                let store = store.ok_or_else(|| t!("secrets.store_unavailable").to_string())?;
                let found = store
                    .get_all(&secrets)
                    .await
                    .map_err(|e| t!("request.could_not_read_secrets", error = format!("{e:#}")).to_string())?;
                variables.extend(found);
            }
            let (request, missing) = Request::resolve(&file, &variables)?;
            Ok((request, missing, variables))
        })
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send_with(false, window, cx);
    }

    /// Sends the request. `resume` reconnects an ended event stream, keeping its log and
    /// sending the last event id.
    fn send_with(&mut self, resume: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let id = self.next_send_id;
        let websocket = self.is_websocket(cx);
        let state = self.responses.entry(path.clone()).or_default();
        if state.live.is_some() {
            return;
        }
        self.next_send_id += 1;
        let last_event_id = state
            .sse
            .as_ref()
            .filter(|_| resume)
            .and_then(|log| log.last_event_id.clone());
        let mut live = Live::new(id, resume);
        live.websocket = websocket;
        state.live = Some(live);
        if websocket {
            state.ws = Some(ws::WsLog::new());
            state.sse = None;
        }
        state.missing_variables.clear();
        let mut file = self.current(cx);
        if websocket {
            // The composer's text is sent as messages, not with the handshake.
            file.body = None;
            file.graphql = None;
        }
        let resolving = self.resolve_in_background(file, cx);
        let timeout = self.timeout(cx);
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let resolved = resolving.await;
            let events = this.update(cx, |this, cx| {
                let state = this.responses.entry(path.clone()).or_default();
                let (request, missing, variables) = match resolved {
                    Ok(resolved) => resolved,
                    Err(message) => {
                        if state.live(id).is_some() {
                            state.finish(StoredResponse::failed(response_cache::now(), message));
                            cx.notify();
                        }
                        return None;
                    }
                };
                state.missing_variables = missing;
                let live = state.live(id)?; // cancelled while resolving
                let (handle, events) = if live.websocket {
                    live.variables = variables;
                    transport::start_websocket(request, timeout)
                } else {
                    transport::start_http(request, timeout, last_event_id)
                };
                live.handle = Some(handle);
                Some(events)
            });
            let Ok(Some(events)) = events else {
                return;
            };
            while let Ok(event) = events.recv().await {
                let keep_going = this
                    .update_in(cx, |this, window, cx| {
                        this.on_transport_event(&path, id, event, window, cx)
                    })
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
            }
            // The channel closed without a result: the connection went away.
            this.update_in(cx, |this, window, cx| {
                let message = t!("request.connection_closed").to_string();
                this.on_transport_event(&path, id, transport::Event::Failed(message), window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Applies one event from the send `id` of `path`. Returns false once the send is over or
    /// was cancelled.
    fn on_transport_event(
        &mut self,
        path: &PathBuf,
        id: u64,
        event: transport::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let cache = self.cache(cx).cloned();
        let current = self.path.as_ref() == Some(path);
        let state = self.responses.entry(path.clone()).or_default();
        let Some(live) = state.live.as_mut().filter(|live| live.id == id) else {
            return false;
        };
        let finished = match event {
            transport::Event::Head {
                status,
                reason,
                headers,
                event_stream,
                ..
            } => {
                let status_line = format!("{status} {reason}");
                let headers_text = headers
                    .iter()
                    .map(|(n, v)| format!("{n}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if live.websocket {
                    if let Some(log) = &mut state.ws {
                        log.status = status_line;
                        log.connected = true;
                    }
                } else if event_stream {
                    match &mut state.sse {
                        Some(log) if live.resume => log.ended = None,
                        _ => state.sse = Some(sse::SseLog::new(status_line)),
                    }
                } else {
                    state.sse = None;
                }
                live.head = Some((status, reason, headers));
                if current {
                    self.response_headers
                        .update(cx, |s, cx| s.set_value(headers_text, window, cx));
                }
                None
            }
            transport::Event::Chunk(chunk) => {
                live.append(&chunk);
                if !live.should_repaint() {
                    return true;
                }
                None
            }
            transport::Event::Sse(event) => {
                live.bytes += event.data.len();
                if let Some(log) = &mut state.sse {
                    log.push(event);
                }
                if !live.should_repaint() {
                    return true;
                }
                None
            }
            transport::Event::Ws(message) => {
                if let Some(log) = &mut state.ws {
                    log.push(message);
                }
                None
            }
            transport::Event::Done { elapsed, bytes } => {
                if live.websocket
                    && let Some(log) = &mut state.ws
                {
                    log.connected = false;
                    log.ended = Some(sse::Ending::Closed);
                    state.live = None;
                    cx.notify();
                    return false;
                }
                if let Some(log) = &mut state.sse {
                    log.ended = Some(sse::Ending::Closed);
                    state.live = None;
                    cx.notify();
                    return false;
                }
                let (status, reason, headers) = live.head.take().unwrap_or_default();
                Some(StoredResponse {
                    received_at: response_cache::now(),
                    elapsed_ms: elapsed.as_millis() as u64,
                    outcome: Outcome::Response {
                        status,
                        reason,
                        headers,
                        body: String::from_utf8_lossy(&live.body).into_owned(),
                        body_size: bytes,
                        truncated: live.truncated,
                    },
                })
            }
            transport::Event::Failed(message) => {
                if live.websocket
                    && let Some(log) = &mut state.ws
                {
                    log.connected = false;
                    log.ended = Some(sse::Ending::Failed(message));
                    state.live = None;
                    cx.notify();
                    return false;
                }
                if live.head.is_some()
                    && let Some(log) = &mut state.sse
                {
                    log.ended = Some(sse::Ending::Failed(message));
                    state.live = None;
                    cx.notify();
                    return false;
                }
                Some(StoredResponse::failed(response_cache::now(), message))
            }
        };
        let Some(stored) = finished else {
            cx.notify();
            return true;
        };
        state.finish(stored.clone());
        if let (Some(cache), Some(key)) = (cache, state.cache_key.clone()) {
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = cache.save(&key, &stored) {
                        eprintln!("could not cache response: {e:#}");
                    }
                })
                .detach();
        }
        if current {
            self.show_response(window, cx);
        }
        cx.notify();
        false
    }

    /// Stops the current request's send, keeping whatever response it had before. A stopped
    /// event stream keeps its log so it can be reconnected.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.state_mut() {
            // An open WebSocket closes gracefully; the timeline records the close.
            if let Some(live) = &state.live
                && live.websocket
                && state.ws.as_ref().is_some_and(|log| log.connected)
                && live
                    .handle
                    .as_ref()
                    .is_some_and(|h| h.send(transport::WsPayload::Close(String::new())))
            {
                return;
            }
            if let Some(log) = state.ws.as_mut().filter(|log| log.ended.is_none()) {
                log.connected = false;
                log.ended = Some(sse::Ending::Stopped);
            }
            if state.live.take().is_some()
                && let Some(log) = &mut state.sse
                && log.ended.is_none()
            {
                log.ended = Some(sse::Ending::Stopped);
            }
            cx.notify();
        }
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

    /// The status line above the response. Event streams show their own status in their view,
    /// so this shows only warnings for them.
    fn render_filter_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (status, color) = match &self.filter_status {
            None => (String::new(), theme.muted_foreground),
            Some(json_filter::Filtered::Matches { count, .. }) => (
                t!("request.filter_matches", count = count).to_string(),
                if *count == 0 {
                    theme.warning
                } else {
                    theme.muted_foreground
                },
            ),
            Some(json_filter::Filtered::Invalid(error)) => (error.clone(), theme.danger),
            Some(json_filter::Filtered::NotJson) => (t!("request.filter_not_json").to_string(), theme.warning),
        };
        h_flex()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(text_input(&self.response_filter).small()),
            )
            .when(!status.is_empty(), |bar| {
                bar.child(
                    div()
                        .id("filter-status")
                        .test_support()
                        .max_w(px(220.))
                        .truncate()
                        .text_xs()
                        .text_color(color)
                        .child(status),
                )
            })
    }

    fn render_status(&self, event_stream: bool, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let state = self.state();
        let live = state.and_then(|s| s.live.as_ref());
        let sending = live.is_some();
        let response = state.and_then(|s| s.response.as_ref());
        let (line, color) = match response.map(|r| (r, &r.outcome)) {
            _ if let Some(live) = live => match &live.head {
                Some((status, reason, _)) => (
                    t!(
                        "request.status_receiving",
                        status = format!("{status} {reason}"),
                        size = format_size(live.bytes),
                        elapsed = format_duration(live.started.elapsed().as_millis() as u64)
                    )
                    .to_string(),
                    theme.muted_foreground,
                ),
                None => (t!("request.status_sending").to_string(), theme.muted_foreground),
            },
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
        // Wide enough for a status line; when the tabs don't fit beside it, they wrap below.
        h_flex()
            .flex_1()
            .min_w(px(240.))
            .flex_wrap()
            .gap_x_3()
            .text_sm()
            .when(!event_stream, |this| this.child(div().text_color(color).child(line)))
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
        let graphql = !self.is_websocket(cx) && self.is_graphql(cx);
        let response_tab = if !graphql && self.response_tab == 2 {
            0
        } else {
            self.response_tab
        };
        let header_count = match self.state().and_then(ResponseState::outcome) {
            Some(Outcome::Response { headers, .. }) => {
                t!("request.headers_tab_count", count = headers.len()).to_string()
            }
            _ => t!("request.headers_tab").to_string(),
        };
        let sending = self.state().is_some_and(|s| s.live.is_some());
        let websocket = self.is_websocket(cx);
        let sse_log = self.state().and_then(|s| s.sse.as_ref());
        let ws_log = self.state().and_then(|s| s.ws.as_ref()).filter(|_| websocket);
        let body_tab_label = match (ws_log, sse_log) {
            (Some(log), _) => t!("request.ws_messages_tab", count = log.total).to_string(),
            (None, Some(log)) => t!("request.sse_events_tab", count = log.total).to_string(),
            (None, None) => t!("request.body").to_string(),
        };
        let response_view = match (ws_log, sse_log) {
            _ if response_tab == 2 => self.render_schema(cx),
            (Some(log), _) if response_tab == 0 => self.render_ws(log, cx),
            (None, Some(log)) if response_tab == 0 => self.render_sse(log, sending, cx),
            _ if response_tab == 0
                && matches!(
                    self.state().and_then(ResponseState::outcome),
                    Some(Outcome::Response { .. })
                ) =>
            {
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .child(self.render_filter_bar(cx))
                    .child(readonly_editor(&self.response_body).flex_1().min_h_0())
                    .into_any_element()
            }
            _ => readonly_editor(if response_tab == 0 {
                &self.response_body
            } else {
                &self.response_headers
            })
            .flex_1()
            .min_h_0()
            .into_any_element(),
        };

        v_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &SaveRequest, _, cx| this.save(cx)))
            .on_action(cx.listener(|this, _: &SendRequest, window, cx| {
                if this.ws_connected() {
                    this.send_ws_message(cx)
                } else {
                    this.send(window, cx)
                }
            }))
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
                    .when(!websocket, |row| {
                        row.child(div().w_32().child(Select::new(&self.method)))
                    })
                    .child(div().flex_1().min_w_0().child(single_line_editor(&self.url)))
                    .child(
                        Button::new("send")
                            .when(!sending, |button| {
                                button
                                    .primary()
                                    .label(
                                        if websocket {
                                            t!("request.ws_connect")
                                        } else {
                                            t!("request.send")
                                        }
                                        .to_string(),
                                    )
                                    .tooltip(t!("request.send_shortcut").to_string())
                            })
                            .when(sending, |button| {
                                button.danger().label(
                                    if websocket {
                                        t!("request.ws_disconnect")
                                    } else {
                                        t!("request.cancel")
                                    }
                                    .to_string(),
                                )
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if sending {
                                    this.cancel(cx)
                                } else {
                                    this.send(window, cx)
                                }
                            })),
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
                            .when(graphql, |column| {
                                column
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(div().flex_1().child(label(t!("request.graphql_query").to_string())))
                                            .child(div().w_48().child(text_input(&self.operation_name).small())),
                                    )
                                    .child(code_editor(&self.graphql_query).flex_1().min_h_0())
                                    .child(label(t!("request.graphql_variables").to_string()))
                                    .child(code_editor(&self.graphql_variables).h_32())
                            })
                            .when(!graphql, |column| {
                                column.child(label(
                                    if websocket {
                                        t!("request.ws_message")
                                    } else {
                                        t!("request.body")
                                    }
                                    .to_string(),
                                ))
                            })
                            .when(!graphql, |column| {
                                column.child(code_editor(&self.body).flex_1().min_h_0())
                            })
                            .when(websocket, |column| column.child(self.render_composer_actions(cx))),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(self.render_status(sse_log.is_some() || ws_log.is_some(), cx))
                                    .child(
                                        div().flex_none().child(
                                            TabBar::new("response-tabs")
                                                .segmented()
                                                .small()
                                                .selected_index(response_tab)
                                                .child(Tab::new().label(body_tab_label))
                                                .child(Tab::new().label(header_count))
                                                .when(graphql, |bar| {
                                                    bar.child(Tab::new().label(t!("request.schema_tab").to_string()))
                                                })
                                                .on_click(cx.listener(|this, index: &usize, _, cx| {
                                                    this.response_tab = *index;
                                                    cx.notify();
                                                })),
                                        ),
                                    ),
                            )
                            .child(response_view),
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
