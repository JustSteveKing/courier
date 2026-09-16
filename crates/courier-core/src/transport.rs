//! Network I/O on a background tokio runtime, streamed to the UI as [`Event`]s.
//!
//! [`start_http`] and [`start_websocket`] return a [`Handle`] and a channel of events. Dropping
//! the handle cancels the exchange, so a view that forgets a request never leaks a
//! connection. HTTP responses with `Content-Type: text/event-stream` are parsed into
//! Server-Sent Events instead of body chunks.
//!
//! Timeouts cover connecting and waiting for the response head; once a response starts it
//! may stream for as long as the server keeps it open.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eventsource_stream::Eventsource as _;
use futures_util::{SinkExt as _, StreamExt as _};
use rust_i18n::t;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::http::{Request, Upload, UploadValue};
use crate::model::{EffectiveSettings, ProxySetting, content_type_for};

/// What the connection cost, filled in while a request is being sent. A request that
/// reused a pooled connection leaves both times unset.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Phases {
    pub dns: Option<Duration>,
    pub connect: Option<Duration>,
    /// The address actually connected to, for the timing panel.
    pub address: Option<String>,
}

tokio::task_local! {
    /// The send in flight on this task, so the resolver and connector can report to it.
    static PHASES: Arc<Mutex<Phases>>;
}

fn record(write: impl FnOnce(&mut Phases)) {
    let _ = PHASES.try_with(|phases| {
        if let Ok(mut phases) = phases.lock() {
            write(&mut phases);
        }
    });
}

/// Resolves names the way the standard library does, timing it and keeping the address.
struct TimedDns;

impl reqwest::dns::Resolve for TimedDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let started = Instant::now();
            let host = name.as_str().to_string();
            let addresses: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            let elapsed = started.elapsed();
            let first = addresses.first().map(|address| address.ip().to_string());
            record(|phases| {
                phases.dns = Some(elapsed);
                phases.address = first;
            });
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Times opening a connection (TCP, and the TLS handshake for https).
#[derive(Clone)]
struct TimeConnect;

impl<S> tower_layer::Layer<S> for TimeConnect {
    type Service = TimedConnect<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TimedConnect { inner }
    }
}

#[derive(Clone)]
struct TimedConnect<S> {
    inner: S,
}

impl<S, Request> tower_service::Service<Request> for TimedConnect<S>
where
    S: tower_service::Service<Request>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = TimedConnecting<S::Future>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        TimedConnecting {
            inner: self.inner.call(request),
            started: Instant::now(),
        }
    }
}

pin_project_lite::pin_project! {
    struct TimedConnecting<F> {
        #[pin]
        inner: F,
        started: Instant,
    }
}

impl<F, T, E> std::future::Future for TimedConnecting<F>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    type Output = Result<T, E>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        let this = self.project();
        let started = *this.started;
        let ready = this.inner.poll(cx);
        if let std::task::Poll::Ready(Ok(_)) = &ready {
            let elapsed = started.elapsed();
            // Connecting includes resolving, which is timed separately.
            record(|phases| phases.connect = Some(elapsed.saturating_sub(phases.dns.unwrap_or_default())));
        }
        ready
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The status line and headers arrived.
    Head {
        status: u16,
        reason: String,
        headers: Vec<(String, String)>,
        elapsed: Duration,
        /// The body is a Server-Sent Events stream; expect [`Event::Sse`] rather than chunks.
        event_stream: bool,
        /// What the connection cost, when this request opened one.
        phases: Phases,
    },
    /// Part of a plain response body.
    Chunk(Vec<u8>),
    /// One Server-Sent Event.
    Sse(SseEvent),
    /// A WebSocket message, received or sent.
    Ws(WsMessage),
    /// The exchange ended normally.
    Done {
        elapsed: Duration,
        bytes: usize,
    },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub id: String,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WsMessage {
    pub outgoing: bool,
    pub payload: WsPayload,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WsPayload {
    Text(String),
    Binary(Vec<u8>),
    /// The connection was closed, with the close reason if one was given.
    Close(String),
}

/// Keeps an exchange alive. Drop it to cancel.
pub struct Handle {
    _cancel: oneshot::Sender<()>,
    outgoing: Option<mpsc::UnboundedSender<WsPayload>>,
}

impl Handle {
    /// Queues a WebSocket message (or `Close`). Returns false if this isn't a WebSocket or it
    /// has already closed.
    pub fn send(&self, payload: WsPayload) -> bool {
        self.outgoing.as_ref().is_some_and(|tx| tx.send(payload).is_ok())
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("courier-net")
            .enable_all()
            .build()
            .expect("could not start the network runtime")
    })
}

/// One client for the whole app, so repeated requests to a host reuse its connections.
/// Runs `work` on the HTTP runtime and waits for it, so callers outside tokio (the app's
/// executors, a CLI thread) can await reqwest futures.
pub async fn on_runtime<F, T>(work: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (send, receive) = async_channel::bounded(1);
    runtime().spawn(async move {
        let _ = send.send(work.await).await;
    });
    receive.recv().await.expect("the HTTP runtime dropped the work")
}

/// The shared client, for side requests like fetching an OAuth token.
pub fn plain_client() -> &'static reqwest::Client {
    client()
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let _runtime = runtime().enter();
        reqwest::Client::builder()
            .dns_resolver(std::sync::Arc::new(TimedDns))
            .connector_layer(TimeConnect)
            .build()
            .expect("could not create the HTTP client")
    })
}

/// Runs `work` on the runtime until it finishes or the handle is dropped.
fn spawn<F>(
    outgoing: Option<mpsc::UnboundedSender<WsPayload>>,
    work: impl FnOnce(async_channel::Sender<Event>) -> F,
) -> (Handle, async_channel::Receiver<Event>)
where
    F: Future<Output = Result<(), String>> + Send + 'static,
{
    let (events_tx, events_rx) = async_channel::unbounded();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let work = work(events_tx.clone());
    runtime().spawn(async move {
        tokio::select! {
            // Resolves when the handle is dropped.
            _ = cancel_rx => {}
            result = work => {
                if let Err(message) = result {
                    let _ = events_tx.send(Event::Failed(message)).await;
                }
            }
        }
    });
    (
        Handle {
            _cancel: cancel_tx,
            outgoing,
        },
        events_rx,
    )
}

/// "error: cause: cause" for errors whose `Display` hides the useful part.
fn describe(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !message.contains(&cause_text) {
            message.push_str(": ");
            message.push_str(&cause_text);
        }
        source = cause.source();
    }
    message
}

fn headers_of(map: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .map(|(name, value)| (name.to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
        .collect()
}

/// Sends an HTTP request. `last_event_id` resumes a Server-Sent Events stream.
/// A client that stores response cookies in `store` and sends them with matching requests.
/// Everything an HTTP client is built from. Clients are cached per distinct value, so requests
/// with the same settings share connections.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClientOptions {
    pub follow_redirects: bool,
    pub max_redirects: u32,
    pub verify_tls: bool,
    pub proxy: ProxySetting,
    /// Extra trusted certificate authorities (PEM).
    pub ca_pem: Option<Vec<u8>>,
    /// Client certificate and key for mutual TLS (PEM).
    pub identity_pem: Option<Vec<u8>>,
    pub unix_socket: Option<PathBuf>,
}

impl ClientOptions {
    /// Reads any certificate files the settings name.
    pub fn load(settings: &EffectiveSettings) -> Result<Self, String> {
        let read = |path: &PathBuf, what: &str| {
            std::fs::read(path).map_err(|e| format!("could not read the {what} {}: {e}", path.display()))
        };
        let ca_pem = settings
            .ca_certificate
            .as_ref()
            .map(|p| read(p, "CA certificate"))
            .transpose()?;
        let identity_pem = match (&settings.client_certificate, &settings.client_key) {
            (Some(cert), key) => {
                let mut pem = read(cert, "client certificate")?;
                if let Some(key) = key {
                    pem.push(b'\n');
                    pem.extend(read(key, "client key")?);
                }
                Some(pem)
            }
            (None, Some(_)) => return Err("a client key needs a client certificate too".into()),
            (None, None) => None,
        };
        Ok(Self {
            follow_redirects: settings.follow_redirects,
            max_redirects: settings.max_redirects,
            verify_tls: settings.verify_tls,
            proxy: settings.proxy.clone(),
            ca_pem,
            identity_pem,
            unix_socket: settings.unix_socket.clone(),
        })
    }
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            follow_redirects: true,
            max_redirects: 10,
            verify_tls: true,
            proxy: ProxySetting::System,
            ca_pem: None,
            identity_pem: None,
            unix_socket: None,
        }
    }
}

type Cookies = Arc<reqwest_cookie_store::CookieStoreMutex>;

/// A client for `options`, storing and sending cookies in `cookies` when given.
pub fn client_for(options: &ClientOptions, cookies: Option<&Cookies>) -> Result<reqwest::Client, String> {
    static CLIENTS: OnceLock<Mutex<HashMap<(ClientOptions, usize), reqwest::Client>>> = OnceLock::new();
    let key = (options.clone(), cookies.map_or(0, |c| Arc::as_ptr(c) as usize));
    let clients = CLIENTS.get_or_init(Default::default);
    if let Some(client) = clients.lock().unwrap().get(&key) {
        return Ok(client.clone());
    }
    let _runtime = runtime().enter();
    let mut builder = reqwest::Client::builder()
        .redirect(if options.follow_redirects {
            reqwest::redirect::Policy::limited(options.max_redirects as usize)
        } else {
            reqwest::redirect::Policy::none()
        })
        .danger_accept_invalid_certs(!options.verify_tls);
    builder = match &options.proxy {
        ProxySetting::System => builder,
        ProxySetting::None => builder.no_proxy(),
        ProxySetting::Url(url) => {
            builder.proxy(reqwest::Proxy::all(url).map_err(|e| format!("invalid proxy {url}: {e}"))?)
        }
    };
    if let Some(pem) = &options.ca_pem {
        for certificate in
            reqwest::Certificate::from_pem_bundle(pem).map_err(|e| format!("invalid CA certificate: {e}"))?
        {
            builder = builder.add_root_certificate(certificate);
        }
    }
    if let Some(pem) = &options.identity_pem {
        builder = builder
            .identity(reqwest::Identity::from_pem(pem).map_err(|e| format!("invalid client certificate or key: {e}"))?);
    }
    #[cfg(unix)]
    if let Some(socket) = &options.unix_socket {
        builder = builder.unix_socket(socket.clone());
    }
    if let Some(cookies) = cookies {
        builder = builder.cookie_provider(cookies.clone());
    }
    let client = builder
        .dns_resolver(std::sync::Arc::new(TimedDns))
        .connector_layer(TimeConnect)
        .build()
        .map_err(|e| format!("could not set up the connection: {}", describe(&e)))?;
    clients.lock().unwrap().insert(key, client.clone());
    Ok(client)
}

/// Reads a file to upload, saying which file is missing rather than just "not found".
async fn read_upload(path: &std::path::Path) -> Result<Vec<u8>, String> {
    tokio::fs::read(path).await.map_err(|e| {
        t!(
            "request.upload_unreadable",
            path = path.display(),
            error = e.to_string()
        )
        .to_string()
    })
}

/// Sends an HTTP request, with `client` (e.g. one with a cookie jar) or the shared one.
pub fn start_http(
    request: Request,
    head_timeout: Duration,
    last_event_id: Option<String>,
    client: Option<reqwest::Client>,
) -> (Handle, async_channel::Receiver<Event>) {
    spawn(None, move |events| async move {
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| e.to_string())?;
        let client = client.unwrap_or_else(|| self::client().clone());
        let mut builder = client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(id) = last_event_id {
            builder = builder.header("Last-Event-ID", id);
        }
        if !request.body.is_empty() {
            builder = builder.body(request.body.clone());
        }
        match &request.upload {
            Some(Upload::File(path)) => {
                builder = builder.body(read_upload(path).await?);
            }
            Some(Upload::Multipart(parts)) => {
                let mut form = reqwest::multipart::Form::new();
                for part in parts {
                    form = match &part.value {
                        UploadValue::Text(text) => form.text(part.name.clone(), text.clone()),
                        UploadValue::File(path) => {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| part.name.clone());
                            let field = reqwest::multipart::Part::bytes(read_upload(path).await?)
                                .file_name(name)
                                .mime_str(content_type_for(&path.to_string_lossy()))
                                .map_err(|e| e.to_string())?;
                            form.part(part.name.clone(), field)
                        }
                    };
                }
                builder = builder.multipart(form);
            }
            None => {}
        }

        let started = Instant::now();
        let phases = Arc::new(Mutex::new(Phases::default()));
        let response = PHASES
            .scope(phases.clone(), tokio::time::timeout(head_timeout, builder.send()))
            .await
            .map_err(|_| format!("timed out after {}s waiting for a response", head_timeout.as_secs()))?
            .map_err(|e| describe(&e))?;
        let phases = phases.lock().map(|phases| phases.clone()).unwrap_or_default();

        let status = response.status();
        let event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/event-stream"));
        let head = Event::Head {
            status: status.as_u16(),
            reason: status.canonical_reason().unwrap_or_default().to_string(),
            headers: headers_of(response.headers()),
            elapsed: started.elapsed(),
            event_stream,
            phases,
        };
        if events.send(head).await.is_err() {
            return Ok(());
        }

        let mut bytes = 0;
        if event_stream {
            let mut stream = response.bytes_stream().inspect(|chunk| {
                if let Ok(chunk) = chunk {
                    bytes += chunk.len();
                }
            });
            let mut stream = (&mut stream).eventsource();
            while let Some(event) = stream.next().await {
                let event = event.map_err(|e| describe(&e))?;
                let event = SseEvent {
                    event: event.event,
                    id: event.id,
                    data: event.data,
                };
                if events.send(Event::Sse(event)).await.is_err() {
                    return Ok(());
                }
            }
        } else {
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| describe(&e))?;
                bytes += chunk.len();
                if events.send(Event::Chunk(chunk.to_vec())).await.is_err() {
                    return Ok(());
                }
            }
        }
        let _ = events
            .send(Event::Done {
                elapsed: started.elapsed(),
                bytes,
            })
            .await;
        Ok(())
    })
}

/// Opens a WebSocket (`ws://` or `wss://`). Send messages with [`Handle::send`].
pub fn start_websocket(request: Request, connect_timeout: Duration) -> (Handle, async_channel::Receiver<Event>) {
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<WsPayload>();
    spawn(Some(outgoing_tx), move |events| async move {
        let mut http_request = request.url.as_str().into_client_request().map_err(|e| describe(&e))?;
        for (name, value) in &request.headers {
            let name: tokio_tungstenite::tungstenite::http::HeaderName =
                name.parse().map_err(|_| format!("invalid header name: {name}"))?;
            let value = HeaderValue::from_str(value).map_err(|_| format!("invalid value for header {name}"))?;
            http_request.headers_mut().insert(name, value);
        }

        let started = Instant::now();
        let (socket, response) = tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(http_request))
            .await
            .map_err(|_| format!("timed out after {}s connecting", connect_timeout.as_secs()))?
            .map_err(|e| describe(&e))?;
        let head = Event::Head {
            status: response.status().as_u16(),
            reason: response.status().canonical_reason().unwrap_or_default().to_string(),
            headers: response
                .headers()
                .iter()
                .map(|(n, v)| (n.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect(),
            elapsed: started.elapsed(),
            event_stream: false,
            phases: Phases::default(),
        };
        if events.send(head).await.is_err() {
            return Ok(());
        }

        let (mut write, mut read) = socket.split();
        let mut bytes = 0;
        loop {
            tokio::select! {
                incoming = read.next() => {
                    let payload = match incoming {
                        Some(Ok(Message::Text(text))) => WsPayload::Text(text.as_str().to_string()),
                        Some(Ok(Message::Binary(data))) => WsPayload::Binary(data.to_vec()),
                        Some(Ok(Message::Close(frame))) => {
                            let reason = frame.map(|f| f.reason.as_str().to_string()).unwrap_or_default();
                            let _ = events.send(Event::Ws(WsMessage { outgoing: false, payload: WsPayload::Close(reason) })).await;
                            break;
                        }
                        Some(Ok(_)) => continue, // pings and pongs are answered by tungstenite
                        Some(Err(e)) => return Err(describe(&e)),
                        None => break,
                    };
                    bytes += payload_len(&payload);
                    if events.send(Event::Ws(WsMessage { outgoing: false, payload })).await.is_err() {
                        return Ok(());
                    }
                }
                outgoing = outgoing_rx.recv() => {
                    let Some(payload) = outgoing else { break };
                    let message = match &payload {
                        WsPayload::Text(text) => Message::Text(text.clone().into()),
                        WsPayload::Binary(data) => Message::Binary(data.clone().into()),
                        WsPayload::Close(_) => Message::Close(None),
                    };
                    let closing = matches!(payload, WsPayload::Close(_));
                    write.send(message).await.map_err(|e| describe(&e))?;
                    bytes += payload_len(&payload);
                    let _ = events.send(Event::Ws(WsMessage { outgoing: true, payload })).await;
                    if closing {
                        // Wait for the server's close reply, then finish.
                        while let Some(Ok(message)) = read.next().await {
                            if matches!(message, Message::Close(_)) {
                                break;
                            }
                        }
                        break;
                    }
                }
            }
        }
        let _ = events
            .send(Event::Done {
                elapsed: started.elapsed(),
                bytes,
            })
            .await;
        Ok(())
    })
}

fn payload_len(payload: &WsPayload) -> usize {
    match payload {
        WsPayload::Text(text) => text.len(),
        WsPayload::Binary(data) => data.len(),
        WsPayload::Close(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    use super::*;
    use crate::http::UploadPart;

    fn get(url: &str) -> Request {
        Request {
            method: "GET".into(),
            url: url.into(),
            headers: vec![],
            body: String::new(),
            ..Default::default()
        }
    }

    /// A self-signed certificate authority and a server certificate for 127.0.0.1 it signed,
    /// plus a client certificate it signed: (ca pem, server cert, server key, client pem).
    fn test_pki() -> (String, rcgen::CertifiedKey<rcgen::KeyPair>, String) {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params, ca_key);

        let server_key = rcgen::KeyPair::generate().unwrap();
        let mut server_params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        server_params.subject_alt_names = vec![rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap())];
        let server = server_params.signed_by(&server_key, &issuer).unwrap();

        let client_key = rcgen::KeyPair::generate().unwrap();
        let mut client_params = rcgen::CertificateParams::new(vec!["client".to_string()]).unwrap();
        client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let client = client_params.signed_by(&client_key, &issuer).unwrap();
        let client_pem = format!("{}{}", client.pem(), client_key.serialize_pem());
        (
            ca.pem(),
            rcgen::CertifiedKey {
                cert: server,
                signing_key: server_key,
            },
            client_pem,
        )
    }

    /// A TLS server on 127.0.0.1 answering every request with 204, requiring a client
    /// certificate signed by `client_ca` when given.
    fn tls_server(server: &rcgen::CertifiedKey<rcgen::KeyPair>, client_ca: Option<&str>) -> u16 {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let cert = CertificateDer::from(server.cert.der().to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server.signing_key.serialize_der()));
        let builder = rustls::ServerConfig::builder();
        let config = match client_ca {
            Some(pem) => {
                let mut roots = rustls::RootCertStore::empty();
                for cert in rustls::pki_types::pem::PemObject::pem_slice_iter(pem.as_bytes()) {
                    roots.add(cert.unwrap()).unwrap();
                }
                let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                    .build()
                    .unwrap();
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        }
        .with_single_cert(vec![cert], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        runtime().spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut buf = [0u8; 2048];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        port
    }

    fn send_with(options: &ClientOptions, url: &str) -> Vec<Event> {
        let client = client_for(options, None).unwrap();
        let (_handle, events) = start_http(get(url), Duration::from_secs(5), None, Some(client));
        collect(&events, Duration::from_secs(5))
    }

    #[test]
    fn tls_verification_custom_cas_and_client_certificates() {
        let (ca_pem, server, client_pem) = test_pki();
        let port = tls_server(&server, None);
        let url = format!("https://127.0.0.1:{port}/");
        let ok = |events: &[Event]| matches!(events.first(), Some(Event::Head { status: 204, .. }));

        let events = send_with(&ClientOptions::default(), &url);
        assert!(
            matches!(events.last(), Some(Event::Failed(_))),
            "untrusted by default: {events:?}"
        );
        let insecure = ClientOptions {
            verify_tls: false,
            ..Default::default()
        };
        assert!(ok(&send_with(&insecure, &url)), "verification off");
        let trusted = ClientOptions {
            ca_pem: Some(ca_pem.clone().into_bytes()),
            ..Default::default()
        };
        assert!(ok(&send_with(&trusted, &url)), "with the CA");

        let mtls_port = tls_server(&server, Some(&ca_pem));
        let mtls_url = format!("https://127.0.0.1:{mtls_port}/");
        let events = send_with(&trusted, &mtls_url);
        assert!(!ok(&events), "a client certificate is required: {events:?}");
        let with_identity = ClientOptions {
            identity_pem: Some(client_pem.into_bytes()),
            ..trusted
        };
        assert!(ok(&send_with(&with_identity, &mtls_url)), "with a client certificate");
    }

    #[test]
    fn redirects_can_be_left_alone() {
        let port = serve_once(|mut stream, _| {
            stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let options = ClientOptions {
            follow_redirects: false,
            ..Default::default()
        };
        let events = send_with(&options, &format!("http://127.0.0.1:{port}/"));
        assert!(
            matches!(events.first(), Some(Event::Head { status: 302, .. })),
            "{events:?}"
        );
    }

    #[test]
    fn sends_over_unix_sockets() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let options = ClientOptions {
            unix_socket: Some(socket),
            ..Default::default()
        };
        let events = send_with(&options, "http://localhost/v1/containers");
        assert!(
            matches!(events.first(), Some(Event::Head { status: 200, .. })),
            "{events:?}"
        );
    }

    /// Collects events until the channel closes or `timeout` passes.
    fn collect(events: &async_channel::Receiver<Event>, timeout: Duration) -> Vec<Event> {
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match events.try_recv() {
                Ok(event) => {
                    let end = matches!(event, Event::Done { .. } | Event::Failed(_));
                    out.push(event);
                    if end {
                        break;
                    }
                }
                Err(async_channel::TryRecvError::Closed) => break,
                Err(async_channel::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        out
    }

    /// Serves one connection: reads the request head, then runs `respond` on the stream.
    fn serve_once(respond: impl FnOnce(std::net::TcpStream, String) + Send + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
            }
            respond(stream, String::from_utf8_lossy(&head).into_owned());
        });
        port
    }

    /// Like `serve_once`, but reads the request body too and hands back head and body.
    fn serve_once_reading_body(reply: &'static str) -> (u16, std::sync::mpsc::Receiver<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = Vec::new();
            let mut buf = [0u8; 4096];
            let mut head_end = None;
            let mut length = 0;
            loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                buffer.extend_from_slice(&buf[..n]);
                if head_end.is_none()
                    && let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    let head = String::from_utf8_lossy(&buffer[..at]).into_owned();
                    length = head
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                        })
                        .map(|(_, v)| v.trim().parse().unwrap())
                        .unwrap_or(0);
                    head_end = Some(at + 4);
                }
                if let Some(at) = head_end
                    && buffer.len() >= at + length
                {
                    break;
                }
            }
            let at = head_end.unwrap_or(buffer.len());
            let head = String::from_utf8_lossy(&buffer[..at]).into_owned();
            stream.write_all(reply.as_bytes()).unwrap();
            sent.send((head, buffer[at..].to_vec())).unwrap();
        });
        (port, received)
    }

    fn post_upload(url: &str, upload: Upload) -> Request {
        Request {
            method: "POST".into(),
            url: url.into(),
            headers: vec![],
            body: String::new(),
            upload: Some(upload),
        }
    }

    #[test]
    fn sends_a_multipart_body_with_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "hi there").unwrap();
        let (port, received) = serve_once_reading_body("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let request = post_upload(
            &format!("http://127.0.0.1:{port}/upload"),
            Upload::Multipart(vec![
                UploadPart {
                    name: "name".into(),
                    value: UploadValue::Text("Rex".into()),
                },
                UploadPart {
                    name: "photo".into(),
                    value: UploadValue::File(file),
                },
            ]),
        );
        let (_handle, events) = start_http(request, Duration::from_secs(5), None, None);
        collect(&events, Duration::from_secs(5));

        let (head, body) = received.recv_timeout(Duration::from_secs(5)).unwrap();
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: multipart/form-data; boundary="),
            "{head}"
        );
        assert!(body.contains("name=\"name\"") && body.contains("Rex"), "{body}");
        assert!(
            body.contains("name=\"photo\"") && body.contains("filename=\"hello.txt\"") && body.contains("hi there"),
            "the file's name and bytes are in the part:\n{body}"
        );
        assert!(
            body.contains("Content-Type: text/plain"),
            "the part's type is guessed:\n{body}"
        );
    }

    #[test]
    fn sends_a_file_as_the_whole_body() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("payload.json");
        std::fs::write(&file, "{\"id\":7}").unwrap();
        let (port, received) = serve_once_reading_body("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let (_handle, events) = start_http(
            post_upload(&format!("http://127.0.0.1:{port}/raw"), Upload::File(file)),
            Duration::from_secs(5),
            None,
            None,
        );
        collect(&events, Duration::from_secs(5));

        let (_head, body) = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "{\"id\":7}");
    }

    #[test]
    fn a_missing_upload_says_which_file() {
        let port = serve_once(|mut stream, _| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let (_handle, events) = start_http(
            post_upload(
                &format!("http://127.0.0.1:{port}/raw"),
                Upload::File("/nope/missing.bin".into()),
            ),
            Duration::from_secs(5),
            None,
            None,
        );
        let events = collect(&events, Duration::from_secs(5));
        assert!(
            matches!(&events[..], [Event::Failed(message)] if message.contains("missing.bin")),
            "{events:?}"
        );
    }

    #[test]
    fn reports_where_the_time_went() {
        let port = serve_once(|mut stream, _| {
            std::thread::sleep(Duration::from_millis(60));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi")
                .unwrap();
        });
        // "localhost" so the name is actually resolved, unlike a literal address.
        let (_handle, events) = start_http(
            get(&format!("http://localhost:{port}/")),
            Duration::from_secs(5),
            None,
            None,
        );
        let events = collect(&events, Duration::from_secs(5));
        let Some(Event::Head { elapsed, phases, .. }) = events.iter().find(|e| matches!(e, Event::Head { .. })) else {
            panic!("no head: {events:?}");
        };
        assert!(phases.dns.is_some(), "the name was resolved: {phases:?}");
        assert!(phases.connect.is_some(), "a connection was opened: {phases:?}");
        assert!(
            phases
                .address
                .as_deref()
                .is_some_and(|a| a.contains("127.0.0.1") || a.contains("::1")),
            "{phases:?}"
        );
        let connection = phases.dns.unwrap_or_default() + phases.connect.unwrap_or_default();
        assert!(
            connection <= *elapsed,
            "connecting is part of the first byte: {connection:?} > {elapsed:?}"
        );
        assert!(
            *elapsed >= Duration::from_millis(50),
            "waiting for the slow server counts: {elapsed:?}"
        );
    }

    #[test]
    fn streams_a_chunked_body() {
        let port = serve_once(|mut stream, _| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n")
                .unwrap();
            for part in ["{\"a\":", "1}"] {
                write!(stream, "{:x}\r\n{part}\r\n", part.len()).unwrap();
                stream.flush().unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
            stream.write_all(b"0\r\n\r\n").unwrap();
        });
        let (_handle, events) = start_http(
            get(&format!("http://127.0.0.1:{port}/")),
            Duration::from_secs(5),
            None,
            None,
        );
        let events = collect(&events, Duration::from_secs(5));

        assert!(
            matches!(
                &events[0],
                Event::Head {
                    status: 200,
                    event_stream: false,
                    ..
                }
            ),
            "{events:?}"
        );
        let body: Vec<u8> = events
            .iter()
            .filter_map(|e| if let Event::Chunk(c) = e { Some(c.clone()) } else { None })
            .flatten()
            .collect();
        assert_eq!(body, b"{\"a\":1}");
        assert!(
            matches!(events.last(), Some(Event::Done { bytes: 7, .. })),
            "{events:?}"
        );
    }

    #[test]
    fn parses_server_sent_events_and_resumes_with_last_event_id() {
        let port = serve_once(|mut stream, head| {
            assert!(head.to_ascii_lowercase().contains("last-event-id: 41"), "{head}");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream
                .write_all(b": comment\n\nid: 42\nevent: price\ndata: {\"usd\": 1}\n\n")
                .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            stream.write_all(b"data: line one\ndata: line two\n\n").unwrap();
        });
        let (_handle, events) = start_http(
            get(&format!("http://127.0.0.1:{port}/stream")),
            Duration::from_secs(5),
            Some("41".into()),
            None,
        );
        let events = collect(&events, Duration::from_secs(5));

        assert!(
            matches!(&events[0], Event::Head { event_stream: true, .. }),
            "{events:?}"
        );
        let sse: Vec<_> = events
            .iter()
            .filter_map(|e| if let Event::Sse(s) = e { Some(s.clone()) } else { None })
            .collect();
        assert_eq!(
            sse,
            vec![
                SseEvent {
                    event: "price".into(),
                    id: "42".into(),
                    data: "{\"usd\": 1}".into()
                },
                SseEvent {
                    event: "message".into(),
                    id: "42".into(),
                    data: "line one\nline two".into()
                },
            ]
        );
        assert!(matches!(events.last(), Some(Event::Done { .. })), "{events:?}");
    }

    #[test]
    fn times_out_waiting_for_the_head() {
        let port = serve_once(|stream, _| {
            std::thread::sleep(Duration::from_secs(2));
            drop(stream);
        });
        let (_handle, events) = start_http(
            get(&format!("http://127.0.0.1:{port}/")),
            Duration::from_millis(200),
            None,
            None,
        );
        let events = collect(&events, Duration::from_secs(5));
        assert!(
            matches!(&events[..], [Event::Failed(message)] if message.contains("timed out")),
            "{events:?}"
        );
    }

    #[test]
    fn dropping_the_handle_cancels() {
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let port = serve_once(move |mut stream, _| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .unwrap();
            // Keep writing until the client goes away.
            loop {
                if stream.write_all(b"data: tick\n\n").is_err() || stream.flush().is_err() {
                    closed_tx.send(()).unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let (handle, events) = start_http(
            get(&format!("http://127.0.0.1:{port}/")),
            Duration::from_secs(5),
            None,
            None,
        );
        let first = collect(&events, Duration::from_millis(300));
        assert!(first.iter().any(|e| matches!(e, Event::Sse(_))), "{first:?}");

        drop(handle);
        assert!(
            closed_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "server saw the connection close"
        );
    }

    #[test]
    fn websocket_round_trip() {
        let listener = runtime()
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        runtime().spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Text(text) => socket
                        .send(Message::Text(format!("echo: {}", text.as_str()).into()))
                        .await
                        .unwrap(),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let (handle, events) = start_websocket(get(&format!("ws://127.0.0.1:{port}/")), Duration::from_secs(5));
        let head = collect(&events, Duration::from_millis(500));
        assert!(matches!(&head[0], Event::Head { status: 101, .. }), "{head:?}");

        assert!(handle.send(WsPayload::Text("hello".into())));
        std::thread::sleep(Duration::from_millis(200));
        assert!(handle.send(WsPayload::Close(String::new())));
        let events = collect(&events, Duration::from_secs(5));
        let messages: Vec<_> = events
            .iter()
            .filter_map(|e| if let Event::Ws(m) = e { Some(m.clone()) } else { None })
            .collect();
        assert_eq!(
            messages[..2],
            [
                WsMessage {
                    outgoing: true,
                    payload: WsPayload::Text("hello".into())
                },
                WsMessage {
                    outgoing: false,
                    payload: WsPayload::Text("echo: hello".into())
                },
            ]
        );
        assert!(matches!(events.last(), Some(Event::Done { .. })), "{events:?}");
    }
}
