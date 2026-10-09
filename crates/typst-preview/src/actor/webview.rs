use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use reflexo_typst::debug_loc::DocumentPosition;
use tinymist_std::error::IgnoreLogging;
use tokio::sync::{broadcast, mpsc};

use super::{editor::EditorActorRequest, render::RenderActorRequest};
use crate::{
    CompileView, NavigateMessage, PendingScroll, ViewerWindowStateMessage, WsMessage,
    actor::editor::DocToSrcJumpResolveRequest,
};

// pub type CursorPosition = DocumentPosition;
pub type SrcToDocJumpInfo = DocumentPosition;

#[derive(Debug, Clone)]
pub enum WebviewActorRequest {
    ViewportPosition(DocumentPosition),
    SrcToDocJump(Vec<SrcToDocJumpInfo>),
    // CursorPosition(CursorPosition),
}

fn position_req(
    event: &'static str,
    DocumentPosition { page_no, x, y }: DocumentPosition,
) -> String {
    format!("{event},{page_no} {x} {y}")
}

fn positions_req(event: &'static str, positions: Vec<DocumentPosition>) -> String {
    format!("{event},")
        + &positions
            .iter()
            .map(|DocumentPosition { page_no, x, y }| format!("{page_no} {x} {y}"))
            .collect::<Vec<_>>()
            .join(",")
}

pub struct WebviewActor<'a, C> {
    webview_websocket_conn: std::pin::Pin<&'a mut C>,
    svg_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    mailbox: broadcast::Receiver<WebviewActorRequest>,

    broadcast_sender: broadcast::Sender<WebviewActorRequest>,
    editor_sender: mpsc::UnboundedSender<EditorActorRequest>,
    render_sender: broadcast::Sender<RenderActorRequest>,
    /// The compiled document, for the labels the page asks to land on.
    view: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
    /// A scroll requested before this page had a document: by the editor
    /// before the page connected, or by the page's address before the first
    /// compile. Sent once the page has its first document.
    deferred_scroll: Option<PendingScroll>,
}

pub struct Channels {
    pub svg: (
        mpsc::UnboundedSender<Vec<u8>>,
        mpsc::UnboundedReceiver<Vec<u8>>,
    ),
}

impl<'a, C> WebviewActor<'a, C>
where
    C: futures::Sink<WsMessage, Error = reflexo_typst::Error>
        + futures::Stream<Item = Result<WsMessage, reflexo_typst::Error>>,
{
    pub fn set_up_channels() -> Channels {
        Channels {
            svg: mpsc::unbounded_channel(),
        }
    }
    pub fn new(
        websocket_conn: std::pin::Pin<&'a mut C>,
        svg_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
        broadcast_sender: broadcast::Sender<WebviewActorRequest>,
        mailbox: broadcast::Receiver<WebviewActorRequest>,
        editor_sender: mpsc::UnboundedSender<EditorActorRequest>,
        render_sender: broadcast::Sender<RenderActorRequest>,
        view: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
        deferred_scroll: Option<PendingScroll>,
    ) -> Self {
        Self {
            webview_websocket_conn: websocket_conn,
            svg_receiver,
            mailbox,
            broadcast_sender,
            editor_sender,
            render_sender,
            view,
            deferred_scroll,
        }
    }

    /// Replays the scroll that was waiting for this page. The page itself
    /// retries the scroll until the requested page has rendered.
    async fn flush_deferred_scroll(&mut self) {
        let Some(scroll) = self.deferred_scroll.take() else {
            return;
        };
        log::info!("WebviewActor: replaying the scroll requested before the page had a document");
        match scroll {
            PendingScroll::Label(label) => self.jump_to_label(label).await,
            PendingScroll::Source(req) => {
                self.render_sender
                    .send(RenderActorRequest::ResolveSourceLoc(req))
                    .log_error("WebviewActor");
            }
            PendingScroll::Position(pos) => {
                self.broadcast_sender
                    .send(WebviewActorRequest::ViewportPosition(pos))
                    .log_error("WebviewActor");
            }
        }
    }

    /// Lands this page, and only this page, on the element carrying `label`:
    /// the label its address names (`…/#label`), set by an editor following a
    /// link into this document. Before the first compile the request waits for
    /// the page's first document.
    async fn jump_to_label(&mut self, label: String) {
        let view = self.view.read().clone();
        let Some(view) = view.filter(|view| view.doc().is_some()) else {
            self.deferred_scroll = Some(PendingScroll::Label(label));
            return;
        };
        let Some(pos) = view.resolve_label(&label) else {
            log::info!("WebviewActor: no element carries the label <{label}>");
            return;
        };
        let pos = DocumentPosition {
            page_no: pos.page.into(),
            x: pos.point.x.to_pt() as f32,
            y: pos.point.y.to_pt() as f32,
        };
        log::info!("WebviewActor: landing on <{label}> at {pos:?}");
        let msg = positions_req("jump", vec![pos]);
        self.webview_websocket_conn
            .send(WsMessage::Binary(msg.into()))
            .await
            .log_error("WebViewActor");
    }

    pub async fn run(mut self) {
        loop {
            tokio::select! {
                Ok(msg) = self.mailbox.recv() => {
                    log::trace!("WebviewActor: received message from mailbox: {msg:?}");
                    match msg {
                        WebviewActorRequest::SrcToDocJump(jump_info) => {
                            let msg = positions_req("jump", jump_info);
                            self.webview_websocket_conn.send(WsMessage::Binary(msg.into()))
                              .await.log_error("WebViewActor");
                        }
                        WebviewActorRequest::ViewportPosition(jump_info) => {
                            let msg = position_req("viewport", jump_info);
                            self.webview_websocket_conn.send(WsMessage::Binary(msg.into()))
                              .await.log_error("WebViewActor");
                        }
                    }
                }
                Some(svg) = self.svg_receiver.recv() => {
                    log::trace!("WebviewActor: received svg from renderer");
                    let _scope = typst_timing::TimingScope::new("webview_actor_send_svg");
                    self.webview_websocket_conn.send(WsMessage::Binary(svg.into()))
                    .await.log_error("WebViewActor");
                    self.flush_deferred_scroll().await;
                }
                Some(msg) = self.webview_websocket_conn.next() => {
                    log::trace!("WebviewActor: received message from websocket: {msg:?}");
                    let Ok(msg) = msg else {
                        log::info!("WebviewActor: no more messages from websocket: {}", msg.unwrap_err());
                      break;
                    };
                    let msg = match msg {
                        WsMessage::Text(msg) => msg,
                        WsMessage::Ping(msg) => {
                            let _ = self.webview_websocket_conn.send(WsMessage::Pong(msg)).await;
                            continue;
                        },
                        WsMessage::Pong(..) => {
                            continue;
                        },
                        _ =>  {
                            log::info!("WebviewActor: received non-text message from websocket: {msg:?}");
                            let _ = self.webview_websocket_conn.send(WsMessage::Text(format!("Webview Actor: error, received non-text message: {msg:?}")))
                            .await;
                            break;
                        }
                    };
                    if msg == "current" {
                        self.render_sender.send(RenderActorRequest::RenderFullLatest).log_error("WebViewActor");
                    } else if msg.starts_with("srclocation") {
                        let location = msg.split(' ').nth(1).unwrap();
                        self.editor_sender.send(EditorActorRequest::DocToSrcJumpResolve(
                            DocToSrcJumpResolveRequest {
                                span: location.trim().to_owned(),
                            },
                        )).log_error("WebViewActor");
                    } else if msg.starts_with("outline-sync") {
                        let location = msg.split(',').nth(1).unwrap();
                        let location = location.split(' ').collect::<Vec::<&str>>();
                        let page_no = location[0].parse().unwrap();
                        let x = location.get(1).map(|s| s.parse().unwrap()).unwrap_or(0.);
                        let y = location.get(2).map(|s| s.parse().unwrap()).unwrap_or(0.);
                        let pos = DocumentPosition { page_no, x, y };

                        self.broadcast_sender.send(WebviewActorRequest::ViewportPosition(pos)).log_error("WebViewActor");
                    } else if msg.starts_with("src-point") {
                        let path = msg.split(' ').nth(1).unwrap();
                        let path = serde_json::from_str(path);
                        if let Ok(path) = path {
                            self.render_sender.send(RenderActorRequest::WebviewResolveFrameLoc(path)).log_error("WebViewActor");
                        };
                    } else if let Some(label) = msg.strip_prefix("jump-label ") {
                        self.jump_to_label(label.trim().to_owned()).await;
                    } else if let Some(step) = msg.strip_prefix("nav ") {
                        // `nav <kind> [<page> <x> <y>]`: for the editor's history.
                        let mut parts = step.split_whitespace();
                        let kind = parts.next().unwrap_or_default().to_owned();
                        let numbers: Vec<f32> = parts.filter_map(|s| s.parse().ok()).collect();
                        let position = match numbers[..] {
                            [page_no, x, y] if page_no >= 1. => Some(DocumentPosition { page_no: page_no as usize, x, y }),
                            _ => None,
                        };
                        self.editor_sender.send(EditorActorRequest::Navigate(NavigateMessage { kind, position })).log_error("WebViewActor");
                    } else if let Some(state) = msg.strip_prefix("viewer-window-state ") {
                        if let Ok(state) = serde_json::from_str::<ViewerWindowStateMessage>(state) {
                            self.editor_sender.send(EditorActorRequest::ViewerWindowState(state)).log_error("WebViewActor");
                        };
                    } else {
                        let err = self.webview_websocket_conn.send(WsMessage::Text(format!("error, received unknown message: {msg}"))).await;
                        log::info!("WebviewActor: received unknown message from websocket: {msg} {err:?}");
                        break;
                    }
                }
                else => {
                    break;
                }
            }
        }
        log::info!("WebviewActor: exiting");
    }
}
