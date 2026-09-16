//! Importers that turn other tools' formats into our model.

pub mod asyncapi;
pub mod curl;
pub mod har;
pub mod insomnia;
pub mod openapi;
pub mod postman;
mod spec;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::model::{CollectionFile, ENVIRONMENTS_DIR, EnvironmentFile, RequestFile, Variables, slugify};
use crate::storage::{create_environment, create_request, is_collection, unique_path};

/// A collection converted from another format, ready to write.
#[derive(Debug)]
pub struct CollectionImport {
    /// Secret names are listed in `collection.secrets`; their values are only in `secrets`.
    pub collection: CollectionFile,
    pub items: Vec<ImportItem>,
    /// Environments to create alongside, e.g. one per server of an API spec.
    pub environments: Vec<EnvironmentFile>,
    /// Secret name -> value. Must go to the secret store, never to YAML.
    pub secrets: Variables,
    /// Human-readable, one per dropped or converted feature.
    pub warnings: Vec<String>,
}

// Trees of requests read from or written to disk once; boxing wouldn't buy anything.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ImportItem {
    Folder { name: String, children: Vec<ImportItem> },
    Request(RequestFile),
}

impl ImportItem {
    /// Visits every request in `items`, depth first.
    pub fn for_each_request_mut(items: &mut [ImportItem], f: &mut impl FnMut(&mut RequestFile)) {
        for item in items {
            match item {
                ImportItem::Folder { children, .. } => Self::for_each_request_mut(children, f),
                ImportItem::Request(request) => f(request),
            }
        }
    }

    /// How many requests `items` holds.
    pub fn count_requests(items: &[ImportItem]) -> usize {
        items
            .iter()
            .map(|item| match item {
                ImportItem::Folder { children, .. } => Self::count_requests(children),
                ImportItem::Request(_) => 1,
            })
            .sum()
    }
}

/// The formats a collection can be created from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportFormat {
    Postman,
    OpenApi,
    AsyncApi,
    /// A recording from browser dev tools or a proxy.
    Har,
    Insomnia,
}

impl ImportFormat {
    /// Recognises a document by its telltale top-level keys.
    pub fn detect(document: &serde_json::Value) -> Option<Self> {
        if document.get("openapi").is_some() || document.get("swagger").is_some() {
            Some(Self::OpenApi)
        } else if document.get("asyncapi").is_some() {
            Some(Self::AsyncApi)
        } else if document.pointer("/log/entries").is_some() {
            Some(Self::Har)
        } else if insomnia::is_insomnia(document) {
            Some(Self::Insomnia)
        } else if document.pointer("/info/name").is_some() && document.get("item").is_some() {
            Some(Self::Postman)
        } else {
            None
        }
    }
}

/// Parses a Postman collection, OpenAPI (or Swagger 2.0) or AsyncAPI document, JSON or YAML,
/// whichever it turns out to be.
pub fn parse_collection_file(text: &str) -> Result<(ImportFormat, CollectionImport)> {
    let document = spec::parse_document(text)?;
    let import = match ImportFormat::detect(&document) {
        Some(ImportFormat::Postman) => (ImportFormat::Postman, postman::parse_collection(text)?),
        Some(ImportFormat::OpenApi) => (ImportFormat::OpenApi, keep_order(openapi::convert(&document)?)),
        Some(ImportFormat::AsyncApi) => (ImportFormat::AsyncApi, keep_order(asyncapi::convert(&document)?)),
        Some(ImportFormat::Har) => (
            ImportFormat::Har,
            keep_order(har::convert(&document, &har_name(&document))?),
        ),
        Some(ImportFormat::Insomnia) => (ImportFormat::Insomnia, keep_order(insomnia::convert(&document)?)),
        None => bail!("Not a Postman collection, OpenAPI, AsyncAPI, HAR or Insomnia document"),
    };
    Ok(import)
}

/// A HAR has no name of its own; the tool that recorded it is the best there is.
fn har_name(document: &serde_json::Value) -> String {
    document
        .pointer("/log/creator/name")
        .and_then(serde_json::Value::as_str)
        .map(|creator| format!("{creator} recording"))
        .unwrap_or_else(|| "Recording".to_string())
}

/// Numbers requests in document order, so the sidebar lists them as the spec does.
fn keep_order(mut import: CollectionImport) -> CollectionImport {
    fn number(items: &mut [ImportItem]) {
        for (index, item) in items.iter_mut().enumerate() {
            match item {
                ImportItem::Folder { children, .. } => number(children),
                ImportItem::Request(request) => request.order = Some(index as i64),
            }
        }
    }
    number(&mut import.items);
    import
}

/// Creates the `.courier` collection for `project` from an import; returns its root. Only
/// secret names are written; the caller stores `import.secrets` values in the secret store.
pub fn write_project_collection(project: &Path, import: &CollectionImport) -> Result<PathBuf> {
    let root = crate::project::init_with(project, &import.collection)?;
    write_items(&root, &import.items)?;
    for environment in &import.environments {
        create_environment(&root, environment)?;
    }
    Ok(root)
}

/// Writes items into an existing collection directory or folder.
pub fn write_items(dir: &Path, items: &[ImportItem]) -> Result<()> {
    for item in items {
        match item {
            ImportItem::Folder { name, children } => {
                // A root folder called "environments" would be mistaken for environment files.
                let name = if is_collection(dir) && slugify(name) == ENVIRONMENTS_DIR {
                    format!("{name} folder")
                } else {
                    name.clone()
                };
                let folder = unique_path(dir, &name, "");
                fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
                write_items(&folder, children)?;
            }
            ImportItem::Request(request) => {
                create_request(dir, request)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `COURIER_IMPORT_FILES=a.yaml:b.json cargo test real_files -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_files() {
        let files = std::env::var("COURIER_IMPORT_FILES").unwrap_or_default();
        for file in files.split(':').filter(|f| !f.is_empty()) {
            let text = fs::read_to_string(file).unwrap();
            match parse_collection_file(&text) {
                Ok((format, import)) => {
                    println!(
                        "{file}: {format:?} \"{}\" requests={} folders={} environments={} secrets={:?} variables={:?}",
                        import.collection.name,
                        ImportItem::count_requests(&import.items),
                        import
                            .items
                            .iter()
                            .filter(|i| matches!(i, ImportItem::Folder { .. }))
                            .count(),
                        import.environments.len(),
                        import.collection.secrets,
                        import.collection.variables.keys().collect::<Vec<_>>(),
                    );
                    for warning in &import.warnings {
                        println!("  warning: {warning}");
                    }
                    if let Ok(out) = std::env::var("COURIER_IMPORT_OUT") {
                        let project = Path::new(&out).join(slugify(&import.collection.name));
                        fs::create_dir_all(&project).unwrap();
                        write_project_collection(&project, &import).unwrap();
                    }
                }
                Err(e) => println!("{file}: ERROR {e:#}"),
            }
        }
    }
}
