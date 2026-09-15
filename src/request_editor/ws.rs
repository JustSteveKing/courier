//! WebSocket requests: connect/disconnect, a message composer with saved templates, and a
//! timeline of sent and received messages.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::InputState;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use super::sse::Ending;
use super::{RequestEditor, RequestEditorEvent, format_duration, format_size};
use crate::http;
use crate::model::{MessageTemplate, interpolate, is_websocket_url};
use crate::storage::write_yaml;
use crate::transport::{WsMessage, WsPayload};
use crate::ui::{dialog_footer, focus_in_dialog, readonly_editor, text_input};

const MAX_MESSAGES: usize = 5_000;
const MAX_ROWS: usize = 500;

pub(super) struct WsLog {
    pub status: String,
    pub started: Instant,
    pub connected: bool,
    pub messages: VecDeque<WsRecord>,
    pub total: usize,
    pub ended: Option<Ending>,
    pub selected: Option<usize>,
}

pub(super) struct WsRecord {
    pub seq: usize,
    pub at: Duration,
    pub message: WsMessage,
}

impl WsLog {
    pub fn new() -> Self {
        Self {
            status: String::new(),
            started: Instant::now(),
            connected: false,
            messages: VecDeque::new(),
            total: 0,
            ended: None,
            selected: None,
        }
    }

    pub fn push(&mut self, message: WsMessage) {
        self.messages.push_back(WsRecord {
            seq: self.total,
            at: self.started.elapsed(),
            message,
        });
        self.total += 1;
        if self.messages.len() > MAX_MESSAGES {
            self.messages.pop_front();
        }
    }

    fn record(&self, seq: usize) -> Option<&WsRecord> {
        self.messages.iter().find(|r| r.seq == seq)
    }
}

/// A single-line preview, and the full text for the detail pane.
fn describe(payload: &WsPayload) -> (String, String) {
    match payload {
        WsPayload::Text(text) => (
            text.chars()
                .take(200)
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect(),
            http::pretty_body(text),
        ),
        WsPayload::Binary(data) => {
            let hex = data
                .iter()
                .take(4096)
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            (t!("request.ws_binary", size = format_size(data.len())).to_string(), hex)
        }
        WsPayload::Close(reason) => {
            let text = if reason.is_empty() {
                t!("request.ws_close").to_string()
            } else {
                t!("request.ws_close_reason", reason = reason).to_string()
            };
            (text.clone(), text)
        }
    }
}

fn payload_size(payload: &WsPayload) -> usize {
    match payload {
        WsPayload::Text(text) => text.len(),
        WsPayload::Binary(data) => data.len(),
        WsPayload::Close(_) => 0,
    }
}

impl RequestEditor {
    /// Whether the URL, with plain variables resolved, is `ws://` or `wss://`.
    pub(super) fn is_websocket(&self, cx: &App) -> bool {
        is_websocket_url(&interpolate(&self.url.read(cx).value(), &self.variables).0)
    }

    pub(super) fn ws_connected(&self) -> bool {
        self.state()
            .and_then(|s| s.ws.as_ref())
            .is_some_and(|log| log.connected)
    }

    /// Sends the composer's text on the open connection, resolving `{{variables}}` with the
    /// values (including secrets) resolved when connecting.
    pub(super) fn send_ws_message(&mut self, cx: &mut Context<Self>) {
        let text = self.body.read(cx).value().to_string();
        let Some(live) = self.state().and_then(|s| s.live.as_ref()) else {
            return;
        };
        let (text, _) = interpolate(&text, &live.variables);
        if let Some(handle) = &live.handle {
            handle.send(WsPayload::Text(text));
        }
    }

    pub(super) fn select_ws_message(&mut self, seq: usize, window: &mut Window, cx: &mut Context<Self>) {
        let detail = self.state_mut().and_then(|state| {
            let log = state.ws.as_mut()?;
            log.selected = Some(seq);
            log.record(seq).map(|r| describe(&r.message.payload).1)
        });
        if let Some(detail) = detail {
            self.stream_detail.update(cx, |s, cx| s.set_value(detail, window, cx));
        }
        cx.notify();
    }

    pub(super) fn insert_template(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(content) = self
            .saved
            .as_ref()
            .and_then(|r| r.messages.get(index))
            .map(|m| m.content.clone())
        else {
            return;
        };
        self.body.update(cx, |s, cx| s.set_value(content, window, cx));
        self.update_dirty(cx);
    }

    /// Asks for a name, then saves the composer's text as a template in the request file.
    pub(super) fn save_template_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(t!("request.ws_template_name").to_string()));
        focus_in_dialog(&name, window, cx);
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (name, weak) = (name.clone(), weak.clone());
            dialog
                .title(t!("request.ws_save_template").to_string())
                .w(px(420.))
                .content({
                    let name = name.clone();
                    move |content, _, _| content.child(text_input(&name))
                })
                .footer(dialog_footer(
                    Some(t!("request.ws_save").to_string()),
                    ButtonVariant::Primary,
                ))
                .on_ok(move |_, _, cx| {
                    let template_name = name.read(cx).value().trim().to_string();
                    if template_name.is_empty() {
                        return false;
                    }
                    weak.update(cx, |this, cx| this.add_template(template_name, cx)).ok();
                    true
                })
        });
    }

    pub(super) fn add_template(&mut self, name: String, cx: &mut Context<Self>) {
        let (Some(path), Some(saved)) = (self.path.clone(), self.saved.as_mut()) else {
            return;
        };
        let content = self.body.read(cx).value().to_string();
        saved.messages.push(MessageTemplate { name, content });
        // Template changes save straight away; other unsaved edits stay pending.
        let mut on_disk: crate::model::RequestFile = match crate::storage::read_yaml(&path) {
            Ok(file) => file,
            Err(e) => {
                cx.emit(RequestEditorEvent::Error(format!("{e:#}")));
                return;
            }
        };
        on_disk.messages = saved.messages.clone();
        match write_yaml(&path, &on_disk) {
            Ok(()) => cx.emit(RequestEditorEvent::Saved(path)),
            Err(e) => cx.emit(RequestEditorEvent::Error(
                t!("request.could_not_save", error = format!("{e:#}")).to_string(),
            )),
        }
        cx.notify();
    }

    /// Buttons under the composer: send, templates, save as template.
    pub(super) fn render_composer_actions(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let connected = self.ws_connected();
        let templates: Vec<(usize, String)> = self
            .saved
            .as_ref()
            .map(|r| {
                r.messages
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (i, m.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let weak = cx.entity().downgrade();
        h_flex()
            .gap_2()
            .child(
                Button::new("ws-send-message")
                    .small()
                    .primary()
                    .icon(IconName::ArrowUp)
                    .label(t!("request.ws_send_message").to_string())
                    .tooltip(t!("request.send_shortcut").to_string())
                    .disabled(!connected)
                    .on_click(cx.listener(|this, _, _, cx| this.send_ws_message(cx))),
            )
            .child(
                Button::new("ws-templates")
                    .small()
                    .ghost()
                    .label(t!("request.ws_templates", count = templates.len()).to_string())
                    .disabled(templates.is_empty())
                    .dropdown_menu(move |mut menu, _, _| {
                        for (index, name) in &templates {
                            let (index, weak) = (*index, weak.clone());
                            menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, window, cx| {
                                weak.update(cx, |this, cx| this.insert_template(index, window, cx)).ok();
                            }));
                        }
                        menu
                    }),
            )
            .child(
                Button::new("ws-save-template")
                    .small()
                    .ghost()
                    .icon(IconName::Plus)
                    .label(t!("request.ws_save_template").to_string())
                    .on_click(cx.listener(|this, _, window, cx| this.save_template_dialog(window, cx))),
            )
    }

    pub(super) fn render_ws(&self, log: &WsLog, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let count = log.total;
        let (status, color) = match &log.ended {
            None if log.connected => (
                t!(
                    "request.ws_connected",
                    status = log.status,
                    count = count,
                    elapsed = format_duration(log.started.elapsed().as_millis() as u64)
                )
                .to_string(),
                theme.success,
            ),
            None => (t!("request.ws_connecting").to_string(), theme.muted_foreground),
            Some(Ending::Closed) | Some(Ending::Stopped) => (
                t!("request.ws_disconnected", count = count).to_string(),
                theme.muted_foreground,
            ),
            Some(Ending::Failed(error)) => (
                t!("request.ws_failed", error = error, count = count).to_string(),
                theme.danger,
            ),
        };
        let filter = self.stream_filter.read(cx).value().to_lowercase();

        let mut rows = v_flex()
            .id("ws-messages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_px();
        let visible = log.messages.iter().rev().filter_map(|record| {
            let (preview, _) = describe(&record.message.payload);
            (filter.is_empty() || preview.to_lowercase().contains(&filter)).then_some((record, preview))
        });
        for (record, preview) in visible.take(MAX_ROWS) {
            let seq = record.seq;
            let outgoing = record.message.outgoing;
            rows = rows.child(
                h_flex()
                    .id(("ws-message", seq))
                    .test_support()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .when(log.selected == Some(seq), |row| row.bg(theme.accent))
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
                            .text_color(if outgoing { theme.warning } else { theme.info })
                            .child(if outgoing { "↑" } else { "↓" }),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(preview))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format_size(payload_size(&record.message.payload))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.select_ws_message(seq, window, cx))),
            );
        }

        v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(div().text_sm().text_color(color).child(status))
            .child(text_input(&self.stream_filter).small())
            .child(rows)
            .child(if log.selected.is_some() {
                readonly_editor(&self.stream_detail).h(px(180.)).into_any_element()
            } else {
                div()
                    .h(px(40.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("request.ws_select_hint").to_string())
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

    #[test]
    fn keeps_the_latest_messages() {
        let mut log = WsLog::new();
        for i in 0..MAX_MESSAGES + 3 {
            log.push(WsMessage {
                outgoing: i % 2 == 0,
                payload: WsPayload::Text(i.to_string()),
            });
        }
        assert_eq!(log.messages.len(), MAX_MESSAGES);
        assert_eq!(log.total, MAX_MESSAGES + 3);
        assert!(log.record(2).is_none());
    }

    #[test]
    fn describes_payloads() {
        assert_eq!(describe(&WsPayload::Text("{\"a\":1}".into())).1, "{\n  \"a\": 1\n}");
        assert!(describe(&WsPayload::Binary(vec![0xde, 0xad])).1.starts_with("de ad"));
    }
}
