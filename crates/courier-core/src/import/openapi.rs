//! Creating a collection from an OpenAPI 3.x or Swagger 2.0 document: one request per
//! operation, grouped into folders by tag, with example bodies, and a base URL per server.

use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde_json::{Map, Value};

use super::spec::{Refs, fill_server_variables, plain, secret_name, templated_path, truncate};
use super::{CollectionImport, ImportItem};
use crate::encoding::percent_encode;
use crate::model::{Body, BodyKind, CollectionFile, EnvironmentFile, Header, RequestFile, Variables};

const METHODS: [&str; 8] = ["get", "put", "post", "delete", "options", "head", "patch", "trace"];
/// Headers that the request body or security scheme already sets.
const MANAGED_HEADERS: [&str; 3] = ["accept", "content-type", "authorization"];

pub fn convert(doc: &Value) -> Result<CollectionImport> {
    let swagger = match (doc.get("openapi"), doc.get("swagger")) {
        (Some(version), _) if plain(version).starts_with('3') => false,
        (_, Some(version)) if plain(version).starts_with('2') => true,
        (Some(version), _) | (_, Some(version)) => bail!("OpenAPI version {} isn't supported", plain(version)),
        _ => bail!("Not an OpenAPI document: expected an `openapi` or `swagger` version"),
    };
    let refs = Refs::new(doc);
    let title = doc
        .pointer("/info/title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("API");
    let mut collection = CollectionFile::new(truncate(title, 80));
    let mut warnings = Vec::new();

    let servers = if swagger { swagger_servers(doc) } else { servers(doc) };
    let base_url = servers.first().map(|(_, url)| url.clone()).unwrap_or_default();
    if base_url.is_empty() {
        warnings.push("the document lists no servers; set `base_url` in the collection defaults".into());
    } else if base_url.starts_with('/') {
        warnings.push(format!(
            "the server URL `{base_url}` is relative; add the host to `base_url`"
        ));
    }
    collection.variables.insert("base_url".into(), base_url);
    let environments = if servers.len() > 1 {
        servers
            .iter()
            .map(|(name, url)| {
                let mut env = EnvironmentFile::new(truncate(name, 60));
                env.variables.insert("base_url".into(), url.clone());
                env
            })
            .collect()
    } else {
        Vec::new()
    };

    let schemes = if swagger {
        doc.get("securityDefinitions")
    } else {
        doc.pointer("/components/securitySchemes")
    };
    let mut converter = Converter {
        refs: &refs,
        swagger,
        schemes: schemes.and_then(Value::as_object),
        global_security: doc.get("security"),
        global_consumes: doc.get("consumes"),
        variables: &mut collection.variables,
        secret_names: &mut collection.secrets,
        warnings: &mut warnings,
    };

    // Folders per tag, in the order the document lists its tags, then as first used.
    let mut folders: IndexMap<String, Vec<ImportItem>> = IndexMap::new();
    for tag in doc.get("tags").and_then(Value::as_array).into_iter().flatten() {
        if let Some(name) = tag.get("name").and_then(Value::as_str) {
            folders.insert(name.to_string(), Vec::new());
        }
    }
    let mut untagged = Vec::new();
    for (path, item) in doc.get("paths").and_then(Value::as_object).into_iter().flatten() {
        let item = refs.resolve(item);
        for method in METHODS {
            let Some(operation) = item.get(method) else {
                continue;
            };
            let request = converter.request(path, method, item, operation);
            match operation
                .get("tags")
                .and_then(Value::as_array)
                .and_then(|t| t.first())
                .and_then(Value::as_str)
            {
                Some(tag) => folders
                    .entry(tag.to_string())
                    .or_default()
                    .push(ImportItem::Request(request)),
                None => untagged.push(ImportItem::Request(request)),
            }
        }
    }
    if doc
        .get("webhooks")
        .and_then(Value::as_object)
        .is_some_and(|w| !w.is_empty())
    {
        warnings.push("webhooks were skipped: they describe requests the API sends, not ones you send".into());
    }

    let mut items: Vec<ImportItem> = folders
        .into_iter()
        .filter(|(_, children)| !children.is_empty())
        .map(|(name, children)| ImportItem::Folder { name, children })
        .collect();
    items.extend(untagged);
    if items.is_empty() {
        warnings.push("the document has no operations under `paths`".into());
    }

    Ok(CollectionImport {
        collection,
        items,
        environments,
        secrets: Variables::new(),
        warnings,
    })
}

/// (name, URL) for each OpenAPI 3 server.
fn servers(doc: &Value) -> Vec<(String, String)> {
    doc.get("servers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|server| {
            let url = fill_server_variables(server.get("url")?.as_str()?, server.get("variables"));
            let name = server
                .get("description")
                .and_then(Value::as_str)
                .filter(|d| !d.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| url.clone());
            Some((name, url))
        })
        .collect()
}

/// Swagger 2.0 describes one host with a list of schemes.
fn swagger_servers(doc: &Value) -> Vec<(String, String)> {
    let host = doc.get("host").and_then(Value::as_str).unwrap_or("localhost");
    let base_path = doc.get("basePath").and_then(Value::as_str).unwrap_or("");
    let schemes: Vec<&str> = doc
        .get("schemes")
        .and_then(Value::as_array)
        .map(|s| s.iter().filter_map(Value::as_str).collect())
        .filter(|s: &Vec<&str>| !s.is_empty())
        .unwrap_or_else(|| vec!["https"]);
    schemes
        .into_iter()
        .map(|scheme| {
            let url = format!("{scheme}://{host}{base_path}")
                .trim_end_matches('/')
                .to_string();
            (url.clone(), url)
        })
        .collect()
}

struct Converter<'a, 'd> {
    refs: &'a Refs<'d>,
    swagger: bool,
    schemes: Option<&'d Map<String, Value>>,
    global_security: Option<&'d Value>,
    global_consumes: Option<&'d Value>,
    variables: &'a mut Variables,
    secret_names: &'a mut Vec<String>,
    warnings: &'a mut Vec<String>,
}

impl<'d> Converter<'_, 'd> {
    fn request(&mut self, path: &str, method: &str, item: &'d Value, operation: &'d Value) -> RequestFile {
        let label = format!("{} {path}", method.to_ascii_uppercase());
        let name = operation
            .get("summary")
            .or_else(|| operation.get("operationId"))
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .map(|n| truncate(n, 80))
            .unwrap_or_else(|| label.clone());
        let mut request = RequestFile::new(name);
        request.method = method.to_ascii_uppercase();

        let (templated, path_params) = templated_path(path);
        let mut query = Vec::new();
        for parameter in self.parameters(item, operation) {
            let Some(param_name) = parameter.get("name").and_then(Value::as_str) else {
                continue;
            };
            let required = parameter.get("required").and_then(Value::as_bool) == Some(true);
            let example = self.parameter_example(parameter);
            match parameter.get("in").and_then(Value::as_str) {
                Some("path") => {
                    let current = self.variables.get(param_name).cloned().unwrap_or_default();
                    if current.is_empty() {
                        self.variables.insert(param_name.to_string(), example);
                    }
                }
                Some("query") if required => {
                    query.push(format!("{}={}", percent_encode(param_name), percent_encode(&example)))
                }
                Some("header") if !MANAGED_HEADERS.contains(&param_name.to_ascii_lowercase().as_str()) => {
                    request.headers.push(Header {
                        name: param_name.to_string(),
                        value: example,
                        enabled: required,
                    });
                }
                _ => {}
            }
        }
        for name in path_params {
            self.variables.entry(name).or_default();
        }
        request.url = format!("{{{{base_url}}}}{templated}");
        for pair in query {
            append_query(&mut request.url, &pair);
        }

        self.apply_security(&mut request, operation);
        request.body = if self.swagger {
            self.swagger_body(operation, &label)
        } else {
            self.body(operation, &label)
        };
        if operation
            .get("callbacks")
            .and_then(Value::as_object)
            .is_some_and(|c| !c.is_empty())
        {
            self.warnings.push(format!("{label}: callbacks were skipped"));
        }
        request
    }

    /// Path-level parameters overridden by operation-level ones with the same name and location.
    fn parameters(&self, item: &'d Value, operation: &'d Value) -> Vec<&'d Value> {
        let mut parameters: Vec<&Value> = Vec::new();
        for list in [item.get("parameters"), operation.get("parameters")] {
            for parameter in list.and_then(Value::as_array).into_iter().flatten() {
                let parameter = self.refs.resolve(parameter);
                let key = |p: &Value| (p.get("name").cloned(), p.get("in").cloned());
                parameters.retain(|existing| key(existing) != key(parameter));
                parameters.push(parameter);
            }
        }
        parameters
    }

    fn parameter_example(&self, parameter: &Value) -> String {
        if let Some(example) = parameter.get("example") {
            return plain(example);
        }
        if let Some(example) = parameter
            .get("examples")
            .and_then(Value::as_object)
            .and_then(|e| e.values().next())
            .map(|e| self.refs.resolve(e))
            .and_then(|e| e.get("value"))
        {
            return plain(example);
        }
        // Swagger 2.0 puts the schema keywords on the parameter itself.
        let schema = parameter.get("schema").unwrap_or(parameter);
        let has_hint = ["example", "default", "enum", "$ref"]
            .iter()
            .any(|k| schema.get(k).is_some());
        if has_hint {
            plain(&self.refs.example(schema))
        } else {
            String::new()
        }
    }

    fn body(&mut self, operation: &'d Value, label: &str) -> Option<Body> {
        let request_body = self.refs.resolve(operation.get("requestBody")?);
        let content = request_body.get("content")?.as_object()?;
        let pick =
            |wanted: &dyn Fn(&str) -> bool| content.iter().find(|(media, _)| wanted(&media.to_ascii_lowercase()));
        let Some((media, media_type)) = pick(&|m| m.contains("json"))
            .or_else(|| pick(&|m| m.contains("x-www-form-urlencoded")))
            .or_else(|| pick(&|m| m.contains("xml")))
            .or_else(|| pick(&|m| m.starts_with("text/")))
        else {
            if content.keys().any(|m| m.starts_with("multipart/")) {
                self.warnings
                    .push(format!("{label}: multipart bodies aren't supported yet"));
            }
            return None;
        };
        let example = media_type
            .get("example")
            .cloned()
            .or_else(|| {
                media_type
                    .get("examples")
                    .and_then(Value::as_object)
                    .and_then(|e| e.values().next())
                    .map(|e| self.refs.resolve(e))
                    .and_then(|e| e.get("value"))
                    .cloned()
            })
            .or_else(|| media_type.get("schema").map(|schema| self.refs.example(schema)))
            .unwrap_or(Value::Null);
        if media_type.get("schema").is_some_and(Refs::is_external) {
            self.warnings.push(format!(
                "{label}: the body schema is in another file, so the example is empty"
            ));
        }
        Some(body_for(BodyKind::from_content_type(media), &example))
    }

    fn swagger_body(&mut self, operation: &'d Value, label: &str) -> Option<Body> {
        let parameters: Vec<&Value> = operation
            .get("parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| self.refs.resolve(p))
            .collect();
        if let Some(body) = parameters
            .iter()
            .find(|p| p.get("in").and_then(Value::as_str) == Some("body"))
        {
            let example = body.get("schema").map(|s| self.refs.example(s)).unwrap_or(Value::Null);
            return Some(body_for(BodyKind::Json, &example));
        }
        let form: Map<String, Value> = parameters
            .iter()
            .filter(|p| p.get("in").and_then(Value::as_str) == Some("formData"))
            .filter_map(|p| {
                if p.get("type").and_then(Value::as_str) == Some("file") {
                    self.warnings
                        .push(format!("{label}: file uploads aren't supported yet"));
                    return None;
                }
                Some((
                    p.get("name")?.as_str()?.to_string(),
                    Value::String(self.parameter_example(p)),
                ))
            })
            .collect();
        if form.is_empty() {
            return None;
        }
        let consumes = operation.get("consumes").or(self.global_consumes);
        let multipart = consumes
            .and_then(Value::as_array)
            .is_some_and(|c| c.iter().any(|m| m.as_str() == Some("multipart/form-data")));
        if multipart {
            self.warnings
                .push(format!("{label}: sent as a URL-encoded form instead of multipart"));
        }
        Some(body_for(BodyKind::FormUrlencoded, &Value::Object(form)))
    }

    /// Adds the operation's (or the document's) first security requirement as a header or
    /// query parameter referring to a secret.
    fn apply_security(&mut self, request: &mut RequestFile, operation: &Value) {
        let requirements = operation.get("security").or(self.global_security);
        // An empty requirement (`- {}`) means the operation can be called without auth.
        let Some(requirement) = requirements
            .and_then(Value::as_array)
            .and_then(|r| r.first())
            .and_then(Value::as_object)
        else {
            return;
        };
        let Some(scheme_name) = requirement.keys().next() else {
            return;
        };
        let Some(scheme) = self
            .schemes
            .and_then(|s| s.get(scheme_name))
            .map(|s| self.refs.resolve(s))
        else {
            self.warnings.push(format!(
                "security scheme `{scheme_name}` isn't defined, so it was skipped"
            ));
            return;
        };
        let secret = secret_name(scheme_name);
        let value = crate::model::placeholder(&secret);
        let kind = scheme.get("type").and_then(Value::as_str).unwrap_or_default();
        let http_scheme = scheme
            .get("scheme")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        match (kind, http_scheme.as_str()) {
            ("http", "bearer") | ("oauth2" | "openIdConnect", _) => request
                .headers
                .push(Header::new("Authorization", format!("Bearer {value}"))),
            ("http", "basic") | ("basic", _) => request
                .headers
                .push(Header::new("Authorization", format!("Basic {value}"))),
            ("apiKey", _) => {
                let key = scheme.get("name").and_then(Value::as_str).unwrap_or("X-API-Key");
                match scheme.get("in").and_then(Value::as_str) {
                    Some("query") => append_query(&mut request.url, &format!("{}={value}", percent_encode(key))),
                    Some("cookie") => request.headers.push(Header::new("Cookie", format!("{key}={value}"))),
                    _ => request.headers.push(Header::new(key, value)),
                }
            }
            _ => {
                self.warnings.push(format!(
                    "security scheme `{scheme_name}` ({kind}) isn't supported, so it was skipped"
                ));
                return;
            }
        }
        if kind == "http" && http_scheme == "basic" || kind == "basic" {
            let note = format!("set `{secret}` to base64 of `username:password` for `{scheme_name}`");
            if !self.warnings.contains(&note) {
                self.warnings.push(note);
            }
        }
        if !self.secret_names.contains(&secret) {
            self.secret_names.push(secret);
        }
    }
}

/// Appends a `name=value` pair to a URL that may already have a query.
fn append_query(url: &mut String, pair: &str) {
    url.push(if url.contains('?') { '&' } else { '?' });
    url.push_str(pair);
}

fn body_for(kind: BodyKind, example: &Value) -> Body {
    let content = match (kind, example) {
        (BodyKind::Json, value) => serde_json::to_string_pretty(value).unwrap_or_default(),
        (BodyKind::FormUrlencoded, Value::Object(fields)) => fields
            .iter()
            .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(&plain(v))))
            .collect::<Vec<_>>()
            .join("&"),
        (_, value) => plain(value),
    };
    Body { kind, content }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::spec::parse_document;

    const PETSTORE: &str = r#"
openapi: 3.0.3
info:
  title: Petstore
servers:
  - url: https://{region}.petstore.test/v1
    description: Production
    variables:
      region: { default: eu }
  - url: http://localhost:8080/v1
    description: Local
tags:
  - name: pets
security:
  - bearerAuth: []
paths:
  /pets:
    get:
      tags: [pets]
      summary: List pets
      parameters:
        - { name: limit, in: query, required: true, schema: { type: integer, example: 20 } }
        - { name: species, in: query, schema: { type: string } }
        - { name: X-Request-Id, in: header, schema: { type: string, format: uuid } }
    post:
      tags: [pets]
      operationId: createPet
      requestBody:
        content:
          application/json:
            schema: { $ref: '#/components/schemas/NewPet' }
  /pets/{petId}:
    parameters:
      - $ref: '#/components/parameters/PetId'
    get:
      tags: [pets]
      summary: Get a pet
      security: []
    delete:
      summary: Delete a pet
      security:
        - apiKey: []
  /login:
    post:
      summary: Log in
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              properties:
                username: { type: string, example: sam }
                password: { type: string }
components:
  parameters:
    PetId: { name: petId, in: path, required: true, schema: { type: string, example: rex-1 } }
  schemas:
    NewPet:
      type: object
      properties:
        name: { type: string, example: Rex }
        species: { type: string, enum: [dog, cat] }
  securitySchemes:
    bearerAuth: { type: http, scheme: bearer }
    apiKey: { type: apiKey, in: query, name: api_key }
"#;

    fn requests(items: &[ImportItem]) -> Vec<&RequestFile> {
        items
            .iter()
            .flat_map(|item| match item {
                ImportItem::Request(r) => vec![r],
                ImportItem::Folder { children, .. } => requests(children),
            })
            .collect()
    }

    #[test]
    fn converts_openapi_3() {
        let import = convert(&parse_document(PETSTORE).unwrap()).unwrap();
        assert_eq!(import.collection.name, "Petstore");
        assert_eq!(import.collection.variables["base_url"], "https://eu.petstore.test/v1");
        assert_eq!(import.collection.variables["petId"], "rex-1");
        assert_eq!(import.collection.secrets, ["bearer_auth", "api_key"]);
        assert!(import.secrets.is_empty(), "no secret values come from a spec");
        let environments: Vec<_> = import.environments.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(environments, ["Production", "Local"]);
        assert_eq!(import.environments[1].variables["base_url"], "http://localhost:8080/v1");

        let ImportItem::Folder { name, children } = &import.items[0] else {
            panic!("tagged operations are in a folder");
        };
        assert_eq!((name.as_str(), children.len()), ("pets", 3));
        let all = requests(&import.items);
        assert_eq!(all.len(), 5);

        let list = all[0];
        assert_eq!((list.name.as_str(), list.method.as_str()), ("List pets", "GET"));
        assert_eq!(list.url, "{{base_url}}/pets?limit=20", "only required query parameters");
        let headers: Vec<_> = list
            .headers
            .iter()
            .map(|h| (h.name.as_str(), h.value.as_str(), h.enabled))
            .collect();
        assert_eq!(
            headers,
            [
                ("X-Request-Id", "", false),
                ("Authorization", "Bearer {{bearer_auth}}", true)
            ]
        );

        let create = all[1];
        assert_eq!(create.name, "createPet");
        let body = create.body.as_ref().unwrap();
        assert_eq!(body.kind, BodyKind::Json);
        let json: Value = serde_json::from_str(&body.content).unwrap();
        assert_eq!(json, serde_json::json!({ "name": "Rex", "species": "dog" }));

        let get = all[2];
        assert_eq!(get.url, "{{base_url}}/pets/{{petId}}");
        assert!(get.headers.is_empty(), "`security: []` means no auth");

        let delete = all[3];
        assert_eq!(delete.url, "{{base_url}}/pets/{{petId}}?api_key={{api_key}}");

        let login = all[4];
        let body = login.body.as_ref().unwrap();
        assert_eq!(
            (body.kind, body.content.as_str()),
            (BodyKind::FormUrlencoded, "username=sam&password=string")
        );
    }

    #[test]
    fn converts_swagger_2() {
        let doc = serde_json::json!({
            "swagger": "2.0",
            "info": { "title": "Legacy" },
            "host": "api.legacy.test",
            "basePath": "/v2",
            "schemes": ["https"],
            "securityDefinitions": { "basic": { "type": "basic" } },
            "paths": {
                "/users": {
                    "post": {
                        "summary": "Create user",
                        "security": [{ "basic": [] }],
                        "parameters": [{ "in": "body", "name": "user", "schema": {
                            "properties": { "email": { "type": "string", "format": "email" } }
                        } }]
                    }
                }
            }
        });
        let import = convert(&doc).unwrap();
        assert_eq!(import.collection.variables["base_url"], "https://api.legacy.test/v2");
        assert!(import.environments.is_empty(), "one server, no environments");
        let ImportItem::Request(create) = &import.items[0] else {
            panic!()
        };
        assert_eq!(create.headers[0].value, "Basic {{basic}}");
        assert!(create.body.as_ref().unwrap().content.contains("user@example.com"));
        assert!(import.warnings.iter().any(|w| w.contains("base64")));
    }

    #[test]
    fn rejects_other_documents() {
        assert!(convert(&serde_json::json!({ "openapi": "4.0" })).is_err());
        assert!(convert(&serde_json::json!({ "asyncapi": "3.0.0" })).is_err());
    }
}
