//! GraphQL schemas: the introspection query, the schema model built from its result, and
//! the on-disk cache that keeps schemas across restarts. Editor assistance (completions and
//! hover docs) lives in [`assist`].

pub mod assist;

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};

use crate::response_cache::{TIDY_MAX_AGE, fnv1a};

/// The standard introspection query (without directives, which the editor doesn't use).
pub const INTROSPECTION_QUERY: &str = "query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types { ...FullType }
  }
}
fragment FullType on __Type {
  kind name description
  fields(includeDeprecated: true) {
    name description
    args { ...InputValue }
    type { ...TypeRef }
    isDeprecated deprecationReason
  }
  inputFields { ...InputValue }
  interfaces { ...TypeRef }
  enumValues(includeDeprecated: true) { name description isDeprecated deprecationReason }
  possibleTypes { ...TypeRef }
}
fragment InputValue on __InputValue { name description type { ...TypeRef } defaultValue }
fragment TypeRef on __Type {
  kind name
  ofType { kind name ofType { kind name ofType { kind name ofType { kind name
    ofType { kind name ofType { kind name ofType { kind name } } } } } } }
}";
pub const INTROSPECTION_OPERATION: &str = "IntrospectionQuery";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TypeKind {
    Scalar,
    Object,
    Interface,
    Union,
    Enum,
    InputObject,
    List,
    NonNull,
}

impl TypeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::Object => "type",
            Self::Interface => "interface",
            Self::Union => "union",
            Self::Enum => "enum",
            Self::InputObject => "input",
            Self::List => "list",
            Self::NonNull => "non-null",
        }
    }

    /// Types that can be used for variables and arguments.
    pub fn is_input(self) -> bool {
        matches!(self, Self::Scalar | Self::Enum | Self::InputObject)
    }
}

/// A reference to a type, possibly wrapped in lists and non-null markers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeRef {
    pub kind: TypeKind,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of_type: Option<Box<TypeRef>>,
}

impl TypeRef {
    /// The named type inside any wrappers.
    pub fn named(&self) -> &str {
        match (&self.name, &self.of_type) {
            (Some(name), _) => name,
            (None, Some(inner)) => inner.named(),
            (None, None) => "",
        }
    }
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.kind, &self.of_type) {
            (TypeKind::List, Some(inner)) => write!(f, "[{inner}]"),
            (TypeKind::NonNull, Some(inner)) => write!(f, "{inner}!"),
            _ => f.write_str(self.named()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputValue {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub ty: TypeRef,
    #[serde(default)]
    pub default_value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub args: Vec<InputValue>,
    #[serde(rename = "type")]
    pub ty: TypeRef,
    #[serde(default)]
    pub is_deprecated: bool,
    #[serde(default)]
    pub deprecation_reason: Option<String>,
}

impl Field {
    /// `name(arg: Type, …): Type`
    pub fn signature(&self) -> String {
        let args = self
            .args
            .iter()
            .map(|a| format!("{}: {}", a.name, a.ty))
            .collect::<Vec<_>>()
            .join(", ");
        if args.is_empty() {
            format!("{}: {}", self.name, self.ty)
        } else {
            format!("{}({args}): {}", self.name, self.ty)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnumValue {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_deprecated: bool,
    #[serde(default)]
    pub deprecation_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeDef {
    pub kind: TypeKind,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub fields: Vec<Field>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub input_fields: Vec<InputValue>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub interfaces: Vec<TypeRef>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub enum_values: Vec<EnumValue>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub possible_types: Vec<TypeRef>,
}

impl TypeDef {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Built-in introspection types such as `__Type`.
    pub fn is_introspection(&self) -> bool {
        self.name.starts_with("__")
    }
}

/// Introspection returns `null` rather than `[]` for lists that don't apply to a kind.
fn null_as_empty<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Query,
    Mutation,
    Subscription,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    pub query_type: Option<String>,
    pub mutation_type: Option<String>,
    pub subscription_type: Option<String>,
    pub types: IndexMap<String, TypeDef>,
}

impl Schema {
    /// Builds a schema from an introspection response body. Accepts the usual
    /// `{"data": {"__schema": …}}` as well as a bare `{"__schema": …}` (a saved schema file).
    pub fn from_introspection(body: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Named {
            name: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            query_type: Option<Named>,
            mutation_type: Option<Named>,
            subscription_type: Option<Named>,
            types: Vec<TypeDef>,
        }

        let json: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
        let raw = json
            .get("data")
            .and_then(|data| data.get("__schema"))
            .or_else(|| json.get("__schema"))
            .filter(|schema| !schema.is_null());
        let Some(raw) = raw else {
            // A GraphQL error response (e.g. introspection disabled) explains itself.
            let errors: Vec<&str> = json
                .get("errors")
                .and_then(|e| e.as_array())
                .map(|errors| errors.iter().filter_map(|e| e.get("message")?.as_str()).collect())
                .unwrap_or_default();
            return Err(if errors.is_empty() {
                "the response has no __schema".to_string()
            } else {
                errors.join("; ")
            });
        };
        let raw = Raw::deserialize(raw).map_err(|e| format!("unexpected introspection result: {e}"))?;
        Ok(Self {
            query_type: raw.query_type.map(|t| t.name),
            mutation_type: raw.mutation_type.map(|t| t.name),
            subscription_type: raw.subscription_type.map(|t| t.name),
            types: raw.types.into_iter().map(|t| (t.name.clone(), t)).collect(),
        })
    }

    pub fn get(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(name)
    }

    pub fn root_name(&self, kind: OperationKind) -> Option<&str> {
        match kind {
            OperationKind::Query => self.query_type.as_deref(),
            OperationKind::Mutation => self.mutation_type.as_deref(),
            OperationKind::Subscription => self.subscription_type.as_deref(),
        }
    }

    pub fn root(&self, kind: OperationKind) -> Option<&TypeDef> {
        self.root_name(kind).and_then(|name| self.get(name))
    }

    /// Types other than the built-in introspection ones, in schema order.
    pub fn user_types(&self) -> impl Iterator<Item = &TypeDef> {
        self.types.values().filter(|t| !t.is_introspection())
    }
}

/// A schema saved with when it was fetched.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedSchema {
    /// Seconds since the Unix epoch.
    pub fetched_at: u64,
    pub schema: Schema,
}

/// Schemas saved in the cache directory, one file per endpoint.
#[derive(Clone, Debug)]
pub struct SchemaCache {
    dir: PathBuf,
}

impl SchemaCache {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            dir: cache_dir.join("graphql-schemas"),
        }
    }

    /// The cache key of an endpoint: the collection (or request file, outside a collection)
    /// and the URL as written, with `{{variables}}` unresolved, so no secret value is hashed.
    pub fn key(owner: &str, url_template: &str) -> String {
        format!("{:016x}", fnv1a(&format!("{owner}\n{}", url_template.trim())))
    }

    fn file(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn save(&self, key: &str, schema: &CachedSchema) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.file(key);
        let tmp = path.with_extension("json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec(schema)?)?;
        file.sync_all()?;
        fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Loading marks the file as used, so schemas in use survive [`SchemaCache::tidy`].
    pub fn load(&self, key: &str) -> Option<CachedSchema> {
        let path = self.file(key);
        let bytes = fs::read(&path).ok()?;
        let cached = serde_json::from_slice(&bytes)
            .inspect_err(|e| eprintln!("ignoring unreadable cached schema {key}: {e}"))
            .ok()?;
        if let Ok(file) = OpenOptions::new().append(true).open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }
        Some(cached)
    }

    /// Deletes schemas not used for [`TIDY_MAX_AGE`]. Returns how many were removed.
    pub fn tidy(&self, now: SystemTime) -> Result<usize> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Ok(0);
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let age = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .unwrap_or_default();
            if age > TIDY_MAX_AGE {
                fs::remove_file(entry.path()).with_context(|| format!("removing {}", entry.path().display()))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// A small introspection result for tests here and in the app.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use super::Schema;

    /// A pet store with an interface, a union, an enum and an input type.
    pub const PETSTORE: &str = include_str!("graphql/petstore.json");

    pub fn petstore() -> Schema {
        Schema::from_introspection(PETSTORE).unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use super::*;

    pub use super::fixtures::{PETSTORE, petstore};

    #[test]
    fn parses_introspection() {
        let schema = petstore();
        assert_eq!(schema.query_type.as_deref(), Some("Query"));
        assert_eq!(schema.mutation_type.as_deref(), Some("Mutation"));
        assert_eq!(schema.subscription_type, None);

        let pets = schema.root(OperationKind::Query).unwrap().field("pets").unwrap();
        assert_eq!(pets.ty.to_string(), "[Pet!]!");
        assert_eq!(pets.ty.named(), "Pet");
        assert_eq!(pets.signature(), "pets(first: Int, species: Species): [Pet!]!");
        assert_eq!(pets.description.as_deref(), Some("Pets in the store, newest first."));

        let species = schema.get("Species").unwrap();
        assert_eq!(species.kind, TypeKind::Enum);
        assert_eq!(species.enum_values.len(), 3);
        assert!(
            schema.get("Pet").unwrap().input_fields.is_empty(),
            "null lists become empty"
        );
        assert!(schema.user_types().all(|t| !t.name.starts_with("__")));
    }

    #[test]
    fn reports_graphql_errors() {
        let error = Schema::from_introspection(r#"{"errors": [{"message": "Introspection is disabled"}]}"#);
        assert_eq!(error.unwrap_err(), "Introspection is disabled");
        assert!(Schema::from_introspection("<html>").is_err());
        let bare = format!(
            r#"{{"__schema": {}}}"#,
            serde_json::from_str::<serde_json::Value>(PETSTORE).unwrap()["data"]["__schema"]
        );
        assert_eq!(Schema::from_introspection(&bare).unwrap(), petstore());
    }

    #[test]
    fn cache_round_trips_and_tidies() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = SchemaCache::new(tmp.path());
        let key = SchemaCache::key("collection-1", " {{base_url}}/graphql ");
        assert_eq!(key, SchemaCache::key("collection-1", "{{base_url}}/graphql"));
        assert_ne!(key, SchemaCache::key("collection-2", "{{base_url}}/graphql"));
        assert!(cache.load(&key).is_none());

        let cached = CachedSchema {
            fetched_at: 1_700_000_000,
            schema: petstore(),
        };
        cache.save(&key, &cached).unwrap();
        let loaded = cache.load(&key).unwrap();
        assert_eq!(loaded.fetched_at, cached.fetched_at);
        assert_eq!(loaded.schema, cached.schema);

        assert_eq!(cache.tidy(SystemTime::now()).unwrap(), 0);
        let later = SystemTime::now() + TIDY_MAX_AGE + Duration::from_secs(60);
        assert_eq!(cache.tidy(later).unwrap(), 1);
        assert!(cache.load(&key).is_none());
    }
}
