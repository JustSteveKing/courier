//! gRPC calls in the editor: choosing a method from the server's own description or from
//! `.proto` files, writing the message as JSON, and watching the replies come back.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::select::{SearchableVec, Select};
use gpui_kit::component::{ActiveTheme as _, IconName, IndexPath, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use rust_i18n::t;

use super::sse::Ending;
use super::{RequestEditor, RequestEditorEvent};
use crate::grpc;
use crate::transport::{WsMessage, WsPayload};

/// What the editor knows about the server it's calling.
#[derive(Default)]
pub(super) struct GrpcState {
    /// The schema, and what it was loaded from, so it reloads when that changes.
    pub schema: Option<(String, Arc<grpc::Schema>)>,
    pub loading: bool,
    /// Why the last load failed, for the row under the method picker.
    pub error: Option<String>,
}

impl GrpcState {
    pub fn methods(&self) -> &[grpc::Method] {
        self.schema
            .as_ref()
            .map(|(_, schema)| schema.methods.as_slice())
            .unwrap_or_default()
    }
}

impl RequestEditor {
    /// What a schema is loaded from: the endpoint, or the `.proto` files.
    fn grpc_source(&self, cx: &App) -> String {
        let protos = self.grpc_protos.read(cx).value().trim().to_string();
        if protos.is_empty() {
            format!("reflect:{}", self.url.read(cx).value().trim())
        } else {
            format!("protos:{protos}")
        }
    }

    /// Reads the server's API, from `.proto` files when given, otherwise by asking the
    /// server. `force` reloads even when it hasn't changed.
    pub(super) fn load_grpc_schema(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.grpc_source(cx);
        if self.grpc.loading || (!force && self.grpc.schema.as_ref().is_some_and(|(had, _)| *had == source)) {
            return;
        }
        let endpoint = crate::model::interpolate(&self.url.read(cx).value(), &self.variables).0;
        let protos = self.grpc_protos.read(cx).value().trim().to_string();
        let project = self.project_dir().to_path_buf();
        let timeout = self.effective_settings(&self.current(cx), cx).timeout_secs;
        self.grpc.loading = true;
        self.grpc.error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    if protos.is_empty() {
                        let options = grpc::CallOptions {
                            timeout_secs: timeout,
                            ..Default::default()
                        };
                        crate::transport::on_runtime(
                            async move { grpc::schema_from_reflection(&endpoint, &options).await },
                        )
                        .await
                    } else {
                        grpc::schema_from_protos(&grpc::proto_paths(&protos, &project), &[])
                    }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.grpc.loading = false;
                match loaded {
                    Ok(schema) => {
                        this.grpc.schema = Some((source, Arc::new(schema)));
                        this.grpc.error = None;
                        this.sync_grpc_methods(window, cx);
                    }
                    Err(e) => this.grpc.error = Some(format!("{e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Puts the loaded methods in the picker, keeping the one the request names.
    pub(super) fn sync_grpc_methods(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = self
            .saved
            .as_ref()
            .and_then(|saved| saved.grpc.as_ref())
            .map(|grpc| grpc.method.clone());
        let labels: Vec<SharedString> = self
            .grpc
            .methods()
            .iter()
            .map(|method| SharedString::from(method.label()))
            .collect();
        let selected = wanted
            .as_deref()
            .and_then(|path| self.grpc.methods().iter().position(|method| method.path() == path))
            .unwrap_or(0);
        self.grpc_method.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(labels), window, cx);
            state.set_selected_index(Some(IndexPath::new(selected)), window, cx);
        });
        cx.notify();
    }

    /// The method the picker is on.
    pub(super) fn selected_grpc_method(&self, cx: &App) -> Option<grpc::Method> {
        let index = self.grpc_method.read(cx).selected_index(cx).map_or(0, |ix| ix.row);
        self.grpc.methods().get(index).cloned()
    }

    /// Makes the call, putting every message and the closing status on the timeline.
    pub(super) fn send_grpc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some((_, schema)) = self.grpc.schema.clone() else {
            // Nothing to call with yet: read the API first, then the send can be repeated.
            self.load_grpc_schema(true, window, cx);
            cx.emit(RequestEditorEvent::Error(t!("grpc.no_schema").to_string()));
            return;
        };
        let Some(method) = self.selected_grpc_method(cx) else {
            cx.emit(RequestEditorEvent::Error(t!("grpc.no_method").to_string()));
            return;
        };

        let file = self.current(cx);
        let settings = self.effective_settings(&file, cx);
        let variables = self.variables.clone();
        let endpoint = crate::model::interpolate(&self.url.read(cx).value(), &variables).0;
        // One message for a plain call; a client-streaming call sends the saved ones too.
        let mut messages = vec![crate::model::interpolate(&self.body.read(cx).value(), &variables).0];
        if method.client_streaming {
            messages.extend(
                file.messages
                    .iter()
                    .map(|template| crate::model::interpolate(&template.content, &variables).0),
            );
            messages.retain(|message| !message.trim().is_empty());
        }
        let options = grpc::CallOptions {
            metadata: file
                .headers
                .iter()
                .filter(|header| header.enabled)
                .map(|header| {
                    (
                        crate::model::interpolate(&header.name, &variables).0,
                        crate::model::interpolate(&header.value, &variables).0,
                    )
                })
                .collect(),
            timeout_secs: settings.timeout_secs,
            accept_invalid_certs: !settings.verify_tls,
            ca_pem: settings
                .ca_certificate
                .as_ref()
                .and_then(|path| std::fs::read(path).ok()),
        };

        // The timeline starts fresh, with what we're sending already on it.
        let state = self.responses.entry(path.clone()).or_default();
        let mut log = super::ws::WsLog::new();
        log.status = method.label();
        log.connected = true;
        for message in &messages {
            log.push(WsMessage {
                outgoing: true,
                payload: WsPayload::Text(message.clone()),
            });
        }
        state.ws = Some(log);
        state.sse = None;
        cx.notify();

        let (events, incoming) = async_channel::unbounded();
        cx.spawn_in(window, async move |this, cx| {
            let call = cx.background_executor().spawn({
                let schema = schema.clone();
                async move {
                    crate::transport::on_runtime(async move {
                        grpc::call(&endpoint, &schema, &method.path(), &messages, &options, |event| {
                            let _ = events.send_blocking(event);
                        })
                        .await
                    })
                    .await
                }
            });
            while let Ok(event) = incoming.recv().await {
                let stop = this
                    .update(cx, |this, cx| {
                        let Some(log) = this.responses.get_mut(&path).and_then(|state| state.ws.as_mut()) else {
                            return true;
                        };
                        match event {
                            grpc::Event::Message(json) => log.push(WsMessage {
                                outgoing: false,
                                payload: WsPayload::Text(json),
                            }),
                            grpc::Event::Finished {
                                code,
                                status,
                                message,
                                metadata,
                            } => {
                                log.connected = false;
                                let detail = [message, metadata_summary(&metadata)]
                                    .into_iter()
                                    .filter(|part| !part.is_empty())
                                    .collect::<Vec<_>>()
                                    .join(" · ");
                                log.status = if detail.is_empty() {
                                    status.clone()
                                } else {
                                    format!("{status} · {detail}")
                                };
                                log.ended = Some(if code == 0 {
                                    Ending::Closed
                                } else {
                                    Ending::Failed(log.status.clone())
                                });
                            }
                        }
                        cx.notify();
                        false
                    })
                    .unwrap_or(true);
                if stop {
                    return;
                }
            }
            if let Err(e) = call.await {
                this.update(cx, |this, cx| {
                    if let Some(log) = this.responses.get_mut(&path).and_then(|state| state.ws.as_mut()) {
                        log.connected = false;
                        log.ended = Some(Ending::Failed(format!("{e:#}")));
                    }
                    cx.emit(RequestEditorEvent::Error(format!("{e:#}")));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// The row above a gRPC call: which method, where its description comes from, and how
    /// that went.
    pub(super) fn render_grpc_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let streaming = self
            .selected_grpc_method(cx)
            .filter(|method| method.streaming())
            .map(|method| match (method.client_streaming, method.server_streaming) {
                (true, true) => t!("grpc.both_streaming"),
                (true, false) => t!("grpc.client_streaming"),
                _ => t!("grpc.server_streaming"),
            });
        let status = match (&self.grpc.error, self.grpc.loading) {
            (Some(error), _) => Some((error.clone(), theme.danger)),
            (None, true) => Some((t!("grpc.loading").to_string(), theme.muted_foreground)),
            (None, false) if self.grpc.schema.is_none() => {
                Some((t!("grpc.no_schema_yet").to_string(), theme.muted_foreground))
            }
            _ => None,
        };

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(Select::new(&self.grpc_method).small()))
                    .child(
                        Button::new("grpc-reload")
                            .ghost()
                            .small()
                            .icon(IconName::Undo2)
                            .tooltip(t!("grpc.reload").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.load_grpc_schema(true, window, cx))),
                    ),
            )
            .child(crate::ui::code_editor(&self.grpc_protos).h_10())
            .children(streaming.map(|label| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(label.to_string())
            }))
            .children(status.map(|(text, color)| {
                div()
                    .id("grpc-status")
                    .test_support()
                    .text_xs()
                    .text_color(color)
                    .child(text)
            }))
            .into_any_element()
    }
}

fn metadata_summary(metadata: &[(String, String)]) -> String {
    metadata
        .iter()
        // Plumbing every gRPC response carries; not worth the room.
        .filter(|(name, _)| !matches!(name.as_str(), "content-type" | "date" | "grpc-status" | "grpc-message"))
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join(", ")
}
