//! Running a folder or a collection in the app: the requests in order, each with its
//! response status and checks as they come in. The engine is [`crate::runner`], the same one
//! the command line uses.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use indexmap::IndexMap;
use rust_i18n::t;

use crate::chain;
use crate::model::RequestFile;
use crate::response_cache::StoredResponse;
use crate::runner::{self, RunOptions, RunResult, RunStatus, RunSummary};
use crate::secret_store::{SecretRef, SecretStore};

pub enum RunnerViewEvent {
    /// Show this request in the editor.
    Open(PathBuf),
    /// Responses from the run, to keep as those requests' latest.
    Ran(Vec<(PathBuf, StoredResponse)>),
    /// Run the same scope again, with fresh variables.
    RunAgain(Scope),
    Close,
}

impl EventEmitter<RunnerViewEvent> for RunnerView {}

/// What to run: a collection root, or a folder inside one.
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub root: PathBuf,
    /// The folder, or the root itself for a whole collection.
    pub path: PathBuf,
    /// What to call it: the collection's or the folder's name.
    pub title: String,
}

/// Everything a run needs; the secrets are read when it starts, not before.
pub struct RunSetup {
    pub scope: Scope,
    pub requests: Vec<(PathBuf, RequestFile)>,
    pub context: chain::Context,
    pub secrets: IndexMap<String, SecretRef>,
    pub store: Option<SecretStore>,
}

pub struct RunnerView {
    scope: Option<Scope>,
    total: usize,
    results: Vec<RunResult>,
    summary: Option<RunSummary>,
    error: Option<String>,
    stop_on_failure: bool,
    /// Held while running; dropping it stops the run after the request in flight.
    task: Option<Task<()>>,
}

impl RunnerView {
    pub fn new() -> Self {
        Self {
            scope: None,
            total: 0,
            results: Vec::new(),
            summary: None,
            error: None,
            stop_on_failure: false,
            task: None,
        }
    }

    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }

    pub fn running(&self) -> bool {
        self.task.is_some()
    }

    pub fn start(&mut self, setup: RunSetup, cx: &mut Context<Self>) {
        let RunSetup {
            scope,
            requests,
            mut context,
            secrets,
            store,
        } = setup;
        self.scope = Some(scope);
        self.total = requests.len();
        self.results.clear();
        self.summary = None;
        self.error = None;
        let options = RunOptions {
            stop_on_failure: self.stop_on_failure,
        };

        let (send, receive) = async_channel::unbounded();
        let run = cx.background_executor().spawn(async move {
            if !secrets.is_empty() {
                let store = store.ok_or_else(|| t!("secrets.store_unavailable").to_string())?;
                let found = store
                    .get_all(&secrets)
                    .await
                    .map_err(|e| t!("request.could_not_read_secrets", error = format!("{e:#}")).to_string())?;
                context.variables.extend(found);
            }
            Ok(runner::run(&requests, &context, options, |result| {
                let _ = send.send_blocking(result.clone());
            })
            .await)
        });

        self.task = Some(cx.spawn(async move |this, cx| {
            while let Ok(result) = receive.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.results.push(result);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            let outcome = run.await;
            this.update(cx, |this, cx| this.finish(outcome, cx)).ok();
        }));
        cx.notify();
    }

    fn finish(&mut self, outcome: Result<RunSummary, String>, cx: &mut Context<Self>) {
        self.task = None;
        match outcome {
            Ok(summary) => self.summary = Some(summary),
            Err(message) => {
                self.error = Some(message);
                self.summary = Some(self.summary_so_far());
            }
        }
        self.emit_responses(cx);
        cx.notify();
    }

    /// Stops after the request in flight, keeping the results so far.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.task.take().is_none() {
            return;
        }
        let mut summary = self.summary_so_far();
        summary.stopped_early = self.results.len() < self.total;
        self.summary = Some(summary);
        self.emit_responses(cx);
        cx.notify();
    }

    fn summary_so_far(&self) -> RunSummary {
        let mut summary = RunSummary::default();
        for result in &self.results {
            summary.add(result);
            summary.elapsed_ms += result.elapsed_ms.unwrap_or_default();
        }
        summary
    }

    fn emit_responses(&self, cx: &mut Context<Self>) {
        let responses: Vec<_> = self
            .results
            .iter()
            .filter_map(|result| Some((result.path.clone(), result.response.clone()?)))
            .collect();
        if !responses.is_empty() {
            cx.emit(RunnerViewEvent::Ran(responses));
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.task = None;
        self.scope = None;
        self.results.clear();
        self.summary = None;
        cx.emit(RunnerViewEvent::Close);
        cx.notify();
    }

    fn run_again(&mut self, cx: &mut Context<Self>) {
        if let Some(scope) = self.scope.clone() {
            cx.emit(RunnerViewEvent::RunAgain(scope));
        }
    }

    #[cfg(test)]
    pub fn results_for_test(&self) -> Vec<(String, RunStatus)> {
        self.results
            .iter()
            .map(|result| (result.request.clone(), result.status.clone()))
            .collect()
    }

    #[cfg(test)]
    pub fn summary_for_test(&self) -> Option<&RunSummary> {
        self.summary.as_ref()
    }

    fn render_result(&self, index: usize, result: &RunResult, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (icon, color) = match result.status {
            RunStatus::Passed => (IconName::Check, theme.success),
            RunStatus::Failed | RunStatus::Error { .. } => (IconName::Close, theme.danger),
            RunStatus::Skipped { .. } => (IconName::Minus, theme.muted_foreground),
        };
        let detail = match &result.status {
            RunStatus::Error { message } => Some((message.clone(), theme.danger)),
            RunStatus::Skipped { reason } => Some((reason.clone(), theme.muted_foreground)),
            _ => None,
        };
        let failed_checks: Vec<_> = result.checks.iter().filter(|check| !check.passed).collect();
        let passed = result.checks.len() - failed_checks.len();
        let path = result.path.clone();

        v_flex()
            .id(("run-result", index))
            .test_support()
            .px_2()
            .py_1()
            .gap_1()
            .rounded(theme.radius)
            .hover(|row| row.bg(theme.accent))
            .cursor_pointer()
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(RunnerViewEvent::Open(path.clone()))))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(icon).small().text_color(color))
                    .child(
                        div()
                            .w_10()
                            .flex_none()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(result.method.clone()),
                    )
                    .child(div().flex_1().min_w_0().text_sm().truncate().child(result.name.clone()))
                    .when_some(result.response_status, |row, status| {
                        row.child(
                            div()
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(if status < 400 { theme.success } else { theme.danger })
                                .child(status.to_string()),
                        )
                    })
                    .when_some(result.elapsed_ms, |row, ms| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("run.milliseconds", count = ms).to_string()),
                        )
                    })
                    .when(!result.checks.is_empty(), |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(if failed_checks.is_empty() {
                                    theme.muted_foreground
                                } else {
                                    theme.danger
                                })
                                .child(
                                    t!("run.checks_passed", passed = passed, total = result.checks.len()).to_string(),
                                ),
                        )
                    }),
            )
            .when_some(detail, |row, (text, color)| {
                row.child(div().pl_7().text_xs().text_color(color).child(text))
            })
            .children(failed_checks.into_iter().map(|check| {
                let why = match &check.error {
                    Some(error) => error.clone(),
                    None => t!("request.check_actual", actual = check.actual).to_string(),
                };
                h_flex()
                    .pl_7()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.danger)
                            .child(check.check.clone()),
                    )
                    .child(div().text_color(theme.muted_foreground).child(why))
            }))
            .into_any_element()
    }

    fn render_summary(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(summary) = &self.summary else {
            return div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("run.in_progress", done = self.results.len(), total = self.total).to_string())
                .into_any_element();
        };
        let mut parts = vec![(t!("run.passed", count = summary.passed).to_string(), theme.success)];
        if summary.failed > 0 {
            parts.push((t!("run.failed", count = summary.failed).to_string(), theme.danger));
        }
        if summary.errors > 0 {
            parts.push((t!("run.errors", count = summary.errors).to_string(), theme.danger));
        }
        if summary.skipped > 0 {
            parts.push((
                t!("run.skipped", count = summary.skipped).to_string(),
                theme.muted_foreground,
            ));
        }
        h_flex()
            .gap_2()
            .text_sm()
            .children(
                parts
                    .into_iter()
                    .map(|(text, color)| div().text_color(color).child(text)),
            )
            .child(
                div().text_color(theme.muted_foreground).child(
                    t!(
                        "run.took",
                        seconds = format!("{:.2}", summary.elapsed_ms as f64 / 1000.)
                    )
                    .to_string(),
                ),
            )
            .when(summary.stopped_early, |row| {
                row.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(t!("run.stopped_early").to_string()),
                )
            })
            .into_any_element()
    }
}

impl Render for RunnerView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(scope) = self.scope.clone() else {
            return div().into_any_element();
        };
        let running = self.running();

        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_lg()
                            .truncate()
                            .child(t!("run.title", name = scope.title).to_string()),
                    )
                    .child(
                        Button::new("run-stop-on-failure")
                            .ghost()
                            .small()
                            .selected(self.stop_on_failure)
                            .label(t!("run.stop_on_failure").to_string())
                            .tooltip(t!("run.stop_on_failure_tooltip").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.stop_on_failure = !this.stop_on_failure;
                                cx.notify();
                            })),
                    )
                    .child(if running {
                        Button::new("run-stop")
                            .small()
                            .danger()
                            .icon(IconName::Close)
                            .label(t!("run.stop").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                    } else {
                        Button::new("run-again")
                            .small()
                            .primary()
                            .icon(IconName::Play)
                            .label(t!("run.run_again").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.run_again(cx)))
                    })
                    .child(
                        Button::new("run-close")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(t!("run.close").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    ),
            )
            .child(self.render_summary(cx))
            .when_some(self.error.clone(), |view, error| {
                view.child(div().text_sm().text_color(theme.danger).child(error))
            })
            .child(
                v_flex()
                    .id("run-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_1()
                    .when(self.total == 0, |list| {
                        list.child(
                            div()
                                .p_2()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("run.nothing_to_run").to_string()),
                        )
                    })
                    .children(
                        self.results
                            .iter()
                            .enumerate()
                            .map(|(ix, result)| self.render_result(ix, result, cx)),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("run.hint").to_string()),
            )
            .into_any_element()
    }
}
