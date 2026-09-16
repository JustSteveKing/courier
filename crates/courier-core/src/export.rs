//! Exporting a collection for other tools: a Postman collection, an OpenAPI skeleton, or a
//! request's history as a HAR file.
//!
//! Secret values are never exported. Collections hold `{{placeholders}}` and the values stay
//! in the secret store, so an export carries the names and nothing else.

use serde_json::{Value, json};

use crate::model::{Auth, BodyKind, Header, RequestFile};
use crate::response_cache::{Outcome, StoredResponse};
use crate::storage::{Collection, Item};

/// A Postman collection (schema v2.1), the format most other tools read.
pub fn postman(collection: &Collection) -> Value {
    let variables: Vec<Value> = collection
        .file
        .variables
        .iter()
        .map(|(name, value)| json!({ "key": name, "value": value }))
        .chain(
            // Secret values stay behind; the name travels so the import asks for it.
            collection
                .file
                .secrets
                .iter()
                .map(|name| json!({ "key": name, "value": "", "type": "secret" })),
        )
        .collect();

    let mut document = json!({
        "info": {
            "name": collection.file.name,
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json",
            "_exporter_id": "courier",
        },
        "item": postman_items(&collection.items),
        "variable": variables,
    });
    if let Some(auth) = postman_auth(&collection.file.auth) {
        document["auth"] = auth;
    }
    document
}

fn postman_items(items: &[Item]) -> Vec<Value> {
    items
        .iter()
        .map(|item| match item {
            Item::Folder { name, children, .. } => json!({ "name": name, "item": postman_items(children) }),
            Item::Request { request, .. } => postman_request(request),
        })
        .collect()
}

fn postman_request(request: &RequestFile) -> Value {
    let headers: Vec<Value> = request
        .headers
        .iter()
        .map(|header| {
            json!({
                "key": header.name,
                "value": header.value,
                "disabled": !header.enabled,
            })
        })
        .chain(request.disabled_params.iter().map(|_| Value::Null))
        .filter(|value| !value.is_null())
        .collect();

    let mut sent = json!({
        "method": request.method.to_ascii_uppercase(),
        "header": headers,
        "url": postman_url(request),
    });
    if let Some(auth) = postman_auth(&request.auth) {
        sent["auth"] = auth;
    }
    if let Some(body) = postman_body(request) {
        sent["body"] = body;
    }
    json!({ "name": request.name, "request": sent })
}

/// Postman wants the URL split up as well as whole; `raw` is what it actually uses.
fn postman_url(request: &RequestFile) -> Value {
    let raw = &request.url;
    let query: Vec<Value> = request
        .disabled_params
        .iter()
        .map(|param| json!({ "key": param.name, "value": param.value, "disabled": true }))
        .collect();
    if query.is_empty() {
        json!({ "raw": raw })
    } else {
        json!({ "raw": raw, "query": query })
    }
}

fn postman_body(request: &RequestFile) -> Option<Value> {
    if let Some(graphql) = &request.graphql {
        return Some(json!({
            "mode": "graphql",
            "graphql": { "query": graphql.query, "variables": graphql.variables },
        }));
    }
    let body = request.body.as_ref()?;
    Some(match body.kind {
        BodyKind::FormUrlencoded => json!({
            "mode": "urlencoded",
            "urlencoded": body
                .content
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                    json!({ "key": name, "value": value })
                })
                .collect::<Vec<_>>(),
        }),
        BodyKind::Multipart => json!({
            "mode": "formdata",
            "formdata": crate::model::parts_from_text(&body.content)
                .unwrap_or_default()
                .into_iter()
                .map(|part| match part.value {
                    crate::model::PartValue::File(path) => {
                        json!({ "key": part.name, "type": "file", "src": path, "disabled": !part.enabled })
                    }
                    crate::model::PartValue::Text(value) => {
                        json!({ "key": part.name, "type": "text", "value": value, "disabled": !part.enabled })
                    }
                })
                .collect::<Vec<_>>(),
        }),
        BodyKind::File => json!({ "mode": "file", "file": { "src": body.content } }),
        kind => json!({
            "mode": "raw",
            "raw": body.content,
            "options": { "raw": { "language": match kind {
                BodyKind::Json => "json",
                BodyKind::Xml => "xml",
                _ => "text",
            } } },
        }),
    })
}

fn postman_auth(auth: &Auth) -> Option<Value> {
    Some(match auth {
        Auth::Basic { username, password } => json!({
            "type": "basic",
            "basic": [
                { "key": "username", "value": username },
                { "key": "password", "value": password },
            ],
        }),
        Auth::Bearer { token } => json!({ "type": "bearer", "bearer": [{ "key": "token", "value": token }] }),
        Auth::ApiKey { name, value, in_query } => json!({
            "type": "apikey",
            "apikey": [
                { "key": "key", "value": name },
                { "key": "value", "value": value },
                { "key": "in", "value": if *in_query { "query" } else { "header" } },
            ],
        }),
        Auth::None => json!({ "type": "noauth" }),
        // Inherit is Postman's default, and the rest have no Postman equivalent worth faking.
        _ => return None,
    })
}

/// An OpenAPI 3.1 skeleton: the paths and methods a collection calls, with the bodies it
/// sends as examples. A starting point to fill in, not a specification.
pub fn openapi(collection: &Collection) -> Value {
    let base = collection
        .file
        .variables
        .get("base_url")
        .cloned()
        .unwrap_or_else(|| "https://example.com".into());
    let mut paths = serde_json::Map::new();
    let mut warnings = Vec::new();

    for entry in collection.requests() {
        let request = entry.request;
        if crate::model::is_websocket_url(&request.url) {
            warnings.push(request.name.clone());
            continue;
        }
        let (path, query) = split_path(&request.url, &base);
        let method = request.method.to_ascii_lowercase();
        let mut operation = json!({
            "summary": request.name,
            "responses": { "200": { "description": "" } },
        });
        if !entry.folders.is_empty() {
            operation["tags"] = json!([entry.folders.join(" / ")]);
        }
        let parameters: Vec<Value> = path_parameters(&path)
            .into_iter()
            .map(|name| json!({ "name": name, "in": "path", "required": true, "schema": { "type": "string" } }))
            .chain(query.into_iter().map(|(name, value)| {
                json!({ "name": name, "in": "query", "schema": { "type": "string" }, "example": value })
            }))
            .chain(header_parameters(&request.headers))
            .collect();
        if !parameters.is_empty() {
            operation["parameters"] = json!(parameters);
        }
        if let Some(body) = openapi_body(request) {
            operation["requestBody"] = body;
        }
        paths
            .entry(path)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("a path holds its operations")
            .insert(method, operation);
    }

    let mut document = json!({
        "openapi": "3.1.0",
        "info": { "title": collection.file.name, "version": "1.0.0" },
        "servers": [{ "url": base }],
        "paths": paths,
    });
    if !warnings.is_empty() {
        document["info"]["description"] = json!(format!(
            "Exported from Courier. Left out, as OpenAPI has no place for them: {}",
            warnings.join(", ")
        ));
    }
    document
}

/// The path part of a URL, with `{{variables}}` turned into OpenAPI's `{placeholders}`, and
/// the query parameters it carried.
fn split_path(url: &str, base: &str) -> (String, Vec<(String, String)>) {
    let without_base = url.strip_prefix(base).unwrap_or(url);
    let without_base = without_base
        .strip_prefix("{{base_url}}")
        .unwrap_or(without_base)
        .to_string();
    let (path, query) = match without_base.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (without_base, String::new()),
    };
    let path = path.replace("{{", "{").replace("}}", "}");
    let path = if path.starts_with('/') {
        path
    } else if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{path}")
    };
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (name.to_string(), value.to_string())
        })
        .collect();
    (path, query)
}

fn path_parameters(path: &str) -> Vec<String> {
    path.split('/')
        .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}').map(str::to_string))
        .collect()
}

/// Headers worth describing: not the ones every request has, and not auth.
fn header_parameters(headers: &[Header]) -> Vec<Value> {
    const IMPLIED: [&str; 4] = ["content-type", "accept", "authorization", "user-agent"];
    headers
        .iter()
        .filter(|header| header.enabled && !IMPLIED.contains(&header.name.to_ascii_lowercase().as_str()))
        .map(|header| {
            json!({
                "name": header.name,
                "in": "header",
                "schema": { "type": "string" },
                "example": header.value,
            })
        })
        .collect()
}

fn openapi_body(request: &RequestFile) -> Option<Value> {
    if request.graphql.is_some() {
        return Some(json!({
            "content": { "application/json": { "schema": { "type": "object" } } },
        }));
    }
    let body = request.body.as_ref().filter(|body| !body.content.trim().is_empty())?;
    let media = match body.kind {
        BodyKind::Json => "application/json",
        BodyKind::Xml => "application/xml",
        BodyKind::FormUrlencoded => "application/x-www-form-urlencoded",
        BodyKind::Multipart => "multipart/form-data",
        BodyKind::File | BodyKind::Text => "text/plain",
    };
    // A JSON body doubles as its own example, and as a rough schema.
    let example: Value = serde_json::from_str(&body.content).unwrap_or_else(|_| json!(body.content));
    Some(json!({
        "content": { media: { "schema": schema_of(&example), "example": example } },
    }))
}

/// A shape for a value, deep enough to be a useful starting point.
fn schema_of(value: &Value) -> Value {
    match value {
        Value::Object(fields) => json!({
            "type": "object",
            "properties": fields
                .iter()
                .map(|(name, value)| (name.clone(), schema_of(value)))
                .collect::<serde_json::Map<_, _>>(),
        }),
        Value::Array(items) => json!({
            "type": "array",
            "items": items.first().map(schema_of).unwrap_or_else(|| json!({})),
        }),
        Value::String(_) => json!({ "type": "string" }),
        Value::Bool(_) => json!({ "type": "boolean" }),
        Value::Number(number) if number.is_i64() || number.is_u64() => json!({ "type": "integer" }),
        Value::Number(_) => json!({ "type": "number" }),
        Value::Null => json!({ "type": "null" }),
    }
}

/// A request's saved responses as a HAR file, for sharing what actually happened.
///
/// The exchange is reconstructed from the request as it stands now and the responses that
/// were kept, so a request edited since will show its current shape.
pub fn har(request: &RequestFile, responses: &[StoredResponse]) -> Value {
    let entries: Vec<Value> = responses.iter().map(|response| har_entry(request, response)).collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "Courier", "version": env!("CARGO_PKG_VERSION") },
            "entries": entries,
        }
    })
}

fn har_entry(request: &RequestFile, response: &StoredResponse) -> Value {
    let headers = |headers: &[(String, String)]| -> Vec<Value> {
        headers
            .iter()
            .map(|(name, value)| json!({ "name": name, "value": value }))
            .collect()
    };
    let sent = json!({
        "method": request.method.to_ascii_uppercase(),
        "url": request.url,
        "httpVersion": "HTTP/1.1",
        "headers": request
            .headers
            .iter()
            .filter(|header| header.enabled)
            .map(|header| json!({ "name": header.name, "value": header.value }))
            .collect::<Vec<_>>(),
        "queryString": [],
        "cookies": [],
        "headersSize": -1,
        "bodySize": request.body.as_ref().map(|body| body.content.len()).unwrap_or(0),
    });
    let mut sent = sent;
    if let Some(body) = &request.body {
        sent["postData"] = json!({
            "mimeType": content_type_of(&request.headers).unwrap_or_else(|| "text/plain".into()),
            "text": body.content,
        });
    }

    let received = match &response.outcome {
        Outcome::Response {
            status,
            reason,
            headers: response_headers,
            body,
            body_size,
            ..
        } => json!({
            "status": status,
            "statusText": reason,
            "httpVersion": "HTTP/1.1",
            "headers": headers(response_headers),
            "cookies": [],
            "content": {
                "size": body_size,
                "mimeType": response_headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                    .map(|(_, value)| value.clone())
                    .unwrap_or_else(|| "text/plain".into()),
                "text": body,
            },
            "redirectURL": "",
            "headersSize": -1,
            "bodySize": body_size,
        }),
        Outcome::Error { message } => json!({
            "status": 0,
            "statusText": message,
            "httpVersion": "HTTP/1.1",
            "headers": [],
            "cookies": [],
            "content": { "size": 0, "mimeType": "text/plain", "text": message },
            "redirectURL": "",
            "headersSize": -1,
            "bodySize": 0,
        }),
    };

    // HAR wants every phase; -1 means "not measured", which is honest for what we don't have.
    let timing = response.timing.clone().unwrap_or_default();
    json!({
        "startedDateTime": rfc3339(response.received_at.saturating_sub(response.elapsed_ms / 1000)),
        "time": response.elapsed_ms,
        "request": sent,
        "response": received,
        "cache": {},
        "timings": {
            "blocked": -1,
            "dns": timing.dns_ms.map(|ms| ms as i64).unwrap_or(-1),
            "connect": timing.connect_ms.map(|ms| ms as i64).unwrap_or(-1),
            "send": 0,
            "wait": timing.waiting_ms(),
            "receive": timing.download_ms,
            "ssl": -1,
        },
        "serverIPAddress": timing.address.unwrap_or_default(),
    })
}

fn content_type_of(headers: &[Header]) -> Option<String> {
    headers
        .iter()
        .find(|header| header.enabled && header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.clone())
}

/// Unix seconds as the timestamp HAR asks for.
fn rfc3339(seconds: u64) -> String {
    let (year, month, day, hour, minute, second) = civil(seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil(seconds: u64) -> (u64, u64, u64, u64, u64, u64) {
    // Same arithmetic as the AWS timestamps; both want UTC without a date library.
    let stamp = crate::sigv4::timestamp(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds));
    let number = |range: std::ops::Range<usize>| stamp[range].parse().unwrap_or_default();
    (
        number(0..4),
        number(4..6),
        number(6..8),
        number(9..11),
        number(11..13),
        number(13..15),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Body, CollectionFile, RequestFile};

    fn collection() -> Collection {
        let mut file = CollectionFile::new("Pets");
        file.variables.insert("base_url".into(), "https://api.test".into());
        file.secrets.push("token".into());
        file.auth = Auth::Bearer {
            token: "{{token}}".into(),
        };

        let mut list = RequestFile::new("List pets");
        list.url = "{{base_url}}/pets?limit=10".into();
        list.headers = crate::model::headers_from_text("X-Tenant: acme\n# X-Debug: 1");

        let mut create = RequestFile::new("Create pet");
        create.method = "POST".into();
        create.url = "{{base_url}}/pets".into();
        create.body = Some(Body {
            kind: BodyKind::Json,
            content: r#"{"name":"Rex","age":3,"tags":["good"]}"#.into(),
        });
        create.auth = Auth::Basic {
            username: "u".into(),
            password: "{{password}}".into(),
        };

        let mut one = RequestFile::new("Get pet");
        one.url = "{{base_url}}/pets/{{petId}}".into();

        Collection {
            root: std::path::PathBuf::from("/tmp/.courier"),
            file,
            items: vec![
                Item::Folder {
                    name: "Pets".into(),
                    path: std::path::PathBuf::from("/tmp/.courier/Pets"),
                    children: vec![
                        Item::Request {
                            path: std::path::PathBuf::from("/tmp/.courier/Pets/list.yaml"),
                            request: list,
                        },
                        Item::Request {
                            path: std::path::PathBuf::from("/tmp/.courier/Pets/create.yaml"),
                            request: create,
                        },
                    ],
                },
                Item::Request {
                    path: std::path::PathBuf::from("/tmp/.courier/one.yaml"),
                    request: one,
                },
            ],
            environments: Vec::new(),
            env_files: Vec::new(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn exports_a_postman_collection_our_own_importer_reads_back() {
        let document = postman(&collection());
        assert_eq!(document["info"]["name"], "Pets");
        assert!(
            document["info"]["schema"].as_str().unwrap().contains("v2.1.0"),
            "the schema Postman expects"
        );
        // Secret names travel, values do not.
        let secret = document["variable"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variable| variable["key"] == "token")
            .unwrap();
        assert_eq!(secret["value"], "", "a secret's value is never exported");

        let text = serde_json::to_string(&document).unwrap();
        let (format, import) = crate::import::parse_collection_file(&text).unwrap();
        assert_eq!(format, crate::import::ImportFormat::Postman);
        assert_eq!(import.collection.name, "Pets");
        assert_eq!(crate::import::ImportItem::count_requests(&import.items), 3);
        let names: Vec<String> = {
            let mut names = Vec::new();
            let mut items = import.items;
            crate::import::ImportItem::for_each_request_mut(&mut items, &mut |request| {
                names.push(format!("{} {}", request.method, request.url))
            });
            names
        };
        assert!(
            names.contains(&"GET {{base_url}}/pets?limit=10".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"POST {{base_url}}/pets".to_string()), "{names:?}");
    }

    #[test]
    fn exports_an_openapi_skeleton() {
        let document = openapi(&collection());
        assert_eq!(document["openapi"], "3.1.0");
        assert_eq!(document["servers"][0]["url"], "https://api.test");
        let paths = document["paths"].as_object().unwrap();
        assert!(paths.contains_key("/pets"), "{paths:?}");
        assert!(
            paths.contains_key("/pets/{petId}"),
            "a variable becomes a path parameter"
        );

        let list = &paths["/pets"]["get"];
        assert_eq!(list["summary"], "List pets");
        assert_eq!(list["tags"][0], "Pets", "the folder becomes a tag");
        let parameters = list["parameters"].as_array().unwrap();
        assert!(
            parameters.iter().any(|p| p["name"] == "limit" && p["in"] == "query"),
            "{parameters:?}"
        );
        assert!(
            parameters
                .iter()
                .any(|p| p["name"] == "X-Tenant" && p["in"] == "header"),
            "an unusual header is worth describing: {parameters:?}"
        );
        assert!(
            !parameters.iter().any(|p| p["name"] == "X-Debug"),
            "a switched-off header isn't"
        );

        let body = &paths["/pets"]["post"]["requestBody"]["content"]["application/json"];
        assert_eq!(body["example"]["name"], "Rex");
        assert_eq!(body["schema"]["properties"]["age"]["type"], "integer");
        assert_eq!(body["schema"]["properties"]["tags"]["items"]["type"], "string");

        let one = &paths["/pets/{petId}"]["get"]["parameters"][0];
        assert_eq!(
            (one["name"].as_str(), one["in"].as_str()),
            (Some("petId"), Some("path"))
        );

        // And it comes back through our own importer.
        let text = serde_json::to_string(&document).unwrap();
        let (format, import) = crate::import::parse_collection_file(&text).unwrap();
        assert_eq!(format, crate::import::ImportFormat::OpenApi);
        assert_eq!(crate::import::ImportItem::count_requests(&import.items), 3);
    }

    #[test]
    fn exports_history_as_har() {
        let mut request = RequestFile::new("List pets");
        request.url = "https://api.test/pets".into();
        request.headers = crate::model::headers_from_text("Accept: application/json");
        let response = StoredResponse {
            received_at: 1_440_938_160,
            elapsed_ms: 150,
            outcome: Outcome::Response {
                status: 200,
                reason: "OK".into(),
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: "{\"ok\":true}".into(),
                body_size: 11,
                truncated: false,
            },
            timing: Some(crate::response_cache::Timing {
                dns_ms: Some(5),
                connect_ms: Some(20),
                ttfb_ms: 100,
                download_ms: 50,
                address: Some("93.184.216.34".into()),
            }),
        };
        let document = har(&request, std::slice::from_ref(&response));
        let entry = &document["log"]["entries"][0];
        assert_eq!(document["log"]["creator"]["name"], "Courier");
        assert_eq!(entry["request"]["method"], "GET");
        assert_eq!(entry["request"]["url"], "https://api.test/pets");
        assert_eq!(entry["response"]["status"], 200);
        assert_eq!(entry["response"]["content"]["text"], "{\"ok\":true}");
        assert_eq!(entry["time"], 150);
        assert_eq!(entry["timings"]["dns"], 5);
        assert_eq!(entry["timings"]["wait"], 75, "waiting is the first byte less the rest");
        assert_eq!(entry["serverIPAddress"], "93.184.216.34");
        assert_eq!(
            entry["startedDateTime"], "2015-08-30T12:36:00Z",
            "when it started, to the second we have"
        );

        // A response with no timing says so rather than inventing numbers.
        let bare = StoredResponse {
            timing: None,
            ..response
        };
        let document = har(&request, &[bare]);
        assert_eq!(document["log"]["entries"][0]["timings"]["dns"], -1);

        // And our own importer reads it back.
        let text = serde_json::to_string(&document).unwrap();
        let (format, import) = crate::import::parse_collection_file(&text).unwrap();
        assert_eq!(format, crate::import::ImportFormat::Har);
        assert_eq!(crate::import::ImportItem::count_requests(&import.items), 1);
    }
}
