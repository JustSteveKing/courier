//! The last response for each request, kept in memory by the request editor and saved to
//! `$XDG_CACHE_HOME/<app>/responses/` so it survives restarts.
//!
//! Only what the server returned is saved, never the resolved request (which may contain
//! secret values). Before writing, credential-bearing headers are masked and very large
//! bodies are truncated. Files are `0600` in a `0700` directory; the whole cache is
//! disposable and can be cleared from the command palette.
//!
//! Each file records which collection and request it belongs to (ids and relative paths,
//! nothing sensitive), so [`ResponseCache::tidy`] can delete responses whose request no
//! longer exists. The workspace runs it in the background.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::credentials::looks_sensitive_name;

/// Bodies beyond this are cut before saving to disk (the in-memory copy stays whole).
pub const MAX_SAVED_BODY: usize = 2 * 1024 * 1024;
pub const MASK: &str = "••••••";

/// Files this recent are never tidied, so a response saved for a request created moments
/// after the tidy took its snapshot of live requests is safe.
pub const TIDY_GRACE: Duration = Duration::from_secs(10 * 60);
/// Responses for collections that aren't open are kept this long, then removed.
pub const TIDY_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Leftover temporary files from an interrupted write.
const STALE_TMP_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoredResponse {
    /// Unix seconds when the response arrived.
    pub received_at: u64,
    pub elapsed_ms: u64,
    pub outcome: Outcome,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Response {
        status: u16,
        reason: String,
        headers: Vec<(String, String)>,
        body: String,
        /// Size of the body as received, even if `body` was truncated.
        body_size: usize,
        #[serde(default)]
        truncated: bool,
    },
    Error {
        message: String,
    },
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

impl StoredResponse {
    pub fn failed(received_at: u64, message: impl Into<String>) -> Self {
        Self {
            received_at,
            elapsed_ms: 0,
            outcome: Outcome::Error {
                message: message.into(),
            },
        }
    }

    /// The copy that is safe to write to disk: sensitive headers masked, body capped.
    pub fn for_disk(&self) -> Self {
        let mut copy = self.clone();
        if let Outcome::Response {
            headers,
            body,
            truncated,
            ..
        } = &mut copy.outcome
        {
            for (name, value) in headers.iter_mut() {
                if is_sensitive_header(name) {
                    *value = MASK.to_string();
                }
            }
            if body.len() > MAX_SAVED_BODY {
                let mut end = MAX_SAVED_BODY;
                while !body.is_char_boundary(end) {
                    end -= 1;
                }
                body.truncate(end);
                *truncated = true;
            }
        }
        copy
    }
}

pub fn is_sensitive_header(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "set-cookie",
        "cookie",
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "api-key",
    ];
    NAMES.iter().any(|n| name.eq_ignore_ascii_case(n)) || looks_sensitive_name(name)
}

/// Identifies a request in the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheKey {
    /// File stem: a hash of the identity below.
    pub key: String,
    pub collection_id: Option<String>,
    /// Path inside the collection, or the absolute path for collections without an id.
    pub request: String,
}

/// On-disk format: the response plus what it belongs to.
#[derive(Serialize, Deserialize)]
struct CacheFile {
    collection_id: Option<String>,
    request: String,
    response: StoredResponse,
}

/// A stable cache key for a request. Keyed by collection id and the path inside the
/// collection, so moving the collection folder keeps its responses.
pub fn cache_key(collection_id: Option<&str>, collection_root: &Path, request: &Path) -> CacheKey {
    let (identity, collection_id, request) = match (collection_id, request.strip_prefix(collection_root)) {
        (Some(id), Ok(relative)) => {
            let relative = relative.display().to_string();
            (format!("{id}/{relative}"), Some(id.to_string()), relative)
        }
        _ => (request.display().to_string(), None, request.display().to_string()),
    };
    // FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in identity.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    CacheKey {
        key: format!("{hash:016x}"),
        collection_id,
        request,
    }
}

/// What currently exists, for [`ResponseCache::tidy`].
#[derive(Clone, Debug, Default)]
pub struct Liveness {
    /// Cache keys of every request in an open collection.
    pub keys: HashSet<String>,
    /// Ids of open collections. A response for one of these whose key isn't live is an
    /// orphan: its request was deleted or renamed.
    pub collection_ids: HashSet<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TidyReport {
    pub removed: usize,
    pub kept: usize,
}

#[derive(Clone, Debug)]
pub struct ResponseCache {
    dir: PathBuf,
}

impl ResponseCache {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            dir: cache_dir.join("responses"),
        }
    }

    fn file(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn save(&self, key: &CacheKey, response: &StoredResponse) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let file = CacheFile {
            collection_id: key.collection_id.clone(),
            request: key.request.clone(),
            response: response.for_disk(),
        };
        let json = serde_json::to_vec(&file)?;
        let path = self.file(&key.key);
        let tmp = path.with_extension("json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// A missing or unreadable entry is simply no saved response.
    pub fn load(&self, key: &CacheKey) -> Option<StoredResponse> {
        let bytes = fs::read(self.file(&key.key)).ok()?;
        serde_json::from_slice::<CacheFile>(&bytes)
            .inspect_err(|e| eprintln!("ignoring unreadable cached response {}: {e}", key.key))
            .ok()
            .map(|file| file.response)
    }

    /// Deletes responses that no longer belong to anything:
    /// - responses for an open collection whose request no longer exists,
    /// - responses for collections that haven't been open for [`TIDY_MAX_AGE`],
    /// - unreadable files and stale temporary files.
    ///
    /// Files modified within [`TIDY_GRACE`] are always kept.
    pub fn tidy(&self, live: &Liveness, now: SystemTime) -> Result<TidyReport> {
        let mut report = TidyReport::default();
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Ok(report);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let age = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .unwrap_or_default();

            let remove = if name.ends_with(".json.tmp") {
                age > STALE_TMP_AGE
            } else if let Some(key) = name.strip_suffix(".json") {
                if age < TIDY_GRACE || live.keys.contains(key) {
                    false
                } else {
                    match fs::read(&path)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<CacheFile>(&b).ok())
                    {
                        None => true,
                        Some(file) => match &file.collection_id {
                            Some(id) if live.collection_ids.contains(id) => true,
                            _ => age > TIDY_MAX_AGE,
                        },
                    }
                }
            } else {
                // Not ours; leave it alone.
                continue;
            };

            if remove {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                report.removed += 1;
            } else {
                report.kept += 1;
            }
        }
        Ok(report)
    }

    /// Deletes every saved response. Returns how many were removed.
    pub fn clear(&self) -> Result<usize> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Ok(0);
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json" || e == "tmp") {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn response(body: &str, received_at: u64) -> StoredResponse {
        StoredResponse {
            received_at,
            elapsed_ms: 42,
            outcome: Outcome::Response {
                status: 200,
                reason: "OK".into(),
                headers: vec![
                    ("content-type".into(), "application/json".into()),
                    ("set-cookie".into(), "session=SECRET_SESSION".into()),
                    ("x-auth-token".into(), "SECRET_TOKEN".into()),
                    ("x-request-id".into(), "abc123".into()),
                ],
                body: body.into(),
                body_size: body.len(),
                truncated: false,
            },
        }
    }

    #[test]
    fn saves_masked_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ResponseCache::new(tmp.path());
        let stored = response("{\"ok\":true}", 1_700_000_000);
        let key = cache_key(Some("c1"), Path::new("/api"), Path::new("/api/users.yaml"));
        cache.save(&key, &stored).unwrap();

        let file = tmp.path().join(format!("responses/{}.json", key.key));
        let on_disk = fs::read_to_string(&file).unwrap();
        assert!(
            !on_disk.contains("SECRET_SESSION") && !on_disk.contains("SECRET_TOKEN"),
            "{on_disk}"
        );
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(tmp.path().join("responses")).unwrap().permissions().mode() & 0o777,
            0o700
        );

        assert!(
            on_disk.contains("\"request\":\"users.yaml\""),
            "records what it belongs to: {on_disk}"
        );
        let loaded = cache.load(&key).unwrap();
        let Outcome::Response { headers, body, .. } = &loaded.outcome else {
            panic!()
        };
        assert_eq!(body, "{\"ok\":true}");
        assert_eq!(headers[1].1, MASK);
        assert_eq!(headers[3].1, "abc123", "ordinary headers are kept");
        assert_eq!(loaded.elapsed_ms, 42);

        assert_eq!(cache.clear().unwrap(), 1);
        assert!(cache.load(&key).is_none());
    }

    #[test]
    fn truncates_huge_bodies_on_a_char_boundary() {
        let body = "é".repeat(MAX_SAVED_BODY); // 2 bytes each
        let stored = response(&body, 0).for_disk();
        let Outcome::Response {
            body,
            body_size,
            truncated,
            ..
        } = stored.outcome
        else {
            panic!()
        };
        assert!(truncated);
        assert!(body.len() <= MAX_SAVED_BODY);
        assert_eq!(body_size, MAX_SAVED_BODY * 2);
    }

    #[test]
    fn errors_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ResponseCache::new(tmp.path());
        let stored = StoredResponse::failed(5, "connection refused");
        let key = cache_key(None, Path::new("/x"), Path::new("/y/e.yaml"));
        cache.save(&key, &stored).unwrap();
        assert_eq!(cache.load(&key), Some(stored));
    }

    #[test]
    fn keys_are_stable_and_follow_the_collection_not_its_folder() {
        let a = cache_key(Some("c1"), Path::new("/old/api"), Path::new("/old/api/users/list.yaml"));
        let b = cache_key(
            Some("c1"),
            Path::new("/new/place"),
            Path::new("/new/place/users/list.yaml"),
        );
        assert_eq!(a.key, b.key);
        assert_eq!(a.request, "users/list.yaml");
        assert_ne!(
            a.key,
            cache_key(Some("c2"), Path::new("/old/api"), Path::new("/old/api/users/list.yaml")).key
        );
        assert_eq!(cache_key(None, Path::new("/x"), Path::new("/y/z.yaml")).key.len(), 16);
    }

    #[test]
    fn tidy_removes_only_orphans_and_stale_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ResponseCache::new(tmp.path());
        let stored = response("{}", 0);
        let root = Path::new("/api");
        let key = |collection: &str, request: &str| cache_key(Some(collection), root, &root.join(request));
        let now = SystemTime::now();
        let age = |key: &CacheKey, ago: Duration| {
            let file = fs::File::options()
                .write(true)
                .open(tmp.path().join(format!("responses/{}.json", key.key)))
                .unwrap();
            file.set_modified(now - ago).unwrap();
        };
        let hour = Duration::from_secs(3600);

        let live_request = key("open", "users.yaml");
        let deleted_request = key("open", "deleted.yaml");
        let just_created = key("open", "brand-new.yaml");
        let closed_recent = key("closed", "a.yaml");
        let closed_old = key("closed", "b.yaml");
        for k in [
            &live_request,
            &deleted_request,
            &just_created,
            &closed_recent,
            &closed_old,
        ] {
            cache.save(k, &stored).unwrap();
        }
        age(&live_request, TIDY_MAX_AGE * 2);
        age(&deleted_request, hour);
        age(&closed_recent, hour);
        age(&closed_old, TIDY_MAX_AGE + hour);
        // `just_created` stays within the grace period.
        let dir = tmp.path().join("responses");
        fs::write(dir.join("0000000000000000.json"), "not json").unwrap();
        fs::File::options()
            .write(true)
            .open(dir.join("0000000000000000.json"))
            .unwrap()
            .set_modified(now - hour)
            .unwrap();
        fs::write(dir.join("stale.json.tmp"), "x").unwrap();
        fs::File::options()
            .write(true)
            .open(dir.join("stale.json.tmp"))
            .unwrap()
            .set_modified(now - hour * 2)
            .unwrap();
        fs::write(dir.join("README"), "not ours").unwrap();

        let live = Liveness {
            keys: [live_request.key.clone(), just_created.key.clone()].into(),
            collection_ids: ["open".to_string()].into(),
        };
        let report = cache.tidy(&live, now).unwrap();

        assert!(
            cache.load(&live_request).is_some(),
            "a live request keeps its response, however old"
        );
        assert!(
            cache.load(&deleted_request).is_none(),
            "orphan in an open collection is removed"
        );
        assert!(cache.load(&just_created).is_some());
        assert!(
            cache.load(&closed_recent).is_some(),
            "closed collections keep recent responses"
        );
        assert!(cache.load(&closed_old).is_none(), "and lose them after the max age");
        assert!(!dir.join("0000000000000000.json").exists() && !dir.join("stale.json.tmp").exists());
        assert!(dir.join("README").exists(), "unrelated files are untouched");
        assert_eq!(report, TidyReport { removed: 4, kept: 3 });
    }
}
