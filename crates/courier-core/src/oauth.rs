//! OAuth 2.0: getting an access token and keeping it until it expires.
//!
//! Tokens live in the secret store, never in the collection, so a token fetched in the app is
//! there after a restart and never lands in a file that could be committed.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rust_i18n::t;
use serde::{Deserialize, Serialize};

use crate::encoding::form_encode;
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

/// What a browser sign-in needs from whoever is driving it: somewhere to send the person.
pub enum Prompt {
    /// Open this URL, then wait for the redirect to come back.
    Browser(String),
    /// Show this code and URL; the person types the code there.
    Device {
        user_code: String,
        verification_url: String,
        /// The same page with the code filled in, when the provider offers one.
        complete_url: Option<String>,
    },
}

/// The authorization code grant with PKCE: opens a browser, catches the redirect on a
/// loopback port, then swaps the code for tokens. `show` is called once with the URL.
pub async fn authorization_code(config: &Config, show: impl FnOnce(Prompt)) -> Result<Tokens, String> {
    config.check()?;
    if config.auth_url.is_empty() {
        return Err(t!("oauth.no_auth_url").to_string());
    }
    // Only loopback is allowed to be registered without a fixed port, so bind one now and
    // tell the provider which it is.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| t!("oauth.no_listener", error = e.to_string()).to_string())?;
    let port = listener
        .local_addr()
        .map_err(|e| t!("oauth.no_listener", error = e.to_string()).to_string())?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let verifier = random_token();
    let challenge = challenge_for(&verifier);
    let state = random_token();

    let separator = if config.auth_url.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{}{separator}response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        config.auth_url,
        form_encode(&config.client_id),
        form_encode(&redirect_uri),
        form_encode(&state),
        form_encode(&challenge)
    );
    if !config.scope.is_empty() {
        url.push_str(&format!("&scope={}", form_encode(&config.scope)));
    }
    if !config.audience.is_empty() {
        url.push_str(&format!("&audience={}", form_encode(&config.audience)));
    }
    show(Prompt::Browser(url));

    let code = wait_for_redirect(listener, state).await?;
    fetch(
        config,
        crate::transport::plain_client(),
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
        ],
    )
    .await
}

/// Waits for the provider to send the person back, on a thread so the socket's blocking
/// accept doesn't hold up anything else. Times out rather than waiting forever.
async fn wait_for_redirect(listener: std::net::TcpListener, state: String) -> Result<String, String> {
    let (send, receive) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = send.send_blocking(read_redirect(listener, &state));
    });
    let waiting = async move {
        receive
            .recv()
            .await
            .unwrap_or_else(|_| Err(t!("oauth.sign_in_cancelled").to_string()))
    };
    crate::transport::on_runtime(async move {
        tokio::time::timeout(std::time::Duration::from_secs(300), waiting)
            .await
            .unwrap_or_else(|_| Err(t!("oauth.sign_in_timeout").to_string()))
    })
    .await
}

/// Reads the one request the browser makes to the loopback port and answers it with a page
/// telling the person to go back to Courier.
fn read_redirect(listener: std::net::TcpListener, state: &str) -> Result<String, String> {
    use std::io::{BufRead as _, Write as _};
    let (stream, _) = listener
        .accept()
        .map_err(|e| t!("oauth.no_redirect", error = e.to_string()).to_string())?;
    let mut reader = std::io::BufReader::new(stream);
    let mut request = String::new();
    reader
        .read_line(&mut request)
        .map_err(|e| t!("oauth.no_redirect", error = e.to_string()).to_string())?;
    let target = request.split_whitespace().nth(1).unwrap_or_default();
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or_default();
    let mut code = None;
    let mut returned_state = None;
    let mut error = None;
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let value = form_decode(value);
        match name {
            "code" => code = Some(value),
            "state" => returned_state = Some(value),
            "error_description" => error = Some(value),
            "error" if error.is_none() => error = Some(value),
            _ => {}
        }
    }
    let outcome = match (code, error) {
        (_, Some(error)) => Err(t!("oauth.provider_error", error = error).to_string()),
        (None, None) => Err(t!("oauth.no_code").to_string()),
        (Some(_), _) if returned_state.as_deref() != Some(state) => Err(t!("oauth.bad_state").to_string()),
        (Some(code), None) => Ok(code),
    };
    let message = match &outcome {
        Ok(_) => t!("oauth.browser_done").to_string(),
        Err(error) => error.clone(),
    };
    let page = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Courier</title>\
         <body style=\"font:16px system-ui;margin:4rem auto;max-width:28rem;text-align:center\">{message}</body>"
    );
    let _ = write!(
        reader.get_mut(),
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    outcome
}

#[derive(Deserialize)]
struct DeviceResponse {
    #[serde(default)]
    device_code: String,
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// The device code grant: the person types a short code on another screen while this polls
/// the token endpoint. `show` is called once with the code and where to type it.
pub async fn device_code(config: &Config, show: impl FnOnce(Prompt)) -> Result<Tokens, String> {
    config.check()?;
    if config.auth_url.is_empty() {
        return Err(t!("oauth.no_device_url").to_string());
    }
    let client = crate::transport::plain_client();
    let mut fields = vec![("client_id", config.client_id.as_str())];
    if !config.scope.is_empty() {
        fields.push(("scope", config.scope.as_str()));
    }
    let body = form_body(&fields);
    let request = client
        .post(&config.auth_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body);
    let text = crate::transport::on_runtime(async move {
        let response = request
            .send()
            .await
            .map_err(|e| t!("oauth.request_failed", error = e.to_string()).to_string())?;
        Ok::<_, String>(response.text().await.unwrap_or_default())
    })
    .await?;
    let started: DeviceResponse = serde_json::from_str(&text).map_err(|_| {
        let snippet: String = text.chars().take(200).collect();
        t!("oauth.bad_response", status = 200, body = snippet).to_string()
    })?;
    if let Some(error) = started.error_description.or(started.error) {
        return Err(t!("oauth.provider_error", error = error).to_string());
    }
    if started.device_code.is_empty() {
        return Err(t!("oauth.no_device_code").to_string());
    }
    show(Prompt::Device {
        user_code: started.user_code,
        verification_url: started.verification_uri,
        complete_url: started.verification_uri_complete,
    });

    // The provider says how often to ask; 5 seconds is the spec's default.
    let interval = std::time::Duration::from_secs(started.interval.unwrap_or(5).clamp(1, 60));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let attempt = fetch(
            config,
            client,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &started.device_code),
            ],
        )
        .await;
        match attempt {
            Ok(tokens) => return Ok(tokens),
            // "not yet" and "slow down" are the provider asking us to keep waiting.
            Err(message) if message.contains("authorization_pending") || message.contains("slow_down") => {}
            Err(message) => return Err(message),
        }
        if std::time::Instant::now() >= deadline {
            return Err(t!("oauth.sign_in_timeout").to_string());
        }
        crate::transport::on_runtime(async move { tokio::time::sleep(interval).await }).await;
    }
}

fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", form_encode(name), form_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_decode(text: &str) -> String {
    let bytes = text.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[index + 1..index + 3]), 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A random, URL-safe string for PKCE verifiers and state.
fn random_token() -> String {
    let bytes: Vec<u8> = (0..2).flat_map(|_| uuid::Uuid::new_v4().into_bytes()).collect();
    crate::encoding::base64_url_encode(&bytes)
}

/// The PKCE S256 challenge for a verifier.
fn challenge_for(verifier: &str) -> String {
    use sha2::{Digest as _, Sha256};
    crate::encoding::base64_url_encode(&Sha256::digest(verifier.as_bytes()))
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
    fn signs_in_through_the_browser_with_pkce() {
        let (url, seen) = token_server(vec![(
            200,
            r#"{"access_token":"from-code","token_type":"Bearer","expires_in":600,"refresh_token":"r9"}"#,
        )]);
        let mut config = config(url);
        // A public client: no secret, so the client id goes in the body.
        config.client_secret = String::new();
        config.grant = OAuthGrant::AuthorizationCode;
        config.auth_url = "https://id.example/authorize?prompt=consent".into();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // Stand in for the browser: read the URL, then call back with the code.
        let (sent_url, browser) = std::sync::mpsc::channel();
        let tokens = runtime.block_on(async move {
            let (browser_done, wait) = async_channel::bounded::<()>(1);
            std::thread::spawn(move || {
                let url: String = browser.recv().unwrap();
                let query = url.split_once('?').unwrap().1;
                let field = |name: &str| {
                    query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                        .unwrap_or_default()
                        .to_string()
                };
                assert_eq!(field("response_type"), "code");
                assert_eq!(field("code_challenge_method"), "S256");
                assert!(!field("code_challenge").is_empty(), "PKCE challenge is sent");
                assert!(field("redirect_uri").contains("127.0.0.1"), "loopback redirect");
                let redirect = form_decode(&field("redirect_uri"));
                let state = field("state");
                // What the browser does after the person signs in.
                let response = std::process::Command::new("curl")
                    .args([
                        "-s",
                        "-o",
                        "/dev/null",
                        &format!("{redirect}?code=abc123&state={state}"),
                    ])
                    .status();
                assert!(response.is_ok_and(|status| status.success()));
                let _ = browser_done.send_blocking(());
            });
            let tokens = authorization_code(&config, |prompt| {
                let Prompt::Browser(url) = prompt else {
                    panic!("expected a browser prompt")
                };
                sent_url.send(url).unwrap();
            })
            .await;
            let _ = wait.recv().await;
            tokens
        });

        let tokens = tokens.unwrap();
        assert_eq!(tokens.access_token, "from-code");
        let posted = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(posted.contains("grant_type=authorization_code"), "{posted}");
        assert!(posted.contains("code=abc123"), "{posted}");
        assert!(
            posted.contains("code_verifier="),
            "the verifier proves it was us: {posted}"
        );
        assert!(
            posted.contains("client_id=courier"),
            "a public client identifies in the body"
        );
    }

    #[test]
    fn signs_in_with_a_device_code() {
        // The same port answers the device request, then a "not yet", then the token.
        let (url, seen) = token_server(vec![
            (
                200,
                r#"{"device_code":"dev-1","user_code":"WDJB-MJHT","verification_uri":"https://id.example/device","verification_uri_complete":"https://id.example/device?code=WDJB-MJHT","interval":1}"#,
            ),
            (400, r#"{"error":"authorization_pending"}"#),
            (
                200,
                r#"{"access_token":"from-device","token_type":"Bearer","expires_in":600}"#,
            ),
        ]);
        let mut config = config(url.clone());
        config.grant = OAuthGrant::DeviceCode;
        config.auth_url = url;
        config.client_secret = String::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let shown = std::sync::Arc::new(std::sync::Mutex::new(None));
        let recorded = shown.clone();
        let tokens = runtime
            .block_on(device_code(&config, move |prompt| {
                *recorded.lock().unwrap() = Some(prompt);
            }))
            .unwrap();
        assert_eq!(tokens.access_token, "from-device");
        match shown.lock().unwrap().take() {
            Some(Prompt::Device {
                user_code,
                verification_url,
                complete_url,
            }) => {
                assert_eq!(user_code, "WDJB-MJHT");
                assert_eq!(verification_url, "https://id.example/device");
                assert!(complete_url.is_some(), "the shortcut URL is passed on");
            }
            other => panic!("expected a device prompt, got {}", other.is_some()),
        }
        let asked = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(asked.contains("client_id=courier"), "{asked}");
        let polled = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(polled.contains("device_code=dev-1"), "{polled}");
        assert!(
            polled.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
            "{polled}"
        );
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
