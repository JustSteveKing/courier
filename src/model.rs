//! The on-disk YAML format. Everything here is plain data with no I/O.
//!
//! A collection is the `.courier/` directory of a project (see `crate::project`):
//!
//! ```text
//! my-project/.courier/
//!   collection.yaml        CollectionFile
//!   environments/
//!     local.yaml           EnvironmentFile
//!   get-users.yaml         RequestFile
//!   admin/                 folders are plain subdirectories
//!     delete-user.yaml
//! ```

use indexmap::IndexMap;
use rust_i18n::t;
use serde::{Deserialize, Serialize};

pub const COLLECTION_FILE: &str = "collection.yaml";
pub const ENVIRONMENTS_DIR: &str = "environments";
pub const FORMAT_VERSION: u32 = 1;

/// Variable names to values, kept in the order they were written.
pub type Variables = IndexMap<String, String>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CollectionFile {
    #[serde(default = "format_version")]
    pub version: u32,
    /// Stable identity used to find this collection's secrets in the keyring, so moving
    /// or renaming the folder doesn't orphan them. Older files may not have one yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    /// Defaults that any environment can override.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: Variables,
    /// Names of secret defaults. Their values live in the secret store, never in YAML.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

impl CollectionFile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: FORMAT_VERSION,
            id: Some(new_collection_id()),
            name: name.into(),
            variables: Variables::new(),
            secrets: Vec::new(),
        }
    }

    /// Returns the id, assigning one first if this file predates ids. The caller must
    /// save the file if this returns true in the second position.
    pub fn ensure_id(&mut self) -> (String, bool) {
        match &self.id {
            Some(id) => (id.clone(), false),
            None => {
                let id = new_collection_id();
                self.id = Some(id.clone());
                (id, true)
            }
        }
    }
}

pub fn new_collection_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct EnvironmentFile {
    pub name: String,
    #[serde(default)]
    pub variables: Variables,
    /// Names of secret variables. Their values live in the secret store, never in YAML.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

impl EnvironmentFile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            variables: Variables::new(),
            secrets: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RequestFile {
    pub name: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
    /// Sort position within its folder; requests without one sort after, by file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// Saved messages for a WebSocket request (`ws://` or `wss://` URL).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<MessageTemplate>,
    /// Present for a GraphQL request; the body is built from it when sending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphql: Option<Graphql>,
}

impl RequestFile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            method: default_method(),
            url: String::new(),
            headers: Vec::new(),
            body: None,
            order: None,
            messages: Vec::new(),
            graphql: None,
        }
    }
}

/// Whether a (resolved) URL opens a WebSocket rather than an HTTP request.
pub fn is_websocket_url(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("ws://") || url.starts_with("wss://")
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MessageTemplate {
    pub name: String,
    pub content: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Graphql {
    pub query: String,
    /// JSON object text; may contain `{{variables}}`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub variables: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Header {
    pub name: String,
    pub value: String,
    #[serde(default = "enabled", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
}

impl Header {
    /// An enabled header.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            enabled: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Body {
    #[serde(rename = "type")]
    pub kind: BodyKind,
    pub content: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BodyKind {
    Json,
    Xml,
    Text,
    FormUrlencoded,
}

impl BodyKind {
    /// Guesses the kind from a Content-Type header value.
    pub fn from_content_type(content_type: &str) -> Self {
        let content_type = content_type.to_ascii_lowercase();
        if content_type.contains("json") {
            Self::Json
        } else if content_type.contains("xml") {
            Self::Xml
        } else if content_type.contains("x-www-form-urlencoded") {
            Self::FormUrlencoded
        } else {
            Self::Text
        }
    }
}

fn format_version() -> u32 {
    FORMAT_VERSION
}

fn default_method() -> String {
    "GET".into()
}

fn enabled() -> bool {
    true
}

fn is_enabled(enabled: &bool) -> bool {
    *enabled
}

/// Renders headers for the text editor: `Name: value`, disabled ones prefixed with `# `.
pub fn headers_to_text(headers: &[Header]) -> String {
    headers
        .iter()
        .map(|h| {
            let prefix = if h.enabled { "" } else { "# " };
            format!("{prefix}{}: {}", h.name, h.value)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses the header editor text. Lines starting with `#` are disabled headers.
pub fn headers_from_text(text: &str) -> Vec<Header> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (enabled, line) = match line.strip_prefix('#') {
                Some(rest) => (false, rest.trim_start()),
                None => (true, line),
            };
            let (name, value) = line.split_once(':')?;
            let name = name.trim();
            (!name.is_empty()).then(|| Header {
                name: name.to_string(),
                value: value.trim().to_string(),
                enabled,
            })
        })
        .collect()
}

/// Renders variables for the text editor, one `name: value` per line.
pub fn variables_to_text(variables: &Variables) -> String {
    variables
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses the variable editor text. Unlike headers, mistakes are errors rather than
/// silently dropped lines, because a lost variable is hard to notice.
pub fn variables_from_text(text: &str) -> Result<Variables, String> {
    let mut variables = Variables::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(t!("vars.line_expected", line = line_no).to_string());
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(t!("vars.line_name_empty", line = line_no).to_string());
        }
        if name.contains(['{', '}']) || name.chars().any(char::is_whitespace) {
            return Err(t!("vars.line_name_invalid", line = line_no, name = name).to_string());
        }
        if variables.insert(name.to_string(), value.trim().to_string()).is_some() {
            return Err(t!("vars.line_duplicate", line = line_no, name = name).to_string());
        }
    }
    Ok(variables)
}

/// The `{{name}}` placeholder for a variable.
pub fn placeholder(name: &str) -> String {
    format!("{{{{{name}}}}}")
}

/// Replaces `{{name}}` with values from `variables`. Returns the names that had no value;
/// those placeholders are left in place.
pub fn interpolate(text: &str, variables: &Variables) -> (String, Vec<String>) {
    let mut output = String::with_capacity(text.len());
    let mut missing = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        output.push_str(&rest[..start]);
        let placeholder = &rest[start..start + 2 + len + 2];
        let name = rest[start + 2..start + 2 + len].trim();
        match variables.get(name) {
            Some(value) => output.push_str(value),
            None => {
                output.push_str(placeholder);
                if !missing.iter().any(|m| m == name) {
                    missing.push(name.to_string());
                }
            }
        }
        rest = &rest[start + placeholder.len()..];
    }
    output.push_str(rest);
    (output, missing)
}

/// Turns a display name into a file-system-friendly stem: `Get Users!` becomes `get-users`.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "untitled".into()
    } else {
        slug.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_yaml() {
        let request = RequestFile {
            name: "Create user".into(),
            method: "POST".into(),
            url: "{{base_url}}/users".into(),
            headers: vec![
                Header {
                    name: "Accept".into(),
                    value: "application/json".into(),
                    enabled: true,
                },
                Header {
                    name: "X-Debug".into(),
                    value: "1".into(),
                    enabled: false,
                },
            ],
            body: Some(Body {
                kind: BodyKind::Json,
                content: "{\n  \"name\": \"Ada\"\n}".into(),
            }),
            order: Some(2),
            messages: vec![MessageTemplate {
                name: "Subscribe".into(),
                content: "{\"op\": \"sub\"}".into(),
            }],
            graphql: None,
        };
        let yaml = serde_norway::to_string(&request).unwrap();
        assert!(!yaml.contains("enabled: true"), "enabled headers stay terse:\n{yaml}");
        assert_eq!(serde_norway::from_str::<RequestFile>(&yaml).unwrap(), request);
    }

    #[test]
    fn minimal_request_uses_defaults() {
        let request: RequestFile = serde_norway::from_str("name: Ping\nurl: https://x.test").unwrap();
        assert_eq!(request.method, "GET");
        assert!(request.headers.is_empty() && request.body.is_none());
    }

    #[test]
    fn headers_text_round_trips() {
        let text = "Accept: application/json\n# X-Debug: 1\n\nnot a header\nX-Token:  abc:def ";
        let headers = headers_from_text(text);
        assert_eq!(headers.len(), 3);
        assert!(!headers[1].enabled);
        assert_eq!(headers[2].value, "abc:def");
        assert_eq!(
            headers_to_text(&headers),
            "Accept: application/json\n# X-Debug: 1\nX-Token: abc:def"
        );
    }

    #[test]
    fn interpolates_and_reports_missing() {
        let vars = Variables::from([("host".to_string(), "api.test".to_string())]);
        let (out, missing) = interpolate("https://{{ host }}/{{id}}/{{id}}?q={{", &vars);
        assert_eq!(out, "https://api.test/{{id}}/{{id}}?q={{");
        assert_eq!(missing, vec!["id"]);
    }

    #[test]
    fn variables_text_round_trips_in_order() {
        let text = "zeta: 1\n\nbase_url: https://api.test:8443/v1\ntoken:";
        let variables = variables_from_text(text).unwrap();
        assert_eq!(variables.keys().collect::<Vec<_>>(), ["zeta", "base_url", "token"]);
        assert_eq!(variables["base_url"], "https://api.test:8443/v1");
        assert_eq!(variables["token"], "");
        assert_eq!(
            variables_to_text(&variables),
            "zeta: 1\nbase_url: https://api.test:8443/v1\ntoken: "
        );

        let yaml = serde_norway::to_string(&EnvironmentFile {
            name: "Dev".into(),
            variables,
            secrets: Vec::new(),
        })
        .unwrap();
        assert!(
            yaml.find("zeta").unwrap() < yaml.find("base_url").unwrap(),
            "order kept on disk:\n{yaml}"
        );
    }

    #[test]
    fn variables_text_reports_mistakes() {
        assert_eq!(
            variables_from_text("a: 1\nnope").unwrap_err(),
            "Line 2: expected `name: value`"
        );
        assert_eq!(
            variables_from_text("a: 1\na: 2").unwrap_err(),
            "Line 2: `a` is defined twice"
        );
        assert!(variables_from_text(": x").is_err());
        assert!(variables_from_text("base url: x").is_err());
    }

    /// Validation messages are translated, keeping the interpolated values intact. Uses an
    /// explicit locale: the global one is process-wide and tests run in parallel.
    #[test]
    fn validation_messages_are_translated() {
        let cases = [
            ("en", "Line 2: `a` is defined twice"),
            ("es", "Línea 2: `a` está definida dos veces"),
            ("de", "Zeile 2: `a` ist doppelt definiert"),
            ("fr", "Ligne 2 : `a` est défini deux fois"),
        ];
        for (locale, expected) in cases {
            assert_eq!(
                t!("vars.line_duplicate", locale = locale, line = 2, name = "a"),
                expected
            );
        }
        assert_eq!(t!("request.send_shortcut", locale = "de"), "Strg+Eingabe");
        assert_eq!(t!("env.copy_name", locale = "fr", name = "Prod"), "Prod (copie)");
    }

    #[test]
    fn slugifies_names() {
        assert_eq!(slugify("Get Users!"), "get-users");
        assert_eq!(slugify("  ✨ "), "untitled");
        assert_eq!(slugify("Ümlaut Café"), "ümlaut-café");
    }
}
