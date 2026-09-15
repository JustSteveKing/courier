//! `courier` on the command line: send one request, run a folder or collection with its
//! checks, list requests and environments. Uses the same engine as the app, so a request
//! behaves the same in a terminal, a script or CI.

use std::ffi::OsString;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{CommandFactory as _, Parser, Subcommand, ValueEnum};
use courier_core::chain::Context;
use courier_core::cookies::Cookies;
use courier_core::model::Variables;
use courier_core::paths::AppPaths;
use courier_core::response_cache::Outcome;
use courier_core::runner::{self, RunOptions, RunResult, RunStatus, RunSummary};
use courier_core::secret_store::{self, SecretStore};
use courier_core::storage::Collection;

/// Subcommands the app binary hands over to the command line.
pub const SUBCOMMANDS: [&str; 5] = ["send", "run", "list", "envs", "completions"];

#[derive(Parser, Debug)]
#[command(
    name = "courier",
    version,
    about = "Send requests and run checks from Courier collections",
    after_help = "Secrets come from your keyring. In CI, set COURIER_SECRET_<NAME> (e.g. COURIER_SECRET_API_TOKEN) instead."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Send one request and print its response body.
    Send {
        /// The request: its name, or its path in the collection (e.g. auth/login).
        request: String,
        #[command(flatten)]
        common: Common,
        /// Print the status line and headers before the body.
        #[arg(short, long)]
        include: bool,
    },
    /// Send a folder's or the collection's requests in order and evaluate their checks.
    Run {
        /// A folder or request in the collection; the whole collection when left out.
        target: Option<String>,
        #[command(flatten)]
        common: Common,
        /// Stop at the first failed check or error.
        #[arg(long)]
        bail: bool,
        /// How to report results.
        #[arg(long, value_enum, default_value_t = Report::Pretty)]
        report: Report,
        /// Write the report to a file instead of standard output.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// List the collection's requests with their paths.
    List {
        #[arg(short, long, default_value = ".")]
        project: PathBuf,
    },
    /// List the collection's environments.
    Envs {
        #[arg(short, long, default_value = ".")]
        project: PathBuf,
    },
    /// Print a shell completion script (bash, zsh, fish, …).
    Completions { shell: clap_complete::Shell },
}

#[derive(clap::Args, Debug)]
struct Common {
    /// The project folder, or anywhere inside it.
    #[arg(short, long, default_value = ".")]
    project: PathBuf,
    /// The environment to use, by name.
    #[arg(short, long)]
    env: Option<String>,
    /// Set a variable, overriding the environment: --var base_url=http://localhost:8080
    #[arg(long = "var", value_name = "NAME=VALUE")]
    vars: Vec<String>,
    /// Don't read secrets from the keyring; use COURIER_SECRET_* variables only.
    #[arg(long)]
    no_keyring: bool,
    /// Seconds to wait for each response to start, unless a request's settings say otherwise.
    #[arg(long, default_value_t = 30)]
    timeout: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Report {
    Pretty,
    Json,
    Junit,
}

/// Runs the command line with `args` (including the program name). Returns the exit code:
/// 0 when everything passed, 1 when a check failed or a request errored, 2 for usage errors.
pub fn main_from(args: impl IntoIterator<Item = OsString>) -> i32 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            let _ = e.print();
            return code;
        }
    };
    match futures_lite::future::block_on(execute(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("courier: {e:#}");
            2
        }
    }
}

async fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "courier", &mut std::io::stdout());
            Ok(0)
        }
        Command::List { project } => {
            let collection = load(&project)?;
            for entry in collection.requests() {
                let relative = entry.path.strip_prefix(&collection.root).unwrap_or(entry.path);
                let request = relative.to_string_lossy();
                println!(
                    "{:<7} {:<40} {}",
                    entry.request.method,
                    request.strip_suffix(".yaml").unwrap_or(&request),
                    entry.request.name
                );
            }
            Ok(0)
        }
        Command::Envs { project } => {
            let collection = load(&project)?;
            if collection.environments.is_empty() {
                eprintln!("{} has no environments", collection.file.name);
            }
            for env in &collection.environments {
                println!("{}", env.file.name);
            }
            Ok(0)
        }
        Command::Send {
            request,
            common,
            include,
        } => {
            let collection = load(&common.project)?;
            let requests = runner::resolve_target(&collection, Some(&request)).map_err(|e| anyhow!(e))?;
            let [one] = requests.as_slice() else {
                bail!(
                    "\"{request}\" is a folder with {} requests; use `courier run` for those",
                    requests.len()
                );
            };
            let context = context(&collection, &common).await?;
            let mut result = None;
            runner::run(std::slice::from_ref(one), &context, RunOptions::default(), |r| {
                result = Some(r.clone())
            })
            .await;
            let result = result.context("the request didn't run")?;
            print_send(&result, include);
            Ok(match result.status {
                RunStatus::Passed => 0,
                _ => 1,
            })
        }
        Command::Run {
            target,
            common,
            bail,
            report,
            output,
        } => {
            let collection = load(&common.project)?;
            let requests = runner::resolve_target(&collection, target.as_deref()).map_err(|e| anyhow!(e))?;
            let context = context(&collection, &common).await?;
            let pretty = report == Report::Pretty && output.is_none();
            let colors = Colors::detect();
            if pretty {
                let scope = target.as_deref().map(|t| format!(" › {t}")).unwrap_or_default();
                let env = common
                    .env
                    .as_deref()
                    .map(|e| format!(" (env: {e})"))
                    .unwrap_or_default();
                println!(
                    "Running {} request{} from {}{scope}{env}\n",
                    requests.len(),
                    if requests.len() == 1 { "" } else { "s" },
                    collection.file.name
                );
            }
            let mut results = Vec::new();
            let summary = runner::run(&requests, &context, RunOptions { stop_on_failure: bail }, |result| {
                if pretty {
                    print_result(result, &colors);
                }
                results.push(result.clone());
            })
            .await;
            let text = match report {
                Report::Pretty if pretty => {
                    println!("\n{}", summary_line(&summary, &colors));
                    None
                }
                Report::Pretty => Some(
                    results
                        .iter()
                        .map(|r| result_line(r, &Colors::none()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Report::Json => Some(serde_json::to_string_pretty(&serde_json::json!({
                    "collection": collection.file.name,
                    "results": results,
                    "summary": summary,
                }))?),
                Report::Junit => Some(runner::junit(&collection.file.name, &results, &summary)),
            };
            if let Some(text) = text {
                match &output {
                    Some(path) => {
                        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
                        eprintln!("{}", summary_line(&summary, &Colors::none()));
                    }
                    None => println!("{text}"),
                }
            }
            Ok(if summary.succeeded() { 0 } else { 1 })
        }
    }
}

fn load(project: &Path) -> Result<Collection> {
    let start = project
        .canonicalize()
        .with_context(|| format!("{} doesn't exist", project.display()))?;
    runner::load_collection_for(&start).map_err(|e| anyhow!(e))
}

/// Variables from the collection and environment, secrets from the environment variables
/// or keyring, then `--var` overrides.
async fn context(collection: &Collection, common: &Common) -> Result<Context> {
    let environment = match &common.env {
        None => None,
        Some(name) => Some(
            collection
                .environments
                .iter()
                .find(|env| {
                    env.file.name.eq_ignore_ascii_case(name)
                        || env.path.file_stem().is_some_and(|stem| stem.eq_ignore_ascii_case(name))
                })
                .ok_or_else(|| {
                    let names: Vec<&str> = collection.environments.iter().map(|e| e.file.name.as_str()).collect();
                    anyhow!(
                        "no environment named \"{name}\"; {}",
                        if names.is_empty() {
                            "this collection has none".to_string()
                        } else {
                            format!("try one of: {}", names.join(", "))
                        }
                    )
                })?,
        ),
    };
    let layered = secret_store::layer(&collection.file, environment.map(|env| (env.path.as_path(), &env.file)));
    let mut variables: Variables = layered.variables;

    let mut from_keyring = layered.secrets.clone();
    for name in layered.secrets.keys() {
        if let Ok(value) = std::env::var(secret_env_name(name)) {
            variables.insert(name.clone(), value);
            from_keyring.shift_remove(name);
        }
    }
    if !from_keyring.is_empty() && !common.no_keyring {
        match open_store().await {
            Ok(store) => match store.get_all(&from_keyring).await {
                Ok(found) => variables.extend(found),
                Err(e) => eprintln!("courier: couldn't read secrets from the keyring: {e:#}"),
            },
            Err(e) => eprintln!("courier: no keyring available ({e:#}); set COURIER_SECRET_* instead"),
        }
    }
    for pair in &common.vars {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("--var expects NAME=VALUE, not \"{pair}\""))?;
        variables.insert(name.trim().to_string(), value.to_string());
    }

    Ok(Context {
        root: collection.root.clone(),
        variables,
        latest: Default::default(),
        cache: None,
        collection_id: collection.file.id.clone(),
        default_timeout_secs: common.timeout,
        // A fresh jar per run, so logins within the run carry over without touching the app's.
        cookies: Some(Cookies::default().store().clone()),
    })
}

async fn open_store() -> Result<SecretStore> {
    let paths = AppPaths::from_env()?;
    SecretStore::open(&paths).await
}

/// `api_token` → `COURIER_SECRET_API_TOKEN`.
pub fn secret_env_name(name: &str) -> String {
    let upper: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("COURIER_SECRET_{upper}")
}

struct Colors {
    enabled: bool,
}

impl Colors {
    fn detect() -> Self {
        Self {
            enabled: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn none() -> Self {
        Self { enabled: false }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

fn result_line(result: &RunResult, colors: &Colors) -> String {
    let (mark, color) = match result.status {
        RunStatus::Passed => ("✓", "32"),
        RunStatus::Failed | RunStatus::Error { .. } => ("✗", "31"),
        RunStatus::Skipped { .. } => ("–", "2"),
    };
    let mut line = format!(
        "  {} {:<5} {:<28}",
        colors.paint(color, mark),
        result.method,
        result.name
    );
    match &result.status {
        RunStatus::Error { message } => line.push_str(&colors.paint("31", message)),
        RunStatus::Skipped { reason } => line.push_str(&colors.paint("2", &format!("skipped: {reason}"))),
        RunStatus::Passed | RunStatus::Failed => {
            line.push_str(&format!(
                "{:>4}  {:>6}",
                result.response_status.map(|s| s.to_string()).unwrap_or_default(),
                result.elapsed_ms.map(|ms| format!("{ms} ms")).unwrap_or_default()
            ));
            if !result.checks.is_empty() {
                let passed = result.checks.iter().filter(|c| c.passed).count();
                line.push_str(&format!("  {passed}/{} checks", result.checks.len()));
            }
            for check in result.checks.iter().filter(|c| !c.passed) {
                let why = match &check.error {
                    Some(error) => error.clone(),
                    None => format!("got {}", check.actual),
                };
                line.push_str(&format!(
                    "\n      {} {}  {}",
                    colors.paint("31", "✗"),
                    check.check,
                    colors.paint("2", &format!("({why})"))
                ));
            }
        }
    }
    line
}

fn print_result(result: &RunResult, colors: &Colors) {
    println!("{}", result_line(result, colors));
    let _ = std::io::stdout().flush();
}

fn summary_line(summary: &RunSummary, colors: &Colors) -> String {
    let mut parts = vec![colors.paint("32", &format!("{} passed", summary.passed))];
    if summary.failed > 0 {
        parts.push(colors.paint("31", &format!("{} failed", summary.failed)));
    }
    if summary.errors > 0 {
        parts.push(colors.paint(
            "31",
            &format!("{} error{}", summary.errors, if summary.errors == 1 { "" } else { "s" }),
        ));
    }
    if summary.skipped > 0 {
        parts.push(format!("{} skipped", summary.skipped));
    }
    let mut line = format!("{} in {:.2} s", parts.join(", "), summary.elapsed_ms as f64 / 1000.0);
    if summary.stopped_early {
        line.push_str(" (stopped at the first failure)");
    }
    line
}

fn print_send(result: &RunResult, include: bool) {
    let colors = Colors {
        enabled: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };
    match (&result.status, &result.response) {
        (RunStatus::Error { message } | RunStatus::Skipped { reason: message }, _) => {
            eprintln!("courier: {message}");
        }
        (_, Some(response)) => {
            if let Outcome::Response {
                status,
                reason,
                headers,
                body,
                ..
            } = &response.outcome
            {
                let status_line = format!("{status} {reason}  {} ms", response.elapsed_ms);
                if include {
                    println!("{status_line}");
                    for (name, value) in headers {
                        println!("{name}: {value}");
                    }
                    println!();
                } else {
                    eprintln!(
                        "{}",
                        colors.paint(if *status < 400 { "32" } else { "31" }, &status_line)
                    );
                }
                println!("{}", courier_core::http::pretty_body(body));
            }
            for check in &result.checks {
                let mark = if check.passed {
                    colors.paint("32", "✓")
                } else {
                    colors.paint("31", "✗")
                };
                let detail = match (&check.error, check.passed) {
                    (Some(error), _) => format!("  ({error})"),
                    (None, false) => format!("  (got {})", check.actual),
                    _ => String::new(),
                };
                eprintln!("{mark} {}{detail}", check.check);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_secret_variables() {
        assert_eq!(secret_env_name("api_token"), "COURIER_SECRET_API_TOKEN");
        assert_eq!(secret_env_name("stripe-key"), "COURIER_SECRET_STRIPE_KEY");
    }

    #[test]
    fn parses_commands() {
        let cli = Cli::try_parse_from([
            "courier", "run", "auth", "--env", "staging", "--var", "a=b", "--bail", "--report", "junit",
        ])
        .unwrap();
        let Command::Run {
            target,
            common,
            bail,
            report,
            ..
        } = cli.command
        else {
            panic!()
        };
        assert_eq!(target.as_deref(), Some("auth"));
        assert_eq!(common.env.as_deref(), Some("staging"));
        assert_eq!(common.vars, ["a=b"]);
        assert!(bail);
        assert_eq!(report, Report::Junit);
        assert!(Cli::try_parse_from(["courier", "run", "--report", "html"]).is_err());
        Cli::command().debug_assert();
    }

    #[test]
    fn runs_a_collection_end_to_end() {
        use courier_core::model::{CollectionFile, EnvironmentFile, RequestFile};
        use courier_core::storage;
        use std::io::BufRead as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let body = r#"{"ok":true}"#;
            write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            seen_tx.send(head).unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        let mut file = CollectionFile::new("Api");
        file.secrets.push("api_token".into());
        let root = courier_core::project::init_with(tmp.path(), &file).unwrap();
        let mut env = EnvironmentFile::new("Local");
        env.variables
            .insert("base_url".into(), format!("http://127.0.0.1:{port}"));
        storage::create_environment(&root, &env).unwrap();
        let mut health = RequestFile::new("Health");
        health.url = "{{base_url}}/health".into();
        health.headers = courier_core::model::headers_from_text("Authorization: Bearer {{api_token}}");
        health.checks = vec!["status == 200".into(), "$.ok == true".into()];
        storage::create_request(&root, &health).unwrap();

        // SAFETY: tests in this crate don't read this variable concurrently.
        unsafe { std::env::set_var("COURIER_SECRET_API_TOKEN", "from-ci") };
        let report = tmp.path().join("report.xml");
        let code = main_from(
            [
                "courier",
                "run",
                "--project",
                tmp.path().to_str().unwrap(),
                "--env",
                "local",
                "--no-keyring",
                "--report",
                "junit",
                "--output",
                report.to_str().unwrap(),
            ]
            .map(OsString::from),
        );
        assert_eq!(code, 0);
        let head = seen_rx.recv().unwrap().to_ascii_lowercase();
        assert!(head.contains("authorization: bearer from-ci"), "{head}");
        let xml = std::fs::read_to_string(&report).unwrap();
        assert!(xml.contains("tests=\"1\" failures=\"0\" errors=\"0\""), "{xml}");

        let bad = main_from(
            [
                "courier",
                "run",
                "--project",
                tmp.path().to_str().unwrap(),
                "--env",
                "nope",
            ]
            .map(OsString::from),
        );
        assert_eq!(bad, 2, "unknown environment is a usage error");
    }
}
