use std::ops::Range;
use std::sync::Arc;

use reflexo_typst::debug_loc::{DocumentPosition, LspPosition, SourceLocation, SourceSpanOffset};
use reflexo_vec2svg::IncrSvgDocServer;
use tinymist_std::typst::TypstDocument;
use tokio::sync::{broadcast, mpsc, watch};

use super::{editor::EditorActorRequest, webview::WebviewActorRequest};
use crate::debug_loc::SpanInterner;
use crate::outline::Outline;
use crate::protocol;
use crate::{CompileView, DocToSrcJumpInfo, ResolveSourceLocRequest};

/// A page connected to the preview, as its actors know it.
pub type PageId = u64;

#[derive(Debug, Clone)]
pub enum RenderActorRequest {
    /// The whole document again, for one page: the one that asked (it has just
    /// connected, or reconnected). Every page's actors hear it, and the others
    /// leave it: a full render starts a page's render state afresh, and a page
    /// that did not ask would have to take the whole document again.
    RenderFullLatest(PageId),
    RenderIncremental,
    EditorResolveSpanRange(Range<SourceSpanOffset>),
    WebviewResolveFrameLoc(DocumentPosition),
    ResolveSourceLoc(ResolveSourceLocRequest),
}

impl RenderActorRequest {
    pub fn is_full_render(&self, page: PageId) -> bool {
        match self {
            Self::RenderFullLatest(asked) => *asked == page,
            Self::RenderIncremental => false,
            Self::EditorResolveSpanRange(_) => false,
            Self::ResolveSourceLoc(_) => false,
            Self::WebviewResolveFrameLoc(_) => false,
        }
    }
}

pub struct RenderActor {
    /// The page this actor renders for.
    page: PageId,
    mailbox: broadcast::Receiver<RenderActorRequest>,
    /// Closes when the actor's page disconnects.
    page_gone: watch::Receiver<()>,
    view: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
    renderer: IncrSvgDocServer,
    editor_conn_sender: mpsc::UnboundedSender<EditorActorRequest>,
    svg_sender: mpsc::UnboundedSender<Vec<u8>>,
    webview_sender: broadcast::Sender<WebviewActorRequest>,
    /// See [`crate::PreviewConfig::navigate_in_page`].
    navigate_in_page: bool,
}

impl RenderActor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        page: PageId,
        mailbox: broadcast::Receiver<RenderActorRequest>,
        page_gone: watch::Receiver<()>,
        view: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
        editor_conn_sender: mpsc::UnboundedSender<EditorActorRequest>,
        svg_sender: mpsc::UnboundedSender<Vec<u8>>,
        webview_sender: broadcast::Sender<WebviewActorRequest>,
        navigate_in_page: bool,
    ) -> Self {
        Self {
            page,
            mailbox,
            page_gone,
            view,
            renderer: Self::new_renderer(),
            editor_conn_sender,
            svg_sender,
            webview_sender,
            navigate_in_page,
        }
    }

    fn new_renderer() -> IncrSvgDocServer {
        IncrSvgDocServer::default()
    }

    async fn process_message(&mut self, msg: RenderActorRequest) -> bool {
        log::trace!("RenderActor: received message: {msg:?}");

        let res = msg.is_full_render(self.page);
        match msg {
            RenderActorRequest::EditorResolveSpanRange(span_range) => {
                log::debug!("RenderActor: resolving EditorResolveSpanRange: {span_range:?}");

                self.editor_resolve_span_range(span_range);
            }
            RenderActorRequest::WebviewResolveFrameLoc(frame_loc) => {
                log::debug!("RenderActor: resolving WebviewResolveFrameLoc: {frame_loc:?}");
                let spans = self.resolve_span_by_frame_loc(&frame_loc);

                log::debug!("RenderActor: resolved WebviewResolveFrameLoc: {spans:?}");
                // end position is used
                if let Some(spans) = spans {
                    self.editor_resolve_span_range(spans.0..spans.1);
                }
            }
            RenderActorRequest::ResolveSourceLoc(req) => {
                log::debug!("RenderActor: resolving ResolveSourceLoc: {req:?}");

                self.resolve_source_loc(req);
            }
            RenderActorRequest::RenderFullLatest(_) | RenderActorRequest::RenderIncremental => {}
        }

        res
    }

    pub async fn run(mut self) {
        loop {
            let mut has_full_render = false;
            log::debug!("RenderActor: waiting for message");
            let msg = tokio::select! {
                biased;
                _ = self.page_gone.changed() => {
                    log::info!("RenderActor: the page is gone");
                    break;
                }
                msg = self.mailbox.recv() => msg,
            };
            match msg {
                Ok(msg) => {
                    has_full_render |= self.process_message(msg).await;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    log::info!("RenderActor: no more messages");
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    log::info!("RenderActor: lagged message. Some events are dropped");
                }
            }
            // read the queue to empty
            while let Ok(msg) = self.mailbox.try_recv() {
                has_full_render |= self.process_message(msg).await;
            }
            // if a full render is requested, we render the latest document
            // otherwise, we render the incremental changes for only once
            let has_full_render = has_full_render;
            log::debug!("RenderActor: has_full_render: {has_full_render}");
            let Some(document) = self.view.read().as_ref().and_then(|view| view.doc()) else {
                log::info!("RenderActor: document is not ready");
                continue;
            };

            let data = self.render(has_full_render, &document);
            let Ok(_) = self.svg_sender.send(data) else {
                log::info!("RenderActor: svg_sender is dropped");
                break;
            };
            // A page without an editor (the hub's, on a phone) holds changes
            // back while it is read; the version of what it was just sent
            // tells it whether there is anything to show, and an unchanged
            // document coming again on a new connection from a changed one.
            if self.navigate_in_page
                && let TypstDocument::Paged(paged) = &document
            {
                let pages = paged.pages().iter();
                let version = typst::utils::hash128(&Drawn(pages.map(|p| (&p.frame, &p.fill))));
                let frame = format!("doc-version,{version:032x}").into_bytes();
                if self.svg_sender.send(frame).is_err() {
                    break;
                }
            }
        }
        log::info!("RenderActor: exiting")
    }

    fn render(&mut self, has_full_render: bool, document: &TypstDocument) -> Vec<u8> {
        if has_full_render {
            self.render_full(document)
        } else {
            self.render_delta(document)
        }
    }

    #[typst_macros::time]
    fn render_full(&mut self, document: &TypstDocument) -> Vec<u8> {
        let mut renderer = Self::new_renderer();
        let delta = renderer.pack_delta(document);
        self.renderer = renderer;
        match protocol::full_current_frame_from_delta(&delta) {
            Some(frame) => frame,
            None => {
                log::warn!("fresh preview renderer did not produce a diff-v1 frame");
                delta
            }
        }
    }

    #[typst_macros::time]
    fn render_delta(&mut self, document: &TypstDocument) -> Vec<u8> {
        self.renderer.pack_delta(document)
    }

    fn view(&self) -> Option<Arc<dyn CompileView>> {
        self.view.read().clone()
    }

    fn editor_resolve_span_range(&self, span_range: Range<SourceSpanOffset>) -> Option<()> {
        let info = self.resolve_span_range(span_range)?;
        if self.navigate_in_page {
            // Every page's render actor hears every click, so the page acts on
            // it only if it was clicked itself (the frontend checks).
            if info.cross_document {
                let target = serde_json::json!({ "filepath": info.filepath, "label": info.label });
                let mut data = b"navigate,".to_vec();
                serde_json::to_writer(&mut data, &target).ok()?;
                let _ = self.svg_sender.send(data);
            }
            return Some(());
        }
        let req = EditorActorRequest::DocToSrcJump(info);
        let _ = self.editor_conn_sender.send(req);

        Some(())
    }

    fn resolve_span_range(&self, range: Range<SourceSpanOffset>) -> Option<DocToSrcJumpInfo> {
        let view = self.view()?;

        // A click on a link into another document goes to the linked place in
        // that document, not to the link's own source.
        if let Some(target) = view.resolve_cross_document_link(range.start.span, range.start.offset)
        {
            return Some(target);
        }

        // Resolves FileLoC of start, end, and the element wide
        let st_res = view.resolve_span(range.start.span, Some(range.start.offset));
        let ed_res = view.resolve_span(range.end.span, Some(range.end.offset));
        let elem_res = view.resolve_span(range.end.span, None);

        // Combines the result of start and end
        let range_res = match (st_res, ed_res) {
            (Some(st), Some(ed)) => {
                if st.filepath == ed.filepath
                    && matches!((&st.start, &st.end), (Some(x), Some(y)) if x <= y)
                {
                    Some(DocToSrcJumpInfo {
                        filepath: st.filepath,
                        start: st.start,
                        end: ed.start,
                        cross_document: false,
                        label: None,
                    })
                } else {
                    Some(ed)
                }
            }
            (Some(info), None) | (None, Some(info)) => Some(info),
            (None, None) => None,
        };

        // Account for the case where the start and end are out of order.
        //
        // This could happen because typst supports scripting, which makes text out of
        // order
        let range_res = {
            let mut range_res = range_res;
            if let Some(info) = &mut range_res
                && let Some((x, y)) = info.start.zip(info.end)
                && y <= x
            {
                std::mem::swap(&mut info.start, &mut info.end);
            }

            range_res
        };

        // Restricts the range to the element's range
        match (elem_res, range_res) {
            (Some(elem), Some(mut rng)) if elem.filepath == rng.filepath => {
                // Account for the case where the element's range is out of order.
                let elem_start = elem.start.or(elem.end);
                let elem_end = elem.end.or(elem_start);

                // Account for the case where the range is out of order.
                let rng_start = rng.start.or(rng.end);
                let rng_end = rng.end.or(rng_start);

                if let Some((((u, inner_u), inner_v), v)) =
                    elem_start.zip(rng_start).zip(rng_end).zip(elem_end)
                {
                    rng.start = Some(inner_u.max(u).min(v));
                    rng.end = Some(inner_v.max(u).min(v));
                }
                Some(rng)
            }
            (.., Some(info)) | (Some(info), None) => Some(info),
            (None, None) => None,
        }
    }

    fn resolve_source_loc(&self, req: ResolveSourceLocRequest) -> Option<()> {
        // todo: change name to resolve resolve src position
        let info = self
            .view()?
            .resolve_document_position(crate::Location::Src(SourceLocation {
                filepath: req.filepath.to_string_lossy().to_string(),
                pos: LspPosition {
                    line: req.line,
                    character: req.character,
                },
            }));

        if info.is_empty() {
            return None;
        }

        let _ = self.webview_sender.send(WebviewActorRequest::SrcToDocJump(
            info.into_iter()
                .map(|info| DocumentPosition {
                    page_no: info.page.into(),
                    x: info.point.x.to_pt() as f32,
                    y: info.point.y.to_pt() as f32,
                })
                .collect(),
        ));

        Some(())
    }

    /// Gets the span range of the given frame loc.
    pub fn resolve_span_by_frame_loc(
        &mut self,
        pos: &DocumentPosition,
    ) -> Option<(SourceSpanOffset, SourceSpanOffset)> {
        let view = self.view.read();
        view.as_ref()?.resolve_frame_loc(pos)
    }
}

/// What a document's pages draw, to hash: its version as a page without an
/// editor sees it. Typst's own hash of the pages covers the source positions
/// too, which the incremental parser renumbers, so an edit undone, or a
/// comment changed, would make another version of an unchanged document.
struct Drawn<'a, I>(I)
where
    I: Iterator<Item = (&'a typst::layout::Frame, &'a typst::foundations::Smart<Option<typst::visualize::Paint>>)>
        + Clone;

impl<'a, I> std::hash::Hash for Drawn<'a, I>
where
    I: Iterator<Item = (&'a typst::layout::Frame, &'a typst::foundations::Smart<Option<typst::visualize::Paint>>)>
        + Clone,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for (frame, fill) in self.0.clone() {
            fill.hash(state);
            drawn(frame, state);
        }
    }
}

fn drawn<H: std::hash::Hasher>(frame: &typst::layout::Frame, state: &mut H) {
    use std::hash::Hash;
    use typst::layout::FrameItem;
    use typst::model::Destination;
    frame.size().hash(state);
    for (point, item) in frame.items() {
        point.hash(state);
        match item {
            FrameItem::Group(group) => {
                0u8.hash(state);
                group.transform.hash(state);
                group.clip.hash(state);
                drawn(&group.frame, state);
            }
            FrameItem::Text(text) => {
                1u8.hash(state);
                text.font.hash(state);
                text.size.hash(state);
                text.fill.hash(state);
                text.stroke.hash(state);
                text.text.hash(state);
                for glyph in &text.glyphs {
                    glyph.id.hash(state);
                    glyph.x_advance.hash(state);
                    glyph.x_offset.hash(state);
                    glyph.y_advance.hash(state);
                    glyph.y_offset.hash(state);
                }
            }
            FrameItem::Shape(shape, _) => {
                2u8.hash(state);
                shape.hash(state);
            }
            FrameItem::Image(image, size, _) => {
                3u8.hash(state);
                image.hash(state);
                size.hash(state);
            }
            FrameItem::Link(destination, size) => {
                4u8.hash(state);
                match destination {
                    Destination::Url(url) => url.hash(state),
                    Destination::Position(position) => position.hash(state),
                    // where a location lands is in the pages already
                    Destination::Location(_) => {}
                }
                size.hash(state);
            }
            FrameItem::Tag(_) => {}
        }
    }
}

pub struct OutlineRenderActor {
    signal: broadcast::Receiver<RenderActorRequest>,
    /// Closes when the actor's page disconnects.
    page_gone: watch::Receiver<()>,
    document: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
    editor_tx: mpsc::UnboundedSender<EditorActorRequest>,
    /// The page's own channel: its contents panel shows the outline too.
    page_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// The outline the page has, so that an unchanged one is not sent again.
    page_outline: Vec<u8>,

    span_interner: SpanInterner,
}

impl OutlineRenderActor {
    pub fn new(
        signal: broadcast::Receiver<RenderActorRequest>,
        page_gone: watch::Receiver<()>,
        document: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
        editor_tx: mpsc::UnboundedSender<EditorActorRequest>,
        page_tx: mpsc::UnboundedSender<Vec<u8>>,
        span_interner: SpanInterner,
    ) -> Self {
        Self {
            signal,
            page_gone,
            document,
            editor_tx,
            page_tx,
            page_outline: Vec::new(),
            span_interner,
        }
    }

    pub async fn run(mut self) {
        loop {
            log::debug!("OutlineRenderActor: waiting for message");
            let msg = tokio::select! {
                biased;
                _ = self.page_gone.changed() => {
                    log::info!("OutlineRenderActor: the page is gone");
                    break;
                }
                msg = self.signal.recv() => msg,
            };
            match msg {
                Ok(msg) => {
                    log::debug!("OutlineRenderActor: received message: {msg:?}");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    log::info!("OutlineRenderActor: no more messages");
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    log::info!("OutlineRenderActor: lagged message. Some events are dropped");
                }
            }
            // read the queue to empty
            while self.signal.try_recv().is_ok() {}
            // if a full render is requested, we render the latest document
            // otherwise, we render the incremental changes for only once
            let Some(document) = self.document.read().as_ref().and_then(|view| view.doc()) else {
                log::info!("OutlineRenderActor: document is not ready");
                continue;
            };
            let data = self.outline(&document).await;
            let mut page_outline = b"outline,".to_vec();
            if serde_json::to_writer(&mut page_outline, &data).is_ok()
                && page_outline != self.page_outline
            {
                if self.page_tx.send(page_outline.clone()).is_err() {
                    log::info!("OutlineRenderActor: the page's channel is closed");
                    break;
                }
                self.page_outline = page_outline;
            }
            log::debug!("OutlineRenderActor: sending outline");
            let Ok(_) = self.editor_tx.send(EditorActorRequest::Outline(data)) else {
                log::info!("OutlineRenderActor: outline_sender is dropped");
                break;
            };
        }
        log::info!("OutlineRenderActor: exiting")
    }

    async fn outline(&self, document: &TypstDocument) -> Outline {
        self.span_interner
            .with_writer(|interner| {
                interner.reset();
                crate::outline::outline(interner, document)
            })
            .await
    }
}
