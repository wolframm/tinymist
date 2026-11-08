//! `tinymist hub`: the previews of a documents folder, for a phone or a tablet.
//!
//! The hub listens on the loopback only; `tailscale serve` publishes it inside
//! the tailnet. Its home page lists folders, the favourite ones and the
//! documents changed last. `/v/<path>` is the preview of one document: the hub
//! starts a `tinymist preview` for it on first use, on ports of its own on the
//! loopback, and keeps a session for each page: one connection to the
//! preview, which stays while the page is away (a phone closes the page's
//! connection whenever its screen locks), so that the page, back, gets only
//! what it missed. Large frames go compressed. A preview stops a while after
//! its last session has ended, or when more than a few run. There is no
//! editor: a click on a link into another document makes the page open that
//! one (`--navigate-in-page`).
//!
//! Every request must name the host `tailscale serve` is reached by and carry
//! the `Tailscale-User-Login` of a permitted user: Serve sets that header from
//! the tailnet identity and drops one a client sent. A web page in a browser on
//! this Mac can set neither, so it cannot use the hub through the loopback.
//! The hub only reads: it lists folders and `.typ` files under the documents
//! folder and previews them; the one thing it writes is the favourites list.

use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::ffi::OsStr;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_tungstenite::HyperWebsocket;
use hyper_tungstenite::tungstenite::Message;
use hyper_tungstenite::tungstenite::client::IntoClientRequest;
use hyper_tungstenite::tungstenite::protocol::WebSocketConfig;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use tinymist_assets::TYPST_PREVIEW_HTML;
use tinymist_preview::{PreviewMode, frontend_html};
use tinymist_std::error::prelude::*;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Notify, oneshot, watch};

const HOME_HTML: &str = include_str!("hub/index.html");
const ICON_PNG: &[u8] = include_bytes!("hub/icon.png");
const MANIFEST: &str = r##"{"name":"Typst documents","short_name":"Typst","start_url":"/","display":"standalone","background_color":"#f6f6f4","theme_color":"#f6f6f4","icons":[{"src":"/icon.png","sizes":"512x512","type":"image/png","purpose":"any"}]}"##;

/// Frames from this size on go to the page compressed (`z,` and raw deflate).
const COMPRESS_FROM: usize = 64 * 1024;
/// A page that has not answered a ping for this long is gone (a phone that
/// lost its network closes nothing).
const PAGE_TIMEOUT: Duration = Duration::from_secs(80);
/// How long a session waits for its page to come back.
const AWAY: Duration = Duration::from_secs(2 * 60 * 60);
/// A session whose page is away while this much piles up is given up: the
/// page gets the whole document again when it comes back.
const BEHIND_LIMIT: usize = 256 << 20;

type ChildSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// Serve the previews of a documents folder to a phone (through `tailscale
/// serve`).
#[derive(Debug, Clone, clap::Parser)]
pub struct HubArgs {
    /// The address to listen on. It must be on the loopback.
    #[clap(long, default_value = "127.0.0.1:23690", value_name = "ADDR")]
    pub listen: SocketAddr,

    /// The folder whose documents the hub shows [default: ~/Documents].
    #[clap(long, value_name = "DIR")]
    pub documents: Option<PathBuf>,

    /// The host a request must name, as `tailscale serve` passes it on, e.g.
    /// `mac.tailnet.ts.net:8443`. Repeat it for several.
    #[clap(long = "host", value_name = "HOST", required = true)]
    pub hosts: Vec<String>,

    /// The Tailscale login a request must come from. Repeat it for several.
    #[clap(long = "user", value_name = "LOGIN", required = true)]
    pub users: Vec<String>,

    /// The file that keeps the favourite folders [default:
    /// ~/Library/Application Support/tinymist-hub/favorites.json].
    #[clap(long, value_name = "FILE")]
    pub favorites: Option<PathBuf>,

    /// How many previews may run at once.
    #[clap(long, default_value_t = 4)]
    pub max_previews: usize,

    /// Minutes a preview stays after its last page has gone.
    #[clap(long, default_value_t = 15)]
    pub idle_minutes: u64,

    /// The previews' `--invert-colors`.
    #[clap(long, default_value = "auto")]
    pub invert_colors: String,
}

/// Entry point of the hub.
pub async fn hub_main(args: HubArgs) -> Result<()> {
    run(args)
        .await
        .map_err(|err| error_once!("the hub stopped", err: format!("{err:#}")))
}

async fn run(args: HubArgs) -> anyhow::Result<()> {
    if !args.listen.ip().is_loopback() {
        anyhow::bail!("--listen {} is not on the loopback", args.listen);
    }
    let documents = match args.documents {
        Some(dir) => dir,
        None => dirs::document_dir().ok_or_else(|| anyhow::anyhow!("no documents folder"))?,
    };
    let root = fs::canonicalize(&documents)
        .map_err(|err| anyhow::anyhow!("cannot open {}: {err}", documents.display()))?;
    let favorites_file = match args.favorites {
        Some(file) => file,
        None => dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("no application support folder"))?
            .join("tinymist-hub/favorites.json"),
    };
    let favorites = load_favorites(&favorites_file);
    let hub = Arc::new(Hub {
        root_str: root.to_string_lossy().into_owned(),
        root,
        origins: args.hosts.iter().map(|h| format!("https://{h}")).collect(),
        hosts: args.hosts,
        users: args.users,
        favorites_file,
        favorites: Mutex::new(favorites),
        previews: Mutex::default(),
        sessions: Mutex::default(),
        max_previews: args.max_previews.max(1),
        idle: Duration::from_secs(args.idle_minutes * 60),
        invert_colors: args.invert_colors,
        exe: std::env::current_exe()?,
        next_id: AtomicU64::new(1),
    });

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    log::info!(
        "hub: {} on http://{} for {:?} at {:?}",
        hub.root.display(),
        args.listen,
        hub.users,
        hub.hosts
    );

    tokio::spawn(hub.clone().reap_idle());

    let serve = {
        let hub = hub.clone();
        async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(err) => {
                        log::warn!("hub: accept: {err}");
                        continue;
                    }
                };
                let hub = hub.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let hub = hub.clone();
                        async move { Ok::<_, Infallible>(hub.handle(req).await) }
                    });
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(TokioIo::new(stream), service)
                        .await;
                });
            }
        }
    };

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = serve => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    hub.stop_all().await;
    log::info!("hub: stopped");
    Ok(())
}

struct Hub {
    /// The documents folder, canonical.
    root: PathBuf,
    root_str: String,
    hosts: Vec<String>,
    users: Vec<String>,
    origins: Vec<String>,
    favorites_file: PathBuf,
    favorites: Mutex<Vec<String>>,
    /// The previews by document, canonical.
    previews: Mutex<HashMap<PathBuf, Arc<Preview>>>,
    /// The pages' sessions, by the id each page chose.
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    max_previews: usize,
    idle: Duration,
    invert_colors: String,
    exe: PathBuf,
    next_id: AtomicU64,
}

/// One running `tinymist preview`.
struct Preview {
    id: u64,
    file: PathBuf,
    rel: String,
    state: watch::Receiver<Start>,
    /// The sessions on it now.
    pages: AtomicUsize,
    last_used: Mutex<Instant>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
}

#[derive(Debug, Clone)]
enum Start {
    Starting,
    Ready(SocketAddr),
    Ended(String),
}

impl Preview {
    fn stop(&self) {
        if let Some(kill) = self.kill.lock().take() {
            let _ = kill.send(());
        }
    }

    async fn address(&self) -> Result<SocketAddr, String> {
        let mut state = self.state.clone();
        let waited = tokio::time::timeout(
            Duration::from_secs(60),
            state.wait_for(|s| !matches!(s, Start::Starting)),
        )
        .await;
        let state = match waited {
            Ok(Ok(state)) => state.clone(),
            Ok(Err(_)) => return Err("the preview ended".into()),
            Err(_) => return Err("the preview did not start within a minute".into()),
        };
        match state {
            Start::Ready(addr) => Ok(addr),
            Start::Ended(why) => Err(why),
            Start::Starting => Err("the preview did not start".into()),
        }
    }
}

/// A session on a preview, while it lasts.
struct PageGuard(Arc<Preview>);

impl PageGuard {
    fn new(preview: Arc<Preview>) -> Self {
        preview.pages.fetch_add(1, Ordering::SeqCst);
        *preview.last_used.lock() = Instant::now();
        Self(preview)
    }
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        *self.0.last_used.lock() = Instant::now();
        self.0.pages.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A page's session: its own connection to the preview, kept while the page is
/// away. What the preview sends is numbered and waits here until the page has
/// confirmed it; a page coming back says how many frames it has and gets the
/// rest, so its document goes on from where it was.
struct Session {
    preview: Arc<Preview>,
    _guard: PageGuard,
    to_child: tokio::sync::Mutex<SplitSink<ChildSocket, Message>>,
    state: Mutex<SessionState>,
    changed: Notify,
    reader: Mutex<Option<tokio::task::AbortHandle>>,
}

#[derive(Default)]
struct SessionState {
    /// The frames the page has not confirmed, by number.
    waiting: VecDeque<(u64, Message)>,
    waiting_bytes: usize,
    /// The number of the last frame from the preview.
    last: u64,
    /// The number of the last frame the page confirmed.
    confirmed: u64,
    /// The page's connection that has the session now.
    holder: Option<u64>,
    away_since: Option<Instant>,
    ended: bool,
}

impl Session {
    fn push(&self, frame: Message) {
        {
            let mut state = self.state.lock();
            if state.ended {
                return;
            }
            state.last += 1;
            let n = state.last;
            state.waiting_bytes += frame.len();
            state.waiting.push_back((n, frame));
            if state.waiting_bytes > BEHIND_LIMIT {
                log::info!(
                    "hub: {}: a page away fell {} MB behind; it starts afresh when back",
                    self.preview.rel,
                    state.waiting_bytes >> 20
                );
                state.ended = true;
            }
        }
        self.changed.notify_waiters();
    }

    fn confirm(&self, n: u64) {
        let mut state = self.state.lock();
        if n > state.last || n <= state.confirmed {
            return;
        }
        state.confirmed = n;
        while state.waiting.front().is_some_and(|(m, _)| *m <= n) {
            if let Some((_, frame)) = state.waiting.pop_front() {
                state.waiting_bytes -= frame.len();
            }
        }
    }

    /// Gives the session to the page's connection `conn`, which has every frame
    /// up to `have`; a connection that had it lets go.
    fn take(&self, conn: u64, have: u64) -> bool {
        {
            let mut state = self.state.lock();
            if state.ended || have < state.confirmed || have > state.last {
                return false;
            }
            state.holder = Some(conn);
            state.away_since = None;
        }
        self.confirm(have);
        self.changed.notify_waiters();
        true
    }

    fn release(&self, conn: u64) {
        let mut state = self.state.lock();
        if state.holder == Some(conn) {
            state.holder = None;
            state.away_since = Some(Instant::now());
        }
    }

    /// The frames after `sent`, once there are any; `None` once `conn` no
    /// longer has the session.
    async fn next_frames(&self, conn: u64, sent: u64) -> Option<Vec<(u64, Message)>> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.state.lock();
                if state.ended || state.holder != Some(conn) {
                    return None;
                }
                let frames: Vec<_> = state
                    .waiting
                    .iter()
                    .filter(|(n, _)| *n > sent)
                    .cloned()
                    .collect();
                if !frames.is_empty() {
                    return Some(frames);
                }
            }
            notified.await;
        }
    }

    /// What a page coming back with `have` frames missed: frames and bytes.
    fn missed(&self, have: u64) -> (usize, usize) {
        let state = self.state.lock();
        state
            .waiting
            .iter()
            .filter(|(n, _)| *n > have)
            .fold((0, 0), |(count, bytes), (_, frame)| (count + 1, bytes + frame.len()))
    }

    async fn send(&self, message: Message) -> bool {
        self.to_child.lock().await.send(message).await.is_ok()
    }

    fn away_too_long(&self) -> bool {
        let state = self.state.lock();
        state.holder.is_none() && state.away_since.is_some_and(|t| t.elapsed() > AWAY)
    }

    fn is_over(&self) -> bool {
        self.state.lock().ended || self.away_too_long()
    }

    fn end(&self) {
        self.state.lock().ended = true;
        if let Some(reader) = self.reader.lock().take() {
            reader.abort();
        }
        self.changed.notify_waiters();
    }
}

/// What the preview sends a session, numbered, the large frames compressed.
async fn read_child(session: Arc<Session>, mut from_child: SplitStream<ChildSocket>) {
    while let Some(message) = from_child.next().await {
        let frame = match message {
            Ok(Message::Binary(data)) if data.len() >= COMPRESS_FROM => {
                let (size, started) = (data.len(), Instant::now());
                let Ok(packed) = tokio::task::spawn_blocking(move || pack(&data)).await else {
                    break;
                };
                if size >= 1 << 20 {
                    log::info!(
                        "hub: {}: {:.1} MB compressed to {:.1} MB ({} ms)",
                        session.preview.rel,
                        size as f64 / 1e6,
                        packed.len() as f64 / 1e6,
                        started.elapsed().as_millis()
                    );
                }
                Message::Binary(packed.into())
            }
            Ok(message @ (Message::Text(_) | Message::Binary(_))) => message,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        session.push(frame);
    }
    session.end();
}

/// `hub-hello <session> <frames it has>`: the first thing a page says.
fn parse_hello(text: &str) -> Option<(String, u64)> {
    let mut words = text.strip_prefix("hub-hello ")?.split(' ');
    let id = words.next()?;
    let have = words.next()?.parse().ok()?;
    let fits = (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric());
    fits.then(|| (id.to_owned(), have))
}

/// A line a page asks the hub to log, on one line and short.
fn clean(text: &str) -> String {
    text.chars()
        .take(400)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn ago(since: Option<Instant>) -> String {
    let Some(since) = since else {
        return "a moment".into();
    };
    let s = since.elapsed().as_secs();
    if s < 120 {
        format!("{s} s")
    } else {
        format!("{} min", s / 60)
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Folder,
    Document,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    dir: bool,
    mtime: u64,
}

#[derive(Serialize)]
struct FileEntry {
    path: String,
    mtime: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct FavoritesFile {
    favorites: Vec<String>,
}

#[derive(Deserialize)]
struct FavoriteRequest {
    path: String,
    on: bool,
}

impl Hub {
    // ------------------------------------------------------------- requests

    async fn handle(self: Arc<Self>, mut req: Request<Incoming>) -> Response<Full<Bytes>> {
        if let Err(why) = self.check(&req) {
            log::warn!("hub: refused {} {}: {why}", req.method(), req.uri().path());
            return text(StatusCode::FORBIDDEN, why);
        }
        let path = req.uri().path().to_owned();
        let method = req.method().clone();

        if let Some(rest) = path.strip_prefix("/v/") {
            let Some(rel) = decode(rest) else {
                return not_found();
            };
            let Some(file) = self.resolve(&rel, Kind::Document) else {
                return not_found();
            };
            let Some(rel) = self.rel(&file) else {
                return not_found();
            };
            if hyper_tungstenite::is_upgrade_request(&req) {
                return match hyper_tungstenite::upgrade(&mut req, None) {
                    Ok((response, page)) => {
                        tokio::spawn(self.clone().bridge(file, rel, page));
                        response
                    }
                    Err(err) => text(StatusCode::BAD_REQUEST, &err.to_string()),
                };
            }
            return match method {
                Method::GET => {
                    // started now, it compiles while the page loads
                    self.preview(&file, &rel);
                    html(self.preview_page(&rel))
                }
                Method::HEAD => html(String::new()),
                _ => text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
            };
        }

        match (method, path.as_str()) {
            (Method::GET | Method::HEAD, "/") => html(HOME_HTML.to_owned()),
            (Method::GET, "/manifest.webmanifest") => {
                respond(StatusCode::OK, "application/manifest+json", MANIFEST.into())
            }
            (Method::GET, "/icon.png") => {
                let mut res = respond(StatusCode::OK, "image/png", Bytes::from_static(ICON_PNG));
                res.headers_mut().insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("max-age=86400"),
                );
                res
            }
            (Method::GET, "/api/home") => {
                let hub = self.clone();
                match tokio::task::spawn_blocking(move || hub.home()).await {
                    Ok(body) => json(&body),
                    Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "cannot list"),
                }
            }
            (Method::GET, "/api/list") => {
                let dir = query(&req, "dir").unwrap_or_default();
                let hub = self.clone();
                let listed = tokio::task::spawn_blocking(move || hub.list(&dir)).await;
                match listed {
                    Ok(Some(body)) => json(&body),
                    _ => not_found(),
                }
            }
            (Method::POST, "/api/favorite") => self.set_favorite(req).await,
            _ => not_found(),
        }
    }

    /// Through Tailscale, as a permitted user, by the published name; a page
    /// of the hub's own origin for anything that is not a plain read.
    fn check(&self, req: &Request<Incoming>) -> Result<(), &'static str> {
        let headers = req.headers();
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .or_else(|| req.uri().authority().map(|a| a.as_str()))
            .unwrap_or_default();
        if !self.hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            return Err("not the published host name");
        }
        let user = headers
            .get("tailscale-user-login")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        if !self.users.iter().any(|u| u == user) {
            return Err("not a permitted Tailscale user");
        }
        let plain_read = matches!(*req.method(), Method::GET | Method::HEAD)
            && !hyper_tungstenite::is_upgrade_request(req);
        match headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
            Some(origin) if self.origins.iter().any(|o| o.eq_ignore_ascii_case(origin)) => Ok(()),
            Some(_) => Err("a foreign origin"),
            None if plain_read => Ok(()),
            None => Err("no origin"),
        }
    }

    // ---------------------------------------------------------------- paths

    /// `rel` (`/`-separated, under the documents folder) as a canonical path
    /// that stays inside it; no part may be hidden.
    fn resolve(&self, rel: &str, kind: Kind) -> Option<PathBuf> {
        let mut path = self.root.clone();
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            if part.starts_with('.') || part.contains('\0') || part.contains('\\') {
                return None;
            }
            path.push(part);
        }
        let path = fs::canonicalize(&path).ok()?;
        if !path.starts_with(&self.root) {
            return None;
        }
        let meta = fs::metadata(&path).ok()?;
        let fits = match kind {
            Kind::Folder => meta.is_dir(),
            Kind::Document => meta.is_file() && path.extension() == Some(OsStr::new("typ")),
        };
        fits.then_some(path)
    }

    fn rel(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        Some(rel.to_string_lossy().replace('\\', "/"))
    }

    // ---------------------------------------------------------------- lists

    fn home(&self) -> serde_json::Value {
        let favorites = self.favorites.lock().clone();
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        for favorite in &favorites {
            let Some(dir) = self.resolve(favorite, Kind::Folder) else {
                continue;
            };
            let walk = walkdir::WalkDir::new(&dir)
                .max_depth(8)
                .into_iter()
                .filter_entry(|e| e.depth() == 0 || !hidden(&e.file_name().to_string_lossy()));
            for entry in walk.flatten() {
                if !entry.file_type().is_file()
                    || entry.path().extension() != Some(OsStr::new("typ"))
                {
                    continue;
                }
                let Some(rel) = self.rel(entry.path()) else {
                    continue;
                };
                if seen.insert(rel.clone()) {
                    let mtime = entry.metadata().map(|m| mtime(&m)).unwrap_or(0);
                    files.push(FileEntry { path: rel, mtime });
                }
                if files.len() >= 5000 {
                    break;
                }
            }
        }
        files.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        let favorites: Vec<_> = favorites
            .into_iter()
            .map(|path| {
                let exists = self.resolve(&path, Kind::Folder).is_some();
                serde_json::json!({ "path": path, "exists": exists })
            })
            .collect();
        serde_json::json!({ "favorites": favorites, "files": files })
    }

    fn list(&self, dir: &str) -> Option<serde_json::Value> {
        let folder = self.resolve(dir, Kind::Folder)?;
        let rel = self.rel(&folder)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&folder).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if hidden(&name) {
                continue;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // a link counts only when it leads to somewhere inside
            let target = if file_type.is_symlink() {
                match fs::canonicalize(&path) {
                    Ok(target) if target.starts_with(&self.root) => target,
                    _ => continue,
                }
            } else {
                path
            };
            let Ok(meta) = fs::metadata(&target) else {
                continue;
            };
            let dir = meta.is_dir();
            if dir || name.ends_with(".typ") {
                entries.push(Entry {
                    name,
                    dir,
                    mtime: mtime(&meta),
                });
            }
        }
        entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let favorite = self.favorites.lock().contains(&rel);
        Some(serde_json::json!({ "dir": rel, "favorite": favorite, "entries": entries }))
    }

    async fn set_favorite(&self, req: Request<Incoming>) -> Response<Full<Bytes>> {
        let is_json = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|t| t.starts_with("application/json"));
        if !is_json {
            return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "JSON expected");
        }
        let Ok(body) = Limited::new(req.into_body(), 4096).collect().await else {
            return text(StatusCode::BAD_REQUEST, "unreadable body");
        };
        let Ok(ask) = serde_json::from_slice::<FavoriteRequest>(&body.to_bytes()) else {
            return text(StatusCode::BAD_REQUEST, "unreadable request");
        };
        let favorites = {
            let mut favorites = self.favorites.lock();
            if ask.on {
                let Some(rel) = self
                    .resolve(&ask.path, Kind::Folder)
                    .and_then(|p| self.rel(&p))
                else {
                    return not_found();
                };
                if !favorites.contains(&rel) {
                    favorites.push(rel);
                }
            } else {
                favorites.retain(|f| f != &ask.path);
            }
            favorites.clone()
        };
        if let Err(err) = save_favorites(&self.favorites_file, &favorites) {
            log::warn!("hub: cannot save the favourites: {err}");
            return text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot save the favourites",
            );
        }
        json(&serde_json::json!({ "favorites": favorites }))
    }

    // ------------------------------------------------------------- previews

    /// The page of `rel`'s preview: tinymist's own, connecting to its own
    /// address, with what the page needs to know about the hub.
    fn preview_page(&self, rel: &str) -> String {
        let title = rel.rsplit('/').next().unwrap_or(rel);
        let html = frontend_html(
            TYPST_PREVIEW_HTML,
            PreviewMode::Document,
            &address(rel),
            title,
        );
        let info = serde_json::json!({ "root": self.root_str, "path": rel })
            .to_string()
            .replace('<', "\\u003c");
        let head = format!(
            "<link rel=\"apple-touch-icon\" href=\"/icon.png\">\
             <meta name=\"apple-mobile-web-app-capable\" content=\"yes\">\
             <script>window.typstHub={info};</script></head>"
        );
        html.replacen("</head>", &head, 1)
    }

    /// The preview of `file`, started if it does not run.
    fn preview(self: &Arc<Self>, file: &Path, rel: &str) -> Arc<Preview> {
        let mut previews = self.previews.lock();
        if let Some(preview) = previews.get(file)
            && !matches!(*preview.state.borrow(), Start::Ended(_))
        {
            return preview.clone();
        }
        // room for it: an unused preview goes first, the oldest first
        while previews.len() >= self.max_previews {
            let victim = previews
                .values()
                .min_by_key(|p| (p.pages.load(Ordering::SeqCst) > 0, *p.last_used.lock()))
                .cloned();
            let Some(victim) = victim else { break };
            log::info!("hub: stops {} to make room", victim.rel);
            previews.remove(&victim.file);
            victim.stop();
        }
        let (state_tx, state) = watch::channel(Start::Starting);
        let (kill, kill_rx) = oneshot::channel();
        let preview = Arc::new(Preview {
            id: self.next_id.fetch_add(1, Ordering::SeqCst),
            file: file.to_path_buf(),
            rel: rel.to_owned(),
            state,
            pages: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
            kill: Mutex::new(Some(kill)),
        });
        previews.insert(file.to_path_buf(), preview.clone());
        tokio::spawn(self.clone().run_preview(preview.clone(), state_tx, kill_rx));
        preview
    }

    async fn run_preview(
        self: Arc<Self>,
        preview: Arc<Preview>,
        state: watch::Sender<Start>,
        kill: oneshot::Receiver<()>,
    ) {
        let state = Arc::new(state);
        let root = compile_root(&self.root, &preview.file);
        let mut command = tokio::process::Command::new(&self.exe);
        command
            .arg("preview")
            .args(["--no-open", "--navigate-in-page"])
            .args(["--data-plane-host", "127.0.0.1:0"])
            .args(["--control-plane-host", "127.0.0.1:0"])
            .args(["--invert-colors", &self.invert_colors])
            .arg("--root")
            .arg(&root)
            .arg(&preview.file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                log::warn!("hub: cannot start the preview of {}: {err}", preview.rel);
                state.send_replace(Start::Ended(format!("cannot start the preview: {err}")));
                self.forget(&preview);
                return;
            }
        };
        log::info!(
            "hub: preview of {} started (root {})",
            preview.rel,
            root.display()
        );

        // Its log: the addresses it listens on, then whatever it reports.
        if let Some(stderr) = child.stderr.take() {
            let state = state.clone();
            let rel = preview.rel.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(addr) = listening(&line, "Data plane server listening on: ") {
                        state.send_replace(Start::Ready(addr));
                    } else if let Some(addr) =
                        listening(&line, "Control panel server listening on: ")
                    {
                        tokio::spawn(drain_control_plane(addr));
                    } else if !line.contains("Static file server listening on")
                        && !line.contains("--static-file-host is deprecated")
                    {
                        log::info!("hub: [{rel}] {line}");
                    }
                }
            });
        }

        tokio::select! {
            status = child.wait() => {
                log::info!("hub: preview of {} ended: {status:?}", preview.rel);
            }
            _ = kill => {
                let _ = child.kill().await;
                log::info!("hub: preview of {} stopped", preview.rel);
            }
        }
        state.send_replace(Start::Ended("the preview ended".into()));
        self.forget(&preview);
    }

    fn forget(&self, preview: &Preview) {
        let mut previews = self.previews.lock();
        if previews
            .get(&preview.file)
            .is_some_and(|p| p.id == preview.id)
        {
            previews.remove(&preview.file);
        }
    }

    async fn reap_idle(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let over: Vec<_> = {
                let mut sessions = self.sessions.lock();
                let ids: Vec<_> = sessions
                    .iter()
                    .filter(|(_, s)| s.is_over())
                    .map(|(id, _)| id.clone())
                    .collect();
                ids.iter().filter_map(|id| sessions.remove(id)).collect()
            };
            for session in over {
                if session.away_too_long() {
                    log::info!(
                        "hub: {}: a page away for {:?} is given up",
                        session.preview.rel,
                        AWAY
                    );
                }
                session.end();
            }
            let idle: Vec<_> = self
                .previews
                .lock()
                .values()
                .filter(|p| {
                    p.pages.load(Ordering::SeqCst) == 0 && p.last_used.lock().elapsed() > self.idle
                })
                .cloned()
                .collect();
            for preview in idle {
                log::info!("hub: stops {}, unused for {:?}", preview.rel, self.idle);
                preview.stop();
            }
        }
    }

    async fn stop_all(&self) {
        let previews: Vec<_> = self.previews.lock().values().cloned().collect();
        for preview in &previews {
            preview.stop();
        }
        for _ in 0..40 {
            if self.previews.lock().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A new session for the page that chose `id`, on a connection of its own
    /// to the document's preview.
    async fn open_session(
        self: &Arc<Self>,
        file: &Path,
        rel: &str,
        id: &str,
    ) -> Result<Arc<Session>, String> {
        let preview = self.preview(file, rel);
        let addr = preview.address().await?;
        let child = match connect(addr).await {
            Ok(child) => child,
            Err(err) => {
                preview.stop();
                return Err(format!("cannot reach the preview: {err}"));
            }
        };
        let (to_child, from_child) = child.split();
        let session = Arc::new(Session {
            _guard: PageGuard::new(preview.clone()),
            preview,
            to_child: tokio::sync::Mutex::new(to_child),
            state: Mutex::default(),
            changed: Notify::new(),
            reader: Mutex::new(None),
        });
        let reader = tokio::spawn(read_child(session.clone(), from_child));
        *session.reader.lock() = Some(reader.abort_handle());
        if let Some(old) = self.sessions.lock().insert(id.to_owned(), session.clone()) {
            old.end();
        }
        Ok(session)
    }

    /// A page's connection. The page names its session and what it has; the
    /// hub goes on with that session, or starts a new one, and says which
    /// (`hub,resumed` or `hub,fresh`). Then frames go to the page, and the
    /// page's own messages to the preview, except those for the hub: `hub-ack
    /// <n>` (it has every frame up to n), `hub-ping` (is this connection
    /// alive?), `hub-log <text>` (a line for the hub's log).
    async fn bridge(self: Arc<Self>, file: PathBuf, rel: String, page: HyperWebsocket) {
        let page = match page.await {
            Ok(page) => page,
            Err(err) => {
                log::warn!("hub: page of {rel}: {err}");
                return;
            }
        };
        let (mut page_tx, mut page_rx) = page.split();
        let hello = match tokio::time::timeout(Duration::from_secs(30), page_rx.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => text.to_string(),
            _ => return,
        };
        let Some((id, have)) = parse_hello(&hello) else {
            // a page from before sessions: it reloads, and gets the new one
            log::info!("hub: {rel}: a page without a session; it reloads");
            return;
        };
        let conn = self.next_id.fetch_add(1, Ordering::SeqCst);
        let existing = self.sessions.lock().get(&id).cloned();
        let (session, mut sent, answer) = match existing {
            Some(session) if session.take(conn, have) => {
                if have > 0 {
                    let away = ago(session.state.lock().away_since);
                    let (count, bytes) = session.missed(have);
                    log::info!(
                        "hub: {rel}: a page is back after {away}; it missed {count} frames ({} kB)",
                        bytes / 1000
                    );
                }
                (session, have, "hub,resumed")
            }
            existing => {
                if have > 0 {
                    let why = if existing.is_some() {
                        "its session could not go on"
                    } else {
                        "its session had ended"
                    };
                    log::info!("hub: {rel}: a page is back; {why}, so it gets the document again");
                }
                match self.open_session(&file, &rel, &id).await {
                    Ok(session) if session.take(conn, 0) => (session, 0, "hub,fresh"),
                    Ok(_) => return,
                    Err(why) => {
                        log::warn!("hub: {rel}: {why}");
                        return;
                    }
                }
            }
        };
        if page_tx
            .send(Message::Binary(Bytes::from_static(answer.as_bytes())))
            .await
            .is_err()
        {
            session.release(conn);
            return;
        }

        let mut ping = tokio::time::interval(Duration::from_secs(25));
        ping.tick().await;
        let mut last_pong = Instant::now();
        'connection: loop {
            tokio::select! {
                frames = session.next_frames(conn, sent) => {
                    let Some(frames) = frames else { break };
                    for (n, frame) in frames {
                        if page_tx.send(frame).await.is_err() {
                            break 'connection;
                        }
                        sent = n;
                    }
                }
                message = page_rx.next() => match message {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(n) = text.strip_prefix("hub-ack ") {
                            if let Ok(n) = n.trim().parse() {
                                session.confirm(n);
                            }
                        } else if text.as_str() == "hub-ping" {
                            let pong = Message::Binary(Bytes::from_static(b"hub,pong"));
                            if page_tx.send(pong).await.is_err() {
                                break;
                            }
                        } else if let Some(line) = text.strip_prefix("hub-log ") {
                            log::info!("hub: {rel}: the page: {}", clean(line));
                        } else if !session.send(Message::Text(text)).await {
                            break;
                        }
                    }
                    Some(Ok(message @ Message::Binary(_))) => {
                        if !session.send(message).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => last_pong = Instant::now(),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
                _ = ping.tick() => {
                    if last_pong.elapsed() > PAGE_TIMEOUT {
                        log::info!("hub: {rel}: a page stopped answering");
                        break;
                    }
                    if page_tx.send(Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
            }
        }
        session.release(conn);
    }
}

/// The folder a document compiles from: the PyCharm project it is in (as the
/// editor's preview does), else its git repository, else its own folder; never
/// above the documents folder.
fn compile_root(documents: &Path, file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or(documents);
    let ancestors = || {
        parent
            .ancestors()
            .take_while(|dir| dir.starts_with(documents))
    };
    for marker in [".idea", ".git"] {
        if let Some(dir) = ancestors().find(|dir| dir.join(marker).exists()) {
            return dir.to_path_buf();
        }
    }
    parent.to_path_buf()
}

/// Left out of every list.
fn hidden(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with(".nosync")
        || matches!(name, "node_modules" | "target" | "__pycache__" | "pdf")
}

fn mtime(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn listening(line: &str, what: &str) -> Option<SocketAddr> {
    let at = line.find(what)?;
    line[at + what.len()..].trim().parse().ok()
}

async fn connect(
    addr: SocketAddr,
) -> anyhow::Result<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>> {
    let stream = tokio::net::TcpStream::connect(addr).await?;
    let mut request = format!("ws://{addr}/").into_client_request()?;
    request.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_str(&format!("http://{addr}"))?,
    );
    // A whole document is one frame of tens of megabytes, above tungstenite's
    // default limits.
    let config = WebSocketConfig::default()
        .max_message_size(None)
        .max_frame_size(None);
    let (socket, _) =
        tokio_tungstenite::client_async_with_config(request, stream, Some(config)).await?;
    Ok(socket)
}

/// A preview run without an editor still sends it what it would get (an
/// outline with every compile); this takes it, so that it does not pile up.
async fn drain_control_plane(addr: SocketAddr) {
    match connect(addr).await {
        Ok(mut socket) => while let Some(Ok(_)) = socket.next().await {},
        Err(err) => log::warn!("hub: control plane {addr}: {err}"),
    }
}

/// A large frame, compressed: `z:`, the frame's own name and a comma, then the
/// whole frame in raw deflate. The name tells the page what the frame is
/// before it unpacks it.
fn pack(data: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::DeflateEncoder};
    use std::io::Write;
    let name = data
        .iter()
        .take(32)
        .position(|&b| b == b',')
        .map_or(&[][..], |at| &data[..at]);
    let mut out = Vec::with_capacity(data.len() / 5 + 48);
    out.extend_from_slice(b"z:");
    out.extend_from_slice(name);
    out.push(b',');
    let mut encoder = DeflateEncoder::new(out, Compression::new(3));
    encoder.write_all(data).expect("deflate into memory");
    encoder.finish().expect("deflate into memory")
}

fn load_favorites(file: &Path) -> Vec<String> {
    fs::read(file)
        .ok()
        .and_then(|data| serde_json::from_slice::<FavoritesFile>(&data).ok())
        .unwrap_or_default()
        .favorites
}

fn save_favorites(file: &Path, favorites: &[String]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_vec_pretty(&FavoritesFile {
        favorites: favorites.to_vec(),
    })?;
    let partial = file.with_extension("json.partial");
    fs::write(&partial, data)?;
    fs::rename(&partial, file)
}

// ---------------------------------------------------------------- encoding

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// `/v/` and `rel`, each part percent-encoded.
fn address(rel: &str) -> String {
    let parts: Vec<String> = rel
        .split('/')
        .map(|p| utf8_percent_encode(p, SEGMENT).to_string())
        .collect();
    format!("/v/{}", parts.join("/"))
}

fn decode(s: &str) -> Option<String> {
    Some(percent_decode_str(s).decode_utf8().ok()?.into_owned())
}

fn query(req: &Request<Incoming>, key: &str) -> Option<String> {
    req.uri().query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| decode(&v.replace('+', " "))).flatten()
    })
}

// ---------------------------------------------------------------- responses

fn respond(status: StatusCode, content_type: &str, body: Bytes) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(body));
    *res.status_mut() = status;
    let headers = res.headers_mut();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

fn html(body: String) -> Response<Full<Bytes>> {
    respond(StatusCode::OK, "text/html; charset=utf-8", body.into())
}

fn json(body: &impl Serialize) -> Response<Full<Bytes>> {
    match serde_json::to_vec(body) {
        Ok(data) => respond(StatusCode::OK, "application/json", data.into()),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "cannot encode"),
    }
}

fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    respond(
        status,
        "text/plain; charset=utf-8",
        Bytes::copy_from_slice(body.as_bytes()),
    )
}

fn not_found() -> Response<Full<Bytes>> {
    text(StatusCode::NOT_FOUND, "not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_the_project() {
        // next to the crate: no system temp folder on this machine
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR")).join("hub-test.nosync");
        let docs = tmp.join("docs");
        fs::create_dir_all(docs.join("p/.idea")).unwrap();
        fs::create_dir_all(docs.join("p/sub/.git")).unwrap();
        fs::create_dir_all(docs.join("q/r")).unwrap();
        assert_eq!(
            compile_root(&docs, &docs.join("p/sub/a.typ")),
            docs.join("p")
        );
        assert_eq!(
            compile_root(&docs, &docs.join("q/r/b.typ")),
            docs.join("q/r")
        );
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn addresses_round_trip() {
        let rel = "health context/a b#c.typ";
        let addr = address(rel);
        assert_eq!(addr, "/v/health%20context/a%20b%23c.typ");
        assert_eq!(decode(addr.strip_prefix("/v/").unwrap()).unwrap(), rel);
    }

    #[test]
    fn packed_frames_unpack() {
        use std::io::Read;
        let data = b"new,".repeat(50_000);
        let z = pack(&data);
        assert_eq!(&z[..6], b"z:new,");
        let mut out = Vec::new();
        flate2::read::DeflateDecoder::new(&z[6..])
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn hellos() {
        assert_eq!(
            parse_hello("hub-hello 0123abcdef 12"),
            Some(("0123abcdef".into(), 12))
        );
        assert_eq!(parse_hello("hub-hello short 0"), None);
        assert_eq!(parse_hello("hub-hello 0123abcd/ef 0"), None);
        assert_eq!(parse_hello("current"), None);
    }
}
