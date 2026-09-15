//! Creating a collection from an AsyncAPI 2.x or 3.0 document: a WebSocket request per
//! channel with its messages as templates (or a GET request for HTTP servers, which suits
//! event streams). Other protocols (MQTT, Kafka, AMQP, …) can't be sent from Courier.

use anyhow::{Result, bail};
use serde_json::Value;

use super::spec::{Refs, fill_server_variables, plain, secret_name, templated_path, truncate};
use super::{CollectionImport, ImportItem};
use crate::encoding::percent_encode;
use crate::model::{
    Body, BodyKind, CollectionFile, EnvironmentFile, Header, MessageTemplate, RequestFile, Variables, placeholder,
};

/// A server Courier can talk to.
struct Server<'d> {
    name: String,
    url: String,
    websocket: bool,
    security: Option<&'d Value>,
}

pub fn convert(doc: &Value) -> Result<CollectionImport> {
    let version = doc.get("asyncapi").map(plain).unwrap_or_default();
    let v3 = match version.chars().next() {
        Some('2') => false,
        Some('3') => true,
        _ if version.is_empty() => bail!("Not an AsyncAPI document: expected an `asyncapi` version"),
        _ => bail!("AsyncAPI version {version} isn't supported"),
    };
    let refs = Refs::new(doc);
    let title = doc
        .pointer("/info/title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("Async API");
    let mut collection = CollectionFile::new(truncate(title, 80));
    let mut warnings = Vec::new();

    let (servers, unsupported) = servers(doc, &refs, v3);
    if !unsupported.is_empty() {
        warnings.push(format!(
            "Courier sends over WebSockets and HTTP only, so these servers were skipped: {}",
            unsupported.join(", ")
        ));
    }
    let websocket = servers.first().is_none_or(|s| s.websocket);
    match servers.first() {
        Some(server) => {
            collection.variables.insert("base_url".into(), server.url.clone());
        }
        None => {
            warnings.push("no WebSocket or HTTP server is listed; set `base_url` in the collection defaults".into());
            collection.variables.insert("base_url".into(), String::new());
        }
    }
    let environments = if servers.len() > 1 {
        servers
            .iter()
            .map(|server| {
                let mut env = EnvironmentFile::new(truncate(&server.name, 60));
                env.variables.insert("base_url".into(), server.url.clone());
                env
            })
            .collect()
    } else {
        Vec::new()
    };
    let auth_headers = servers
        .first()
        .and_then(|s| s.security)
        .map(|security| security_headers(security, &refs, doc, &mut collection.secrets, &mut warnings))
        .unwrap_or_default();

    let mut items = Vec::new();
    for channel in channels(doc, &refs, v3) {
        let (path, params) = templated_path(&channel.address);
        for name in params {
            let example = channel
                .parameters
                .and_then(|p| p.get(&name))
                .map(|p| refs.resolve(p))
                .and_then(|p| p.get("schema").or(Some(p)))
                .filter(|schema| {
                    schema.get("example").is_some() || schema.get("default").is_some() || schema.get("enum").is_some()
                })
                .map(|schema| plain(&refs.example(schema)))
                .unwrap_or_default();
            let current = collection.variables.get(&name).cloned().unwrap_or_default();
            if current.is_empty() {
                collection.variables.insert(name, example);
            }
        }
        let separator = if path.starts_with('/') || path.is_empty() {
            ""
        } else {
            "/"
        };
        let mut request = RequestFile::new(truncate(&channel.name, 80));
        request.url = format!("{}{separator}{path}", placeholder("base_url"));
        request.headers = auth_headers.headers.clone();
        for pair in &auth_headers.query {
            request.url.push(if request.url.contains('?') { '&' } else { '?' });
            request.url.push_str(pair);
        }
        if websocket {
            request.messages = channel
                .messages
                .iter()
                .map(|message| MessageTemplate {
                    name: message_name(message),
                    content: message_example(message, &refs),
                })
                .collect();
            request.body = request.messages.first().map(|m| Body {
                kind: if serde_json::from_str::<Value>(&m.content).is_ok() {
                    BodyKind::Json
                } else {
                    BodyKind::Text
                },
                content: m.content.clone(),
            });
        }
        items.push(ImportItem::Request(request));
    }
    if items.is_empty() {
        warnings.push("the document has no channels".into());
    }

    Ok(CollectionImport {
        collection,
        items,
        environments,
        secrets: Variables::new(),
        warnings,
    })
}

/// Supported servers, and descriptions of the unsupported ones.
fn servers<'d>(doc: &'d Value, refs: &Refs<'d>, v3: bool) -> (Vec<Server<'d>>, Vec<String>) {
    let mut supported = Vec::new();
    let mut unsupported = Vec::new();
    for (name, server) in doc.get("servers").and_then(Value::as_object).into_iter().flatten() {
        let server = refs.resolve(server);
        let protocol = server
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let websocket = match protocol.as_str() {
            "ws" | "wss" => true,
            "http" | "https" => false,
            _ => {
                unsupported.push(format!("{name} ({protocol})"));
                continue;
            }
        };
        let raw = if v3 {
            let host = server.get("host").and_then(Value::as_str).unwrap_or_default();
            let pathname = server.get("pathname").and_then(Value::as_str).unwrap_or_default();
            format!("{host}{pathname}")
        } else {
            server
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let raw = if raw.contains("://") {
            raw
        } else {
            format!("{protocol}://{raw}")
        };
        let url = fill_server_variables(&raw, server.get("variables"));
        let label = server
            .get("title")
            .or_else(|| server.get("description"))
            .and_then(Value::as_str)
            .filter(|d| !d.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| name.clone());
        supported.push(Server {
            name: label,
            url,
            websocket,
            security: server.get("security"),
        });
    }
    (supported, unsupported)
}

struct Channel<'d> {
    name: String,
    address: String,
    parameters: Option<&'d serde_json::Map<String, Value>>,
    /// Messages a client sends first, then the ones it receives.
    messages: Vec<(String, &'d Value)>,
}

fn channels<'d>(doc: &'d Value, refs: &Refs<'d>, v3: bool) -> Vec<Channel<'d>> {
    let mut channels = Vec::new();
    for (key, channel) in doc.get("channels").and_then(Value::as_object).into_iter().flatten() {
        let channel = refs.resolve(channel);
        let parameters = channel.get("parameters").and_then(Value::as_object);
        if v3 {
            let address = channel
                .get("address")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = channel
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| key.clone());
            let pointer = format!("#/channels/{}", key.replace('~', "~0").replace('/', "~1"));
            // In 3.0, `receive` operations are what the application receives: what a client sends.
            let mut sent: Vec<(String, &Value)> = Vec::new();
            for operation in doc
                .get("operations")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(_, o)| refs.resolve(o))
            {
                let on_channel = operation.pointer("/channel/$ref").and_then(Value::as_str) == Some(pointer.as_str());
                if !on_channel || operation.get("action").and_then(Value::as_str) != Some("receive") {
                    continue;
                }
                for message in operation
                    .get("messages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let id = message
                        .get("$ref")
                        .and_then(Value::as_str)
                        .and_then(|r| r.rsplit('/').next())
                        .unwrap_or("message")
                        .to_string();
                    sent.push((id, refs.resolve(message)));
                }
            }
            let mut messages = sent;
            for (id, message) in channel.get("messages").and_then(Value::as_object).into_iter().flatten() {
                let message = refs.resolve(message);
                if !messages.iter().any(|(_, m)| std::ptr::eq(*m, message)) {
                    messages.push((id.clone(), message));
                }
            }
            channels.push(Channel {
                name,
                address,
                parameters,
                messages,
            });
        } else {
            // In 2.x, `publish` is what clients send to the application.
            let mut messages = Vec::new();
            for operation in ["publish", "subscribe"] {
                let Some(message) = channel
                    .pointer(&format!("/{operation}/message"))
                    .map(|m| refs.resolve(m))
                else {
                    continue;
                };
                let variants: Vec<&Value> = match message.get("oneOf").and_then(Value::as_array) {
                    Some(list) => list.iter().map(|m| refs.resolve(m)).collect(),
                    None => vec![message],
                };
                for variant in variants {
                    let id = variant
                        .get("messageId")
                        .and_then(Value::as_str)
                        .unwrap_or(operation)
                        .to_string();
                    messages.push((id, variant));
                }
            }
            channels.push(Channel {
                name: key.clone(),
                address: key.clone(),
                parameters,
                messages,
            });
        }
    }
    channels
}

fn message_name((id, message): &(String, &Value)) -> String {
    ["name", "title", "summary"]
        .iter()
        .find_map(|key| message.get(key).and_then(Value::as_str))
        .map(|n| truncate(n, 60))
        .unwrap_or_else(|| id.clone())
}

fn message_example((_, message): &(String, &Value), refs: &Refs) -> String {
    let example = message
        .get("examples")
        .and_then(Value::as_array)
        .and_then(|e| e.first())
        .and_then(|e| e.get("payload"))
        .cloned()
        .or_else(|| {
            let payload = refs.resolve(message.get("payload")?);
            // A 3.0 multi-format schema wraps the schema itself.
            let schema = if payload.get("schemaFormat").is_some() {
                payload.get("schema")?
            } else {
                payload
            };
            Some(refs.example(schema))
        })
        .unwrap_or(Value::Null);
    match example {
        Value::String(text) => text,
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(&other).unwrap_or_default(),
    }
}

#[derive(Default)]
struct Auth {
    headers: Vec<Header>,
    query: Vec<String>,
}

/// Headers or query parameters for a server's first security requirement.
fn security_headers(
    security: &Value,
    refs: &Refs,
    doc: &Value,
    secret_names: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Auth {
    let mut auth = Auth::default();
    let Some(first) = security.as_array().and_then(|s| s.first()) else {
        return auth;
    };
    // 2.x: `{ schemeName: [] }`; 3.0: a scheme object or `$ref` to one.
    let (scheme_name, scheme) = match first.get("$ref").and_then(Value::as_str) {
        Some(reference) => (
            reference.rsplit('/').next().unwrap_or("auth").to_string(),
            refs.target(reference),
        ),
        None if first.get("type").is_some() => ("auth".to_string(), Some(first)),
        None => match first.as_object().and_then(|o| o.keys().next()) {
            Some(name) => (
                name.clone(),
                doc.pointer(&format!("/components/securitySchemes/{name}")),
            ),
            None => return auth,
        },
    };
    let Some(scheme) = scheme.map(|s| refs.resolve(s)) else {
        warnings.push(format!(
            "security scheme `{scheme_name}` isn't defined, so it was skipped"
        ));
        return auth;
    };
    let secret = secret_name(&scheme_name);
    let value = placeholder(&secret);
    let kind = scheme.get("type").and_then(Value::as_str).unwrap_or_default();
    let http_scheme = scheme
        .get("scheme")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match (kind, http_scheme.as_str()) {
        ("http", "bearer") | ("oauth2" | "openIdConnect", _) => auth
            .headers
            .push(Header::new("Authorization", format!("Bearer {value}"))),
        ("http", "basic") => auth
            .headers
            .push(Header::new("Authorization", format!("Basic {value}"))),
        ("httpApiKey", _) => {
            let key = scheme.get("name").and_then(Value::as_str).unwrap_or("X-API-Key");
            match scheme.get("in").and_then(Value::as_str) {
                Some("query") => auth.query.push(format!("{}={value}", percent_encode(key))),
                Some("cookie") => auth.headers.push(Header::new("Cookie", format!("{key}={value}"))),
                _ => auth.headers.push(Header::new(key, value)),
            }
        }
        _ => {
            warnings.push(format!(
                "security scheme `{scheme_name}` ({kind}) isn't supported, so it was skipped"
            ));
            return auth;
        }
    }
    if !secret_names.contains(&secret) {
        secret_names.push(secret);
    }
    auth
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::spec::parse_document;

    const V2: &str = r#"
asyncapi: 2.6.0
info: { title: Market feed }
servers:
  production:
    url: feed.example.com/ws
    protocol: wss
    description: Production
    security:
      - token: []
  staging:
    url: ws://staging.example.com/ws
    protocol: ws
  broker:
    url: mqtt.example.com
    protocol: mqtt
channels:
  prices/{symbol}:
    parameters:
      symbol: { schema: { type: string, example: ACME } }
    publish:
      message:
        oneOf:
          - $ref: '#/components/messages/Subscribe'
          - $ref: '#/components/messages/Ping'
    subscribe:
      message:
        name: Price
        payload: { type: object, properties: { price: { type: number } } }
components:
  messages:
    Subscribe:
      name: Subscribe
      examples:
        - payload: { op: subscribe, symbol: ACME }
    Ping:
      messageId: ping
      payload: { type: string, example: ping }
  securitySchemes:
    token: { type: httpApiKey, in: query, name: token }
"#;

    const V3: &str = r#"
asyncapi: 3.0.0
info: { title: Chat }
servers:
  local:
    host: localhost:8080
    protocol: ws
    security:
      - $ref: '#/components/securitySchemes/bearer'
channels:
  room:
    address: /rooms/{roomId}
    title: Chat room
    messages:
      said: { $ref: '#/components/messages/Said' }
      say: { $ref: '#/components/messages/Say' }
operations:
  sendMessage:
    action: receive
    channel: { $ref: '#/channels/room' }
    messages: [{ $ref: '#/channels/room/messages/say' }]
components:
  messages:
    Say:
      title: Say something
      payload: { type: object, properties: { text: { type: string, example: hello } } }
    Said:
      name: Said
      payload: { type: object, properties: { from: { type: string } } }
  securitySchemes:
    bearer: { type: http, scheme: bearer }
"#;

    #[test]
    fn converts_asyncapi_2() {
        let import = convert(&parse_document(V2).unwrap()).unwrap();
        assert_eq!(import.collection.name, "Market feed");
        assert_eq!(import.collection.variables["base_url"], "wss://feed.example.com/ws");
        assert_eq!(import.collection.variables["symbol"], "ACME");
        assert_eq!(import.collection.secrets, ["token"]);
        assert_eq!(import.environments.len(), 2, "the MQTT server is skipped");
        assert!(import.warnings.iter().any(|w| w.contains("broker (mqtt)")));

        let ImportItem::Request(request) = &import.items[0] else {
            panic!()
        };
        assert_eq!(request.url, "{{base_url}}/prices/{{symbol}}?token={{token}}");
        let names: Vec<_> = request.messages.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Subscribe", "ping", "Price"], "client-sent messages first");
        let body = request.body.as_ref().unwrap();
        assert_eq!(body.kind, BodyKind::Json);
        let json: Value = serde_json::from_str(&body.content).unwrap();
        assert_eq!(json, serde_json::json!({ "op": "subscribe", "symbol": "ACME" }));
        assert_eq!(request.messages[1].content, "ping");
    }

    #[test]
    fn converts_asyncapi_3() {
        let import = convert(&parse_document(V3).unwrap()).unwrap();
        assert_eq!(import.collection.variables["base_url"], "ws://localhost:8080");
        assert!(import.environments.is_empty());
        let ImportItem::Request(request) = &import.items[0] else {
            panic!()
        };
        assert_eq!(request.name, "Chat room");
        assert_eq!(request.url, "{{base_url}}/rooms/{{roomId}}");
        assert_eq!(request.headers[0].value, "Bearer {{bearer}}");
        let names: Vec<_> = request.messages.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Say something", "Said"]);
        assert!(request.body.as_ref().unwrap().content.contains("hello"));
    }

    #[test]
    fn http_servers_make_get_requests() {
        let doc = serde_json::json!({
            "asyncapi": "2.6.0",
            "info": { "title": "Events" },
            "servers": { "api": { "url": "https://events.test", "protocol": "https" } },
            "channels": { "/stream": { "subscribe": { "message": { "payload": { "type": "string" } } } } }
        });
        let import = convert(&doc).unwrap();
        let ImportItem::Request(request) = &import.items[0] else {
            panic!()
        };
        assert_eq!(
            (request.method.as_str(), request.url.as_str()),
            ("GET", "{{base_url}}/stream")
        );
        assert!(request.messages.is_empty() && request.body.is_none());
    }
}
