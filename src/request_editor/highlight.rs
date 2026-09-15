//! Colours `{{variables}}` in the request's editors (known, secret or undefined) and query
//! parameter names in the URL, as decorations over the normal syntax highlighting.

use std::ops::Range;

use gpui_kit::base::input::{TextDecoration, TextDecorationCollection};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::EditorState;
use gpui_kit::*;

use super::RequestEditor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Span {
    /// A `{{name}}` with a value in the active environment or defaults.
    Variable,
    /// A `{{name}}` that refers to a secret.
    Secret,
    /// A `{{name}}` with no value anywhere.
    Undefined,
    /// A query parameter name in a URL.
    QueryName,
}

/// `{{name}}` placeholders in `text`: the byte range of each (braces included) and its name.
pub(super) fn placeholders(text: &str) -> Vec<(Range<usize>, &str)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(open) = text[from..].find("{{").map(|i| from + i) {
        let Some(close) = text[open + 2..].find("}}").map(|i| open + 2 + i) else {
            break;
        };
        let name = text[open + 2..close].trim();
        // Stop at line ends, so an unclosed `{{` doesn't colour the rest of the document.
        if name.is_empty() || text[open..close].contains('\n') {
            from = open + 2;
            continue;
        }
        found.push((open..close + 2, name));
        from = close + 2;
    }
    found
}

/// Names in a URL's query string: `?page=1&sort=name` gives the ranges of `page` and `sort`.
pub(super) fn query_names(url: &str) -> Vec<Range<usize>> {
    let Some(query_start) = url.find('?') else {
        return Vec::new();
    };
    let end = url.find('#').filter(|&h| h > query_start).unwrap_or(url.len());
    let mut names = Vec::new();
    let mut start = query_start + 1;
    for part in url[start..end].split('&') {
        let name_len = part.find('=').unwrap_or(part.len());
        if name_len > 0 {
            names.push(start..start + name_len);
        }
        start += part.len() + 1;
    }
    names
}

impl RequestEditor {
    fn span_kind(&self, name: &str) -> Span {
        if self.secrets.contains_key(name) {
            Span::Secret
        } else if self.variables.contains_key(name) {
            Span::Variable
        } else {
            Span::Undefined
        }
    }

    /// The spans to colour in `text`; `url` adds query parameter names.
    pub(super) fn spans(&self, text: &str, url: bool) -> Vec<(Range<usize>, Span)> {
        let mut spans: Vec<_> = placeholders(text)
            .into_iter()
            .map(|(range, name)| (range, self.span_kind(name)))
            .collect();
        if url {
            let names: Vec<_> = query_names(text)
                .into_iter()
                .filter(|range| !spans.iter().any(|(r, _)| r.start < range.end && range.start < r.end))
                .map(|range| (range, Span::QueryName))
                .collect();
            spans.extend(names);
            spans.sort_by_key(|(range, _)| range.start);
        }
        spans
    }

    /// Recolours `editor` from its current text.
    pub(super) fn highlight(
        &self,
        editor: &Entity<EditorState>,
        decorations: &TextDecorationCollection,
        url: bool,
        cx: &mut App,
    ) {
        let theme = cx.theme().clone();
        let text = editor.read(cx).value().to_string();
        let decorated = self
            .spans(&text, url)
            .into_iter()
            .map(|(range, span)| {
                let style = match span {
                    Span::Variable => HighlightStyle {
                        color: Some(theme.info),
                        ..Default::default()
                    },
                    Span::Secret => HighlightStyle {
                        color: Some(theme.magenta),
                        ..Default::default()
                    },
                    Span::Undefined => HighlightStyle {
                        color: Some(theme.warning),
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(theme.warning),
                            wavy: true,
                        }),
                        ..Default::default()
                    },
                    Span::QueryName => HighlightStyle {
                        color: Some(theme.cyan),
                        ..Default::default()
                    },
                };
                TextDecoration::new(range, style)
            })
            .collect();
        decorations.set(decorated, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` (via `super::*`) exports GPUI's test macro; keep Rust's for `#[test]`.
    #[allow(unused_imports)]
    use core::prelude::v1::test;

    #[test]
    fn finds_placeholders() {
        let text = "{{base_url}}/pets/{{ pet_id }}?q={{}}&x={{open\n}}";
        let found: Vec<_> = placeholders(text)
            .into_iter()
            .map(|(range, name)| (&text[range], name))
            .collect();
        assert_eq!(found, [("{{base_url}}", "base_url"), ("{{ pet_id }}", "pet_id")]);
    }

    #[test]
    fn finds_query_names() {
        let url = "{{base_url}}/pets?limit=10&species=&flag#section?no=1";
        let names: Vec<_> = query_names(url).into_iter().map(|r| &url[r]).collect();
        assert_eq!(names, ["limit", "species", "flag"]);
        assert!(query_names("https://x.test/path").is_empty());
    }
}
