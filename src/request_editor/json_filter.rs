//! Filtering a JSON response body with a JSONPath expression (RFC 9535), e.g.
//! `$.items[*].id` or `$..book[?@.price < 10].title`.

use serde_json::Value;
use serde_json_path::JsonPath;

#[derive(Debug, PartialEq)]
pub(super) enum Filtered {
    /// What to show: a single match as itself, several as an array.
    Matches { text: String, count: usize },
    /// The expression doesn't parse (often because it's still being typed).
    Invalid(String),
    /// The body isn't JSON, so there's nothing to filter.
    NotJson,
}

/// Parses a response body for filtering, if it's JSON.
pub(super) fn parse_body(body: &str) -> Option<Value> {
    serde_json::from_str(body).ok()
}

pub(super) fn filter(body: Option<&Value>, expression: &str) -> Filtered {
    let Some(body) = body else {
        return Filtered::NotJson;
    };
    let path = match JsonPath::parse(expression.trim()) {
        Ok(path) => path,
        Err(e) => {
            let message = e.to_string();
            return Filtered::Invalid(message.lines().next().unwrap_or_default().to_string());
        }
    };
    let nodes = path.query(body).all();
    let count = nodes.len();
    let shown = match nodes.as_slice() {
        [single] => serde_json::to_string_pretty(single),
        many => serde_json::to_string_pretty(many),
    };
    Filtered::Matches {
        text: shown.unwrap_or_default(),
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{
        "store": {
            "books": [
                { "title": "Rust", "price": 30, "tags": ["systems"] },
                { "title": "Linux", "price": 8 }
            ],
            "open": true
        }
    }"#;

    fn run(expression: &str) -> Filtered {
        filter(parse_body(BODY).as_ref(), expression)
    }

    #[test]
    fn single_matches_show_the_value_and_many_show_an_array() {
        assert_eq!(
            run("$.store.open"),
            Filtered::Matches {
                text: "true".into(),
                count: 1
            }
        );
        assert_eq!(
            run("$.store.books[*].title"),
            Filtered::Matches {
                text: "[\n  \"Rust\",\n  \"Linux\"\n]".into(),
                count: 2
            }
        );
        let Filtered::Matches { text, count } = run("$..books[?@.price < 10].title") else {
            panic!()
        };
        assert_eq!((text.as_str(), count), ("\"Linux\"", 1));
        assert_eq!(
            run("$.missing"),
            Filtered::Matches {
                text: "[]".into(),
                count: 0
            }
        );
    }

    #[test]
    fn reports_bad_expressions_and_non_json() {
        assert!(matches!(run("$.store.books["), Filtered::Invalid(_)));
        assert!(matches!(run("store"), Filtered::Invalid(_)));
        assert_eq!(filter(parse_body("<html>").as_ref(), "$"), Filtered::NotJson);
    }
}
