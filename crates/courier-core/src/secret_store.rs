//! Where secret variable values live. Collections only ever contain secret *names*.
//!
//! The primary backend is the desktop keyring through the Secret Service API (GNOME Keyring,
//! KWallet, KeePassXC…), or the Secret portal when sandboxed. Machines without one fall back
//! to an encrypted keyring file whose random key is stored separately, in the state directory:
//!
//! - `$XDG_DATA_HOME/<app>/secrets.keyring` — values, encrypted (GNOME Keyring file format)
//! - `$XDG_STATE_HOME/<app>/secret-store.key` — random key, mode 0600
//!
//! The fallback protects against the secrets file leaking on its own (copied, synced,
//! committed); it does not protect against other programs running as the same user.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use indexmap::IndexMap;
use oo7::Secret;
use rust_i18n::t;

use crate::model::{CollectionFile, EnvironmentFile, Variables};
use crate::paths::{APP_ID, AppPaths};

/// Scope name used for a collection's own secret defaults.
pub const DEFAULTS_SCOPE: &str = "defaults";

const KEY_LEN: usize = 64;

/// Identifies one secret value: which collection, which environment (by file stem, or
/// [`DEFAULTS_SCOPE`]), and which variable.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SecretRef {
    pub collection_id: String,
    pub scope: String,
    pub name: String,
}

impl SecretRef {
    pub fn new(collection_id: impl Into<String>, scope: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            collection_id: collection_id.into(),
            scope: scope.into(),
            name: name.into(),
        }
    }

    /// Scope for an environment file: its file stem, which stays fixed when renamed.
    pub fn environment_scope(path: &Path) -> String {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn attributes(&self) -> [(&'static str, &str); 4] {
        [
            ("application", APP_ID),
            ("collection", &self.collection_id),
            ("scope", &self.scope),
            ("variable", &self.name),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Keyring,
    EncryptedFile,
}

impl BackendKind {
    pub fn describe(self) -> String {
        match self {
            Self::Keyring => t!("secrets.backend_keyring").to_string(),
            Self::EncryptedFile => t!("secrets.backend_file").to_string(),
        }
    }
}

enum Backend {
    Keyring(oo7::Keyring),
    File(oo7::file::UnlockedKeyring),
    /// For UI tests: GPUI's test scheduler rejects wake-ups from the file backend's I/O threads.
    #[cfg(any(test, feature = "test-support"))]
    Memory(std::sync::Mutex<std::collections::HashMap<SecretRef, String>>),
}

/// Cheap to clone. All methods are async and meant for a background executor.
#[derive(Clone)]
pub struct SecretStore(Arc<Backend>);

impl SecretStore {
    /// Connects to the desktop keyring, falling back to the encrypted file.
    pub async fn open(paths: &AppPaths) -> Result<Self> {
        match oo7::Keyring::new().await {
            Ok(keyring) => Ok(Self(Arc::new(Backend::Keyring(keyring)))),
            Err(e) => {
                eprintln!("no Secret Service available ({e}); using the encrypted file fallback");
                Self::open_file(paths).await
            }
        }
    }

    pub async fn open_file(paths: &AppPaths) -> Result<Self> {
        let key = load_or_create_key(&paths.state_dir.join("secret-store.key"))?;
        let path = paths.data_dir.join("secrets.keyring");
        fs::create_dir_all(&paths.data_dir)?;
        let keyring = oo7::file::UnlockedKeyring::load(&path, Secret::blob(key))
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self(Arc::new(Backend::File(keyring))))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory() -> Self {
        Self(Arc::new(Backend::Memory(Default::default())))
    }

    pub fn kind(&self) -> BackendKind {
        match &*self.0 {
            Backend::Keyring(_) => BackendKind::Keyring,
            Backend::File(_) => BackendKind::EncryptedFile,
            #[cfg(any(test, feature = "test-support"))]
            Backend::Memory(_) => BackendKind::EncryptedFile,
        }
    }

    /// Keyrings can be locked (e.g. at login without auto-unlock); this shows the unlock prompt.
    async fn ensure_unlocked(&self) -> Result<()> {
        if let Backend::Keyring(keyring) = &*self.0
            && keyring.is_locked().await?
        {
            keyring.unlock().await.context("the keyring is locked")?;
        }
        Ok(())
    }

    pub async fn get(&self, secret: &SecretRef) -> Result<Option<String>> {
        self.ensure_unlocked().await?;
        let attributes = secret.attributes();
        let value = match &*self.0 {
            Backend::Keyring(keyring) => match keyring.search_items(&attributes).await?.first() {
                Some(item) => Some(item.secret().await?),
                None => None,
            },
            Backend::File(file) => match file.lookup_item(&attributes).await? {
                Some(oo7::file::Item::Unlocked(item)) => Some(item.secret()),
                Some(oo7::file::Item::Locked(_)) => bail!("the secret file is locked"),
                None => None,
            },
            #[cfg(any(test, feature = "test-support"))]
            Backend::Memory(map) => return Ok(map.lock().unwrap().get(secret).cloned()),
        };
        value
            .map(|secret| String::from_utf8(secret.as_bytes().to_vec()).map_err(|_| anyhow!("secret is not text")))
            .transpose()
    }

    pub async fn set(&self, secret: &SecretRef, label: &str, value: &str) -> Result<()> {
        self.ensure_unlocked().await?;
        let attributes = secret.attributes();
        match &*self.0 {
            Backend::Keyring(keyring) => {
                keyring
                    .create_item(label, &attributes, Secret::text(value), true)
                    .await?
            }
            Backend::File(file) => {
                file.create_item(label, &attributes, Secret::text(value), true).await?;
            }
            #[cfg(any(test, feature = "test-support"))]
            Backend::Memory(map) => {
                map.lock().unwrap().insert(secret.clone(), value.to_string());
            }
        }
        Ok(())
    }

    pub async fn delete(&self, secret: &SecretRef) -> Result<()> {
        self.ensure_unlocked().await?;
        let attributes = secret.attributes();
        match &*self.0 {
            Backend::Keyring(keyring) => keyring.delete(&attributes).await?,
            Backend::File(file) => file.delete(&attributes).await?,
            #[cfg(any(test, feature = "test-support"))]
            Backend::Memory(map) => {
                map.lock().unwrap().remove(secret);
            }
        }
        Ok(())
    }

    /// The values of every secret that has one. (Unset ones show up as unresolved
    /// placeholders when the request is resolved.)
    pub async fn get_all(&self, secrets: &IndexMap<String, SecretRef>) -> Result<Variables> {
        let mut found = Variables::new();
        for (name, secret) in secrets {
            if let Some(value) = self.get(secret).await? {
                found.insert(name.clone(), value);
            }
        }
        Ok(found)
    }

    /// Stores `sets` and removes `deletes`, stopping at the first failure.
    pub async fn apply(&self, sets: &[SecretWrite], deletes: &[SecretRef]) -> Result<()> {
        for write in sets {
            self.set(&write.secret, &write.label, &write.value).await?;
        }
        for secret in deletes {
            self.delete(secret).await?;
        }
        Ok(())
    }
}

/// Scope label used in keyring labels for a collection's own secret defaults.
pub const DEFAULTS_LABEL: &str = "Defaults";

/// A secret value waiting to be stored, with its keyring label.
#[derive(Clone, Debug)]
pub struct SecretWrite {
    pub secret: SecretRef,
    pub label: String,
    pub value: String,
}

impl SecretWrite {
    /// A write for `name` in a scope of `collection`, whose id must already be assigned.
    pub fn new(
        collection: &CollectionFile,
        scope: &str,
        scope_label: &str,
        name: &str,
        value: impl Into<String>,
    ) -> Self {
        Self {
            secret: SecretRef::new(collection.id.clone().unwrap_or_default(), scope, name),
            label: label(&collection.name, scope_label, name),
            value: value.into(),
        }
    }
}

/// Human-readable keyring item label, shown in tools like Seahorse.
pub fn label(collection_name: &str, scope_name: &str, variable: &str) -> String {
    format!("Courier · {collection_name} · {scope_name} · {variable}")
}

/// Plain variables and secret references in effect for a request, after layering the
/// collection defaults under the active environment. A name defined in the environment
/// (as a variable or a secret) replaces the same name from the defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layered {
    pub variables: Variables,
    pub secrets: IndexMap<String, SecretRef>,
}

pub fn layer(collection: &CollectionFile, environment: Option<(&Path, &EnvironmentFile)>) -> Layered {
    let collection_id = collection.id.clone().unwrap_or_default();
    let mut layered = Layered {
        variables: collection.variables.clone(),
        secrets: collection
            .secrets
            .iter()
            .map(|name| (name.clone(), SecretRef::new(&collection_id, DEFAULTS_SCOPE, name)))
            .collect(),
    };
    if let Some((path, env)) = environment {
        let scope = SecretRef::environment_scope(path);
        for (name, value) in &env.variables {
            layered.secrets.shift_remove(name);
            layered.variables.insert(name.clone(), value.clone());
        }
        for name in &env.secrets {
            layered.variables.shift_remove(name);
            layered
                .secrets
                .insert(name.clone(), SecretRef::new(&collection_id, &scope, name));
        }
    }
    layered
}

fn load_or_create_key(path: &Path) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(key) if key.len() == KEY_LEN => return Ok(key),
        Ok(_) => bail!(
            "{} is not a valid secret store key; refusing to overwrite it",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let key = Secret::random().map_err(|e| anyhow!("no randomness available: {e}"))?;
    let key = key.as_bytes().to_vec();
    debug_assert_eq!(key.len(), KEY_LEN);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(&key)?;
    file.sync_all()?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn file_backend_round_trips_without_plaintext_on_disk() {
        futures_lite::future::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let paths = AppPaths::under(tmp.path());
            let token = SecretRef::new("c1", "production", "api_token");
            let value = "sk_live_super_secret_value_123";

            let store = SecretStore::open_file(&paths).await.unwrap();
            assert_eq!(store.kind(), BackendKind::EncryptedFile);
            assert_eq!(store.get(&token).await.unwrap(), None);
            store.set(&token, "label", value).await.unwrap();
            store.set(&token, "label", "sk_live_rotated").await.unwrap();

            // A fresh instance reads it back with the same key.
            let reopened = SecretStore::open_file(&paths).await.unwrap();
            assert_eq!(reopened.get(&token).await.unwrap().as_deref(), Some("sk_live_rotated"));
            let other_scope = SecretRef::new("c1", "staging", "api_token");
            assert_eq!(reopened.get(&other_scope).await.unwrap(), None);

            let keyring = fs::read(paths.data_dir.join("secrets.keyring")).unwrap();
            assert!(
                !keyring.windows(15).any(|w| w == b"sk_live_rotated"),
                "value must be encrypted"
            );
            let key_path = paths.state_dir.join("secret-store.key");
            assert_eq!(fs::metadata(&key_path).unwrap().permissions().mode() & 0o777, 0o600);

            reopened.delete(&token).await.unwrap();
            assert_eq!(reopened.get(&token).await.unwrap(), None);
        });
    }

    #[test]
    fn file_backend_rejects_a_different_key() {
        futures_lite::future::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let paths = AppPaths::under(tmp.path());
            let token = SecretRef::new("c1", DEFAULTS_SCOPE, "token");
            SecretStore::open_file(&paths)
                .await
                .unwrap()
                .set(&token, "label", "abc")
                .await
                .unwrap();

            // Copying the secrets file to a machine with a different key must not reveal it.
            fs::remove_file(paths.state_dir.join("secret-store.key")).unwrap();
            let result = match SecretStore::open_file(&paths).await {
                Ok(store) => store.get(&token).await.ok().flatten(),
                Err(_) => None,
            };
            assert_eq!(result, None);
        });
    }

    #[test]
    fn environment_overrides_defaults_by_name() {
        let mut collection = CollectionFile::new("API");
        collection.id = Some("c1".into());
        collection.variables.insert("base_url".into(), "https://prod".into());
        collection.variables.insert("token".into(), "public-demo".into());
        collection.secrets = vec!["client_secret".into(), "api_key".into()];

        let mut env = EnvironmentFile::new("Local");
        env.variables.insert("base_url".into(), "http://localhost".into());
        env.variables.insert("api_key".into(), "local-dev-key".into());
        env.secrets = vec!["token".into()];
        let path = Path::new("/x/environments/local.yaml");

        let layered = layer(&collection, Some((path, &env)));
        assert_eq!(layered.variables["base_url"], "http://localhost");
        assert_eq!(
            layered.variables["api_key"], "local-dev-key",
            "env variable replaces default secret"
        );
        assert!(
            !layered.variables.contains_key("token"),
            "env secret replaces default variable"
        );
        assert_eq!(layered.secrets["token"], SecretRef::new("c1", "local", "token"));
        assert_eq!(
            layered.secrets["client_secret"],
            SecretRef::new("c1", DEFAULTS_SCOPE, "client_secret")
        );
        assert!(!layered.secrets.contains_key("api_key"));

        let defaults_only = layer(&collection, None);
        assert_eq!(defaults_only.secrets.len(), 2);
    }

    /// Talks to the real desktop keyring. Run manually: `cargo test real_keyring -- --ignored`
    #[test]
    #[ignore]
    fn real_keyring_round_trip() {
        futures_lite::future::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let store = SecretStore::open(&AppPaths::under(tmp.path())).await.unwrap();
            assert_eq!(store.kind(), BackendKind::Keyring);
            let secret = SecretRef::new("test-collection", DEFAULTS_SCOPE, "round_trip");
            store
                .set(&secret, &label("Test", "Defaults", "round_trip"), "hello")
                .await
                .unwrap();
            assert_eq!(store.get(&secret).await.unwrap().as_deref(), Some("hello"));
            store.delete(&secret).await.unwrap();
            assert_eq!(store.get(&secret).await.unwrap(), None);
        });
    }
}
