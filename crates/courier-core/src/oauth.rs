//! OAuth 2.0: getting an access token and keeping it until it expires.
//!
//! Tokens live in the secret store, never in the collection, so a token fetched in the app is
//! there after a restart and never lands in a file that could be committed.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rust_i18n::t;
use serde::{Deserialize, Serialize};

use crate::model::{Auth, OAuthGrant, Variables, interpolate};
use crate::secret_store::{SecretRef, SecretStore};

/// A token as the provider returned it, with when it stops being usable.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds, when the provider said how long the token lasts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub token_type: String,
}

/// Refresh this long before a token actually expires, so a slow request doesn't race it.
const EARLY: u64 = 30;

impl Tokens {
    pub fn usable(&self) -> bool {
        !self.access_token.is_empty() && !self.expires_within(EARLY)
    }

    pub fn expires_within(&self, seconds: u64) -> bool {
        self.expires_at.is_some_and(|at| at <= now() + seconds)
    }

    /// The header value: `Bearer …`, or whatever type the provider asked for.
    pub fn header_value(&self) -> String {
        let kind = match self.token_type.trim() {
            "" => "Bearer".to_string(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => "Bearer".to_string(),
                }
            }
        };
        format!("{kind} {}", self.access_token)
    }

    /// How long is left, for the auth row.
    pub fn expires_in(&self) -> Option<Duration> {
        self.expires_at.map(|at| Duration::from_secs(at.saturating_sub(now())))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// What the provider sends back from the token endpoint.
#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    token_type: String,
    // An error response comes back on the same endpoint, sometimes with 200.
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// The OAuth settings with `{{variables}}` filled in, ready to use.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub grant: OAuthGrant,
    pub token_url: String,
    pub auth_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    pub audience: String,
}

impl Config {
    /// Resolves an `Auth::OAuth2`'s fields against `variables`.
    pub fn resolve(auth: &Auth, variables: &Variables) -> Option<Self> {
        let Auth::OAuth2 {
            grant,
            token_url,
            auth_url,
            client_id,
            client_secret,
            scope,
            audience,
        } = auth
        else {
            return None;
        };
        let sub = |text: &str| interpolate(text, variables).0;
        Some(Self {
            grant: *grant,
            token_url: sub(token_url).trim().to_string(),
            auth_url: sub(auth_url).trim().to_string(),
            client_id: sub(client_id).trim().to_string(),
            client_secret: sub(client_secret),
            scope: sub(scope).trim().to_string(),
            audience: sub(audience).trim().to_string(),
        })
    }

    /// Where this configuration's tokens are kept. Different settings get different slots, so
    /// changing the scope or the client doesn't hand back the old token.
    pub fn secret(&self, collection_id: &str) -> SecretRef {
        let fingerprint = crate::response_cache::fnv1a(&format!(
            "{:?}|{}|{}|{}|{}",
            self.grant, self.token_url, self.client_id, self.scope, self.audience
        ));
        SecretRef::new(collection_id, "oauth", format!("token-{fingerprint:x}"))
    }

    pub fn label(&self) -> String {
        format!("OAuth token for {} ({})", self.client_id, self.token_url)
    }

    fn check(&self) -> Result<(), String> {
        if self.token_url.is_empty() {
            return Err(t!("oauth.no_token_url").to_string());
        }
        if self.client_id.is_empty() {
            return Err(t!("oauth.no_client_id").to_string());
        }
        Ok(())
    }
}

/// The access token to send: the saved one while it lasts, refreshed or fetched when not.
/// `store` is where tokens are kept; without one, every send fetches a new token.
pub async fn access_token(
    config: &Config,
    collection_id: &str,
    store: Option<&SecretStore>,
    client: &reqwest::Client,
) -> Result<Tokens, String> {
    config.check()?;
    let secret = config.secret(collection_id);
    let saved = match store {
        Some(store) => store.get(&secret).await.ok().flatten(),
        None => None,
    };
    let saved: Option<Tokens> = saved.and_then(|json| serde_json::from_str(&json).ok());
    if let Some(tokens) = saved.as_ref().filter(|tokens| tokens.usable()) {
        return Ok(tokens.clone());
    }
    // An expired token with a refresh token costs one round trip instead of a whole sign-in.
    if let Some(refresh) = saved.as_ref().and_then(|tokens| tokens.refresh_token.clone())
        && let Ok(mut tokens) = fetch(
            config,
            client,
            &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
        )
        .await
    {
        tokens.refresh_token = tokens.refresh_token.or(Some(refresh));
        save(&tokens, config, &secret, store).await;
        return Ok(tokens);
    }
    if config.grant != OAuthGrant::ClientCredentials {
        return Err(t!("oauth.sign_in_needed").to_string());
    }
    let tokens = fetch(config, client, &[("grant_type", "client_credentials")]).await?;
    save(&tokens, config, &secret, store).await;
    Ok(tokens)
}

/// Replaces a request's `Auth::OAuth2` with the header it produces, fetching or refreshing
/// the token first. Resolving a request stays pure and offline this way.
pub async fn authorize(
    file: &mut crate::model::RequestFile,
    variables: &Variables,
    collection_id: &str,
    store: Option<&SecretStore>,
    client: &reqwest::Client,
) -> Result<(), String> {
    let Some(config) = Config::resolve(&file.auth, variables) else {
        return Ok(());
    };
    let tokens = access_token(&config, collection_id, store, client).await?;
    file.auth = Auth::ApiKey {
        name: "Authorization".into(),
        value: tokens.header_value(),
        in_query: false,
    };
    Ok(())
}

/// Keeps `tokens` for next time. Failing to save is not worth failing a request over: the
/// next send fetches a fresh token instead.
pub async fn save(tokens: &Tokens, config: &Config, secret: &SecretRef, store: Option<&SecretStore>) {
    let (Some(store), Ok(json)) = (store, serde_json::to_string(tokens)) else {
        return;
    };
    if let Err(e) = store.set(secret, &config.label(), &json).await {
        eprintln!("could not save the OAuth token: {e:#}");
    }
}

/// Forgets a configuration's tokens, so the next send signs in again.
pub async fn forget(config: &Config, collection_id: &str, store: Option<&SecretStore>) {
    if let Some(store) = store {
        let _ = store.delete(&config.secret(collection_id)).await;
    }
}

/// Posts to the token endpoint. `form` carries the grant-specific fields.
pub async fn fetch(config: &Config, client: &reqwest::Client, form: &[(&str, &str)]) -> Result<Tokens, String> {
    let mut fields: Vec<(&str, &str)> = form.to_vec();
    if !config.scope.is_empty() {
        fields.push(("scope", &config.scope));
    }
    if !config.audience.is_empty() {
        fields.push(("audience", &config.audience));
    }
    // Public clients (PKCE, device code) have no secret and identify in the body instead.
    let mut request = client.post(&config.token_url);
    if config.client_secret.trim().is_empty() {
        fields.push(("client_id", &config.client_id));
    } else {
        request = request.basic_auth(&config.client_id, Some(&config.client_secret));
    }
    let body = fields
        .iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                crate::encoding::form_encode(name),
                crate::encoding::form_encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let request = request
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body);
    // The token request runs on the HTTP runtime like any other, whoever calls this.
    let (status, body) = crate::transport::on_runtime(async move {
        let response = request
            .send()
            .await
            .map_err(|e| t!("oauth.request_failed", error = e.to_string()).to_string())?;
        let status = response.status();
        Ok::<_, String>((status, response.text().await.unwrap_or_default()))
    })
    .await?;
    let parsed: TokenResponse = serde_json::from_str(&body).map_err(|_| {
        let snippet: String = body.chars().take(200).collect();
        t!("oauth.bad_response", status = status.as_u16(), body = snippet).to_string()
    })?;
    if let Some(error) = parsed.error {
        let detail = parsed.error_description.unwrap_or(error);
        return Err(t!("oauth.provider_error", error = detail).to_string());
    }
    if parsed.access_token.is_empty() {
        return Err(t!("oauth.no_access_token").to_string());
    }
    Ok(Tokens {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at: parsed.expires_in.map(|seconds| now() + seconds),
        token_type: parsed.token_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(token_url: String) -> Config {
        Config {
            grant: OAuthGrant::ClientCredentials,
            token_url,
            auth_url: String::new(),
            client_id: "courier".into(),
            client_secret: "s3cret".into(),
            scope: "read write".into(),
            audience: String::new(),
        }
    }

    /// Answers token requests, recording what was posted.
    fn token_server(replies: Vec<(u16, &'static str)>) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{BufRead as _, Read as _, Write as _};
            for (status, reply) in replies {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let (mut head, mut length) = (String::new(), 0usize);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                sent.send(format!("{head}\r\n{}", String::from_utf8_lossy(&body)))
                    .unwrap();
            }
        });
        (format!("http://127.0.0.1:{port}/token"), received)
    }

    #[test]
    fn fetches_caches_and_refreshes_a_token() {
        let (url, seen) = token_server(vec![
            (
                200,
                r#"{"access_token":"first","token_type":"bearer","expires_in":3600,"refresh_token":"r1"}"#,
            ),
            (
                200,
                r#"{"access_token":"second","token_type":"Bearer","expires_in":3600}"#,
            ),
        ]);
        let config = config(url);
        let store = SecretStore::in_memory();
        let client = reqwest::Client::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let tokens = runtime
            .block_on(access_token(&config, "c1", Some(&store), &client))
            .unwrap();
        assert_eq!(tokens.access_token, "first");
        assert_eq!(tokens.header_value(), "Bearer first", "the type is normalised");
        let posted = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(posted.contains("grant_type=client_credentials"), "{posted}");
        assert!(posted.contains("scope=read+write"), "{posted}");
        assert!(
            posted.to_ascii_lowercase().contains("authorization: basic"),
            "a client with a secret authenticates with it:\n{posted}"
        );

        // The saved token is used again without asking the provider.
        let again = runtime
            .block_on(access_token(&config, "c1", Some(&store), &client))
            .unwrap();
        assert_eq!(again.access_token, "first");

        // Once it's about to expire, the refresh token is spent instead of signing in again.
        let secret = config.secret("c1");
        let expiring = Tokens {
            access_token: "first".into(),
            refresh_token: Some("r1".into()),
            expires_at: Some(now() + 5),
            token_type: "bearer".into(),
        };
        runtime.block_on(save(&expiring, &config, &secret, Some(&store)));
        let refreshed = runtime
            .block_on(access_token(&config, "c1", Some(&store), &client))
            .unwrap();
        assert_eq!(refreshed.access_token, "second");
        let posted = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(posted.contains("grant_type=refresh_token"), "{posted}");
        assert!(posted.contains("refresh_token=r1"), "{posted}");
        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("r1"),
            "a provider that doesn't send a new refresh token keeps the old one"
        );

        // Forgetting means the next send starts over.
        runtime.block_on(forget(&config, "c1", Some(&store)));
        assert!(runtime.block_on(store.get(&secret)).unwrap().is_none());
    }

    #[test]
    fn reports_what_the_provider_said() {
        let (url, _seen) = token_server(vec![
            (
                400,
                r#"{"error":"invalid_client","error_description":"Client authentication failed"}"#,
            ),
            (200, "not json at all"),
        ]);
        let config = config(url);
        let client = reqwest::Client::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(access_token(&config, "c1", None, &client))
            .unwrap_err();
        assert!(error.contains("Client authentication failed"), "{error}");

        let error = runtime
            .block_on(access_token(&config, "c1", None, &client))
            .unwrap_err();
        assert!(error.contains("not json at all"), "the body is shown: {error}");

        let mut incomplete = config.clone();
        incomplete.client_id = String::new();
        let error = runtime
            .block_on(access_token(&incomplete, "c1", None, &client))
            .unwrap_err();
        assert!(!error.is_empty(), "missing settings are caught before sending");
    }
}
