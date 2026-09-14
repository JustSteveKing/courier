//! Reading and writing collections on disk.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::model::{
    COLLECTION_FILE, CollectionFile, ENVIRONMENTS_DIR, EnvironmentFile, RequestFile, slugify,
};

#[derive(Clone, Debug)]
pub struct Collection {
    pub root: PathBuf,
    pub file: CollectionFile,
    pub items: Vec<Item>,
    pub environments: Vec<Environment>,
    /// Files that exist but could not be parsed. Shown to the user, never fatal.
    pub errors: Vec<(PathBuf, String)>,
}

#[derive(Clone, Debug)]
pub enum Item {
    Folder { name: String, path: PathBuf, children: Vec<Item> },
    Request { path: PathBuf, request: RequestFile },
}

#[derive(Clone, Debug)]
pub struct Environment {
    pub path: PathBuf,
    pub file: EnvironmentFile,
}

impl Collection {
    pub fn find_request(&self, path: &Path) -> Option<&RequestFile> {
        fn find<'a>(items: &'a [Item], path: &Path) -> Option<&'a RequestFile> {
            items.iter().find_map(|item| match item {
                Item::Request { path: p, request } if p == path => Some(request),
                Item::Folder { children, .. } => find(children, path),
                _ => None,
            })
        }
        find(&self.items, path)
    }
}

pub fn read_yaml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_norway::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Writes via a temporary file and rename, so a crash never leaves half a file behind.
pub fn write_yaml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let yaml = serde_norway::to_string(value)?;
    let tmp = path.with_extension("yaml.tmp");
    fs::write(&tmp, yaml).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn is_collection(dir: &Path) -> bool {
    dir.join(COLLECTION_FILE).is_file()
}

pub fn load_collection(root: &Path) -> Result<Collection> {
    let file: CollectionFile = read_yaml(&root.join(COLLECTION_FILE))?;
    let mut errors = Vec::new();

    let mut environments = Vec::new();
    for path in yaml_files(&root.join(ENVIRONMENTS_DIR)) {
        match read_yaml::<EnvironmentFile>(&path) {
            Ok(file) => environments.push(Environment { path, file }),
            Err(e) => errors.push((path, format!("{e:#}"))),
        }
    }
    environments.sort_by_key(|e| e.file.name.to_lowercase());

    let items = load_items(root, true, &mut errors);
    Ok(Collection { root: root.to_path_buf(), file, items, environments, errors })
}

fn load_items(dir: &Path, is_root: bool, errors: &mut Vec<(PathBuf, String)>) -> Vec<Item> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    let mut requests = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if is_root && file_name == ENVIRONMENTS_DIR {
                continue;
            }
            let children = load_items(&path, false, errors);
            folders.push(Item::Folder { name: file_name, path, children });
        } else if is_yaml(&path) && !(is_root && file_name == COLLECTION_FILE) {
            match read_yaml::<RequestFile>(&path) {
                Ok(request) => requests.push(Item::Request { path, request }),
                Err(e) => errors.push((path, format!("{e:#}"))),
            }
        }
    }
    folders.sort_by_key(|item| match item {
        Item::Folder { name, .. } => name.to_lowercase(),
        Item::Request { .. } => unreachable!(),
    });
    requests.sort_by(|a, b| match (a, b) {
        (Item::Request { path: pa, request: ra }, Item::Request { path: pb, request: rb }) => {
            // Explicit order first, then by file name.
            (ra.order.is_none(), ra.order, pa).cmp(&(rb.order.is_none(), rb.order, pb))
        }
        _ => unreachable!(),
    });
    folders.extend(requests);
    folders
}

fn is_yaml(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some("yaml" | "yml"))
}

fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_yaml(p)).collect()
}

/// Returns `dir/<slug>.yaml`, or `dir/<slug>-2.yaml` and so on if taken.
pub fn unique_path(dir: &Path, name: &str, extension: &str) -> PathBuf {
    let slug = slugify(name);
    let mut path = dir.join(format!("{slug}{extension}"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{slug}-{n}{extension}"));
        n += 1;
    }
    path
}

/// Creates `parent/<slug>/collection.yaml` and returns the new collection's root.
pub fn create_collection(parent: &Path, file: &CollectionFile) -> Result<PathBuf> {
    let root = unique_path(parent, &file.name, "");
    write_yaml(&root.join(COLLECTION_FILE), file)?;
    Ok(root)
}

pub fn create_request(dir: &Path, request: &RequestFile) -> Result<PathBuf> {
    let path = unique_path(dir, &request.name, ".yaml");
    write_yaml(&path, request)?;
    Ok(path)
}

pub fn create_environment(collection_root: &Path, env: &EnvironmentFile) -> Result<PathBuf> {
    let path = unique_path(&collection_root.join(ENVIRONMENTS_DIR), &env.name, ".yaml");
    write_yaml(&path, env)?;
    Ok(path)
}

pub fn save_collection_file(collection_root: &Path, file: &CollectionFile) -> Result<()> {
    write_yaml(&collection_root.join(COLLECTION_FILE), file)
}

pub fn delete_file(path: &Path) -> Result<()> {
    fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_loads_a_collection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = create_collection(tmp.path(), &CollectionFile::new("My API")).unwrap();
        assert_eq!(root, tmp.path().join("my-api"));

        let mut env = EnvironmentFile::new("Local");
        env.variables.insert("base_url".into(), "http://localhost:8080".into());
        create_environment(&root, &env).unwrap();

        let mut second = RequestFile::new("B second");
        second.order = Some(2);
        let mut first = RequestFile::new("Z first");
        first.order = Some(1);
        create_request(&root, &second).unwrap();
        create_request(&root, &first).unwrap();
        create_request(&root, &RequestFile::new("A unordered")).unwrap();
        create_request(&root.join("admin"), &RequestFile::new("Nested")).unwrap();
        fs::write(root.join("broken.yaml"), "name: [unclosed").unwrap();

        let collection = load_collection(&root).unwrap();
        assert_eq!(collection.file.name, "My API");
        assert_eq!(collection.environments.len(), 1);
        assert_eq!(collection.errors.len(), 1);

        let names: Vec<_> = collection
            .items
            .iter()
            .map(|item| match item {
                Item::Folder { name, .. } => name.clone(),
                Item::Request { request, .. } => request.name.clone(),
            })
            .collect();
        assert_eq!(names, ["admin", "Z first", "B second", "A unordered"]);
        assert!(collection.find_request(&root.join("admin/nested.yaml")).is_some());
    }

    #[test]
    fn unique_path_avoids_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        create_request(tmp.path(), &RequestFile::new("Ping")).unwrap();
        assert_eq!(unique_path(tmp.path(), "Ping", ".yaml"), tmp.path().join("ping-2.yaml"));
    }
}
