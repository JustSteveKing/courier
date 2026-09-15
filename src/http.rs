//! Resolving a saved request into what goes on the wire. Sending lives in `crate::transport`.

use rust_i18n::t;

use crate::encoding::{base64_encode, percent_encode};
use crate::model::{Auth, BodyKind, Graphql, RequestFile, Variables, interpolate};

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
    /// variables that had no value, so the UI can warn before sending. Fails only when a
    /// GraphQL request's variables aren't a JSON object.
    pub fn resolve(file: &RequestFile, variables: &Variables) -> Result<(Self, Vec<String>), String> {
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
        let (body, kind) = match &file.graphql {
            Some(graphql) => (graphql_body(graphql, &mut sub)?, Some(BodyKind::Json)),
            None => (
                file.body.as_ref().map(|b| sub(&b.content)).unwrap_or_default(),
                file.body.as_ref().map(|b| b.kind),
            ),
        };

        if let Some(kind) = kind
            && !body.is_empty()
            && !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            let content_type = match kind {
                BodyKind::Json => "application/json",
                BodyKind::Xml => "application/xml",
                BodyKind::Text => "text/plain",
                BodyKind::FormUrlencoded => "application/x-www-form-urlencoded",
            };
            headers.push(("Content-Type".into(), content_type.into()));
        }

        let mut url = sub(file.url.trim());
        // `file.auth` is the effective auth by now: callers resolve `Inherit` first.
        let has_header =
            |headers: &[(String, String)], name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
        match &file.auth {
            Auth::Basic { username, password } if !has_header(&headers, "authorization") => {
                let credentials = format!("{}:{}", sub(username), sub(password));
                headers.push((
                    "Authorization".into(),
                    format!("Basic {}", base64_encode(credentials.as_bytes())),
                ));
            }
            Auth::Bearer { token } if !has_header(&headers, "authorization") => {
                headers.push(("Authorization".into(), format!("Bearer {}", sub(token))));
            }
            Auth::ApiKey { name, value, in_query } if !name.trim().is_empty() => {
                let (name, value) = (sub(name.trim()), sub(value));
                if *in_query {
                    let (before, fragment) = match url.find('#') {
                        Some(i) => (url[..i].to_string(), url[i..].to_string()),
                        None => (url.clone(), String::new()),
                    };
                    let separator = if before.contains('?') { '&' } else { '?' };
                    url = format!(
                        "{before}{separator}{}={}{fragment}",
                        percent_encode(&name),
                        percent_encode(&value)
                    );
                } else if !has_header(&headers, &name) {
                    headers.push((name, value));
                }
            }
            _ => {}
        }

        let request = Self {
            method: file.method.clone(),
            url,
            headers,
            body,
        };
        Ok((request, missing))
    }
}

/// The JSON a GraphQL request posts: `{query, variables, operationName}`. Variables are
/// substituted as text before parsing, like any other body.
fn graphql_body(graphql: &Graphql, sub: &mut impl FnMut(&str) -> String) -> Result<String, String> {
    let variables = sub(&graphql.variables);
    let variables = if variables.trim().is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_str::<serde_json::Value>(&variables) {
            Ok(value @ (serde_json::Value::Object(_) | serde_json::Value::Null)) => value,
            Ok(_) => return Err(t!("request.graphql_variables_not_object").to_string()),
            Err(e) => return Err(t!("request.graphql_invalid_variables", error = e.to_string()).to_string()),
        }
    };
    let mut body = serde_json::json!({ "query": sub(&graphql.query), "variables": variables });
    if let Some(name) = graphql
        .operation_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        body["operationName"] = sub(name).into();
    }
    Ok(body.to_string())
}

impl Request {
    /// The request as a `curl` command, quoted for POSIX shells.
    pub fn to_curl(&self) -> String {
        let method = self.method.to_ascii_uppercase();
        let implied = if self.body.is_empty() { "GET" } else { "POST" };
        let mut command = String::from("curl");
        if method != implied {
            command.push_str(&format!(" -X {method}"));
        }
        command.push(' ');
        command.push_str(&shell_quote(&self.url));
        for (name, value) in &self.headers {
            command.push_str(&format!(" \\\n  -H {}", shell_quote(&format!("{name}: {value}"))));
        }
        if !self.body.is_empty() {
            command.push_str(&format!(" \\\n  --data-raw {}", shell_quote(&self.body)));
        }
        command
    }
}

/// Single-quotes `text` for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A response body, re-indented when it is JSON.
pub fn pretty_body(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| serde_json::to_string_pretty(&json).ok())
        .unwrap_or_else(|| body.to_string())
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
            Header {
                name: "Authorization".into(),
                value: "Bearer {{token}}".into(),
                enabled: true,
            },
            Header {
                name: "X-Off".into(),
                value: "1".into(),
                enabled: false,
            },
        ];
        file.body = Some(Body {
            kind: BodyKind::Json,
            content: "{\"id\": \"{{id}}\"}".into(),
        });
        let vars = Variables::from([
            ("base".to_string(), "https://api.test".to_string()),
            ("id".to_string(), "42".to_string()),
        ]);

        let (request, missing) = Request::resolve(&file, &vars).unwrap();
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

    #[test]
    fn exports_curl() {
        let request = Request {
            method: "POST".into(),
            url: "https://api.test/pets?q=it's".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: "{\"name\": \"Rex\"}".into(),
        };
        assert_eq!(
            request.to_curl(),
            "curl 'https://api.test/pets?q=it'\\''s' \\\n  -H 'Content-Type: application/json' \\\n  --data-raw '{\"name\": \"Rex\"}'"
        );
        let get = Request {
            method: "DELETE".into(),
            url: "https://api.test/pets/1".into(),
            headers: Vec::new(),
            body: String::new(),
        };
        assert_eq!(get.to_curl(), "curl -X DELETE 'https://api.test/pets/1'");

        // What we export, we can import again.
        let file = crate::import::curl::parse(&request.to_curl()).unwrap();
        assert_eq!(
            (file.method.as_str(), file.url.as_str()),
            ("POST", "https://api.test/pets?q=it's")
        );
    }

    #[test]
    fn applies_auth() {
        let vars = Variables::from([
            ("token".to_string(), "t0k".to_string()),
            ("password".to_string(), "p@ss".to_string()),
        ]);
        let mut file = RequestFile::new("Me");
        file.url = "https://api.test/me#top".into();
        let header = |file: &RequestFile| Request::resolve(file, &vars).unwrap().0.headers;

        file.auth = Auth::Bearer {
            token: "{{token}}".into(),
        };
        assert_eq!(header(&file), [("Authorization".to_string(), "Bearer t0k".to_string())]);

        file.auth = Auth::Basic {
            username: "sam".into(),
            password: "{{password}}".into(),
        };
        assert_eq!(header(&file)[0].1, format!("Basic {}", base64_encode(b"sam:p@ss")));

        // An explicit Authorization header wins.
        file.headers = crate::model::headers_from_text("Authorization: Custom 1");
        assert_eq!(header(&file), [("Authorization".to_string(), "Custom 1".to_string())]);
        file.headers.clear();

        file.auth = Auth::ApiKey {
            name: "api key".into(),
            value: "{{token}}".into(),
            in_query: true,
        };
        let (request, _) = Request::resolve(&file, &vars).unwrap();
        assert_eq!(request.url, "https://api.test/me?api%20key=t0k#top");
        assert!(request.headers.is_empty());

        file.auth = Auth::None;
        assert!(header(&file).is_empty());
    }

    #[test]
    fn graphql_posts_query_variables_and_operation() {
        let mut file = RequestFile::new("Search");
        file.method = "POST".into();
        file.url = "{{base}}/graphql".into();
        file.graphql = Some(Graphql {
            query: "query Pets($first: Int) { pets(first: $first) { id } }".into(),
            variables: "{\"first\": {{limit}}}".into(),
            operation_name: Some("Pets".into()),
        });
        let vars = Variables::from([
            ("base".to_string(), "https://api.test".to_string()),
            ("limit".to_string(), "5".to_string()),
        ]);

        let (request, missing) = Request::resolve(&file, &vars).unwrap();
        assert!(missing.is_empty());
        assert_eq!(
            request.headers,
            vec![("Content-Type".into(), "application/json".into())]
        );
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["variables"]["first"], 5);
        assert_eq!(body["operationName"], "Pets");
        assert!(body["query"].as_str().unwrap().starts_with("query Pets"));

        file.graphql.as_mut().unwrap().variables = String::new();
        let (request, _) = Request::resolve(&file, &vars).unwrap();
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert!(body["variables"].is_null());

        file.graphql.as_mut().unwrap().variables = "[1]".into();
        assert!(Request::resolve(&file, &vars).is_err());
        file.graphql.as_mut().unwrap().variables = "{nope".into();
        assert!(Request::resolve(&file, &vars).is_err());
    }
}
