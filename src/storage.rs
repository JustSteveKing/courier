//! Reading and writing collections on disk.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::model::{COLLECTION_FILE, CollectionFile, ENVIRONMENTS_DIR, EnvironmentFile, RequestFile, slugify};
use crate::response_cache::{CacheKey, cache_key};

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
    Folder {
        name: String,
        path: PathBuf,
        children: Vec<Item>,
    },
    Request {
        path: PathBuf,
        request: RequestFile,
    },
}

#[derive(Clone, Debug)]
pub struct Environment {
    pub path: PathBuf,
    pub file: EnvironmentFile,
}

/// A request found by [`Collection::requests`], with the names of the folders it's in.
pub struct RequestEntry<'a> {
    pub folders: Vec<&'a str>,
    pub path: &'a Path,
    pub request: &'a RequestFile,
}

impl Collection {
    /// Every request, depth first in sidebar order.
    pub fn requests(&self) -> Vec<RequestEntry<'_>> {
        fn walk<'a>(items: &'a [Item], folders: &mut Vec<&'a str>, out: &mut Vec<RequestEntry<'a>>) {
            for item in items {
                match item {
                    Item::Folder { name, children, .. } => {
                        folders.push(name);
                        walk(children, folders, out);
                        folders.pop();
                    }
                    Item::Request { path, request } => out.push(RequestEntry {
                        folders: folders.clone(),
                        path,
                        request,
                    }),
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.items, &mut Vec::new(), &mut out);
        out
    }

    pub fn find_request(&self, path: &Path) -> Option<&RequestFile> {
        self.requests()
            .into_iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.request)
    }

    pub fn first_request(&self) -> Option<PathBuf> {
        self.requests().first().map(|entry| entry.path.to_path_buf())
    }

    pub fn environment(&self, path: &Path) -> Option<&Environment> {
        self.environments.iter().find(|e| e.path == path)
    }

    /// The response-cache key for one of this collection's requests.
    pub fn response_key(&self, request: &Path) -> CacheKey {
        cache_key(self.file.id.as_deref(), &self.root, request)
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
    Ok(Collection {
        root: root.to_path_buf(),
        file,
        items,
        environments,
        errors,
    })
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
            folders.push(Item::Folder {
                name: file_name,
                path,
                children,
            });
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
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_yaml(p))
        .collect()
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

/// Deletes a request file or a folder with everything in it.
pub fn delete_item(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("deleting {}", path.display()))
    } else {
        delete_file(path)
    }
}

/// A folder name as typed, checked for use as a directory inside `parent`.
fn folder_name(parent: &Path, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.starts_with('.') || name.contains(['/', '\\', '\0']) {
        anyhow::bail!("“{name}” can't be used as a folder name");
    }
    if is_collection(parent) && slugify(name) == ENVIRONMENTS_DIR {
        anyhow::bail!("“{name}” is reserved for environments");
    }
    Ok(name.to_string())
}

/// Creates a folder named `name` (as typed) inside `parent`.
pub fn create_folder(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = parent.join(folder_name(parent, name)?);
    if path.exists() {
        anyhow::bail!("{} already exists", path.display());
    }
    fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

pub fn rename_folder(path: &Path, name: &str) -> Result<PathBuf> {
    let parent = path.parent().context("a folder has a parent")?;
    let target = parent.join(folder_name(parent, name)?);
    if target == path {
        return Ok(target);
    }
    if target.exists() {
        anyhow::bail!("{} already exists", target.display());
    }
    fs::rename(path, &target).with_context(|| format!("renaming {}", path.display()))?;
    Ok(target)
}

/// Renames a request, moving its file to match the new name. Returns the new path.
pub fn rename_request(path: &Path, name: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("a request needs a name");
    }
    let mut request: RequestFile = read_yaml(path)?;
    request.name = name.to_string();
    let dir = path.parent().context("a request is in a folder")?;
    let same_slug = path
        .file_stem()
        .is_some_and(|stem| stem.to_string_lossy() == slugify(name));
    let target = if same_slug {
        path.to_path_buf()
    } else {
        unique_path(dir, name, ".yaml")
    };
    write_yaml(&target, &request)?;
    if target != path {
        delete_file(path)?;
    }
    Ok(target)
}

/// Moves a request file into `dir` (another folder or collection), renaming it if the name is
/// taken there. Returns the new path.
pub fn move_request(path: &Path, dir: &Path) -> Result<PathBuf> {
    let request: RequestFile = read_yaml(path)?;
    let target = unique_path(dir, &request.name, ".yaml");
    if fs::rename(path, &target).is_err() {
        // Across filesystems (e.g. from the data dir into a project on another disk).
        write_yaml(&target, &request)?;
        delete_file(path)?;
    }
    Ok(target)
}

/// Copies a request next to itself as “<name> copy”, just after it in the list.
pub fn duplicate_request(path: &Path, copy_name: &str) -> Result<PathBuf> {
    let mut request: RequestFile = read_yaml(path)?;
    request.name = copy_name.to_string();
    let dir = path.parent().context("a request is in a folder")?;
    create_request(dir, &request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_renames_duplicates_and_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::project::init_with(tmp.path(), &CollectionFile::new("API")).unwrap();

        let folder = create_folder(&root, " Pet store ").unwrap();
        assert_eq!(folder, root.join("Pet store"), "kept as typed");
        assert!(create_folder(&root, "Pet store").is_err(), "already exists");
        for bad in ["", "..", ".hidden", "a/b", "environments"] {
            assert!(create_folder(&root, bad).is_err(), "rejects {bad:?}");
        }
        assert!(
            create_folder(&folder, "environments").is_ok(),
            "only reserved at the root"
        );

        let request = create_request(&folder, &RequestFile::new("List pets")).unwrap();
        let renamed = rename_request(&request, "Search pets").unwrap();
        assert_eq!(renamed, folder.join("search-pets.yaml"));
        assert!(!request.exists());
        assert_eq!(read_yaml::<RequestFile>(&renamed).unwrap().name, "Search pets");
        let same = rename_request(&renamed, "search pets").unwrap();
        assert_eq!(same, renamed, "same slug keeps the file");
        assert!(rename_request(&renamed, "  ").is_err());

        let copy = duplicate_request(&renamed, "search pets copy").unwrap();
        assert_eq!(copy, folder.join("search-pets-copy.yaml"));

        let moved = rename_folder(&folder, "Pets").unwrap();
        assert!(moved.join("search-pets.yaml").exists());
        assert!(rename_folder(&moved, "environments").is_err());

        let other = create_folder(&root, "Other").unwrap();
        let relocated = move_request(&moved.join("search-pets-copy.yaml"), &other).unwrap();
        assert_eq!(relocated, other.join("search-pets-copy.yaml"));
        delete_item(&relocated).unwrap();
        delete_item(&moved).unwrap();
        assert!(!moved.exists());
    }

    #[test]
    fn creates_and_loads_a_collection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::project::init_with(tmp.path(), &CollectionFile::new("My API")).unwrap();
        assert_eq!(root, tmp.path().join(".courier"));

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
