//! Importing Postman collections (format v2.0 and v2.1) and environment exports.

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use super::{CollectionImport, ImportItem};
use crate::credentials::{hoist_credentials_with, looks_sensitive_name};
use crate::encoding::{base64_encode, percent_encode};
use crate::model::{
    Body, BodyKind, CollectionFile, EnvironmentFile, Header, RequestFile, Variables, headers_from_text,
};

#[derive(Debug)]
pub struct EnvironmentImport {
    /// Secret names are listed in `file.secrets`; their values are only in `secrets`.
    pub file: EnvironmentFile,
    /// Secret name -> value. Must go to the secret store, never to YAML.
    pub secrets: Variables,
    pub warnings: Vec<String>,
}

pub fn parse_collection(json: &str) -> Result<CollectionImport> {
    let root: Value = serde_json::from_str(json).context("The file is not valid JSON")?;
    let (Some(name), Some(items)) = (
        root.pointer("/info/name").and_then(Value::as_str),
        root.get("item").and_then(Value::as_array),
    ) else {
        bail!("Not a Postman collection: expected `info.name` and an `item` list");
    };

    let mut collection = CollectionFile::new(name);
    let mut secrets = Variables::new();
    let mut warnings = Vec::new();
    for variable in root.get("variable").and_then(Value::as_array).into_iter().flatten() {
        if variable.get("disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(key) = variable.get("key").and_then(Value::as_str) {
            let value = variable.get("value").map(value_to_string).unwrap_or_default();
            sort_variable(
                key,
                value,
                variable,
                &mut collection.variables,
                &mut collection.secrets,
                &mut secrets,
                &mut warnings,
            );
        }
    }

    warn_events(&root, &format!("collection \"{name}\""), &mut warnings);
    let auth = Inherited::from(root.get("auth"), None);
    let mut items = convert_items(items, auth, "", &mut warnings);

    let variables = collection.variables.clone();
    let mut hoisted = Vec::new();
    hoist_items(
        &mut items,
        &mut secrets,
        &|name| variables.contains_key(name),
        &mut hoisted,
    );
    if !hoisted.is_empty() {
        warnings.push(format!(
            "{} credentials moved to secrets: {}",
            hoisted.len(),
            hoisted.join(", ")
        ));
    }
    for name in secrets.keys() {
        if !collection.secrets.contains(name) {
            collection.secrets.push(name.clone());
        }
    }

    Ok(CollectionImport {
        collection,
        items,
        environments: Vec::new(),
        secrets,
        warnings,
    })
}

/// Routes one Postman variable to plain variables or secrets. Secret-typed values and literal
/// values with credential-like names become secrets.
fn sort_variable(
    key: &str,
    value: String,
    entry: &Value,
    variables: &mut Variables,
    secret_names: &mut Vec<String>,
    secrets: &mut Variables,
    warnings: &mut Vec<String>,
) {
    let typed_secret = entry.get("type").and_then(Value::as_str) == Some("secret");
    let sensitive_literal = looks_sensitive_name(key) && !value.is_empty() && !value.contains("{{");
    if typed_secret || sensitive_literal {
        if !typed_secret {
            warnings.push(format!("moved `{key}` to secrets"));
        }
        if !secret_names.iter().any(|n| n == key) {
            secret_names.push(key.to_string());
        }
        secrets.insert(key.to_string(), value);
    } else {
        variables.insert(key.to_string(), value);
    }
}

fn hoist_items(
    items: &mut [ImportItem],
    secrets: &mut Variables,
    is_reserved: &dyn Fn(&str) -> bool,
    hoisted: &mut Vec<String>,
) {
    ImportItem::for_each_request_mut(items, &mut |request| {
        hoisted.extend(hoist_credentials_with(request, secrets, is_reserved));
    });
}

pub fn parse_environment(json: &str) -> Result<EnvironmentImport> {
    let root: Value = serde_json::from_str(json).context("The file is not valid JSON")?;
    let (Some(name), Some(values)) = (
        root.get("name").and_then(Value::as_str),
        root.get("values").and_then(Value::as_array),
    ) else {
        bail!("Not a Postman environment: expected `name` and a `values` list");
    };

    let mut file = EnvironmentFile::new(name);
    let mut secrets = Variables::new();
    let mut warnings = Vec::new();
    for entry in values {
        if entry.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if let Some(key) = entry.get("key").and_then(Value::as_str) {
            let value = entry.get("value").map(value_to_string).unwrap_or_default();
            sort_variable(
                key,
                value,
                entry,
                &mut file.variables,
                &mut file.secrets,
                &mut secrets,
                &mut warnings,
            );
        }
    }
    Ok(EnvironmentImport {
        file,
        secrets,
        warnings,
    })
}

/// The auth that applies at some level of the tree, after `inherit`/`noauth` resolution.
#[derive(Clone, Copy)]
struct Inherited<'a>(Option<&'a Value>);

impl<'a> Inherited<'a> {
    fn from(auth: Option<&'a Value>, parent: Option<Inherited<'a>>) -> Self {
        match auth {
            None | Some(Value::Null) => parent.unwrap_or(Inherited(None)),
            Some(auth) => match auth.get("type").and_then(Value::as_str) {
                Some("inherit") => parent.unwrap_or(Inherited(None)),
                Some("noauth") | None => Inherited(None),
                Some(_) => Inherited(Some(auth)),
            },
        }
    }
}

fn convert_items(items: &[Value], auth: Inherited, parent_path: &str, warnings: &mut Vec<String>) -> Vec<ImportItem> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("Untitled");
            let path = if parent_path.is_empty() {
                name.to_string()
            } else {
                format!("{parent_path} / {name}")
            };
            let item_auth = Inherited::from(item.get("auth"), Some(auth));
            warn_events(item, &format!("\"{path}\""), warnings);

            if let Some(children) = item.get("item").and_then(Value::as_array) {
                Some(ImportItem::Folder {
                    name: name.to_string(),
                    children: convert_items(children, item_auth, &path, warnings),
                })
            } else {
                let request = item.get("request")?;
                let request_auth = Inherited::from(request.get("auth"), Some(item_auth));
                let mut request = convert_request(name, request, request_auth, &path, warnings);
                request.order = Some(index as i64);
                Some(ImportItem::Request(request))
            }
        })
        .collect()
}

fn convert_request(
    name: &str,
    request: &Value,
    auth: Inherited,
    path: &str,
    warnings: &mut Vec<String>,
) -> RequestFile {
    let mut file = RequestFile::new(name);

    // v2.0 allows a bare URL string as the whole request.
    if let Value::String(url) = request {
        file.url = url.clone();
        return file;
    }

    if let Some(method) = request.get("method").and_then(Value::as_str) {
        file.method = method.to_uppercase();
    }
    file.url = request.get("url").map(convert_url).unwrap_or_default();

    file.headers = match request.get("header") {
        Some(Value::String(text)) => headers_from_text(text),
        Some(Value::Array(headers)) => headers
            .iter()
            .filter_map(|h| {
                Some(Header {
                    name: h.get("key")?.as_str()?.to_string(),
                    value: h.get("value").map(value_to_string).unwrap_or_default(),
                    enabled: h.get("disabled").and_then(Value::as_bool) != Some(true),
                })
            })
            .collect(),
        _ => Vec::new(),
    };

    if let Some(body) = request.get("body") {
        file.body = convert_body(body, &file.headers, path, warnings);
    }
    if let Some(auth) = auth.0 {
        apply_auth(&mut file, auth, path, warnings);
    }
    file
}

fn convert_url(url: &Value) -> String {
    match url {
        Value::String(raw) => raw.clone(),
        Value::Object(parts) => {
            if let Some(raw) = parts.get("raw").and_then(Value::as_str) {
                return raw.to_string();
            }
            let join = |value: Option<&Value>, separator: &str| match value {
                Some(Value::Array(segments)) => {
                    segments.iter().map(value_to_string).collect::<Vec<_>>().join(separator)
                }
                Some(other) => value_to_string(other),
                None => String::new(),
            };
            let mut out = String::new();
            if let Some(protocol) = parts.get("protocol").and_then(Value::as_str) {
                out.push_str(&format!("{protocol}://"));
            }
            out.push_str(&join(parts.get("host"), "."));
            if let Some(port) = parts.get("port") {
                out.push_str(&format!(":{}", value_to_string(port)));
            }
            let path = join(parts.get("path"), "/");
            if !path.is_empty() {
                if !path.starts_with('/') {
                    out.push('/');
                }
                out.push_str(&path);
            }
            let query: Vec<String> = parts
                .get("query")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|q| q.get("disabled").and_then(Value::as_bool) != Some(true))
                .filter_map(|q| {
                    let key = q.get("key")?.as_str()?;
                    Some(match q.get("value").filter(|v| !v.is_null()) {
                        Some(value) => format!("{key}={}", value_to_string(value)),
                        None => key.to_string(),
                    })
                })
                .collect();
            if !query.is_empty() {
                out.push('?');
                out.push_str(&query.join("&"));
            }
            out
        }
        _ => String::new(),
    }
}

fn convert_body(body: &Value, headers: &[Header], path: &str, warnings: &mut Vec<String>) -> Option<Body> {
    if body.get("disabled").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    match body.get("mode").and_then(Value::as_str)? {
        "raw" => {
            let content = body.get("raw").and_then(Value::as_str).unwrap_or_default();
            if content.is_empty() {
                return None;
            }
            let language = body.pointer("/options/raw/language").and_then(Value::as_str);
            let content_type = headers
                .iter()
                .find(|h| h.enabled && h.name.eq_ignore_ascii_case("Content-Type"))
                .map(|h| h.value.as_str());
            let kind = match (language, content_type) {
                (Some("json"), _) => BodyKind::Json,
                (Some("xml"), _) => BodyKind::Xml,
                (Some(_), _) => BodyKind::Text,
                (None, Some(content_type)) => BodyKind::from_content_type(content_type),
                (None, None) if serde_json::from_str::<Value>(content).is_ok() => BodyKind::Json,
                (None, None) => BodyKind::Text,
            };
            Some(Body {
                kind,
                content: content.to_string(),
            })
        }
        "urlencoded" => {
            let pairs: Vec<String> = body
                .get("urlencoded")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|p| p.get("disabled").and_then(Value::as_bool) != Some(true))
                .filter_map(|p| {
                    let key = p.get("key")?.as_str()?;
                    let value = p.get("value").map(value_to_string).unwrap_or_default();
                    Some(format!("{}={}", percent_encode(key), percent_encode(&value)))
                })
                .collect();
            (!pairs.is_empty()).then(|| Body {
                kind: BodyKind::FormUrlencoded,
                content: pairs.join("&"),
            })
        }
        "graphql" => {
            let graphql = body.get("graphql")?;
            let query = graphql.get("query").and_then(Value::as_str).unwrap_or_default();
            let variables = match graphql.get("variables") {
                Some(Value::String(text)) if !text.trim().is_empty() => {
                    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
                }
                Some(value @ Value::Object(_)) => value.clone(),
                _ => Value::Object(Default::default()),
            };
            let json = serde_json::json!({ "query": query, "variables": variables });
            Some(Body {
                kind: BodyKind::Json,
                content: serde_json::to_string_pretty(&json).unwrap_or_default(),
            })
        }
        mode @ ("formdata" | "file") => {
            warnings.push(format!("\"{path}\": {mode} body is not supported yet and was skipped"));
            None
        }
        other => {
            warnings.push(format!("\"{path}\": unknown body mode `{other}` was skipped"));
            None
        }
    }
}

fn apply_auth(file: &mut RequestFile, auth: &Value, path: &str, warnings: &mut Vec<String>) {
    let kind = auth.get("type").and_then(Value::as_str).unwrap_or_default();
    let param = |key: &str| auth_param(auth, kind, key);

    let header = match kind {
        "bearer" => Some((
            "Authorization".to_string(),
            format!("Bearer {}", param("token").unwrap_or_default()),
        )),
        "basic" => {
            let username = param("username").unwrap_or_default();
            let password = param("password").unwrap_or_default();
            if username.contains("{{") || password.contains("{{") {
                warnings.push(format!(
                    "\"{path}\": basic auth uses variables, which cannot be resolved inside the encoded header; edit it after import"
                ));
            }
            let encoded = base64_encode(format!("{username}:{password}").as_bytes());
            Some(("Authorization".to_string(), format!("Basic {encoded}")))
        }
        "apikey" => {
            let key = param("key").unwrap_or_else(|| "X-API-Key".into());
            let value = param("value").unwrap_or_default();
            if param("in").as_deref() == Some("query") {
                warnings.push(format!(
                    "\"{path}\": API key in query string was not imported; add `{key}` to the URL"
                ));
                None
            } else {
                Some((key, value))
            }
        }
        other => {
            warnings.push(format!("\"{path}\": {other} auth is not supported and was skipped"));
            None
        }
    };

    if let Some((name, value)) = header
        && !file
            .headers
            .iter()
            .any(|h| h.enabled && h.name.eq_ignore_ascii_case(&name))
    {
        file.headers.push(Header::new(name, value));
    }
}

/// Auth parameters are `[{key, value}]` in v2.1 and `{key: value}` in v2.0.
fn auth_param(auth: &Value, kind: &str, key: &str) -> Option<String> {
    match auth.get(kind)? {
        Value::Array(params) => params
            .iter()
            .find(|p| p.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|p| p.get("value"))
            .map(value_to_string),
        Value::Object(params) => params.get(key).map(value_to_string),
        _ => None,
    }
}

fn warn_events(item: &Value, label: &str, warnings: &mut Vec<String>) {
    for event in item.get("event").and_then(Value::as_array).into_iter().flatten() {
        let exec = event.pointer("/script/exec");
        let has_code = match exec {
            Some(Value::Array(lines)) => lines.iter().any(|l| l.as_str().is_some_and(|l| !l.trim().is_empty())),
            Some(Value::String(code)) => !code.trim().is_empty(),
            _ => false,
        };
        if has_code {
            let listen = match event.get("listen").and_then(Value::as_str) {
                Some("prerequest") => "pre-request script",
                Some("test") => "test script",
                _ => "script",
            };
            warnings.push(format!("{label}: {listen} was dropped"));
        }
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::import::{write_items, write_project_collection};
    use crate::storage::{Item, load_collection};

    const COLLECTION: &str = r#"{
      "info": {
        "name": "Petstore",
        "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
      },
      "auth": { "type": "bearer", "bearer": [{ "key": "token", "value": "{{token}}", "type": "string" }] },
      "variable": [
        { "key": "base_url", "value": "https://petstore.test/v1" },
        { "key": "retries", "value": 3 },
        { "key": "client_secret", "value": "cs-literal-777" },
        { "key": "vault", "value": "typed-secret-555", "type": "secret" },
        { "key": "api_token", "value": "{{from_env}}" },
        { "key": "old", "value": "x", "disabled": true }
      ],
      "item": [
        {
          "name": "List pets",
          "request": {
            "method": "GET",
            "header": [
              { "key": "Accept", "value": "application/json" },
              { "key": "X-Api-Key", "value": "hdr-key-888" },
              { "key": "X-Debug", "value": "1", "disabled": true }
            ],
            "url": { "raw": "{{base_url}}/pets?limit=10", "host": ["{{base_url}}"], "path": ["pets"] }
          },
          "event": [
            { "listen": "test", "script": { "type": "text/javascript", "exec": ["pm.test('ok', () => {});"] } },
            { "listen": "prerequest", "script": { "exec": [""] } }
          ]
        },
        {
          "name": "Admin",
          "auth": { "type": "basic", "basic": [
            { "key": "username", "value": "admin" }, { "key": "password", "value": "secret" }
          ] },
          "item": [
            {
              "name": "Create pet",
              "request": {
                "method": "post",
                "header": [],
                "body": { "mode": "raw", "raw": "{\"name\": \"Rex\"}", "options": { "raw": { "language": "json" } } },
                "url": { "protocol": "https", "host": ["petstore", "test"], "path": ["v1", "pets"],
                         "query": [{ "key": "dry_run", "value": "true" }] }
              }
            },
            {
              "name": "Login",
              "request": {
                "auth": { "type": "noauth" },
                "method": "POST",
                "body": { "mode": "urlencoded", "urlencoded": [
                  { "key": "user", "value": "a b" }, { "key": "pass", "value": "{{password}}" },
                  { "key": "skip", "value": "1", "disabled": true }
                ] },
                "url": "{{base_url}}/login"
              }
            },
            {
              "name": "Upload",
              "request": {
                "auth": { "type": "oauth2" },
                "method": "POST",
                "body": { "mode": "formdata", "formdata": [{ "key": "file", "type": "file" }] },
                "url": "{{base_url}}/upload"
              }
            }
          ]
        },
        {
          "name": "Search",
          "request": {
            "method": "POST",
            "body": { "mode": "graphql", "graphql": { "query": "{ pets { id } }", "variables": "{\"first\": 5}" } },
            "url": "{{base_url}}/graphql"
          }
        }
      ]
    }"#;

    fn requests(items: &[ImportItem]) -> Vec<&RequestFile> {
        items
            .iter()
            .flat_map(|item| match item {
                ImportItem::Request(r) => vec![r],
                ImportItem::Folder { children, .. } => requests(children),
            })
            .collect()
    }

    fn header<'a>(request: &'a RequestFile, name: &str) -> Option<&'a Header> {
        request.headers.iter().find(|h| h.name == name)
    }

    #[test]
    fn converts_a_v21_collection() {
        let import = parse_collection(COLLECTION).unwrap();
        assert_eq!(import.collection.name, "Petstore");
        assert_eq!(
            import.collection.variables.len(),
            3,
            "{:?}",
            import.collection.variables
        );
        assert_eq!(import.collection.variables["retries"], "3");
        assert_eq!(
            import.collection.variables["api_token"], "{{from_env}}",
            "templated values stay plain"
        );
        assert_eq!(
            import.collection.secrets,
            ["client_secret", "vault", "api_key", "basic_auth"]
        );
        assert_eq!(import.secrets["client_secret"], "cs-literal-777");
        assert_eq!(import.secrets["vault"], "typed-secret-555");
        assert_eq!(import.secrets["api_key"], "hdr-key-888");
        assert_eq!(import.secrets["basic_auth"], "YWRtaW46c2VjcmV0");

        let all = requests(&import.items);
        let [list, create, login, upload, search] = all.as_slice() else {
            panic!("expected 5 requests, got {}", all.len());
        };

        assert_eq!(list.url, "{{base_url}}/pets?limit=10");
        assert!(!header(list, "X-Debug").unwrap().enabled);
        assert_eq!(header(list, "Authorization").unwrap().value, "Bearer {{token}}");
        assert_eq!(header(list, "X-Api-Key").unwrap().value, "{{api_key}}");
        assert_eq!(list.order, Some(0));

        assert_eq!(create.method, "POST");
        assert_eq!(create.url, "https://petstore.test/v1/pets?dry_run=true");
        assert_eq!(create.body.as_ref().unwrap().kind, BodyKind::Json);
        assert_eq!(header(create, "Authorization").unwrap().value, "Basic {{basic_auth}}");

        let body = login.body.as_ref().unwrap();
        assert_eq!(
            (body.kind, body.content.as_str()),
            (BodyKind::FormUrlencoded, "user=a%20b&pass={{password}}")
        );
        assert!(header(login, "Authorization").is_none(), "noauth stops inheritance");

        assert!(upload.body.is_none());
        let graphql: Value = serde_json::from_str(&search.body.as_ref().unwrap().content).unwrap();
        assert_eq!(graphql["variables"]["first"], 5);
        assert_eq!(header(search, "Authorization").unwrap().value, "Bearer {{token}}");

        assert_eq!(import.warnings.len(), 5, "{:#?}", import.warnings);
        assert!(import.warnings.iter().any(|w| w == "moved `client_secret` to secrets"));
        assert!(
            import
                .warnings
                .iter()
                .any(|w| w == "2 credentials moved to secrets: api_key, basic_auth")
        );
        assert!(
            import
                .warnings
                .iter()
                .any(|w| w.contains("List pets") && w.contains("test script"))
        );
        assert!(
            import
                .warnings
                .iter()
                .any(|w| w.contains("Admin / Upload") && w.contains("formdata"))
        );
        assert!(import.warnings.iter().any(|w| w.contains("oauth2")));
    }

    #[test]
    fn converts_v20_shapes() {
        let json = r#"{
          "info": { "name": "Old", "schema": "https://schema.getpostman.com/json/collection/v2.0.0/collection.json" },
          "item": [
            { "name": "Bare", "request": "https://x.test/ping" },
            { "name": "Keyed", "request": {
                "url": "https://x.test/k", "method": "GET", "header": "Accept: text/plain\n",
                "auth": { "type": "apikey", "apikey": { "key": "X-Key", "value": "abc", "in": "header" } }
            } }
          ]
        }"#;
        let import = parse_collection(json).unwrap();
        let all = requests(&import.items);
        assert_eq!(all[0].url, "https://x.test/ping");
        assert_eq!(header(all[1], "Accept").unwrap().value, "text/plain");
        assert_eq!(header(all[1], "X-Key").unwrap().value, "{{key}}");
        assert_eq!(import.secrets["key"], "abc");
        assert_eq!(import.collection.secrets, ["key"]);
        assert_eq!(import.warnings, ["1 credentials moved to secrets: key"]);
    }

    #[test]
    fn rejects_non_collections() {
        assert!(
            parse_collection("{}")
                .unwrap_err()
                .to_string()
                .contains("Not a Postman collection")
        );
        assert!(parse_collection("nope").is_err());
    }

    #[test]
    fn parses_environment_exports() {
        let json = r#"{ "id": "1", "name": "Staging", "values": [
            { "key": "base_url", "value": "https://staging.test", "enabled": true },
            { "key": "token", "value": "t", "type": "secret" },
            { "key": "off", "value": "x", "enabled": false }
        ], "_postman_variable_scope": "environment" }"#;
        let env = parse_environment(json).unwrap();
        assert_eq!(env.file.name, "Staging");
        assert_eq!(env.file.variables.keys().collect::<Vec<_>>(), ["base_url"]);
        assert_eq!(env.file.secrets, ["token"]);
        assert_eq!(env.secrets["token"], "t");
        assert!(env.warnings.is_empty(), "typed secrets need no warning");
        let yaml = serde_norway::to_string(&env.file).unwrap();
        assert!(!yaml.contains(": t\n") && yaml.contains("- token"), "{yaml}");
        assert!(parse_environment(COLLECTION).is_err());
    }

    #[test]
    fn writes_a_collection_that_loads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let mut import = parse_collection(COLLECTION).unwrap();
        import.items.push(ImportItem::Folder {
            name: "Environments".into(),
            children: vec![],
        });
        let root = write_project_collection(tmp.path(), &import).unwrap();
        assert_eq!(root, tmp.path().join(".courier"));

        let loaded = load_collection(&root).unwrap();
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.file.variables["base_url"], "https://petstore.test/v1");
        assert!(loaded.file.id.is_some());
        assert_eq!(loaded.file.secrets, import.collection.secrets);

        // No secret value may appear in any written file.
        fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, files)
                } else {
                    files.push(path)
                }
            }
        }
        let mut files = Vec::new();
        walk(tmp.path(), &mut files);
        assert!(files.len() > 5);
        for file in &files {
            let text = fs::read_to_string(file).unwrap();
            for value in import.secrets.values() {
                assert!(
                    !text.contains(value.as_str()),
                    "{} leaks a secret value:\n{text}",
                    file.display()
                );
            }
        }

        let names: Vec<_> = loaded
            .items
            .iter()
            .map(|item| match item {
                Item::Folder { name, .. } => name.clone(),
                Item::Request { request, .. } => request.name.clone(),
            })
            .collect();
        assert_eq!(names, ["admin", "environments-folder", "List pets", "Search"]);

        let Item::Folder { children, .. } = &loaded.items[0] else {
            panic!()
        };
        let order: Vec<_> = children
            .iter()
            .map(|c| match c {
                Item::Request { request, .. } => request.name.as_str(),
                Item::Folder { .. } => "folder",
            })
            .collect();
        assert_eq!(order, ["Create pet", "Login", "Upload"]);

        // Importing the same items again into the existing collection must not clobber files.
        write_items(&root, &import.items).unwrap();
        assert!(root.join("list-pets-2.yaml").is_file());
        assert!(root.join("admin-2").is_dir());
    }
}
