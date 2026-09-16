//! Insomnia exports: the v4 JSON format (a flat list of resources joined by `parentId`) and
//! the v5 YAML format (a tree of items).

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::model::{Auth, Body, BodyKind, CollectionFile, EnvironmentFile, Header, QueryParam, RequestFile, Variables};

use super::{CollectionImport, ImportItem};

/// Converts either format, whichever this document is.
pub fn convert(document: &Value) -> Result<CollectionImport> {
    if document.get("resources").is_some() {
        v4(document)
    } else {
        v5(document)
    }
}

/// Whether this looks like an Insomnia export at all.
pub fn is_insomnia(document: &Value) -> bool {
    let v4 = document.get("_type").and_then(Value::as_str) == Some("export") && document.get("resources").is_some();
    let v5 = document
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("collection.insomnia.rest"));
    v4 || v5
}

// MARK: v4

fn v4(document: &Value) -> Result<CollectionImport> {
    let resources = document
        .get("resources")
        .and_then(Value::as_array)
        .context("this Insomnia export has no resources")?;
    let mut warnings = Vec::new();

    let name = resources
        .iter()
        .find(|resource| kind_of(resource) == "workspace")
        .and_then(|workspace| workspace.get("name").and_then(Value::as_str))
        .unwrap_or("Insomnia");
    let mut collection = CollectionFile::new(name);

    // Requests and folders point at their parent, so the tree is built from the bottom up.
    let mut children: HashMap<String, Vec<&Value>> = HashMap::new();
    for resource in resources {
        if matches!(kind_of(resource), "request" | "request_group") {
            let parent = resource
                .get("parentId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            children.entry(parent).or_default().push(resource);
        }
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| sort_key(a).total_cmp(&sort_key(b)));
    }
    let roots: Vec<String> = resources
        .iter()
        .filter(|resource| kind_of(resource) == "workspace")
        .filter_map(|workspace| workspace.get("_id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut items = Vec::new();
    for root in roots.iter().map(String::as_str).chain(std::iter::once("")) {
        items.extend(build_v4(root, &children, &mut warnings));
    }

    // Environments: the base one seeds the collection's variables, the rest become files.
    let mut environments = Vec::new();
    for resource in resources.iter().filter(|r| kind_of(r) == "environment") {
        let data = resource
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let variables: Vec<(String, String)> = data.into_iter().map(|(name, value)| (name, as_text(&value))).collect();
        let name = resource.get("name").and_then(Value::as_str).unwrap_or("Environment");
        let is_base = resource
            .get("parentId")
            .and_then(Value::as_str)
            .is_some_and(|parent| roots.iter().any(|root| root == parent));
        if is_base {
            collection.variables.extend(variables);
        } else {
            let mut file = EnvironmentFile::new(name);
            file.variables.extend(variables);
            environments.push(file);
        }
    }

    if super::ImportItem::count_requests(&items) == 0 {
        anyhow::bail!("this Insomnia export holds no requests");
    }
    let secrets = hoist(&mut items, &mut collection, &mut warnings);
    Ok(CollectionImport {
        collection,
        items,
        environments,
        secrets,
        warnings,
    })
}

fn build_v4(parent: &str, children: &HashMap<String, Vec<&Value>>, warnings: &mut Vec<String>) -> Vec<ImportItem> {
    let Some(list) = children.get(parent) else {
        return Vec::new();
    };
    list.iter()
        .map(|resource| {
            let name = resource.get("name").and_then(Value::as_str).unwrap_or("Untitled");
            let id = resource.get("_id").and_then(Value::as_str).unwrap_or_default();
            if kind_of(resource) == "request_group" {
                ImportItem::Folder {
                    name: name.to_string(),
                    children: build_v4(id, children, warnings),
                }
            } else {
                ImportItem::Request(request_from(resource, name, warnings))
            }
        })
        .collect()
}

fn kind_of(resource: &Value) -> &str {
    resource.get("_type").and_then(Value::as_str).unwrap_or_default()
}

/// Insomnia orders siblings with a fractional key.
fn sort_key(resource: &Value) -> f64 {
    resource.get("metaSortKey").and_then(Value::as_f64).unwrap_or(f64::MAX)
}

// MARK: v5

fn v5(document: &Value) -> Result<CollectionImport> {
    let name = document.get("name").and_then(Value::as_str).unwrap_or("Insomnia");
    let mut collection = CollectionFile::new(name);
    let mut warnings = Vec::new();
    let tree = document
        .get("collection")
        .and_then(Value::as_array)
        .context("this Insomnia file has no collection")?;
    let mut items = build_v5(tree, &mut warnings);

    let mut environments = Vec::new();
    if let Some(environment) = document.get("environments") {
        let data = environment
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        collection
            .variables
            .extend(data.into_iter().map(|(name, value)| (name, as_text(&value))));
        for sub in environment
            .get("subEnvironments")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            let name = sub.get("name").and_then(Value::as_str).unwrap_or("Environment");
            let data = sub.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
            let mut file = EnvironmentFile::new(name);
            file.variables
                .extend(data.into_iter().map(|(name, value)| (name, as_text(&value))));
            environments.push(file);
        }
    }

    if super::ImportItem::count_requests(&items) == 0 {
        anyhow::bail!("this Insomnia file holds no requests");
    }
    let secrets = hoist(&mut items, &mut collection, &mut warnings);
    Ok(CollectionImport {
        collection,
        items,
        environments,
        secrets,
        warnings,
    })
}

fn build_v5(items: &[Value], warnings: &mut Vec<String>) -> Vec<ImportItem> {
    items
        .iter()
        .map(|item| {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("Untitled");
            match item.get("children").and_then(Value::as_array) {
                Some(children) => ImportItem::Folder {
                    name: name.to_string(),
                    children: build_v5(children, warnings),
                },
                None => ImportItem::Request(request_from(item, name, warnings)),
            }
        })
        .collect()
}

// MARK: shared

/// Moves credentials written literally in the export into secrets, leaving placeholders.
/// An export often carries real tokens, and those must not land in a file.
fn hoist(items: &mut [ImportItem], collection: &mut CollectionFile, warnings: &mut Vec<String>) -> Variables {
    let mut secrets = Variables::new();
    let mut hoisted = Vec::new();
    let reserved = collection.variables.clone();
    ImportItem::for_each_request_mut(items, &mut |request| {
        if let Some(name) = crate::credentials::hoist_auth(request, &mut secrets, &|name| reserved.contains_key(name)) {
            hoisted.push(name);
        }
        hoisted.extend(crate::credentials::hoist_credentials_with(
            request,
            &mut secrets,
            &|name| reserved.contains_key(name),
        ));
    });
    if !hoisted.is_empty() {
        hoisted.sort();
        hoisted.dedup();
        warnings.push(format!(
            "{} credential(s) moved to secrets: {}",
            hoisted.len(),
            hoisted.join(", ")
        ));
        collection.secrets.extend(hoisted);
    }
    secrets
}

/// Both formats describe a request the same way, give or take.
fn request_from(resource: &Value, name: &str, warnings: &mut Vec<String>) -> RequestFile {
    let mut request = RequestFile::new(name);
    request.method = resource
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    request.url = resource
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let (enabled, disabled): (Vec<_>, Vec<_>) = resource
        .get("parameters")
        .and_then(Value::as_array)
        .map(|params| {
            params
                .iter()
                .filter_map(|param| {
                    let name = param.get("name").and_then(Value::as_str)?.to_string();
                    let value = param
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some((QueryParam { name, value }, is_disabled(param)))
                })
                .partition(|(_, disabled)| !*disabled)
        })
        .unwrap_or_default();
    // Insomnia keeps query parameters beside the URL; Courier keeps them in it.
    let params: Vec<QueryParam> = enabled.into_iter().map(|(param, _)| param).collect();
    if !params.is_empty() {
        let separator = if request.url.contains('?') { '&' } else { '?' };
        let query = params
            .iter()
            .map(|param| {
                format!(
                    "{}={}",
                    crate::encoding::percent_encode(&param.name),
                    crate::encoding::percent_encode(&param.value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        request.url = format!("{}{separator}{query}", request.url);
    }
    request.disabled_params = disabled.into_iter().map(|(param, _)| param).collect();

    request.headers = resource
        .get("headers")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|header| {
                    let name = header.get("name").and_then(Value::as_str)?.trim().to_string();
                    (!name.is_empty()).then(|| Header {
                        name,
                        value: header
                            .get("value")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        enabled: !is_disabled(header),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    request.auth = auth_from(resource.get("authentication"), name, warnings);
    request.body = body_from(resource.get("body"), name, warnings);
    request
}

fn is_disabled(value: &Value) -> bool {
    value.get("disabled").and_then(Value::as_bool).unwrap_or(false)
}

fn auth_from(authentication: Option<&Value>, request: &str, warnings: &mut Vec<String>) -> Auth {
    let Some(auth) = authentication.filter(|auth| auth.is_object()) else {
        return Auth::Inherit;
    };
    if auth.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
        return Auth::None;
    }
    let text = |field: &str| auth.get(field).and_then(Value::as_str).unwrap_or_default().to_string();
    match auth.get("type").and_then(Value::as_str).unwrap_or_default() {
        "bearer" => Auth::Bearer { token: text("token") },
        "basic" => Auth::Basic {
            username: text("username"),
            password: text("password"),
        },
        "apikey" => Auth::ApiKey {
            name: text("key"),
            value: text("value"),
            in_query: auth.get("addTo").and_then(Value::as_str) == Some("queryParams"),
        },
        "" | "none" => Auth::Inherit,
        other => {
            warnings.push(format!("\"{request}\" uses {other} auth, which was left unset"));
            Auth::Inherit
        }
    }
}

fn body_from(body: Option<&Value>, request: &str, warnings: &mut Vec<String>) -> Option<Body> {
    let body = body?.as_object()?;
    let mime = body
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(text) = body.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        return Some(Body {
            kind: BodyKind::from_content_type(&mime),
            content: text.to_string(),
        });
    }
    let params = body.get("params").and_then(Value::as_array)?;
    let kind = BodyKind::from_content_type(&mime);
    let lines: Vec<String> = params
        .iter()
        .filter(|param| !is_disabled(param))
        .filter_map(|param| {
            let name = param.get("name").and_then(Value::as_str)?;
            let value = param.get("value").and_then(Value::as_str).unwrap_or_default();
            let file = param.get("fileName").and_then(Value::as_str).unwrap_or_default();
            Some(match kind {
                BodyKind::Multipart if !file.is_empty() => format!("{name}: @{file}"),
                BodyKind::Multipart => format!("{name}: {value}"),
                _ => format!(
                    "{}={}",
                    crate::encoding::form_encode(name),
                    crate::encoding::form_encode(value)
                ),
            })
        })
        .collect();
    if lines.is_empty() {
        if !mime.is_empty() && mime != "application/json" {
            warnings.push(format!("\"{request}\" has a {mime} body that couldn't be converted"));
        }
        return None;
    }
    Some(Body {
        kind,
        content: lines.join(if kind == BodyKind::Multipart { "\n" } else { "&" }),
    })
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V4: &str = r#"{
      "_type": "export",
      "__export_format": 4,
      "resources": [
        { "_id": "wrk_1", "_type": "workspace", "name": "Pets" },
        { "_id": "env_base", "_type": "environment", "parentId": "wrk_1",
          "data": { "base_url": "https://api.test", "retries": 3 } },
        { "_id": "env_prod", "_type": "environment", "parentId": "env_base",
          "name": "Production", "data": { "base_url": "https://api.live" } },
        { "_id": "fld_1", "_type": "request_group", "parentId": "wrk_1", "name": "Pets", "metaSortKey": 1 },
        { "_id": "req_2", "_type": "request", "parentId": "fld_1", "name": "Create pet", "metaSortKey": 2,
          "method": "post", "url": "{{ base_url }}/pets",
          "headers": [{ "name": "Content-Type", "value": "application/json" },
                      { "name": "X-Debug", "value": "1", "disabled": true }],
          "body": { "mimeType": "application/json", "text": "{\"name\":\"Rex\"}" },
          "authentication": { "type": "bearer", "token": "{{ token }}" } },
        { "_id": "req_1", "_type": "request", "parentId": "fld_1", "name": "List pets", "metaSortKey": 1,
          "method": "GET", "url": "{{ base_url }}/pets",
          "parameters": [{ "name": "limit", "value": "10" },
                         { "name": "debug", "value": "1", "disabled": true }],
          "authentication": { "type": "basic", "username": "u", "password": "p" } },
        { "_id": "req_3", "_type": "request", "parentId": "wrk_1", "name": "Health", "metaSortKey": 2,
          "method": "GET", "url": "{{ base_url }}/health",
          "authentication": { "type": "oauth2", "grantType": "client_credentials" } }
      ]
    }"#;

    #[test]
    fn reads_a_v4_export() {
        let document: Value = serde_json::from_str(V4).unwrap();
        assert!(is_insomnia(&document));
        let import = convert(&document).unwrap();
        assert_eq!(import.collection.name, "Pets");
        assert_eq!(import.collection.variables["base_url"], "https://api.test");
        assert_eq!(import.collection.variables["retries"], "3", "numbers become text");
        assert_eq!(
            import.environments.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["Production"],
            "the base environment seeds the collection, the rest are files"
        );

        let ImportItem::Folder { name, children } = &import.items[0] else {
            panic!("expected a folder first: {:?}", import.items[0]);
        };
        assert_eq!(name, "Pets");
        let names: Vec<&str> = children
            .iter()
            .map(|item| match item {
                ImportItem::Request(request) => request.name.as_str(),
                ImportItem::Folder { name, .. } => name.as_str(),
            })
            .collect();
        assert_eq!(names, ["List pets", "Create pet"], "sorted the way Insomnia had them");

        let ImportItem::Request(list) = &children[0] else {
            panic!()
        };
        assert_eq!(list.url, "{{ base_url }}/pets?limit=10", "parameters move into the URL");
        assert_eq!(list.disabled_params.len(), 1, "a switched-off parameter is kept aside");
        assert_eq!(
            list.auth,
            Auth::Basic {
                username: "u".into(),
                password: "{{password}}".into()
            },
            "a password written out in the export moves to a secret"
        );
        assert_eq!(import.secrets["password"], "p");

        let ImportItem::Request(create) = &children[1] else {
            panic!()
        };
        assert_eq!(create.method, "POST");
        assert_eq!(create.body.as_ref().map(|b| b.kind), Some(BodyKind::Json));
        assert!(!create.headers[1].enabled, "a switched-off header stays switched off");

        assert!(
            import.warnings.iter().any(|w| w.contains("oauth2")),
            "unsupported auth is reported: {:?}",
            import.warnings
        );
    }

    #[test]
    fn reads_a_v5_file() {
        let yaml = r#"
type: collection.insomnia.rest/5.0
name: Pets v5
collection:
  - name: List pets
    method: GET
    url: "{{ base_url }}/pets"
    headers:
      - name: Accept
        value: application/json
  - name: Admin
    children:
      - name: Delete pet
        method: DELETE
        url: "{{ base_url }}/pets/1"
        authentication:
          type: apikey
          key: X-Key
          value: secret
environments:
  name: Base
  data:
    base_url: https://api.test
  subEnvironments:
    - name: Local
      data:
        base_url: http://localhost:3000
"#;
        let document = super::super::spec::parse_document(yaml).unwrap();
        assert!(is_insomnia(&document));
        let import = convert(&document).unwrap();
        assert_eq!(import.collection.name, "Pets v5");
        assert_eq!(import.collection.variables["base_url"], "https://api.test");
        assert_eq!(import.environments[0].variables["base_url"], "http://localhost:3000");
        assert_eq!(super::super::ImportItem::count_requests(&import.items), 2);
        let ImportItem::Folder { name, children } = &import.items[1] else {
            panic!("the second item is a folder")
        };
        assert_eq!(name, "Admin");
        let ImportItem::Request(delete) = &children[0] else {
            panic!()
        };
        assert_eq!(delete.method, "DELETE");
        assert_eq!(
            delete.auth,
            Auth::ApiKey {
                name: "X-Key".into(),
                value: "{{x_key}}".into(),
                in_query: false
            },
            "the key's value moves to a secret named after the header"
        );
        assert_eq!(import.secrets["x_key"], "secret");
    }

    #[test]
    fn says_when_there_is_nothing_to_import() {
        let empty: Value = serde_json::from_str(r#"{"_type":"export","resources":[]}"#).unwrap();
        assert!(convert(&empty).unwrap_err().to_string().contains("no requests"));
    }
}
