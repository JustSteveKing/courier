//! The live view for Server-Sent Events responses: an event list (newest first) with a
//! filter, the selected event's data, and stop/reconnect.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use super::{RequestEditor, format_duration};
use crate::http;
use crate::transport::SseEvent;
use crate::ui::{readonly_editor, text_input};

/// Older events are dropped past this, so a long-running stream can't grow without bound.
const MAX_EVENTS: usize = 5_000;
/// Rows rendered at once; the filter narrows the rest.
const MAX_ROWS: usize = 500;

pub(super) struct SseLog {
    pub status: String,
    pub started: Instant,
    pub events: VecDeque<SseRecord>,
    /// Events received in total, including dropped ones; also the next sequence number.
    pub total: usize,
    pub last_event_id: Option<String>,
    pub ended: Option<Ending>,
    pub selected: Option<usize>,
}

pub(super) struct SseRecord {
    pub seq: usize,
    pub at: Duration,
    pub event: SseEvent,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Ending {
    Closed,
    Stopped,
    Failed(String),
}

impl SseLog {
    pub fn new(status: String) -> Self {
        Self {
            status,
            started: Instant::now(),
            events: VecDeque::new(),
            total: 0,
            last_event_id: None,
            ended: None,
            selected: None,
        }
    }

    pub fn push(&mut self, event: SseEvent) {
        if !event.id.is_empty() {
            self.last_event_id = Some(event.id.clone());
        }
        self.events.push_back(SseRecord {
            seq: self.total,
            at: self.started.elapsed(),
            event,
        });
        self.total += 1;
        if self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    pub fn record(&self, seq: usize) -> Option<&SseRecord> {
        self.events.iter().find(|r| r.seq == seq)
    }
}

fn matches_filter(record: &SseRecord, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let filter = filter.to_lowercase();
    record.event.event.to_lowercase().contains(&filter) || record.event.data.to_lowercase().contains(&filter)
}

impl RequestEditor {
    pub(super) fn select_sse_event(&mut self, seq: usize, window: &mut Window, cx: &mut Context<Self>) {
        let data = self.state_mut().and_then(|state| {
            let log = state.sse.as_mut()?;
            log.selected = Some(seq);
            log.record(seq).map(|r| http::pretty_body(&r.event.data))
        });
        if let Some(data) = data {
            self.stream_detail.update(cx, |s, cx| s.set_value(data, window, cx));
        }
        cx.notify();
    }

    pub(super) fn render_sse(&self, log: &SseLog, live: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let count = log.total;
        let (status, color) = match &log.ended {
            None => (
                t!(
                    "request.sse_streaming",
                    status = log.status,
                    count = count,
                    elapsed = format_duration(log.started.elapsed().as_millis() as u64)
                )
                .to_string(),
                theme.success,
            ),
            Some(Ending::Closed) => (
                t!("request.sse_closed", count = count).to_string(),
                theme.muted_foreground,
            ),
            Some(Ending::Stopped) => (
                t!("request.sse_stopped", count = count).to_string(),
                theme.muted_foreground,
            ),
            Some(Ending::Failed(error)) => (
                t!("request.sse_failed", error = error, count = count).to_string(),
                theme.danger,
            ),
        };
        let filter = self.stream_filter.read(cx).value().to_string();

        let mut rows = v_flex()
            .id("sse-events")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_px();
        for record in log
            .events
            .iter()
            .rev()
            .filter(|r| matches_filter(r, &filter))
            .take(MAX_ROWS)
        {
            let seq = record.seq;
            let selected = log.selected == Some(seq);
            let preview: String = record
                .event
                .data
                .chars()
                .take(200)
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect();
            rows = rows.child(
                h_flex()
                    .id(("sse-event", seq))
                    .test_support()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .when(selected, |row| row.bg(theme.accent))
                    .hover(|row| row.bg(theme.accent))
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("+{:.2}s", record.at.as_secs_f64())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px_1()
                            .rounded_sm()
                            .text_xs()
                            .bg(theme.secondary)
                            .text_color(theme.info)
                            .child(record.event.event.clone()),
                    )
                    .when(!record.event.id.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("#{}", record.event.id)),
                        )
                    })
                    .child(div().flex_1().min_w_0().truncate().child(preview))
                    .on_click(cx.listener(move |this, _, window, cx| this.select_sse_event(seq, window, cx))),
            );
        }

        v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().text_sm().text_color(color).child(status))
                    .when(!live, |bar| {
                        let tooltip = match &log.last_event_id {
                            Some(id) => t!("request.sse_resume_tooltip", id = id).to_string(),
                            None => t!("request.sse_reconnect").to_string(),
                        };
                        bar.child(
                            Button::new("sse-reconnect")
                                .small()
                                .icon(IconName::Undo2)
                                .label(t!("request.sse_reconnect").to_string())
                                .tooltip(tooltip)
                                .on_click(cx.listener(|this, _, window, cx| this.send_with(true, window, cx))),
                        )
                    }),
            )
            .child(text_input(&self.stream_filter).small())
            .child(rows)
            .child(if log.selected.is_some() {
                readonly_editor(&self.stream_detail).h(px(180.)).into_any_element()
            } else {
                div()
                    .h(px(40.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("request.sse_select_hint").to_string())
                    .into_any_element()
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` (via `super::*`) exports GPUI's test macro; keep Rust's for `#[test]`.
    #[allow(unused_imports)]
    use core::prelude::v1::test;

    fn event(event: &str, id: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.into(),
            id: id.into(),
            data: data.into(),
        }
    }

    #[test]
    fn keeps_the_latest_events_and_last_id() {
        let mut log = SseLog::new("200 OK".into());
        log.push(event("message", "1", "a"));
        log.push(event("message", "", "b"));
        assert_eq!(
            log.last_event_id.as_deref(),
            Some("1"),
            "events without an id keep the previous one"
        );
        for i in 0..MAX_EVENTS {
            log.push(event("tick", &i.to_string(), "x"));
        }
        assert_eq!(log.events.len(), MAX_EVENTS);
        assert_eq!(log.total, MAX_EVENTS + 2);
        assert!(log.record(0).is_none(), "oldest dropped");
        assert_eq!(log.last_event_id.as_deref(), Some(&*(MAX_EVENTS - 1).to_string()));
    }

    #[test]
    fn filters_by_type_or_data() {
        let record = SseRecord {
            seq: 0,
            at: Duration::ZERO,
            event: event("price.update", "", "{\"symbol\": \"ACME\"}"),
        };
        assert!(matches_filter(&record, ""));
        assert!(matches_filter(&record, "PRICE"));
        assert!(matches_filter(&record, "acme"));
        assert!(!matches_filter(&record, "volume"));
    }
}
