//! Checks: one-line assertions about a response, stored with the request.
//!
//! ```text
//! status == 200
//! status < 400
//! header Content-Type contains json
//! $.user.id exists
//! $.items[0].name == "Rex"
//! $.total >= {{min_total}}
//! body contains ok
//! time < 500          (milliseconds)
//! size <= 1048576     (bytes)
//! # $.debug exists    (switched off)
//! ```
//!
//! Operators: `==`, `!=`, `<`, `<=`, `>`, `>=`, `contains`, `!contains`, `exists`, `missing`.
//! Numbers compare as numbers; JSON values compare with the value parsed as JSON when it is
//! valid JSON (so `== 7`, `== true`, `== "7"` mean what they say), otherwise as text.

use serde_json::Value;
use serde_json_path::JsonPath;

use crate::model::{Variables, interpolate};
use crate::response_cache::{Outcome, StoredResponse};

#[derive(Clone, Debug, PartialEq)]
pub enum Subject {
    Status,
    /// Response time in milliseconds.
    Time,
    /// Body size in bytes.
    Size,
    Body,
    Header(String),
    Json(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    NotContains,
    Exists,
    Missing,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub subject: Subject,
    pub op: Op,
    /// The expected value, as written (may contain `{{variables}}`).
    pub value: Option<String>,
}

/// One line of the checks editor.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    Check(Check),
    /// A `#` line or blank line.
    Off,
    Invalid(String),
}

const OPS: [(&str, Op); 10] = [
    ("!contains", Op::NotContains),
    ("contains", Op::Contains),
    ("exists", Op::Exists),
    ("missing", Op::Missing),
    ("==", Op::Eq),
    ("!=", Op::Ne),
    ("<=", Op::Le),
    (">=", Op::Ge),
    ("<", Op::Lt),
    (">", Op::Gt),
];

pub fn parse_line(line: &str) -> Line {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Line::Off;
    }
    match parse(line) {
        Ok(check) => Line::Check(check),
        Err(error) => Line::Invalid(error),
    }
}

fn parse(line: &str) -> Result<Check, String> {
    let (subject, rest) = if let Some(rest) = line.strip_prefix("header ") {
        let rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        (Subject::Header(rest[..end].to_string()), &rest[end..])
    } else if line.starts_with('$') {
        let end = json_path_end(line);
        (Subject::Json(line[..end].to_string()), &line[end..])
    } else {
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        let subject = match &line[..end] {
            "status" => Subject::Status,
            "time" => Subject::Time,
            "size" => Subject::Size,
            "body" => Subject::Body,
            other => {
                return Err(format!(
                    "`{other}` isn't something to check; use status, time, size, body, header <name> or a JSONPath like $.id"
                ));
            }
        };
        (subject, &line[end..])
    };
    let rest = rest.trim_start();
    let Some((op_text, op)) = OPS.iter().find(|(text, _)| {
        rest.starts_with(text)
            && rest[text.len()..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || !text.chars().all(char::is_alphabetic))
    }) else {
        return Err(format!(
            "expected an operator after the subject: ==, !=, <, <=, >, >=, contains, !contains, exists or missing{}",
            if rest.is_empty() {
                String::new()
            } else {
                format!(", not `{rest}`")
            }
        ));
    };
    let value = rest[op_text.len()..].trim();
    let needs_value = !matches!(op, Op::Exists | Op::Missing);
    match (needs_value, value.is_empty()) {
        (true, true) => return Err(format!("`{op_text}` needs a value to compare with")),
        (false, false) => return Err(format!("`{op_text}` doesn't take a value")),
        _ => {}
    }
    if matches!(subject, Subject::Status | Subject::Time | Subject::Size)
        && matches!(op, Op::Lt | Op::Le | Op::Gt | Op::Ge)
        && !value.contains("{{")
        && value.parse::<f64>().is_err()
    {
        return Err(format!("`{value}` isn't a number"));
    }
    Ok(Check {
        subject,
        op: *op,
        value: needs_value.then(|| value.to_string()),
    })
}

/// Where a JSONPath at the start of `line` ends: at whitespace outside brackets and quotes.
fn json_path_end(line: &str) -> usize {
    let mut depth = 0;
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '[' | '(') => depth += 1,
            (None, ']' | ')') => depth -= 1,
            (None, c) if c.is_whitespace() && depth <= 0 => return i,
            _ => {}
        }
    }
    line.len()
}

/// How one check went.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckResult {
    /// The line as written.
    pub line: String,
    pub passed: bool,
    /// What the response had, for showing next to a failure.
    pub actual: String,
    /// Why the check couldn't be evaluated (bad line, not JSON, bad JSONPath), if so.
    pub error: Option<String>,
}

/// Evaluates every enabled line against `response`, with `{{variables}}` in values resolved.
pub fn evaluate(lines: &[String], response: &StoredResponse, variables: &Variables) -> Vec<CheckResult> {
    let mut json: Option<Option<Value>> = None;
    lines
        .iter()
        .filter_map(|line| {
            let check = match parse_line(line) {
                Line::Off => return None,
                Line::Invalid(error) => {
                    return Some(CheckResult {
                        line: line.trim().to_string(),
                        passed: false,
                        actual: String::new(),
                        error: Some(error),
                    });
                }
                Line::Check(check) => check,
            };
            let value = check.value.as_deref().map(|v| interpolate(v, variables).0);
            Some(match evaluate_one(&check, value.as_deref(), response, &mut json) {
                Ok((passed, actual)) => CheckResult {
                    line: line.trim().to_string(),
                    passed,
                    actual,
                    error: None,
                },
                Err(error) => CheckResult {
                    line: line.trim().to_string(),
                    passed: false,
                    actual: String::new(),
                    error: Some(error),
                },
            })
        })
        .collect()
}

fn evaluate_one(
    check: &Check,
    expected: Option<&str>,
    response: &StoredResponse,
    json: &mut Option<Option<Value>>,
) -> std::result::Result<(bool, String), String> {
    let Outcome::Response {
        status,
        headers,
        body,
        body_size,
        ..
    } = &response.outcome
    else {
        return Err("the request failed, so there's no response to check".into());
    };
    let expected_text = expected.unwrap_or_default();
    match &check.subject {
        Subject::Status => compare_number(check.op, *status as f64, expected_text),
        Subject::Time => compare_number(check.op, response.elapsed_ms as f64, expected_text),
        Subject::Size => compare_number(check.op, *body_size as f64, expected_text),
        Subject::Body => compare_text(check.op, Some(body), expected_text),
        Subject::Header(name) => {
            let value = headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str());
            compare_text(check.op, value, expected_text)
        }
        Subject::Json(path) => {
            let parsed = json.get_or_insert_with(|| serde_json::from_str(body).ok());
            let Some(document) = parsed else {
                return Err("the response isn't JSON".into());
            };
            let expression = JsonPath::parse(path).map_err(|e| {
                format!(
                    "bad JSONPath `{path}`: {}",
                    e.to_string().lines().next().unwrap_or_default()
                )
            })?;
            let found = expression.query(document).first().cloned();
            compare_json(check.op, found.as_ref(), expected_text)
        }
    }
}

fn compare_number(op: Op, actual: f64, expected: &str) -> std::result::Result<(bool, String), String> {
    let shown = format_number(actual);
    let passed = match op {
        Op::Exists => true,
        Op::Missing => false,
        Op::Contains | Op::NotContains => {
            let contains = shown.contains(expected.trim());
            if op == Op::Contains { contains } else { !contains }
        }
        _ => {
            let expected: f64 = expected
                .trim()
                .parse()
                .map_err(|_| format!("`{}` isn't a number", expected.trim()))?;
            ordering(op, actual.partial_cmp(&expected))
        }
    };
    Ok((passed, shown))
}

fn compare_text(op: Op, actual: Option<&str>, expected: &str) -> std::result::Result<(bool, String), String> {
    let expected = unquote(expected);
    let shown = actual.map(preview).unwrap_or_else(|| "(missing)".into());
    let passed = match (op, actual) {
        (Op::Exists, found) => found.is_some(),
        (Op::Missing, found) => found.is_none(),
        (_, None) => op == Op::Ne || op == Op::NotContains,
        (Op::Contains, Some(a)) => a.contains(expected),
        (Op::NotContains, Some(a)) => !a.contains(expected),
        (Op::Eq, Some(a)) => a == expected,
        (Op::Ne, Some(a)) => a != expected,
        (_, Some(a)) => match (a.trim().parse::<f64>(), expected.trim().parse::<f64>()) {
            (Ok(a), Ok(e)) => ordering(op, a.partial_cmp(&e)),
            _ => ordering(op, Some(a.cmp(expected))),
        },
    };
    Ok((passed, shown))
}

fn compare_json(op: Op, actual: Option<&Value>, expected: &str) -> std::result::Result<(bool, String), String> {
    let shown = actual
        .map(|v| preview(&v.to_string()))
        .unwrap_or_else(|| "(missing)".into());
    let expected_json: Value =
        serde_json::from_str(expected.trim()).unwrap_or_else(|_| Value::String(expected.trim().to_string()));
    let passed = match (op, actual) {
        (Op::Exists, found) => found.is_some(),
        (Op::Missing, found) => found.is_none(),
        (_, None) => op == Op::Ne || op == Op::NotContains,
        (Op::Eq, Some(a)) => json_equal(a, &expected_json),
        (Op::Ne, Some(a)) => !json_equal(a, &expected_json),
        (Op::Contains | Op::NotContains, Some(a)) => {
            let contains = match a {
                Value::String(s) => s.contains(unquote(expected)),
                Value::Array(items) => items.iter().any(|item| json_equal(item, &expected_json)),
                Value::Object(map) => map.contains_key(unquote(expected)),
                other => other.to_string().contains(unquote(expected)),
            };
            if op == Op::Contains { contains } else { !contains }
        }
        (_, Some(a)) => match (a.as_f64(), expected_json.as_f64()) {
            (Some(a), Some(e)) => ordering(op, a.partial_cmp(&e)),
            _ => match (a.as_str(), expected_json.as_str()) {
                (Some(a), Some(e)) => ordering(op, Some(a.cmp(e))),
                _ => return Err(format!("can't order {a} and {expected}")),
            },
        },
    };
    Ok((passed, shown))
}

/// Numbers are equal regardless of how they're written (7 == 7.0); strings compare exactly.
fn json_equal(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn ordering(op: Op, ordering: Option<std::cmp::Ordering>) -> bool {
    use std::cmp::Ordering::*;
    matches!(
        (op, ordering),
        (Op::Eq, Some(Equal))
            | (Op::Ne, Some(Less | Greater))
            | (Op::Lt, Some(Less))
            | (Op::Le, Some(Less | Equal))
            | (Op::Gt, Some(Greater))
            | (Op::Ge, Some(Greater | Equal))
    )
}

fn unquote(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
        .unwrap_or(text)
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        n.to_string()
    }
}

fn preview(text: &str) -> String {
    let mut out: String = text.chars().take(120).collect();
    if text.chars().count() > 120 {
        out.push('…');
    }
    out
}

/// A check asserting that `path` equals `value` (a JSON value as shown in a filtered
/// response), for "Add check".
pub fn equals_line(path: &str, value: &str) -> String {
    format!("{path} == {}", value.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: &str) -> StoredResponse {
        StoredResponse {
            timing: None,
            received_at: 0,
            elapsed_ms: 120,
            outcome: Outcome::Response {
                status,
                reason: "OK".into(),
                headers: vec![("content-type".into(), "application/json; charset=utf-8".into())],
                body: body.into(),
                body_size: body.len(),
                truncated: false,
            },
        }
    }

    fn run(lines: &[&str], response: &StoredResponse) -> Vec<(String, bool)> {
        let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let vars = Variables::from([("min".to_string(), "2".to_string())]);
        evaluate(&lines, response, &vars)
            .into_iter()
            .map(|r| (r.line, r.passed))
            .collect()
    }

    #[test]
    fn parses_lines() {
        assert_eq!(
            parse_line("status == 200"),
            Line::Check(Check {
                subject: Subject::Status,
                op: Op::Eq,
                value: Some("200".into())
            })
        );
        assert_eq!(
            parse_line("$.items[?@.name == 'a b'].id exists"),
            Line::Check(Check {
                subject: Subject::Json("$.items[?@.name == 'a b'].id".into()),
                op: Op::Exists,
                value: None
            })
        );
        assert_eq!(
            parse_line("header Content-Type !contains xml"),
            Line::Check(Check {
                subject: Subject::Header("Content-Type".into()),
                op: Op::NotContains,
                value: Some("xml".into())
            })
        );
        assert_eq!(parse_line("# status == 500"), Line::Off);
        assert!(matches!(parse_line("stauts == 200"), Line::Invalid(e) if e.contains("stauts")));
        assert!(matches!(parse_line("status ~ 200"), Line::Invalid(_)));
        assert!(matches!(parse_line("status =="), Line::Invalid(e) if e.contains("needs a value")));
        assert!(matches!(parse_line("$.id exists yes"), Line::Invalid(_)));
        assert!(matches!(parse_line("time < fast"), Line::Invalid(_)));
    }

    #[test]
    fn evaluates_against_a_response() {
        let body = r#"{"id":7,"name":"Rex","tags":["good","dog"],"total":3,"nested":{"ok":true}}"#;
        let ok = response(201, body);
        let results = run(
            &[
                "status >= 200",
                "status < 300",
                "status != 200",
                "time < 500",
                "size > 10",
                "header content-type contains json",
                "header X-Missing missing",
                "body contains Rex",
                "$.id == 7",
                "$.id == 7.0",
                "$.id != \"7\"",
                "$.name == Rex",
                "$.name == \"Rex\"",
                "$.tags contains \"dog\"",
                "$.nested contains ok",
                "$.nested.ok == true",
                "$.total >= {{min}}",
                "$.missing missing",
                "# $.id == 0",
            ],
            &ok,
        );
        let failed: Vec<_> = results.iter().filter(|(_, passed)| !passed).collect();
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(results.len(), 18, "switched-off lines are skipped");

        let results = run(
            &["status == 200", "$.name == Max", "$.id exists"],
            &response(500, "<html>"),
        );
        assert_eq!(
            results,
            [
                ("status == 200".to_string(), false),
                ("$.name == Max".to_string(), false),
                ("$.id exists".to_string(), false),
            ]
        );
        let lines = vec!["$.id exists".to_string()];
        let detail = &evaluate(&lines, &response(200, "<html>"), &Variables::new())[0];
        assert_eq!(detail.error.as_deref(), Some("the response isn't JSON"));

        let failed = StoredResponse::failed(0, "connection refused");
        let lines = vec!["status == 200".to_string()];
        assert!(!evaluate(&lines, &failed, &Variables::new())[0].passed);
    }

    #[test]
    fn shows_actual_values() {
        let lines = vec!["$.name == Max".to_string(), "status == 200".to_string()];
        let results = evaluate(&lines, &response(404, r#"{"name":"Rex"}"#), &Variables::new());
        assert_eq!(results[0].actual, "\"Rex\"");
        assert_eq!(results[1].actual, "404");
        assert_eq!(equals_line("$.name", "\"Rex\"\n"), "$.name == \"Rex\"");
    }
}
