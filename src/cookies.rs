//! A cookie jar per collection: cookies from responses are stored and sent with later requests
//! to matching URLs, like a browser. Cookies are often credentials, so a jar is saved in the
//! secret store (keyring or encrypted file), never in a project or a plain file.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use cookie_store::{CookieExpiration, CookieStore};
use reqwest_cookie_store::CookieStoreMutex;

use crate::secret_store::{SecretRef, SecretStore};

/// One stored cookie, for listing.
#[derive(Clone, Debug, PartialEq)]
pub struct CookieInfo {
    pub domain: String,
    pub path: String,
    pub name: String,
    pub value: String,
    /// The expiry date, or None for a session cookie.
    pub expires: Option<String>,
}

/// Where a jar is saved.
#[derive(Clone)]
struct Saved {
    store: SecretStore,
    secret: SecretRef,
    label: String,
}

#[derive(Clone)]
pub struct Cookies {
    store: Arc<CookieStoreMutex>,
    client: reqwest::Client,
    saved: Arc<Mutex<Option<Saved>>>,
}

impl Default for Cookies {
    fn default() -> Self {
        let store = Arc::new(CookieStoreMutex::new(CookieStore::default()));
        let client = crate::transport::client_with_cookies(store.clone());
        Self {
            store,
            client,
            saved: Default::default(),
        }
    }
}

impl Cookies {
    /// The secret holding a collection's cookie jar.
    pub fn secret(collection_id: &str) -> SecretRef {
        SecretRef::new(collection_id, "cookies", "jar")
    }

    /// Saves this jar in `store` from now on, first loading what was saved there before.
    pub async fn persist_in(&self, store: SecretStore, secret: SecretRef, label: String) -> Result<()> {
        let json = store.get(&secret).await?;
        if let Some(json) = json {
            self.load_json(&json);
        }
        *self.saved.lock().unwrap() = Some(Saved { store, secret, label });
        Ok(())
    }

    /// Replaces an empty jar with saved cookies. A jar that has already received cookies keeps
    /// them; they'll be saved over the old ones.
    fn load_json(&self, json: &str) {
        let Ok(loaded) = cookie_store::serde::json::load(json.as_bytes()) else {
            eprintln!("ignoring unreadable saved cookies");
            return;
        };
        if let Ok(mut store) = self.store.lock()
            && store.iter_any().next().is_none()
        {
            *store = loaded;
        }
    }

    /// An HTTP client that sends and stores this jar's cookies.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// The `Cookie` header value for `url`, for requests the client doesn't send (WebSockets).
    pub fn header_for(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let store = self.store.lock().ok()?;
        let mut pairs: Vec<String> = store
            .get_request_values(&url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        pairs.sort();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    pub fn list(&self) -> Vec<CookieInfo> {
        let Ok(store) = self.store.lock() else {
            return Vec::new();
        };
        let mut cookies: Vec<CookieInfo> = store
            .iter_unexpired()
            .map(|cookie| CookieInfo {
                domain: cookie.domain.as_cow().map(|d| d.into_owned()).unwrap_or_default(),
                path: cookie.path.as_ref().to_string(),
                name: cookie.name().to_string(),
                value: cookie.value().to_string(),
                expires: match &cookie.expires {
                    CookieExpiration::AtUtc(at) => Some(at.date().to_string()),
                    CookieExpiration::SessionEnd => None,
                },
            })
            .collect();
        cookies.sort_by(|a, b| (&a.domain, &a.path, &a.name).cmp(&(&b.domain, &b.path, &b.name)));
        cookies
    }

    pub fn remove(&self, cookie: &CookieInfo) {
        if let Ok(mut store) = self.store.lock() {
            store.remove(&cookie.domain, &cookie.path, &cookie.name);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut store) = self.store.lock() {
            store.clear();
        }
    }

    /// Saves the jar (session cookies included, so a login survives a restart), or removes
    /// the saved one when the jar is empty. Does nothing until [`Cookies::persist_in`].
    pub async fn save(&self) -> Result<()> {
        let Some(saved) = self.saved.lock().unwrap().clone() else {
            return Ok(());
        };
        let json = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("cookie jar is poisoned"))?;
            if store.iter_unexpired().next().is_none() {
                None
            } else {
                let mut json = Vec::new();
                cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut json)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Some(String::from_utf8(json)?)
            }
        };
        match json {
            Some(json) => saved.store.set(&saved.secret, &saved.label, &json).await,
            None => saved.store.delete(&saved.secret).await,
        }
    }

    #[cfg(test)]
    fn add(&self, set_cookie: &str, url: &str) {
        let url = url::Url::parse(url).unwrap();
        self.store.lock().unwrap().parse(set_cookie, &url).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_lists_saves_and_clears() {
        let jar = Cookies::default();
        jar.add("session=s1; Path=/", "https://api.test/login");
        jar.add(
            "theme=dark; Path=/; Expires=Wed, 01 Jan 2099 00:00:00 GMT",
            "https://api.test/login",
        );
        assert_eq!(
            jar.header_for("https://api.test/me").as_deref(),
            Some("session=s1; theme=dark")
        );
        assert_eq!(jar.header_for("https://other.test/"), None);
        let listed = jar.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "session");
        assert_eq!(listed[0].expires, None);
        assert_eq!(listed[1].expires.as_deref(), Some("2099-01-01"));

        let store = SecretStore::in_memory();
        let secret = Cookies::secret("collection-1");
        futures_lite::future::block_on(async {
            jar.persist_in(store.clone(), secret.clone(), "Courier · cookies".into())
                .await
                .unwrap();
            jar.save().await.unwrap();
            let saved = store.get(&secret).await.unwrap().unwrap();
            assert!(saved.contains("session"));

            let reopened = Cookies::default();
            reopened
                .persist_in(store.clone(), secret.clone(), "Courier · cookies".into())
                .await
                .unwrap();
            assert_eq!(reopened.list().len(), 2, "session cookies survive a restart");

            reopened.remove(&listed[1]);
            assert_eq!(reopened.list().len(), 1);
            reopened.clear();
            reopened.save().await.unwrap();
            assert_eq!(store.get(&secret).await.unwrap(), None, "an empty jar isn't kept");
        });
    }
}
