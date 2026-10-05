use std::sync::Arc;

use tinymist_std::typst::TypstDocument;
use tokio::sync::{broadcast, mpsc, watch};

use super::render::RenderActorRequest;
use crate::CompileView;

pub struct HtmlRenderActor {
    mailbox: broadcast::Receiver<RenderActorRequest>,
    /// Closes when the actor's page disconnects.
    page_gone: watch::Receiver<()>,
    view: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
    frame_sender: mpsc::UnboundedSender<Vec<u8>>,
}

impl HtmlRenderActor {
    pub fn new(
        mailbox: broadcast::Receiver<RenderActorRequest>,
        page_gone: watch::Receiver<()>,
        view: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
        frame_sender: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Self {
        Self {
            mailbox,
            page_gone,
            view,
            frame_sender,
        }
    }

    pub async fn run(mut self) {
        loop {
            let msg = tokio::select! {
                biased;
                _ = self.page_gone.changed() => {
                    log::info!("HtmlRenderActor: the page is gone");
                    break;
                }
                msg = self.mailbox.recv() => msg,
            };
            match msg {
                Ok(
                    RenderActorRequest::RenderFullLatest | RenderActorRequest::RenderIncremental,
                ) => {}
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    log::info!("HtmlRenderActor: no more messages");
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    log::info!("HtmlRenderActor: lagged message. Some events are dropped");
                }
            }

            while let Ok(msg) = self.mailbox.try_recv() {
                if !matches!(
                    msg,
                    RenderActorRequest::RenderFullLatest | RenderActorRequest::RenderIncremental
                ) {
                    continue;
                }
            }

            let Some(document) = self.view.read().as_ref().and_then(|view| view.doc()) else {
                log::info!("HtmlRenderActor: document is not ready");
                continue;
            };

            let TypstDocument::Html(document) = &document else {
                continue;
            };

            let html = match typst_html::html(document, &typst_html::HtmlOptions::default()) {
                Ok(html) => html,
                Err(err) => {
                    log::warn!("failed to encode HTML preview document: {err:?}");
                    continue;
                }
            };

            if self
                .frame_sender
                .send(prefixed_frame(b"html,", html.into_bytes()))
                .is_err()
            {
                log::info!("HtmlRenderActor: frame_sender is dropped");
                break;
            }
        }

        log::info!("HtmlRenderActor: exiting");
    }
}

fn prefixed_frame(prefix: &[u8], payload: Vec<u8>) -> Vec<u8> {
    let mut frame = Vec::with_capacity(prefix.len() + payload.len());
    frame.extend_from_slice(prefix);
    frame.extend_from_slice(&payload);
    frame
}
