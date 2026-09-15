//! Autocomplete inside `{{ }}`: variable and function names, and the arguments of
//! `response()` / `response_header()` (request names, JSONPaths into the latest response,
//! header names, when to re-send). Works on the text alone; callers supply the data.

use std::ops::Range;

use serde_json::Value;
use serde_json_path::JsonPath;

/// What the cursor is in the middle of writing.
#[derive(Clone, Debug, PartialEq)]
pub enum Slot {
    /// A variable or function name: `{{ ba|`.
    Name,
    /// The request argument: `{{ response("Lo|`.
    Request,
    /// The JSONPath argument: `{{ response("Login", "$.us|`.
    JsonPath { request: String },
    /// The header argument: `{{ response_header("Login", "Loc|`.
    Header { request: String },
    /// When to re-send: `{{ response("Login", "$.token", "|`.
    Freshness,
}

/// The slot at `offset` and the byte range its partial text occupies (what a suggestion
/// replaces). None outside `{{ }}` or where nothing can be suggested.
pub fn slot_at(text: &str, offset: usize) -> Option<(Range<usize>, Slot)> {
    let offset = offset.min(text.len());
    if !text.is_char_boundary(offset) {
        return None;
    }
    let before = &text[..offset];
    let open = before.rfind("{{")?;
    if before[open..].contains("}}") {
        return None;
    }
    let inner_start = open + 2;
    let inner = &before[inner_start..];

    let Some(paren) = inner.find('(') else {
        let prefix_len = inner
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .map(char::len_utf8)
            .sum::<usize>();
        // Only whitespace may come before the name.
        if !inner[..inner.len() - prefix_len].trim().is_empty() {
            return None;
        }
        return Some((offset - prefix_len..offset, Slot::Name));
    };
    let function = inner[..paren].trim();
    let mut args: Vec<String> = Vec::new();
    let mut quote: Option<char> = None;
    let mut current = String::new();
    let mut current_start = 0;
    let mut chars = inner[paren + 1..].char_indices().peekable();
    let args_start = inner_start + paren + 1;
    while let Some((i, c)) = chars.next() {
        match quote {
            Some(q) if c == q => {
                args.push(std::mem::take(&mut current));
                quote = None;
            }
            Some(_) if c == '\\' => {
                if let Some((_, escaped)) = chars.next() {
                    current.push(escaped);
                }
            }
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                current_start = args_start + i + 1;
            }
            None if c == ')' => return None,
            None => {}
        }
    }
    // Only inside an open quote.
    quote?;
    let range = current_start..offset;
    let slot = match (function, args.len()) {
        ("response" | "response_header", 0) => Slot::Request,
        ("response", 1) => Slot::JsonPath {
            request: args[0].clone(),
        },
        ("response_header", 1) => Slot::Header {
            request: args[0].clone(),
        },
        ("response" | "response_header", 2) => Slot::Freshness,
        _ => return None,
    };
    Some((range, slot))
}

/// A suggestion: what's shown, what replaces the partial text, and a short detail.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub label: String,
    pub insert: String,
    pub detail: Option<String>,
}

impl Suggestion {
    fn new(label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            insert: label.clone(),
            label,
            detail: None,
        }
    }

    fn inserting(mut self, insert: impl Into<String>) -> Self {
        self.insert = insert.into();
        self
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Function names, inserted ready for their arguments.
pub fn functions() -> Vec<Suggestion> {
    vec![
        Suggestion::new("response")
            .inserting("response(\"")
            .detail("response(\"Request\", \"$.path\")"),
        Suggestion::new("response_header")
            .inserting("response_header(\"")
            .detail("response_header(\"Request\", \"Header\")"),
        Suggestion::new("uuid()").detail("random UUID"),
        Suggestion::new("timestamp()").detail("Unix seconds"),
        Suggestion::new("now()").detail("RFC 3339 time"),
    ]
}

pub fn freshness() -> Vec<Suggestion> {
    vec![
        Suggestion::new("latest").detail("send only if it has no response"),
        Suggestion::new("always").detail("send every time"),
        Suggestion::new("5m").detail("re-send if older than 5 minutes"),
        Suggestion::new("1h").detail("re-send if older than an hour"),
    ]
}

/// Keeps suggestions whose label or insert starts with `partial` (ignoring case).
pub fn filter(suggestions: Vec<Suggestion>, partial: &str) -> Vec<Suggestion> {
    let partial = partial.to_lowercase();
    suggestions
        .into_iter()
        .filter(|s| s.label.to_lowercase().starts_with(&partial) || s.insert.to_lowercase().starts_with(&partial))
        .collect()
}

/// JSONPath suggestions one step deeper than `partial`, from a response body.
pub fn json_paths(body: &str, partial: &str) -> Vec<Suggestion> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    if !partial.starts_with('$') {
        return filter(vec![Suggestion::new("$").detail("the whole response")], partial);
    }
    // Split into the path so far and the key being typed.
    let split = partial.rfind(['.', '[']).unwrap_or(1).max(1);
    let (base, fragment) = if partial.len() <= 1 {
        ("$", "")
    } else {
        (
            &partial[..split],
            partial[split..].trim_start_matches(['.', '[', '\'', '"']),
        )
    };
    let Ok(path) = JsonPath::parse(base) else {
        return Vec::new();
    };
    let Some(node) = path.query(&json).first() else {
        return Vec::new();
    };
    let fragment = fragment.to_lowercase();
    match node {
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| key.to_lowercase().starts_with(&fragment))
            .map(|(key, value)| {
                let simple = !key.is_empty()
                    && key.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && !key.starts_with(|c: char| c.is_ascii_digit());
                let insert = if simple {
                    format!("{base}.{key}")
                } else {
                    format!("{base}['{}']", key.replace('\'', "\\'"))
                };
                Suggestion::new(key.clone()).inserting(insert).detail(kind(value))
            })
            .collect(),
        Value::Array(items) => {
            let first = items.first().map(kind).unwrap_or("empty");
            vec![
                Suggestion::new("[0]").inserting(format!("{base}[0]")).detail(first),
                Suggestion::new("[*]")
                    .inserting(format!("{base}[*]"))
                    .detail("every item"),
            ]
        }
        _ => Vec::new(),
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Null => "null",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(text_with_cursor: &str) -> Option<(String, Slot)> {
        let offset = text_with_cursor.find('|').unwrap();
        let text = text_with_cursor.replace('|', "");
        slot_at(&text, offset).map(|(range, slot)| (text[range].to_string(), slot))
    }

    #[test]
    fn finds_the_slot_under_the_cursor() {
        assert_eq!(slot("{{ ba|"), Some(("ba".into(), Slot::Name)));
        assert_eq!(slot("x {{|"), Some(("".into(), Slot::Name)));
        assert_eq!(slot("{{ base_url }} and {{ to|"), Some(("to".into(), Slot::Name)));
        assert_eq!(slot("{{ base_url }}|"), None, "outside");
        assert_eq!(slot("plain|"), None);
        assert_eq!(slot("{{ response(\"Lo|"), Some(("Lo".into(), Slot::Request)));
        assert_eq!(
            slot("{{ response('Login', \"$.us|"),
            Some((
                "$.us".into(),
                Slot::JsonPath {
                    request: "Login".into()
                }
            ))
        );
        assert_eq!(
            slot("{{ response_header(\"Login\", \"Loc|"),
            Some((
                "Loc".into(),
                Slot::Header {
                    request: "Login".into()
                }
            ))
        );
        assert_eq!(
            slot("{{ response(\"Login\", \"$.a\", \"|"),
            Some(("".into(), Slot::Freshness))
        );
        assert_eq!(slot("{{ response(\"Login\", |"), None, "between arguments");
        assert_eq!(slot("{{ uuid(|"), None);
    }

    #[test]
    fn walks_json_paths() {
        let body = r#"{"token":"abc","user":{"id":7,"roles":["admin"],"first name":"Sam"}}"#;
        let inserts =
            |partial: &str| -> Vec<String> { json_paths(body, partial).into_iter().map(|s| s.insert).collect() };
        assert_eq!(inserts(""), ["$"]);
        assert_eq!(inserts("$"), ["$.token", "$.user"]);
        assert_eq!(inserts("$.t"), ["$.token"]);
        assert_eq!(
            inserts("$.user."),
            ["$.user.id", "$.user.roles", "$.user['first name']"]
        );
        assert_eq!(inserts("$.user.ro"), ["$.user.roles"]);
        assert_eq!(inserts("$.user.roles."), ["$.user.roles[0]", "$.user.roles[*]"]);
        assert!(inserts("$.nope.").is_empty());
        assert!(json_paths("<html>", "$").is_empty());
    }

    #[test]
    fn filters_by_prefix() {
        let names: Vec<String> = filter(functions(), "res").into_iter().map(|s| s.label).collect();
        assert_eq!(names, ["response", "response_header"]);
    }
}
