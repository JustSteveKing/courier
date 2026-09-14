//! Blocking HTTP execution. Call from a background executor, never the UI thread.

use std::time::{Duration, Instant};

use rust_i18n::t;

use crate::model::{BodyKind, RequestFile, Variables, interpolate};

/// A request with variables already substituted, ready to go on the wire.
#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    /// Resolves `{{variables}}` and drops disabled headers. Returns the names of any
    /// variables that had no value, so the UI can warn before sending.
    pub fn resolve(file: &RequestFile, variables: &Variables) -> (Self, Vec<String>) {
        let mut missing = Vec::new();
        let mut sub = |text: &str| {
            let (out, names) = interpolate(text, variables);
            for name in names {
                if !missing.contains(&name) {
                    missing.push(name);
                }
            }
            out
        };

        let mut headers: Vec<(String, String)> = file
            .headers
            .iter()
            .filter(|h| h.enabled)
            .map(|h| (sub(&h.name), sub(&h.value)))
            .collect();
        let body = file.body.as_ref().map(|b| sub(&b.content)).unwrap_or_default();

        if let Some(kind) = file.body.as_ref().map(|b| b.kind)
            && !body.is_empty()
            && !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            let content_type = match kind {
                BodyKind::Json => "application/json",
                BodyKind::Xml => "application/xml",
                BodyKind::Text => "text/plain",
                BodyKind::FormUrlencoded => "application/x-www-form-urlencoded",
            };
            headers.push(("Content-Type".into(), content_type.into()));
        }

        let request = Self {
            method: file.method.clone(),
            url: sub(file.url.trim()),
            headers,
            body,
        };
        (request, missing)
    }
}

pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub elapsed: Duration,
}

/// A response body, re-indented when it is JSON.
pub fn pretty_body(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| serde_json::to_string_pretty(&json).ok())
        .unwrap_or_else(|| body.to_string())
}

/// One agent for the whole app, so repeated requests to a host reuse its connections.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| ureq::Agent::config_builder().http_status_as_error(false).build().into())
}

pub fn send(request: &Request) -> Result<Response, String> {
    let agent = agent();

    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(request.url.as_str());
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let http_request = builder
        .body(request.body.clone())
        .map_err(|e| t!("http.invalid_request", error = e).to_string())?;

    let started = Instant::now();
    let mut response = agent.run(http_request).map_err(|e| e.to_string())?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| t!("http.failed_to_read_body", error = e).to_string())?;

    Ok(Response {
        status: response.status().as_u16(),
        reason: response
            .status()
            .canonical_reason()
            .unwrap_or_default()
            .to_string(),
        headers: response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect(),
        body,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Body, Header};

    #[test]
    fn resolves_variables_and_content_type() {
        let mut file = RequestFile::new("Create");
        file.method = "POST".into();
        file.url = " {{base}}/users ".into();
        file.headers = vec![
            Header { name: "Authorization".into(), value: "Bearer {{token}}".into(), enabled: true },
            Header { name: "X-Off".into(), value: "1".into(), enabled: false },
        ];
        file.body = Some(Body { kind: BodyKind::Json, content: "{\"id\": \"{{id}}\"}".into() });
        let vars = Variables::from([
            ("base".to_string(), "https://api.test".to_string()),
            ("id".to_string(), "42".to_string()),
        ]);

        let (request, missing) = Request::resolve(&file, &vars);
        assert_eq!(request.url, "https://api.test/users");
        assert_eq!(request.body, "{\"id\": \"42\"}");
        assert_eq!(
            request.headers,
            vec![
                ("Authorization".into(), "Bearer {{token}}".into()),
                ("Content-Type".into(), "application/json".into()),
            ]
        );
        assert_eq!(missing, vec!["token"]);
    }
}
