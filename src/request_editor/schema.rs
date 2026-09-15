//! GraphQL schemas in the request editor: fetching one by introspection, the Schema tab
//! for browsing types, and completions and hover docs in the query editor.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use gpui_kit::base::input::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{CompletionProvider, HoverProvider};
use gpui_kit::component::{ActiveTheme as _, IconName, Rope, RopeExt as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, Documentation,
    Hover, HoverContents, MarkupContent, MarkupKind, TextEdit,
};
use rust_i18n::t;

use super::RequestEditor;
use crate::graphql::assist::{self, SuggestionKind};
use crate::graphql::{
    CachedSchema, INTROSPECTION_OPERATION, INTROSPECTION_QUERY, OperationKind, Schema, SchemaCache, TypeDef, TypeKind,
};
use crate::model::Graphql;
use crate::response_cache;
use crate::transport;
use crate::ui::text_input;

/// Rows shown at once in the schema browser.
const MAX_ROWS: usize = 300;
/// Introspection results beyond this are refused rather than parsed.
const MAX_SCHEMA_BYTES: usize = 64 * 1024 * 1024;

/// The schema shared with the query editor's completion and hover providers.
pub(super) type SchemaSlot = Rc<RefCell<Option<Arc<CachedSchema>>>>;

/// What's known about one endpoint's schema.
#[derive(Default)]
pub(super) struct SchemaState {
    pub schema: Option<Arc<CachedSchema>>,
    /// Reading the cache or fetching.
    pub loading: bool,
    pub error: Option<String>,
    /// The introspection request in flight; dropping it cancels.
    fetch: Option<transport::Handle>,
    /// Bumped by each fetch and cancel, so a superseded task leaves the state alone.
    generation: u64,
}

impl RequestEditor {
    /// The schema cache key for the request as currently written.
    fn schema_key(&self, cx: &App) -> Option<String> {
        let path = self.path.as_ref()?;
        let owner = self
            .state()
            .and_then(|s| s.cache_key.as_ref())
            .and_then(|k| k.collection_id.clone())
            .unwrap_or_else(|| path.display().to_string());
        Some(SchemaCache::key(&owner, &self.url.read(cx).value()))
    }

    pub(super) fn schema_state(&self, cx: &App) -> Option<&SchemaState> {
        self.schemas.get(&self.schema_key(cx)?)
    }

    /// Underlines problems in the query against the current schema. The editor clears its
    /// diagnostics on every edit, so this runs after each change and when the schema changes.
    pub(super) fn check_query(&mut self, cx: &mut Context<Self>) {
        let schema = self.schema_slot.borrow().clone();
        self.graphql_query.update(cx, |state, cx| {
            let text = state.text().clone();
            let problems = schema
                .map(|cached| assist::validate(&cached.schema, &text.to_string()))
                .unwrap_or_default();
            let Some(diagnostics) = state.diagnostics_mut() else {
                return;
            };
            diagnostics.reset(&text);
            diagnostics.extend(problems.into_iter().map(|problem| {
                let range = text.offset_to_position(problem.range.start)..text.offset_to_position(problem.range.end);
                let severity = if problem.is_warning() {
                    DiagnosticSeverity::Warning
                } else {
                    DiagnosticSeverity::Error
                };
                Diagnostic::new(range, problem.message()).with_severity(severity)
            }));
            cx.notify();
        });
    }

    /// Points the query editor at the schema for the current URL, loading it from the cache
    /// the first time it's needed.
    pub(super) fn sync_schema(&mut self, cx: &mut Context<Self>) {
        if !self.is_graphql(cx) {
            self.schema_slot.replace(None);
            self.check_query(cx);
            return;
        }
        let Some(key) = self.schema_key(cx) else {
            return;
        };
        if let Some(state) = self.schemas.get(&key) {
            let changed = !option_arc_eq(&self.schema_slot.borrow(), &state.schema);
            self.schema_slot.replace(state.schema.clone());
            if changed {
                self.check_query(cx);
            }
            return;
        }
        self.schemas.insert(
            key.clone(),
            SchemaState {
                loading: true,
                ..Default::default()
            },
        );
        self.schema_slot.replace(None);
        self.check_query(cx);
        let cache = self.schema_cache.clone();
        cx.spawn(async move |this, cx| {
            let lookup = key.clone();
            let cached = cx
                .background_executor()
                .spawn(async move { cache.load(&lookup).map(Arc::new) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(state) = this.schemas.get_mut(&key)
                    && state.generation == 0
                {
                    state.loading = false;
                    state.schema = cached;
                    this.sync_schema(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Fetches the schema by sending the introspection query to the request's URL, with its
    /// headers, variables and secrets.
    pub(super) fn fetch_schema(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.schema_key(cx) else {
            return;
        };
        let mut file = self.current(cx);
        file.method = "POST".into();
        file.body = None;
        file.graphql = Some(Graphql {
            query: INTROSPECTION_QUERY.into(),
            variables: String::new(),
            operation_name: Some(INTROSPECTION_OPERATION.into()),
        });
        let resolving = self.resolve_in_background(file, cx);
        let state = self.schemas.entry(key.clone()).or_default();
        state.loading = true;
        state.error = None;
        state.fetch = None;
        state.generation += 1;
        let generation = state.generation;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let resolved = resolving.await?;
                let timeout = std::time::Duration::from_secs(resolved.settings.timeout_secs);
                let events = this
                    .update(cx, |this, _| {
                        let (handle, events) =
                            transport::start_http(resolved.request, timeout, None, Some(resolved.client));
                        if let Some(state) = this.schemas.get_mut(&key)
                            && state.generation == generation
                        {
                            state.fetch = Some(handle);
                        }
                        events
                    })
                    .map_err(|e| e.to_string())?;
                let mut status = (0, String::new());
                let mut body = Vec::new();
                loop {
                    match events.recv().await {
                        Ok(transport::Event::Head {
                            status: code, reason, ..
                        }) => status = (code, reason),
                        Ok(transport::Event::Chunk(chunk)) => {
                            body.extend_from_slice(&chunk);
                            if body.len() > MAX_SCHEMA_BYTES {
                                return Err(t!("request.schema_too_large").to_string());
                            }
                        }
                        Ok(transport::Event::Done { .. }) => break,
                        Ok(transport::Event::Failed(message)) => return Err(message),
                        Ok(_) => {}
                        Err(_) => return Err(t!("request.connection_closed").to_string()),
                    }
                }
                let schema = cx
                    .background_executor()
                    .spawn(async move {
                        let text = String::from_utf8_lossy(&body);
                        Schema::from_introspection(&text).map_err(|error| {
                            if (200..300).contains(&status.0) {
                                error
                            } else {
                                format!("{} {}: {error}", status.0, status.1)
                            }
                        })
                    })
                    .await?;
                Ok(CachedSchema {
                    fetched_at: response_cache::now(),
                    schema,
                })
            }
            .await;

            this.update(cx, |this, cx| {
                let Some(state) = this.schemas.get_mut(&key).filter(|s| s.generation == generation) else {
                    return; // cancelled or fetched again
                };
                state.fetch = None;
                state.loading = false;
                match result {
                    Ok(cached) => {
                        let cached = Arc::new(cached);
                        state.schema = Some(cached.clone());
                        state.error = None;
                        let cache = this.schema_cache.clone();
                        cx.background_executor()
                            .spawn(async move {
                                if let Err(e) = cache.save(&key, &cached) {
                                    eprintln!("could not save schema: {e:#}");
                                }
                            })
                            .detach();
                    }
                    Err(error) => state.error = Some(error),
                }
                this.sync_schema(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cancel_schema_fetch(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.schema_key(cx)
            && let Some(state) = self.schemas.get_mut(&key)
        {
            state.fetch = None;
            state.loading = false;
            state.generation += 1;
        }
        cx.notify();
    }

    pub(super) fn open_schema_type(&mut self, name: String, cx: &mut Context<Self>) {
        if self.schema_nav.last() != Some(&name) {
            self.schema_nav.push(name);
        }
        cx.notify();
    }

    pub(super) fn render_schema(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let state = self.schema_state(cx);
        let loading = state.is_some_and(|s| s.loading);
        let schema = state.and_then(|s| s.schema.clone());
        let error = state.and_then(|s| s.error.clone());

        let (status, color) = match (&schema, loading) {
            (_, true) => (t!("request.schema_fetching").to_string(), theme.muted_foreground),
            (Some(cached), false) => (
                t!(
                    "request.schema_summary",
                    count = cached.schema.user_types().count(),
                    age = fetched_age(response_cache::now().saturating_sub(cached.fetched_at))
                )
                .to_string(),
                theme.muted_foreground,
            ),
            (None, false) => (t!("request.schema_none").to_string(), theme.muted_foreground),
        };

        let bar = h_flex()
            .gap_2()
            .when(!self.schema_nav.is_empty(), |bar| {
                bar.child(
                    Button::new("schema-back")
                        .small()
                        .icon(IconName::ArrowLeft)
                        .tooltip(t!("request.schema_back").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.schema_nav.pop();
                            cx.notify();
                        })),
                )
            })
            .child(div().flex_1().min_w_0().text_sm().text_color(color).child(status))
            .child(if loading {
                Button::new("graphql-fetch-schema")
                    .small()
                    .label(t!("request.cancel").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_schema_fetch(cx)))
            } else {
                Button::new("graphql-fetch-schema")
                    .small()
                    .icon(IconName::Redo2)
                    .label(
                        if schema.is_some() {
                            t!("request.schema_refresh")
                        } else {
                            t!("request.schema_fetch")
                        }
                        .to_string(),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.fetch_schema(window, cx)))
            });

        let mut column = v_flex().flex_1().min_h_0().gap_2().child(bar);
        if let Some(error) = error {
            column = column.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(t!("request.schema_failed", error = error).to_string()),
            );
        }
        let Some(cached) = schema else {
            return column
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("request.schema_hint").to_string()),
                )
                .into_any_element();
        };
        let schema = &cached.schema;
        let filter = self.schema_filter.read(cx).value().trim().to_lowercase();

        let body = if !filter.is_empty() {
            self.render_schema_search(schema, &filter, cx)
        } else if let Some(def) = self.schema_nav.last().and_then(|name| schema.get(name)) {
            self.render_schema_type(def, cx)
        } else {
            self.render_schema_overview(schema, cx)
        };
        column
            .child(text_input(&self.schema_filter).small())
            .child(
                v_flex()
                    .id("schema-docs")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_px()
                    .child(body),
            )
            .into_any_element()
    }

    /// A clickable row that opens `target`.
    fn schema_row(
        &self,
        index: usize,
        title: String,
        detail: Option<String>,
        description: Option<&str>,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        h_flex()
            .id(("schema-row", index))
            .test_support()
            .items_start()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .text_sm()
            .when(target.is_some(), |row| {
                row.cursor_pointer().hover(|row| row.bg(theme.accent))
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_family(theme.mono_font_family.clone()).child(title))
                    .when_some(description.filter(|d| !d.trim().is_empty()), |col, d| {
                        col.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(d.trim().to_string()),
                        )
                    }),
            )
            .when_some(detail, |row, detail| {
                row.child(div().flex_none().text_xs().text_color(theme.info).child(detail))
            })
            .when_some(target, |row, target| {
                row.on_click(cx.listener(move |this, _, _, cx| this.open_schema_type(target.clone(), cx)))
            })
            .into_any_element()
    }

    fn render_schema_overview(&self, schema: &Schema, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut list = v_flex().gap_px();
        let mut index = 0;
        let roots = [
            (OperationKind::Query, "query"),
            (OperationKind::Mutation, "mutation"),
            (OperationKind::Subscription, "subscription"),
        ];
        for (kind, keyword) in roots {
            if let Some(root) = schema.root(kind) {
                list = list.child(self.schema_row(
                    index,
                    format!("{keyword}: {}", root.name),
                    Some(t!("request.schema_field_count", count = root.fields.len()).to_string()),
                    root.description.as_deref(),
                    Some(root.name.clone()),
                    cx,
                ));
                index += 1;
            }
        }
        list = list.child(
            div()
                .pt_2()
                .px_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(t!("request.schema_all_types").to_string()),
        );
        let mut types: Vec<&TypeDef> = schema.user_types().collect();
        types.sort_by(|a, b| a.name.cmp(&b.name));
        for def in types.into_iter().take(MAX_ROWS) {
            list = list.child(self.schema_row(
                index,
                def.name.clone(),
                Some(def.kind.label().to_string()),
                None,
                Some(def.name.clone()),
                cx,
            ));
            index += 1;
        }
        list.into_any_element()
    }

    fn render_schema_type(&self, def: &TypeDef, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut list = v_flex().gap_px().child(
            v_flex()
                .px_2()
                .pb_2()
                .gap_1()
                .child(div().font_family(theme.mono_font_family.clone()).child(format!(
                    "{} {}",
                    def.kind.label(),
                    def.name
                )))
                .when_some(def.description.as_ref(), |col, d| {
                    col.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(d.trim().to_string()),
                    )
                })
                .when(!def.interfaces.is_empty(), |col| {
                    let names: Vec<&str> = def.interfaces.iter().map(|i| i.named()).collect();
                    col.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("request.schema_implements", names = names.join(", ")).to_string()),
                    )
                }),
        );
        let mut index = 0;
        for field in def.fields.iter().take(MAX_ROWS) {
            let mut title = field.signature();
            if field.is_deprecated {
                title.push_str(" ⚠");
            }
            let description = match (&field.description, field.deprecation_reason.as_deref()) {
                (Some(d), Some(reason)) if field.is_deprecated => Some(format!("{d}\n{reason}")),
                (None, Some(reason)) if field.is_deprecated => Some(reason.to_string()),
                (d, _) => d.clone(),
            };
            list = list.child(self.schema_row(
                index,
                title,
                None,
                description.as_deref(),
                Some(field.ty.named().to_string()),
                cx,
            ));
            index += 1;
        }
        for input in def.input_fields.iter().take(MAX_ROWS) {
            let title = match &input.default_value {
                Some(default) => format!("{}: {} = {default}", input.name, input.ty),
                None => format!("{}: {}", input.name, input.ty),
            };
            list = list.child(self.schema_row(
                index,
                title,
                None,
                input.description.as_deref(),
                Some(input.ty.named().to_string()),
                cx,
            ));
            index += 1;
        }
        for value in def.enum_values.iter().take(MAX_ROWS) {
            list = list.child(self.schema_row(
                index,
                value.name.clone(),
                value.is_deprecated.then(|| "⚠".to_string()),
                value.description.as_deref(),
                None,
                cx,
            ));
            index += 1;
        }
        if matches!(def.kind, TypeKind::Union | TypeKind::Interface) && !def.possible_types.is_empty() {
            list = list.child(
                div()
                    .pt_2()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("request.schema_possible_types").to_string()),
            );
            for possible in def.possible_types.iter().take(MAX_ROWS) {
                list = list.child(self.schema_row(
                    index,
                    possible.named().to_string(),
                    None,
                    None,
                    Some(possible.named().to_string()),
                    cx,
                ));
                index += 1;
            }
        }
        list.into_any_element()
    }

    fn render_schema_search(&self, schema: &Schema, filter: &str, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex().gap_px();
        let mut index = 0;
        'types: for def in schema.user_types() {
            if def.name.to_lowercase().contains(filter) {
                list = list.child(self.schema_row(
                    index,
                    def.name.clone(),
                    Some(def.kind.label().to_string()),
                    def.description.as_deref(),
                    Some(def.name.clone()),
                    cx,
                ));
                index += 1;
            }
            for field in &def.fields {
                if index >= MAX_ROWS {
                    break 'types;
                }
                if field.name.to_lowercase().contains(filter) {
                    list = list.child(self.schema_row(
                        index,
                        format!("{}.{}", def.name, field.signature()),
                        None,
                        field.description.as_deref(),
                        Some(def.name.clone()),
                        cx,
                    ));
                    index += 1;
                }
            }
        }
        list.into_any_element()
    }
}

fn option_arc_eq<T>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn fetched_age(seconds: u64) -> String {
    match seconds {
        0..60 => t!("request.fetched_just_now").to_string(),
        60..3600 => t!("request.fetched_minutes_ago", count = seconds / 60).to_string(),
        3600..86400 => t!("request.fetched_hours_ago", count = seconds / 3600).to_string(),
        _ => t!("request.fetched_days_ago", count = seconds / 86400).to_string(),
    }
}

/// Completions and hover docs for the query editor, from the current schema.
pub(super) struct GraphqlAssist {
    pub schema: SchemaSlot,
}

impl CompletionProvider for GraphqlAssist {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let Some(cached) = self.schema.borrow().clone() else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };
        let source = text.to_string();
        let (range, suggestions) = assist::complete(&cached.schema, &source, offset);
        let edit_range = lsp_types::Range {
            start: text.offset_to_position(range.start),
            end: text.offset_to_position(range.end),
        };
        let typed = source[range].to_string();
        let items = suggestions
            .into_iter()
            .map(|s| CompletionItem {
                kind: Some(match s.kind {
                    SuggestionKind::Field => CompletionItemKind::FIELD,
                    SuggestionKind::Argument => CompletionItemKind::PROPERTY,
                    SuggestionKind::Type => CompletionItemKind::CLASS,
                    SuggestionKind::EnumValue => CompletionItemKind::ENUM_MEMBER,
                    SuggestionKind::Variable => CompletionItemKind::VARIABLE,
                    SuggestionKind::Keyword => CompletionItemKind::KEYWORD,
                }),
                detail: s.detail,
                documentation: s.documentation.map(Documentation::String),
                deprecated: Some(s.deprecated),
                filter_text: Some(typed.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: edit_range,
                    new_text: s.label.clone(),
                })),
                label: s.label,
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        self.schema.borrow().is_some()
            && !new_text.is_empty()
            && new_text.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric())
    }
}

impl HoverProvider for GraphqlAssist {
    fn hover(&self, text: &Rope, offset: usize, _: &mut Window, _: &mut App) -> Task<Result<Option<Hover>>> {
        let Some(cached) = self.schema.borrow().clone() else {
            return Task::ready(Ok(None));
        };
        let source = text.to_string();
        let hover = assist::hover(&cached.schema, &source, offset).map(|(range, markdown)| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: Some(lsp_types::Range {
                start: text.offset_to_position(range.start),
                end: text.offset_to_position(range.end),
            }),
        });
        Task::ready(Ok(hover))
    }
}
