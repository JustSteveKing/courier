//! Helpers shared by the OpenAPI and AsyncAPI importers: reading JSON or YAML, following
//! local `$ref`s, and making example values from JSON Schemas.

use anyhow::{Context as _, Result};
use serde_json::{Map, Value, json};

/// How deep example generation follows nested schemas.
const MAX_DEPTH: usize = 8;

/// Parses a JSON or YAML document.
pub fn parse_document(text: &str) -> Result<Value> {
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text).context("The file is not valid JSON");
    }
    serde_norway::from_str(text).context("The file is not valid JSON or YAML")
}

/// Resolves local `$ref`s (`#/components/...`) within one document.
pub struct Refs<'a> {
    root: &'a Value,
}

impl<'a> Refs<'a> {
    pub fn new(root: &'a Value) -> Self {
        Self { root }
    }

    /// The target of a `$ref` pointer, if it's local and exists.
    pub fn target(&self, reference: &str) -> Option<&'a Value> {
        let pointer = reference.strip_prefix('#')?;
        let pointer = pointer
            .split('/')
            .map(|part| part.replace("~1", "/").replace("~0", "~"))
            .collect::<Vec<_>>()
            .join("/");
        self.root.pointer(&pointer)
    }

    /// Follows `$ref` chains until a real value (or a dangling or external reference).
    pub fn resolve(&self, value: &'a Value) -> &'a Value {
        let mut current = value;
        for _ in 0..32 {
            match current.get("$ref").and_then(Value::as_str).and_then(|r| self.target(r)) {
                Some(next) => current = next,
                None => break,
            }
        }
        current
    }

    /// External references (to other files or URLs) can't be followed; import warns about them.
    pub fn is_external(value: &Value) -> bool {
        value
            .get("$ref")
            .and_then(Value::as_str)
            .is_some_and(|r| !r.starts_with('#'))
    }

    /// An example value for `schema`: its `example`, `default` or first `enum` value if it has
    /// one, otherwise one built from its properties and types.
    pub fn example(&self, schema: &Value) -> Value {
        self.example_at(schema, 0, &mut Vec::new())
    }

    fn example_at(&self, schema: &'a Value, depth: usize, visiting: &mut Vec<String>) -> Value {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            if depth > MAX_DEPTH || visiting.iter().any(|v| v == reference) {
                return Value::Null;
            }
            let Some(target) = self.target(reference) else {
                return Value::Null;
            };
            visiting.push(reference.to_string());
            let value = self.example_at(target, depth + 1, visiting);
            visiting.pop();
            return value;
        }
        if depth > MAX_DEPTH {
            return Value::Null;
        }
        for key in ["example", "default", "const"] {
            if let Some(value) = schema.get(key) {
                return value.clone();
            }
        }
        if let Some(first) = schema.get("examples").and_then(Value::as_array).and_then(|e| e.first()) {
            return first.clone();
        }
        if let Some(first) = schema.get("enum").and_then(Value::as_array).and_then(|e| e.first()) {
            return first.clone();
        }
        if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
            let mut merged = Map::new();
            for part in parts {
                if let Value::Object(object) = self.example_at(part, depth + 1, visiting) {
                    merged.extend(object);
                }
            }
            return Value::Object(merged);
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(first) = schema.get(key).and_then(Value::as_array).and_then(|o| o.first()) {
                return self.example_at(first, depth + 1, visiting);
            }
        }

        let ty = match schema.get("type") {
            Some(Value::String(ty)) => Some(ty.as_str()),
            // JSON Schema / OpenAPI 3.1 type lists: the first that isn't "null".
            Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).find(|t| *t != "null"),
            _ => None,
        };
        match ty {
            Some("object") | None if schema.get("properties").is_some() => {
                let mut object = Map::new();
                if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                    for (name, property) in properties {
                        let property = self.resolve(property);
                        if property.get("readOnly").and_then(Value::as_bool) == Some(true) {
                            continue;
                        }
                        object.insert(name.clone(), self.example_at(property, depth + 1, visiting));
                    }
                }
                Value::Object(object)
            }
            Some("object") => match schema.get("additionalProperties") {
                Some(extra @ Value::Object(_)) => json!({ "key": self.example_at(extra, depth + 1, visiting) }),
                _ => json!({}),
            },
            Some("array") => match schema.get("items") {
                Some(items) => json!([self.example_at(items, depth + 1, visiting)]),
                None => json!([]),
            },
            Some("string") => Value::String(
                match schema.get("format").and_then(Value::as_str) {
                    Some("date-time") => "2024-01-01T00:00:00Z",
                    Some("date") => "2024-01-01",
                    Some("time") => "12:00:00",
                    Some("email") => "user@example.com",
                    Some("uuid") => "3fa85f64-5717-4562-b3fc-2c963f66afa6",
                    Some("uri" | "url") => "https://example.com",
                    Some("ipv4") => "192.0.2.1",
                    Some("byte") => "c3RyaW5n",
                    _ => "string",
                }
                .into(),
            ),
            Some("integer") => json!(0),
            Some("number") => json!(0.0),
            Some("boolean") => json!(true),
            _ => Value::Null,
        }
    }
}

/// A value as it would appear in a URL or header: strings as they are, others as JSON.
pub fn plain(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `/pets/{petId}` → `/pets/{{petId}}`, returning the parameter names.
pub fn templated_path(path: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(path.len() + 8);
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        let name = &rest[start + 1..start + len];
        out.push_str(&rest[..start]);
        if name.is_empty() || name.starts_with('{') {
            out.push_str(&rest[start..=start + len]);
        } else {
            out.push_str(&crate::model::placeholder(name));
            names.push(name.to_string());
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    (out, names)
}

/// Fills `{variable}`s in a server URL with their defaults.
pub fn fill_server_variables(url: &str, variables: Option<&Value>) -> String {
    let mut url = url.to_string();
    if let Some(variables) = variables.and_then(Value::as_object) {
        for (name, variable) in variables {
            let default = variable
                .get("default")
                .or_else(|| variable.get("enum").and_then(|e| e.get(0)))
                .map(plain)
                .unwrap_or_default();
            url = url.replace(&format!("{{{name}}}"), &default);
        }
    }
    url.trim_end_matches('/').to_string()
}

/// A secret name for a security scheme: `bearerAuth` → `bearer_auth`.
pub fn secret_name(scheme: &str) -> String {
    let mut name = String::new();
    for (i, c) in scheme.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 && !name.ends_with('_') {
            name.push('_');
        }
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.ends_with('_') && !name.is_empty() {
            name.push('_');
        }
    }
    let name = name.trim_matches('_').to_string();
    if name.is_empty() { "api_key".into() } else { name }
}

/// Short names for requests and environments.
pub fn truncate(text: &str, max: usize) -> String {
    let text = text.trim().replace(['\n', '\r'], " ");
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_json_and_yaml() {
        assert_eq!(parse_document("{\"a\": 1}").unwrap()["a"], 1);
        assert_eq!(parse_document("a: 1\nb: [x]").unwrap()["b"][0], "x");
        assert!(parse_document("{nope").is_err());
    }

    #[test]
    fn builds_examples_from_schemas() {
        let doc = json!({
            "components": { "schemas": {
                "Pet": {
                    "type": "object",
                    "required": ["name"],
                    "properties": {
                        "id": { "type": "integer", "readOnly": true },
                        "name": { "type": "string", "example": "Rex" },
                        "tags": { "type": "array", "items": { "$ref": "#/components/schemas/Tag" } },
                        "status": { "type": "string", "enum": ["available", "sold"] },
                        "owner": { "$ref": "#/components/schemas/Owner" },
                        "born": { "type": ["string", "null"], "format": "date" }
                    }
                },
                "Tag": { "allOf": [
                    { "properties": { "label": { "type": "string" } } },
                    { "properties": { "weight": { "type": "number" } } }
                ] },
                "Owner": { "properties": { "pets": { "type": "array", "items": { "$ref": "#/components/schemas/Pet" } } } }
            } }
        });
        let refs = Refs::new(&doc);
        let pet = refs.example(&json!({ "$ref": "#/components/schemas/Pet" }));
        assert_eq!(pet["name"], "Rex");
        assert!(pet.get("id").is_none(), "read-only properties aren't sent");
        assert_eq!(pet["tags"], json!([{ "label": "string", "weight": 0.0 }]));
        assert_eq!(pet["status"], "available");
        assert_eq!(pet["born"], "2024-01-01");
        assert_eq!(pet["owner"]["pets"], json!([null]), "cycles stop");
    }

    #[test]
    fn templates_paths_and_names_secrets() {
        assert_eq!(
            templated_path("/users/{userId}/pets/{petId}"),
            (
                "/users/{{userId}}/pets/{{petId}}".into(),
                vec!["userId".into(), "petId".into()]
            )
        );
        assert_eq!(
            fill_server_variables(
                "https://{region}.example.com/v1/",
                Some(&json!({ "region": { "default": "eu" } }))
            ),
            "https://eu.example.com/v1"
        );
        assert_eq!(secret_name("bearerAuth"), "bearer_auth");
        assert_eq!(secret_name("api-key"), "api_key");
        assert_eq!(secret_name("OAuth2"), "o_auth2");
        assert_eq!(truncate("a long request name", 6), "a long…");
    }
}
