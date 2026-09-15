//! Completions inside `{{ }}` for the request's editors, from its variables, the requests in
//! its collection and their latest responses. See [`crate::template_assist`].

use std::path::{Path, PathBuf};

use anyhow::Result;
use gpui_kit::component::input::CompletionProvider;
use gpui_kit::component::{Rope, RopeExt as _};
use gpui_kit::*;
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, TextEdit,
};

use super::RequestEditor;
use crate::response_cache::{self, Outcome, StoredResponse};
use crate::template_assist::{self, Slot, Suggestion};

impl RequestEditor {
    /// The requests `{{ response() }}` can refer to, as (name, path).
    pub fn set_collection_requests(&mut self, requests: Vec<(String, PathBuf)>) {
        self.collection_requests = requests;
    }

    /// The path of the request a `response()` argument names, like `chain` resolves it.
    fn request_path(&self, reference: &str) -> Option<PathBuf> {
        let root = self.collection_root.as_deref()?;
        let reference = reference.trim();
        let relative = |path: &Path| path.strip_prefix(root).map(|p| p.to_string_lossy().into_owned());
        self.collection_requests
            .iter()
            .find(|(_, path)| {
                relative(path).is_ok_and(|r| r == reference || r.strip_suffix(".yaml") == Some(reference))
            })
            .or_else(|| {
                self.collection_requests
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(reference))
            })
            .map(|(_, path)| path.clone())
    }

    /// The latest response of the request a `response()` argument names: this session's, or
    /// the saved one.
    fn latest_response_of(&self, reference: &str, cx: &App) -> Option<StoredResponse> {
        let path = self.request_path(reference)?;
        if let Some(response) = self.responses.get(&path).and_then(|s| s.response.clone()) {
            return Some(response);
        }
        let collection_id = self
            .state()
            .and_then(|s| s.cache_key.as_ref())
            .and_then(|k| k.collection_id.clone());
        let key = response_cache::cache_key(collection_id.as_deref(), self.collection_root.as_deref()?, &path);
        self.cache(cx)?.load(&key)
    }

    /// Suggestions for the `{{ }}` slot at `offset` in `text`, and the byte range they replace.
    pub fn template_suggestions(
        &self,
        text: &str,
        offset: usize,
        cx: &App,
    ) -> Option<(std::ops::Range<usize>, Vec<Suggestion>)> {
        let (range, slot) = template_assist::slot_at(text, offset)?;
        let partial = &text[range.clone()];
        let suggestions = match slot {
            Slot::Name => {
                let named = |name: &String, detail: &str| Suggestion {
                    label: name.clone(),
                    insert: name.clone(),
                    detail: Some(detail.to_string()),
                };
                let mut all: Vec<Suggestion> = self.variables.keys().map(|n| named(n, "variable")).collect();
                all.extend(self.secrets.keys().map(|n| named(n, "secret")));
                all.extend(template_assist::functions());
                template_assist::filter(all, partial)
            }
            Slot::Request => {
                let root = self.collection_root.as_deref();
                let all = self
                    .collection_requests
                    .iter()
                    .map(|(name, path)| Suggestion {
                        label: name.clone(),
                        insert: name.clone(),
                        detail: root
                            .and_then(|root| path.strip_prefix(root).ok())
                            .map(|p| p.display().to_string()),
                    })
                    .collect();
                template_assist::filter(all, partial)
            }
            Slot::JsonPath { request } => match self.latest_response_of(&request, cx) {
                Some(StoredResponse {
                    outcome: Outcome::Response { body, .. },
                    ..
                }) => template_assist::json_paths(&body, partial),
                _ => Vec::new(),
            },
            Slot::Header { request } => match self.latest_response_of(&request, cx) {
                Some(StoredResponse {
                    outcome: Outcome::Response { headers, .. },
                    ..
                }) => {
                    let mut names: Vec<String> = headers.into_iter().map(|(name, _)| name).collect();
                    names.sort_by_key(|n| n.to_lowercase());
                    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
                    let all = names
                        .into_iter()
                        .map(|name| Suggestion {
                            label: name.clone(),
                            insert: name,
                            detail: None,
                        })
                        .collect();
                    template_assist::filter(all, partial)
                }
                _ => Vec::new(),
            },
            Slot::Freshness => template_assist::filter(template_assist::freshness(), partial),
        };
        Some((range, suggestions))
    }
}

/// Completions for `{{ }}` in one of the request editor's text editors.
pub(super) struct TemplateCompletion {
    pub editor: WeakEntity<RequestEditor>,
}

impl CompletionProvider for TemplateCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let empty = || Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        let Some(editor) = self.editor.upgrade() else {
            return empty();
        };
        let source = text.to_string();
        let Some((range, suggestions)) = editor.read(cx).template_suggestions(&source, offset, cx) else {
            return empty();
        };
        let edit_range = lsp_types::Range {
            start: text.offset_to_position(range.start),
            end: text.offset_to_position(range.end),
        };
        let typed = source[range].to_string();
        let items = suggestions
            .into_iter()
            .map(|s| CompletionItem {
                kind: Some(CompletionItemKind::VARIABLE),
                detail: s.detail,
                filter_text: Some(typed.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: edit_range,
                    new_text: s.insert,
                })),
                label: s.label,
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        !new_text.is_empty()
            && new_text
                .chars()
                .all(|c| c.is_alphanumeric() || "_-$.[{('\"".contains(c))
    }
}
