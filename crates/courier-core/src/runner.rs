//! Running many requests in order: a folder, a collection or a chosen list. Each request is
//! sent with its inherited auth and settings, can use earlier responses through
//! `{{ response() }}`, and has its checks evaluated. Shared by the app's runner and the CLI.

use std::path::{Path, PathBuf};

use rust_i18n::t;
use serde::Serialize;

use crate::chain::{self, Context, Sent};
use crate::checks::{self, CheckResult};
use crate::model::{RequestFile, RequestKind};
use crate::response_cache::{Outcome, StoredResponse};
use crate::storage::{self, Collection};

/// How one request in a run went.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum RunStatus {
    /// Sent, and every check passed (or it has none).
    Passed,
    /// Sent, and at least one check failed.
    Failed,
    /// It couldn't be sent: a resolution error, a network failure.
    Error { message: String },
    /// Not something a run can send, like a WebSocket.
    Skipped { reason: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct RunResult {
    /// Where the request is, relative to the collection.
    pub request: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub name: String,
    pub method: String,
    #[serde(flatten)]
    pub status: RunStatus,
    /// The response status code, when there was a response.
    pub response_status: Option<u16>,
    pub elapsed_ms: Option<u64>,
    pub checks: Vec<CheckOutcome>,
    #[serde(skip)]
    pub response: Option<StoredResponse>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CheckOutcome {
    pub check: String,
    pub passed: bool,
    pub actual: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<CheckResult> for CheckOutcome {
    fn from(result: CheckResult) -> Self {
        Self {
            check: result.line,
            passed: result.passed,
            actual: result.actual,
            error: result.error,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RunSummary {
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    pub skipped: usize,
    pub elapsed_ms: u64,
    /// The run stopped at the first failure before reaching every request.
    pub stopped_early: bool,
}

impl RunSummary {
    pub fn add(&mut self, result: &RunResult) {
        match result.status {
            RunStatus::Passed => self.passed += 1,
            RunStatus::Failed => self.failed += 1,
            RunStatus::Error { .. } => self.errors += 1,
            RunStatus::Skipped { .. } => self.skipped += 1,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.failed == 0 && self.errors == 0
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RunOptions {
    /// Stop at the first failed or erroring request.
    pub stop_on_failure: bool,
}

/// The requests under `scope` (the collection root, a folder or one request file), in
/// sidebar order.
pub fn requests_in(collection: &Collection, scope: &Path) -> Vec<(PathBuf, RequestFile)> {
    collection
        .requests()
        .into_iter()
        .filter(|entry| entry.path.starts_with(scope))
        .map(|entry| (entry.path.to_path_buf(), entry.request.clone()))
        .collect()
}

/// Sends `requests` in order, calling `on_result` after each. Responses are shared, so a
/// later request's `{{ response("Login", …) }}` uses the one sent earlier in this run.
pub async fn run(
    requests: &[(PathBuf, RequestFile)],
    context: &Context,
    options: RunOptions,
    mut on_result: impl FnMut(&RunResult),
) -> RunSummary {
    let started = std::time::Instant::now();
    let mut summary = RunSummary::default();
    let mut sent = Sent::new();
    for (index, (path, file)) in requests.iter().enumerate() {
        let result = run_one(path, file, context, &mut sent).await;
        summary.add(&result);
        on_result(&result);
        let failed = matches!(result.status, RunStatus::Failed | RunStatus::Error { .. });
        if failed && options.stop_on_failure {
            summary.stopped_early = index + 1 < requests.len();
            break;
        }
    }
    summary.elapsed_ms = started.elapsed().as_millis() as u64;
    summary
}

async fn run_one(path: &Path, file: &RequestFile, context: &Context, sent: &mut Sent) -> RunResult {
    let mut result = RunResult {
        request: path.strip_prefix(&context.root).unwrap_or(path).display().to_string(),
        path: path.to_path_buf(),
        name: file.name.clone(),
        method: match RequestKind::of(file) {
            RequestKind::Graphql => "GQL".into(),
            RequestKind::WebSocket => "WS".into(),
            RequestKind::EventStream => "SSE".into(),
            RequestKind::Grpc => "gRPC".into(),
            RequestKind::Http => file.method.clone(),
        },
        status: RunStatus::Passed,
        response_status: None,
        elapsed_ms: None,
        checks: Vec::new(),
        response: None,
    };
    match RequestKind::of(file) {
        RequestKind::WebSocket => {
            result.status = RunStatus::Skipped {
                reason: t!("run.skip_websocket").to_string(),
            };
            return result;
        }
        RequestKind::EventStream => {
            result.status = RunStatus::Skipped {
                reason: t!("run.skip_event_stream").to_string(),
            };
            return result;
        }
        RequestKind::Grpc => {
            result.status = RunStatus::Skipped {
                reason: t!("run.skip_grpc").to_string(),
            };
            return result;
        }
        RequestKind::Http | RequestKind::Graphql => {}
    }
    let response = match chain::send_path(path, context, sent).await {
        Ok(response) => response,
        Err(message) => {
            result.status = RunStatus::Error { message };
            return result;
        }
    };
    sent.retain(|(p, _)| p != path);
    sent.push((path.to_path_buf(), response.clone()));
    if let Outcome::Response { status, .. } = &response.outcome {
        result.response_status = Some(*status);
    }
    result.elapsed_ms = Some(response.elapsed_ms);
    result.checks = checks::evaluate(&file.checks, &response, &context.variables)
        .into_iter()
        .map(CheckOutcome::from)
        .collect();
    if result.checks.iter().any(|c| !c.passed) {
        result.status = RunStatus::Failed;
    }
    result.response = Some(response);
    result
}

/// A collection's requests for a run from a CLI-style target: nothing (the whole
/// collection), a folder or a request, by path relative to the collection or project.
pub fn resolve_target(collection: &Collection, target: Option<&str>) -> Result<Vec<(PathBuf, RequestFile)>, String> {
    let Some(target) = target.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(requests_in(collection, &collection.root));
    };
    let project = crate::project::project_dir(&collection.root);
    let candidates = [
        collection.root.join(target),
        collection.root.join(format!("{target}.yaml")),
        project.join(target),
    ];
    if let Some(scope) = candidates.iter().find(|p| p.exists()) {
        let requests = requests_in(collection, scope);
        return if requests.is_empty() {
            Err(format!("no requests in {target}"))
        } else {
            Ok(requests)
        };
    }
    let path = chain::find_request(&collection.root, target)?;
    Ok(requests_in(collection, &path))
}

/// Loads the collection holding `start` (a project folder or anything inside it).
pub fn load_collection_for(start: &Path) -> Result<Collection, String> {
    let root = crate::project::find(start).ok_or_else(|| {
        format!(
            "no Courier collection (.courier folder) in {} or its parents",
            start.display()
        )
    })?;
    storage::load_collection(&root).map_err(|e| format!("{e:#}"))
}

/// A JUnit XML report, for CI systems that show test results.
pub fn junit(collection_name: &str, results: &[RunResult], summary: &RunSummary) -> String {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">\n",
        escape(collection_name),
        results.len(),
        summary.failed,
        summary.errors,
        summary.skipped,
        summary.elapsed_ms as f64 / 1000.0
    ));
    for result in results {
        xml.push_str(&format!(
            "  <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\">",
            escape(&result.request),
            escape(&format!("{} {}", result.method, result.name)),
            result.elapsed_ms.unwrap_or(0) as f64 / 1000.0
        ));
        match &result.status {
            RunStatus::Passed => {}
            RunStatus::Failed => {
                let failures: Vec<String> = result
                    .checks
                    .iter()
                    .filter(|c| !c.passed)
                    .map(|c| match &c.error {
                        Some(error) => format!("{}: {error}", c.check),
                        None => format!("{} (got {})", c.check, c.actual),
                    })
                    .collect();
                xml.push_str(&format!(
                    "\n    <failure message=\"{} of {} checks failed\">{}</failure>\n  ",
                    failures.len(),
                    result.checks.len(),
                    escape(&failures.join("\n"))
                ));
            }
            RunStatus::Error { message } => {
                xml.push_str(&format!("\n    <error message=\"{}\"/>\n  ", escape(message)));
            }
            RunStatus::Skipped { reason } => {
                xml.push_str(&format!("\n    <skipped message=\"{}\"/>\n  ", escape(reason)));
            }
        }
        xml.push_str("</testcase>\n");
    }
    xml.push_str("</testsuite>\n");
    xml
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, Read as _, Write as _};

    use super::*;
    use crate::model::{CollectionFile, Variables};

    /// Serves `responses` in order, one connection each, answering with JSON bodies.
    fn server(responses: Vec<(u16, &'static str)>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut rest = vec![0; length];
                reader.read_exact(&mut rest).unwrap();
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        port
    }

    #[test]
    fn runs_a_flow_with_chaining_and_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::project::init_with(tmp.path(), &CollectionFile::new("Pets")).unwrap();
        let port = server(vec![
            (200, r#"{"token":"abc"}"#),
            (201, r#"{"id":7}"#),
            (200, r#"{"id":7,"name":"Rex"}"#),
        ]);
        let base = format!("http://127.0.0.1:{port}");
        let flow = storage::create_folder(&root, "Flow").unwrap();
        let mut login = RequestFile::new("Login");
        login.method = "POST".into();
        login.url = format!("{base}/login");
        login.order = Some(1);
        login.checks = vec!["$.token exists".into()];
        storage::create_request(&flow, &login).unwrap();
        let mut create = RequestFile::new("Create");
        create.method = "POST".into();
        create.url = format!("{base}/pets");
        create.order = Some(2);
        create.headers =
            crate::model::headers_from_text("Authorization: Bearer {{ response(\"Login\", \"$.token\") }}");
        create.checks = vec!["status == 201".into()];
        storage::create_request(&flow, &create).unwrap();
        let mut fetch = RequestFile::new("Fetch");
        fetch.url = format!("{base}/pets/{{{{ response(\"Create\", \"$.id\") }}}}");
        fetch.order = Some(3);
        fetch.checks = vec!["$.name == Rex".into(), "$.name == Max".into()];
        storage::create_request(&flow, &fetch).unwrap();
        let mut socket = RequestFile::new("Live");
        socket.url = "wss://example.test/live".into();
        socket.order = Some(4);
        storage::create_request(&flow, &socket).unwrap();

        let collection = storage::load_collection(&root).unwrap();
        let requests = resolve_target(&collection, Some("Flow")).unwrap();
        assert_eq!(requests.len(), 4);
        let context = Context {
            store: None,
            root: root.clone(),
            variables: Variables::new(),
            latest: Default::default(),
            cache: None,
            collection_id: collection.file.id.clone(),
            default_timeout_secs: 5,
            cookies: None,
        };
        let mut seen = Vec::new();
        let summary = futures_lite::future::block_on(run(&requests, &context, RunOptions::default(), |r| {
            seen.push((r.name.clone(), r.status.clone()));
        }));
        assert_eq!(
            seen,
            [
                ("Login".to_string(), RunStatus::Passed),
                ("Create".to_string(), RunStatus::Passed),
                ("Fetch".to_string(), RunStatus::Failed),
                (
                    "Live".to_string(),
                    RunStatus::Skipped {
                        reason: "WebSocket requests aren't run".into()
                    }
                ),
            ]
        );
        assert_eq!((summary.passed, summary.failed, summary.skipped), (2, 1, 1));
        assert!(!summary.succeeded());
    }

    #[test]
    fn stops_on_failure_and_reports_junit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::project::init_with(tmp.path(), &CollectionFile::new("Api")).unwrap();
        let mut broken = RequestFile::new("Broken");
        broken.url = "http://127.0.0.1:1/{{nope}}".into();
        broken.order = Some(1);
        storage::create_request(&root, &broken).unwrap();
        let mut later = RequestFile::new("Later");
        later.url = "http://127.0.0.1:1/".into();
        later.order = Some(2);
        storage::create_request(&root, &later).unwrap();
        let collection = storage::load_collection(&root).unwrap();
        let requests = resolve_target(&collection, None).unwrap();
        let context = Context {
            store: None,
            root: root.clone(),
            variables: Variables::new(),
            latest: Default::default(),
            cache: None,
            collection_id: None,
            default_timeout_secs: 2,
            cookies: None,
        };
        let mut results = Vec::new();
        let summary =
            futures_lite::future::block_on(run(&requests, &context, RunOptions { stop_on_failure: true }, |r| {
                results.push(r.clone())
            }));
        assert_eq!(results.len(), 1, "stopped after the first");
        assert!(summary.stopped_early);
        assert!(matches!(&results[0].status, RunStatus::Error { message } if message.contains("nope")));

        let xml = junit("Api", &results, &summary);
        assert!(
            xml.contains("<testsuite name=\"Api\" tests=\"1\" failures=\"0\" errors=\"1\""),
            "{xml}"
        );
        assert!(xml.contains("<error message="), "{xml}");
        assert!(resolve_target(&collection, Some("missing-thing")).is_err());
    }
}
