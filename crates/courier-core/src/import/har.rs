//! HAR files: what browser dev tools and proxies export. A HAR is a recording of traffic,
//! so importing one means turning each entry back into a request worth keeping.

use std::collections::HashSet;

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::model::{Auth, Body, BodyKind, CollectionFile, Header, RequestFile};

use super::{CollectionImport, ImportItem};

/// Headers a browser writes for itself. Keeping them makes every request noisy and some of
/// them (`Host`, `Content-Length`) actively wrong once the URL or body changes.
const DROPPED: [&str; 14] = [
    "host",
    "content-length",
    "connection",
    "accept-encoding",
    "sec-fetch-mode",
    "sec-fetch-site",
    "sec-fetch-dest",
    "sec-fetch-user",
    "sec-ch-ua",
    "sec-ch-ua-mobile",
    "sec-ch-ua-platform",
    "upgrade-insecure-requests",
    "te",
    "dnt",
];

/// Converts a HAR document. Entries for the same method and path collapse into one request:
/// a recording usually holds the same call many times over.
pub fn convert(document: &Value, name: &str) -> Result<CollectionImport> {
    let entries = document
        .pointer("/log/entries")
        .and_then(Value::as_array)
        .context("this HAR has no log.entries")?;

    let mut collection = CollectionFile::new(name);
    let mut items = Vec::new();
    let mut warnings = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cookies = 0usize;
    let mut hosts: Vec<String> = Vec::new();
    let mut secrets = crate::model::Variables::new();
    let mut hoisted: Vec<String> = Vec::new();

    for entry in entries {
        let Some(request) = entry.get("request") else {
            continue;
        };
        let url = request.get("url").and_then(Value::as_str).unwrap_or_default();
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let Ok(parsed) = url::Url::parse(url) else {
            warnings.push(format!("skipped an entry with an unusable URL: {url}"));
            continue;
        };
        if !seen.insert(format!(
            "{method} {}{}",
            parsed.host_str().unwrap_or_default(),
            parsed.path()
        )) {
            continue;
        }
        if let Some(host) = parsed.host_str()
            && !hosts.iter().any(|known| known == host)
        {
            hosts.push(host.to_string());
        }

        let mut file = RequestFile::new(name_for(&method, &parsed));
        file.method = method;
        file.url = url.to_string();
        file.headers = headers_of(request, &mut cookies);
        file.disabled_params = Vec::new();
        if let Some((auth, header)) = auth_of(&file.headers) {
            file.auth = auth;
            file.headers.remove(header);
        }
        file.body = body_of(request, &mut warnings);
        // A recorded token is a real credential: it goes to the secret store, not the file.
        if let Some(name) = crate::credentials::hoist_auth(&mut file, &mut secrets, &|_| false) {
            hoisted.push(name);
        }
        hoisted.extend(crate::credentials::hoist_credentials_with(
            &mut file,
            &mut secrets,
            &|_| false,
        ));
        items.push(ImportItem::Request(file));
    }

    if items.is_empty() {
        anyhow::bail!("this HAR holds no requests");
    }
    // One base_url variable per host, so the requests can be pointed elsewhere later.
    if let Some(first) = hosts.first() {
        let scheme_and_host = format!("https://{first}");
        collection.variables.insert("base_url".into(), scheme_and_host.clone());
        for item in &mut items {
            if let ImportItem::Request(request) = item {
                request.url = request.url.replacen(&scheme_and_host, "{{base_url}}", 1);
            }
        }
        if hosts.len() > 1 {
            warnings.push(format!(
                "requests to {} hosts: base_url is set to {first}, the others keep their full URL",
                hosts.len()
            ));
        }
    }
    if cookies > 0 {
        warnings.push(format!(
            "{cookies} recorded Cookie header(s) left out: sign in through the request instead, \
             and Courier keeps the cookies"
        ));
    }

    if !hoisted.is_empty() {
        hoisted.sort();
        hoisted.dedup();
        warnings.push(format!(
            "{} recorded credential(s) moved to secrets: {}",
            hoisted.len(),
            hoisted.join(", ")
        ));
        collection.secrets.extend(hoisted);
    }
    Ok(CollectionImport {
        collection,
        items,
        environments: Vec::new(),
        secrets,
        warnings,
    })
}

/// "GET /pets/7" is what a recording gives us to go on.
fn name_for(method: &str, url: &url::Url) -> String {
    let path = url.path().trim_end_matches('/');
    if path.is_empty() {
        format!("{method} /")
    } else {
        format!("{method} {path}")
    }
}

fn headers_of(request: &Value, cookies: &mut usize) -> Vec<Header> {
    request
        .get("headers")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|header| {
                    let name = header.get("name").and_then(Value::as_str)?.trim();
                    let value = header.get("value").and_then(Value::as_str).unwrap_or_default();
                    // HTTP/2 pseudo-headers describe the request line, not headers.
                    if name.starts_with(':') || DROPPED.contains(&name.to_ascii_lowercase().as_str()) {
                        return None;
                    }
                    if name.eq_ignore_ascii_case("cookie") {
                        *cookies += 1;
                        return None;
                    }
                    Some(Header {
                        name: name.to_string(),
                        value: value.to_string(),
                        enabled: true,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// An Authorization header becomes the request's auth, so it can be inherited and hoisted.
fn auth_of(headers: &[Header]) -> Option<(Auth, usize)> {
    let (index, header) = headers
        .iter()
        .enumerate()
        .find(|(_, header)| header.name.eq_ignore_ascii_case("authorization"))?;
    let value = header.value.trim();
    if let Some(token) = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")) {
        return Some((
            Auth::Bearer {
                token: token.trim().to_string(),
            },
            index,
        ));
    }
    if let Some(encoded) = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic ")) {
        let decoded = crate::encoding::base64_decode(encoded.trim())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default();
        if let Some((username, password)) = decoded.split_once(':') {
            return Some((
                Auth::Basic {
                    username: username.to_string(),
                    password: password.to_string(),
                },
                index,
            ));
        }
    }
    None
}

fn body_of(request: &Value, warnings: &mut Vec<String>) -> Option<Body> {
    let posted = request.get("postData")?;
    let mime = posted.get("mimeType").and_then(Value::as_str).unwrap_or_default();
    let kind = BodyKind::from_content_type(mime);
    if let Some(text) = posted.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        return Some(Body {
            kind,
            content: crate::http::pretty_body(text),
        });
    }
    // A form post may arrive as params rather than text.
    let params = posted.get("params").and_then(Value::as_array)?;
    if kind == BodyKind::Multipart {
        warnings
            .push("a multipart upload was recorded without its files; the parts are listed for you to fill in".into());
    }
    let content = params
        .iter()
        .filter_map(|param| {
            let name = param.get("name").and_then(Value::as_str)?;
            let value = param
                .get("value")
                .and_then(Value::as_str)
                .or_else(|| param.get("fileName").and_then(Value::as_str))
                .unwrap_or_default();
            Some(match kind {
                BodyKind::Multipart => format!("{name}: {value}"),
                _ => format!(
                    "{}={}",
                    crate::encoding::form_encode(name),
                    crate::encoding::form_encode(value)
                ),
            })
        })
        .collect::<Vec<_>>()
        .join(if kind == BodyKind::Multipart { "\n" } else { "&" });
    (!content.is_empty()).then_some(Body { kind, content })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAR: &str = r#"{
      "log": {
        "version": "1.2",
        "creator": { "name": "Firefox", "version": "140" },
        "entries": [
          {
            "request": {
              "method": "GET",
              "url": "https://api.test/pets?limit=2",
              "headers": [
                { "name": ":authority", "value": "api.test" },
                { "name": "Host", "value": "api.test" },
                { "name": "Accept", "value": "application/json" },
                { "name": "Cookie", "value": "session=abc" },
                { "name": "Authorization", "value": "Bearer tok-1" },
                { "name": "sec-fetch-mode", "value": "cors" }
              ]
            }
          },
          {
            "request": {
              "method": "GET",
              "url": "https://api.test/pets?limit=50",
              "headers": []
            }
          },
          {
            "request": {
              "method": "POST",
              "url": "https://api.test/pets",
              "headers": [{ "name": "Content-Type", "value": "application/json" }],
              "postData": { "mimeType": "application/json", "text": "{\"name\":\"Rex\"}" }
            }
          },
          {
            "request": {
              "method": "POST",
              "url": "https://other.test/login",
              "headers": [{ "name": "Authorization", "value": "Basic Y291cmllcjpodW50ZXIy" }],
              "postData": {
                "mimeType": "application/x-www-form-urlencoded",
                "params": [{ "name": "user", "value": "a b" }, { "name": "next", "value": "/home" }]
              }
            }
          }
        ]
      }
    }"#;

    fn requests(import: &CollectionImport) -> Vec<&RequestFile> {
        import
            .items
            .iter()
            .map(|item| match item {
                ImportItem::Request(request) => request,
                ImportItem::Folder { .. } => panic!("HAR imports are flat"),
            })
            .collect()
    }

    #[test]
    fn turns_a_recording_into_requests() {
        let document: Value = serde_json::from_str(HAR).unwrap();
        let import = convert(&document, "Recording").unwrap();
        let requests = requests(&import);
        assert_eq!(
            requests.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["GET /pets", "POST /pets", "POST /login"],
            "the same call twice becomes one request"
        );

        let first = requests[0];
        assert_eq!(
            first.url, "{{base_url}}/pets?limit=2",
            "the first host becomes base_url"
        );
        assert_eq!(import.collection.variables["base_url"], "https://api.test");
        assert_eq!(
            first.headers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            ["Accept"],
            "the browser's own headers and cookies are left out"
        );
        assert_eq!(
            first.auth,
            Auth::Bearer {
                token: "{{token}}".into()
            },
            "an Authorization header becomes auth, with the recorded token moved to a secret"
        );
        assert_eq!(import.secrets["token"], "tok-1");
        assert!(import.collection.secrets.contains(&"token".to_string()));

        let created = requests[1];
        assert_eq!(created.method, "POST");
        assert_eq!(
            created.body.as_ref().map(|b| (b.kind, b.content.as_str())),
            Some((BodyKind::Json, "{\n  \"name\": \"Rex\"\n}")),
            "a JSON body is pretty-printed"
        );

        let login = requests[2];
        assert_eq!(login.url, "https://other.test/login", "another host keeps its URL");
        assert_eq!(
            login.auth,
            Auth::Basic {
                username: "courier".into(),
                password: "{{password}}".into()
            },
            "a recorded Basic password is a secret too"
        );
        assert_eq!(import.secrets["password"], "hunter2");
        assert_eq!(
            login.body.as_ref().map(|b| b.content.as_str()),
            Some("user=a+b&next=%2Fhome"),
            "a form recorded as params is put back together"
        );

        let warnings = import.warnings.join(" ");
        assert!(warnings.contains("Cookie"), "{warnings}");
        assert!(warnings.contains("2 hosts"), "{warnings}");
    }

    #[test]
    fn says_when_there_is_nothing_to_import() {
        let empty: Value = serde_json::from_str(r#"{"log":{"entries":[]}}"#).unwrap();
        assert!(convert(&empty, "x").unwrap_err().to_string().contains("no requests"));
        let wrong: Value = serde_json::from_str(r#"{"hello":true}"#).unwrap();
        assert!(convert(&wrong, "x").unwrap_err().to_string().contains("log.entries"));
    }
}
