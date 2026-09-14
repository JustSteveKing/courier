//! Projects: a collection lives in a `.courier/` directory inside the project it tests,
//! so requests are committed alongside the code.
//!
//! ```text
//! ~/Work/einvoicing/
//!   src/
//!   .courier/
//!     collection.yaml
//!     environments/local.yaml
//!     invoices/create.yaml
//! ```
//!
//! Per-machine data never goes here: settings are in `$XDG_CONFIG_HOME/courier`, open
//! projects in `$XDG_STATE_HOME/courier`, saved responses in `$XDG_CACHE_HOME/courier`,
//! and secret values in the desktop keyring.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::model::CollectionFile;
use crate::storage;

pub const DOT_DIR: &str = ".courier";

/// The collection directory for a project folder (which may not exist yet).
pub fn collection_dir(project: &Path) -> PathBuf {
    project.join(DOT_DIR)
}

/// The project folder that holds a collection directory.
pub fn project_dir(collection_root: &Path) -> &Path {
    collection_root.parent().unwrap_or(collection_root)
}

/// Finds the collection for `start`, looking in `start` and then its ancestors the way
/// git finds a repository. `start` may also be the `.courier` directory itself.
pub fn find(start: &Path) -> Option<PathBuf> {
    if start.file_name().is_some_and(|n| n == DOT_DIR) && storage::is_collection(start) {
        return Some(start.to_path_buf());
    }
    start
        .ancestors()
        .map(collection_dir)
        .find(|dir| storage::is_collection(dir))
}

/// Creates `project/.courier/collection.yaml`, named after the project folder.
pub fn init(project: &Path) -> Result<PathBuf> {
    let name = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Project".into());
    init_with(project, &CollectionFile::new(name))
}

/// Creates the collection directory for `project` from an existing collection file.
pub fn init_with(project: &Path, file: &CollectionFile) -> Result<PathBuf> {
    if !project.is_dir() {
        bail!("{} is not a folder", project.display());
    }
    let root = collection_dir(project);
    if storage::is_collection(&root) {
        bail!("{} already has a collection", project.display());
    }
    let mut file = file.clone();
    file.ensure_id();
    storage::save_collection_file(&root, &file)?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn finds_collections_like_git_finds_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("einvoicing");
        let nested = project.join("src/api");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(find(&project), None);
        let root = init(&project).unwrap();
        assert_eq!(root, project.join(".courier"));
        assert!(
            fs::read_to_string(root.join("collection.yaml"))
                .unwrap()
                .contains("name: einvoicing")
        );

        assert_eq!(find(&project).as_deref(), Some(root.as_path()));
        assert_eq!(find(&nested).as_deref(), Some(root.as_path()), "found from a subfolder");
        assert_eq!(
            find(&root).as_deref(),
            Some(root.as_path()),
            "or from the dot directory itself"
        );
        assert_eq!(project_dir(&root), project);
        assert!(init(&project).is_err(), "never overwrites an existing collection");
        assert!(init(&tmp.path().join("missing")).is_err());
    }
}
