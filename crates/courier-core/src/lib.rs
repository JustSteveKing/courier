//! Courier's engine: the collection file format, loading and saving, resolving variables,
//! auth and settings, chaining, secrets, cookies, the response cache and sending over HTTP,
//! Server-Sent Events and WebSockets. It has no UI, so the app and the command-line tool
//! behave the same.

pub mod chain;
pub mod cookies;
pub mod credentials;
pub mod encoding;
pub mod graphql;
pub mod http;
pub mod import;
pub mod model;
pub mod paths;
pub mod project;
pub mod response_cache;
pub mod secret_store;
pub mod storage;
pub mod template_assist;
pub mod transport;

rust_i18n::i18n!("../../locales", fallback = "en");
