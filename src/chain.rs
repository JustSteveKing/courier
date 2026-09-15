//! Template functions inside `{{ }}`: values from other requests' responses (request
//! chaining), and a few generated values.
//!
//! - `{{ response("Login", "$.token") }}`: the first JSONPath match in the latest response of
//!   the request named "Login" (or at that path in the collection). Sends it first if it has
//!   no response yet. A third argument changes when it's sent: `"always"`, or a maximum age
//!   such as `"30s"`, `"5m"`, `"1h"` or `"1d"`.
//! - `{{ response_header("Login", "Location") }}`: a header from that response.
//! - `{{ uuid() }}`, `{{ timestamp() }}` (Unix seconds), `{{ now() }}` (RFC 3339, UTC).
//!
//! Calls are evaluated before a request is resolved, and their results are handed to
//! [`crate::model::interpolate`] as variables named by the call's text.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use serde_json_path::JsonPath;

use crate::http::Request;
use crate::model::{Auth, COLLECTION_FILE, CollectionFile, RequestFile, Variables};
use crate::response_cache::{self, Outcome, ResponseCache, StoredResponse};
use crate::storage;
use crate::transport;

/// How deep one request's dependencies may go.
const MAX_DEPTH: usize = 5;

/// When a referenced request is sent rather than its latest response used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Freshness {
    /// Only when it has no response yet.
    Latest,
    /// Every time.
    Always,
    /// When its latest response is older than this many seconds.
    MaxAge(u64),
}

impl Freshness {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "latest" => return Some(Self::Latest),
            "always" => return Some(Self::Always),
            _ => {}
        }
        let text = text.trim();
        let unit = text.chars().last()?;
        let amount: u64 = text[..text.len() - unit.len_utf8()].trim().parse().ok()?;
        let seconds = match unit {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        Some(Self::MaxAge(amount * seconds))
    }

    /// Whether a response received at `received_at` can be used at `now`.
    pub fn allows(self, received_at: u64, now: u64) -> bool {
        match self {
            Self::Latest => true,
            Self::Always => false,
            Self::MaxAge(max) => now.saturating_sub(received_at) <= max,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Response {
        request: String,
        path: String,
        freshness: Freshness,
    },
    ResponseHeader {
        request: String,
        header: String,
        freshness: Freshness,
    },
    Uuid,
    Timestamp,
    Now,
}

/// Parses the inside of a `{{ }}` as a function call, or None when it's a plain variable name.
pub fn parse_call(inner: &str) -> Option<Result<Call, String>> {
    let inner = inner.trim();
    let open = inner.find('(')?;
    if !inner.ends_with(')') {
        return Some(Err(format!("`{inner}` is missing a closing parenthesis")));
    }
    let name = inner[..open].trim();
    let args = match parse_args(&inner[open + 1..inner.len() - 1]) {
        Ok(args) => args,
        Err(e) => return Some(Err(format!("{name}(): {e}"))),
    };
    let arity = |min: usize, max: usize| {
        if args.len() < min || args.len() > max {
            Err(format!("{name}() takes {min}–{max} arguments, not {}", args.len()))
        } else {
            Ok(())
        }
    };
    let freshness = |ix: usize| match args.get(ix) {
        None => Ok(Freshness::Latest),
        Some(text) => Freshness::parse(text)
            .ok_or_else(|| format!("{name}(): \"{text}\" should be \"latest\", \"always\" or an age like \"5m\"")),
    };
    Some(match name {
        "response" => arity(2, 3).and_then(|_| {
            Ok(Call::Response {
                request: args[0].clone(),
                path: args[1].clone(),
                freshness: freshness(2)?,
            })
        }),
        "response_header" => arity(2, 3).and_then(|_| {
            Ok(Call::ResponseHeader {
                request: args[0].clone(),
                header: args[1].clone(),
                freshness: freshness(2)?,
            })
        }),
        "uuid" => arity(0, 0).map(|_| Call::Uuid),
        "timestamp" => arity(0, 0).map(|_| Call::Timestamp),
        "now" => arity(0, 0).map(|_| Call::Now),
        other => Err(format!("unknown function `{other}()`")),
    })
}

/// Comma-separated string literals in single or double quotes.
fn parse_args(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut chars = text.trim().chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == ',' {
            chars.next();
            continue;
        }
        if c != '"' && c != '\'' {
            return Err("arguments are quoted strings, like \"Login\"".into());
        }
        chars.next();
        let mut arg = String::new();
        loop {
            match chars.next() {
                None => return Err("an argument is missing its closing quote".into()),
                Some('\\') => arg.extend(chars.next()),
                Some(q) if q == c => break,
                Some(ch) => arg.push(ch),
            }
        }
        args.push(arg);
    }
    Ok(args)
}

/// Every `{{ … }}` call in `texts`, once each, by the exact inner text interpolation will
/// look up.
pub fn calls_in_texts(texts: &[&str]) -> Vec<(String, Result<Call, String>)> {
    let mut seen = HashSet::new();
    let mut calls = Vec::new();
    for text in texts {
        let mut rest = *text;
        while let Some(start) = rest.find("{{") {
            let Some(len) = rest[start + 2..].find("}}") else {
                break;
            };
            let inner = rest[start + 2..start + 2 + len].trim();
            if let Some(call) = parse_call(inner)
                && seen.insert(inner.to_string())
            {
                calls.push((inner.to_string(), call));
            }
            rest = &rest[start + 2 + len + 2..];
        }
    }
    calls
}

/// Every `{{ … }}` call in the text of a request.
pub fn calls_in(file: &RequestFile) -> Vec<(String, Result<Call, String>)> {
    let mut texts: Vec<&str> = vec![&file.url];
    for header in file.headers.iter().filter(|h| h.enabled) {
        texts.push(&header.name);
        texts.push(&header.value);
    }
    if let Some(body) = &file.body {
        texts.push(&body.content);
    }
    if let Some(graphql) = &file.graphql {
        texts.push(&graphql.query);
        texts.push(&graphql.variables);
    }
    match &file.auth {
        Auth::Basic { username, password } => texts.extend([username.as_str(), password.as_str()]),
        Auth::Bearer { token } => texts.push(token),
        Auth::ApiKey { name, value, .. } => texts.extend([name.as_str(), value.as_str()]),
        Auth::Inherit | Auth::None => {}
    }
    calls_in_texts(&texts)
}

/// What evaluating calls needs: the collection, the variables (secret values included), the
/// latest responses the app has, and how to send.
#[derive(Clone)]
pub struct Context {
    pub root: PathBuf,
    pub variables: Variables,
    /// Latest responses held in memory, by request path.
    pub latest: HashMap<PathBuf, StoredResponse>,
    pub cache: Option<ResponseCache>,
    pub collection_id: Option<String>,
    pub timeout: Duration,
    pub client: Option<reqwest::Client>,
}

/// Responses sent while evaluating, to show and save as those requests' latest.
pub type Sent = Vec<(PathBuf, StoredResponse)>;

/// Evaluates the calls in `file`, sending other requests as needed. Returns the values by
/// call text (to add to the variables) and the responses it sent.
pub async fn evaluate(file: &RequestFile, context: &Context) -> Result<(Variables, Sent), String> {
    let mut sent = Sent::new();
    let values = evaluate_calls(calls_in(file), context, &mut Vec::new(), &mut sent).await?;
    Ok((values, sent))
}

/// Evaluates the calls in a piece of text, such as a WebSocket message.
pub async fn evaluate_text(text: &str, context: &Context) -> Result<(Variables, Sent), String> {
    let mut sent = Sent::new();
    let values = evaluate_calls(calls_in_texts(&[text]), context, &mut Vec::new(), &mut sent).await?;
    Ok((values, sent))
}

async fn evaluate_in(
    file: &RequestFile,
    context: &Context,
    visiting: &mut Vec<PathBuf>,
    sent: &mut Sent,
) -> Result<Variables, String> {
    evaluate_calls(calls_in(file), context, visiting, sent).await
}

async fn evaluate_calls(
    calls: Vec<(String, Result<Call, String>)>,
    context: &Context,
    visiting: &mut Vec<PathBuf>,
    sent: &mut Sent,
) -> Result<Variables, String> {
    let mut values = Variables::new();
    for (text, call) in calls {
        let value = match call? {
            Call::Uuid => uuid::Uuid::new_v4().to_string(),
            Call::Timestamp => response_cache::now().to_string(),
            Call::Now => now_rfc3339(),
            Call::Response {
                request,
                path,
                freshness,
            } => {
                let response = response_of(&request, freshness, context, visiting, sent).await?;
                json_value(&response, &path).map_err(|e| format!("response(\"{request}\"): {e}"))?
            }
            Call::ResponseHeader {
                request,
                header,
                freshness,
            } => {
                let response = response_of(&request, freshness, context, visiting, sent).await?;
                let Outcome::Response { headers, .. } = &response.outcome else {
                    return Err(format!("response_header(\"{request}\"): its last send failed"));
                };
                headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(&header))
                    .map(|(_, value)| value.clone())
                    .ok_or_else(|| format!("response_header(\"{request}\"): no `{header}` header"))?
            }
        };
        values.insert(text, value);
    }
    Ok(values)
}

/// The latest response of the request `reference`, sending it when it has none or when
/// `freshness` rules the latest out.
async fn response_of(
    reference: &str,
    freshness: Freshness,
    context: &Context,
    visiting: &mut Vec<PathBuf>,
    sent: &mut Sent,
) -> Result<StoredResponse, String> {
    let path = find_request(&context.root, reference)?;
    // Sent during this evaluation: always fresh enough, and never sent twice.
    if let Some((_, response)) = sent.iter().find(|(p, _)| *p == path) {
        return Ok(response.clone());
    }
    let now = response_cache::now();
    let latest = context.latest.get(&path).cloned().or_else(|| {
        let cache = context.cache.as_ref()?;
        cache.load(&response_cache::cache_key(
            context.collection_id.as_deref(),
            &context.root,
            &path,
        ))
    });
    if let Some(response) = latest.filter(|r| freshness.allows(r.received_at, now)) {
        return Ok(response);
    }
    if visiting.contains(&path) {
        return Err(format!("\"{reference}\" depends on itself"));
    }
    if visiting.len() >= MAX_DEPTH {
        return Err(format!("requests chain more than {MAX_DEPTH} deep at \"{reference}\""));
    }
    visiting.push(path.clone());
    let response = send(&path, context, visiting, sent).await;
    visiting.pop();
    let response = response?;
    sent.retain(|(p, _)| *p != path);
    sent.push((path, response.clone()));
    Ok(response)
}

/// Sends the request at `path` with its inherited auth and its own calls evaluated.
async fn send(
    path: &Path,
    context: &Context,
    visiting: &mut Vec<PathBuf>,
    sent: &mut Sent,
) -> Result<StoredResponse, String> {
    let mut file: RequestFile = storage::read_yaml(path).map_err(|e| format!("{e:#}"))?;
    if file.auth.is_inherit() {
        let collection: CollectionFile =
            storage::read_yaml(&context.root.join(COLLECTION_FILE)).map_err(|e| format!("{e:#}"))?;
        file.auth = storage::folder_auths(&context.root, path)
            .into_iter()
            .rev()
            .map(|(_, auth)| auth)
            .find(|auth| !auth.is_inherit())
            .unwrap_or(collection.auth);
    }
    let mut variables = context.variables.clone();
    variables.extend(Box::pin(evaluate_in(&file, context, visiting, sent)).await?);
    let (request, missing) = Request::resolve(&file, &variables)?;
    if crate::model::is_websocket_url(&request.url) {
        return Err(format!(
            "\"{}\" is a WebSocket, which has no response to use",
            file.name
        ));
    }
    if !missing.is_empty() {
        return Err(format!(
            "\"{}\" uses undefined variables: {}",
            file.name,
            missing.join(", ")
        ));
    }

    let started = std::time::Instant::now();
    let (_handle, events) = transport::start_http(request, context.timeout, None, context.client.clone());
    let (mut head, mut body) = ((0, String::new(), Vec::new()), Vec::new());
    let mut bytes = 0;
    loop {
        match events.recv().await {
            Ok(transport::Event::Head {
                status,
                reason,
                headers,
                ..
            }) => head = (status, reason, headers),
            Ok(transport::Event::Chunk(chunk)) => {
                bytes += chunk.len();
                if body.len() < response_cache::MAX_SAVED_BODY {
                    body.extend_from_slice(&chunk);
                }
            }
            Ok(transport::Event::Sse(event)) => {
                bytes += event.data.len();
                body.extend_from_slice(event.data.as_bytes());
            }
            Ok(transport::Event::Done { .. }) => break,
            Ok(transport::Event::Failed(message)) => return Err(format!("sending \"{}\": {message}", file.name)),
            Ok(transport::Event::Ws(_)) => {}
            Err(_) => return Err(format!("sending \"{}\": the connection closed", file.name)),
        }
    }
    Ok(StoredResponse {
        received_at: response_cache::now(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        outcome: Outcome::Response {
            status: head.0,
            reason: head.1,
            headers: head.2,
            body: String::from_utf8_lossy(&body).into_owned(),
            body_size: bytes,
            truncated: bytes > body.len(),
        },
    })
}

/// Finds a request in the collection at `root` by name (ignoring case) or by path within it,
/// with or without `.yaml`.
pub fn find_request(root: &Path, reference: &str) -> Result<PathBuf, String> {
    let collection = storage::load_collection(root).map_err(|e| format!("{e:#}"))?;
    let reference = reference.trim();
    let by_path = collection.requests().into_iter().find(|entry| {
        entry.path.strip_prefix(root).is_ok_and(|relative| {
            let relative = relative.to_string_lossy();
            relative == reference || relative.strip_suffix(".yaml") == Some(reference)
        })
    });
    if let Some(entry) = by_path {
        return Ok(entry.path.to_path_buf());
    }
    let named: Vec<_> = collection
        .requests()
        .into_iter()
        .filter(|entry| entry.request.name.eq_ignore_ascii_case(reference))
        .map(|entry| entry.path.to_path_buf())
        .collect();
    match named.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("no request named \"{reference}\"")),
        many => Err(format!(
            "{} requests are named \"{reference}\"; use a path such as \"{}\"",
            many.len(),
            many[0].strip_prefix(root).unwrap_or(&many[0]).display()
        )),
    }
}

/// The first JSONPath match in a response body: strings as they are, anything else as JSON.
fn json_value(response: &StoredResponse, path: &str) -> Result<String, String> {
    let Outcome::Response { body, status, .. } = &response.outcome else {
        return Err("its last send failed".into());
    };
    let json: Value = serde_json::from_str(body).map_err(|_| format!("its response ({status}) isn't JSON"))?;
    let expression = JsonPath::parse(path).map_err(|e| {
        let message = e.to_string();
        format!("bad JSONPath `{path}`: {}", message.lines().next().unwrap_or_default())
    })?;
    match expression.query(&json).first() {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(value) => Ok(value.to_string()),
        None => Err(format!("nothing matches `{path}`")),
    }
}

fn now_rfc3339() -> String {
    let secs = response_cache::now() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// A reference to paste into another request: `{{ response("Login", "$.token") }}`.
pub fn reference(request_name: &str, json_path: &str) -> String {
    let quote = |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    format!("{{{{ response({}, {}) }}}}", quote(request_name), quote(json_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_calls_and_leaves_variables_alone() {
        assert_eq!(parse_call("base_url"), None);
        assert_eq!(
            parse_call(r#" response("Login", '$.data["token"]') "#),
            Some(Ok(Call::Response {
                request: "Login".into(),
                path: "$.data[\"token\"]".into(),
                freshness: Freshness::Latest
            }))
        );
        assert_eq!(
            parse_call(r#"response_header("auth/login.yaml", "Location", "always")"#),
            Some(Ok(Call::ResponseHeader {
                request: "auth/login.yaml".into(),
                header: "Location".into(),
                freshness: Freshness::Always
            }))
        );
        assert_eq!(parse_call("uuid()"), Some(Ok(Call::Uuid)));
        assert!(matches!(parse_call("response(\"Login\")"), Some(Err(_))));
        assert!(matches!(parse_call("response(Login, \"$\")"), Some(Err(_))));
        assert!(matches!(parse_call("reponse(\"a\", \"b\")"), Some(Err(e)) if e.contains("unknown")));
        assert!(matches!(
            parse_call("response(\"a\", \"b\", \"sometimes\")"),
            Some(Err(_))
        ));

        let text = reference("Say \"hi\"", "$.token");
        assert_eq!(text, r#"{{ response("Say \"hi\"", "$.token") }}"#);
        let inner = text.trim_start_matches("{{").trim_end_matches("}}");
        assert!(matches!(parse_call(inner), Some(Ok(Call::Response { request, .. })) if request == "Say \"hi\""));
    }

    #[test]
    fn finds_calls_across_a_request() {
        let mut file = RequestFile::new("Me");
        file.url = "{{base_url}}/users/{{ response(\"Login\", \"$.id\") }}".into();
        file.headers = crate::model::headers_from_text("X-Id: {{uuid()}}\n# X-Off: {{timestamp()}}");
        file.auth = Auth::Bearer {
            token: "{{response(\"Login\", \"$.id\")}}".into(),
        };
        let calls: Vec<String> = calls_in(&file).into_iter().map(|(text, _)| text).collect();
        assert_eq!(
            calls,
            ["response(\"Login\", \"$.id\")", "uuid()"],
            "each once, disabled headers skipped"
        );
    }

    #[test]
    fn freshness_decides_when_to_resend() {
        assert_eq!(Freshness::parse("5m"), Some(Freshness::MaxAge(300)));
        assert_eq!(Freshness::parse(" 2h "), Some(Freshness::MaxAge(7200)));
        assert_eq!(Freshness::parse("latest"), Some(Freshness::Latest));
        assert_eq!(Freshness::parse("5 minutes"), None);
        assert_eq!(Freshness::parse("m"), None);
        assert!(Freshness::MaxAge(60).allows(1_000, 1_060));
        assert!(!Freshness::MaxAge(60).allows(1_000, 1_061));
        assert!(Freshness::Latest.allows(0, u64::MAX));
        assert!(!Freshness::Always.allows(10, 10));
        assert!(matches!(
            parse_call("response(\"Login\", \"$.token\", \"10m\")"),
            Some(Ok(Call::Response {
                freshness: Freshness::MaxAge(600),
                ..
            }))
        ));
    }

    #[test]
    fn formats_dates() {
        let now = now_rfc3339();
        assert_eq!(now.len(), 20);
        assert!(now.starts_with("20") && now.ends_with('Z'));
    }

    #[test]
    fn picks_json_values() {
        let response = StoredResponse {
            received_at: 0,
            elapsed_ms: 0,
            outcome: Outcome::Response {
                status: 200,
                reason: "OK".into(),
                headers: Vec::new(),
                body: r#"{"token":"abc","user":{"id":7,"roles":["admin"]}}"#.into(),
                body_size: 0,
                truncated: false,
            },
        };
        assert_eq!(json_value(&response, "$.token").unwrap(), "abc");
        assert_eq!(json_value(&response, "$.user.id").unwrap(), "7");
        assert_eq!(json_value(&response, "$.user.roles").unwrap(), "[\"admin\"]");
        assert!(
            json_value(&response, "$.missing")
                .unwrap_err()
                .contains("nothing matches")
        );
        assert!(json_value(&response, "$[").is_err());
    }
}
