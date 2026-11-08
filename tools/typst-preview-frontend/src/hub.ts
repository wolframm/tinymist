import { Subscription, fromEvent } from "rxjs";
import type {
  TypstPreviewDocument as TypstDocument,
  TypstDomWindowElement,
} from "typst-dom/index.preview.mjs";
import type { ViewPoint } from "typst-dom/typst-doc.mjs";
import { readingPoint } from "./nav";

// A page served by `tinymist hub` (a phone or tablet, through Tailscale). The
// hub puts `window.typstHub` in the page: the documents folder and this
// document's path in it. There is no editor, so the page does by itself what
// the editor does for a page on the Mac: it opens the document a link leads
// to, and keeps the steps it jumps in the browser's history, where the back
// gesture finds them.

export interface HubInfo {
  /// The documents folder, an absolute path.
  root: string;
  /// This document, relative to `root`.
  path: string;
}

export const hub: HubInfo | undefined = (window as any).typstHub;

/// The address of a document under the hub, `rel` relative to its folder.
export function hubAddress(rel: string, label?: string | null): string {
  const path = rel.split("/").map(encodeURIComponent).join("/");
  return `/v/${path}${label ? `#${encodeURIComponent(label)}` : ""}`;
}

const dec = new TextDecoder();
const Z = 0x7a;
const COLON = 0x3a;
const COMMA = 0x2c;

function secs(ms: number): string {
  return `${(ms / 1000).toFixed(1)} s`;
}

/// A line for the hub's log (`~/Library/Logs/typst-hub.log` on the Mac): what
/// the page did and why, which the Mac cannot see by itself.
const logQueue: string[] = [];
let hubLogger: (text: string) => void = (text) => {
  if (logQueue.length < 20) {
    logQueue.push(text);
  }
};
export function hubLog(text: string) {
  hubLogger(text);
}

// --------------------------------------------------------------- the connection

// A phone closes a page's connection whenever its screen locks or Safari
// leaves the screen. The page does not reload for it: it keeps its document,
// and once on screen again it connects again and names its session and the
// frames it has. The hub kept the session (hub.rs) and sends only what the
// page missed. Only a page whose session the hub no longer has (gone for two
// hours, or the hub restarted) gets the whole document again.
//
// While the page is read, changes to the document wait: a button shows that
// there are some, and a tap puts them in at the place being read. Opening the
// document shows its newest version.

export interface HubLinkEvents {
  /// A frame for the document, unpacked, in order.
  deliver(data: ArrayBuffer): void;
  /// Connected. `fresh`: the hub had no session to go on with, so the page
  /// asks for the whole document.
  opened(socket: WebSocket, fresh: boolean): void;
  closed(): void;
}

/// The frames held back while the page is read, still packed.
const held: ArrayBuffer[] = [];
/// Whether a document has gone to the page: from then on its changes wait.
let docShown = false;
// After every render the server says the version of the document it sent
// (`doc-version,`): the one on screen, and the one the held frames lead to.
// Held frames that lead to the version on screen change nothing and go in at
// once; the button is there only for a real change.
let versions = false;
let shownVersion = "";
let heldVersion = "";
/// Where the last document frame went, which the next version belongs to.
let lastDoc: "shown" | "held" | "" = "";
/// A new session's whole document (the page away for two hours, or the hub
/// restarted), held until its version says whether it differs from the one
/// on screen. A version is a hash over what the pages draw, the same in any
/// preview, so a changed document shows the button like any change. Either
/// way the whole document stays first in the queue and goes in afresh with
/// the next change shown: the session's changes fit only onto its own
/// document (it numbers the fonts its own way), and one that draws what is on
/// screen need not be drawn again before then.
let wholeHeld = false;
/// The held frames start with such a whole document.
let heldWhole = false;
/// After held changes go in: the place to put the view back at.
let pendingPlace: { point: ViewPoint; started: number; count: number } | undefined;
let showHeld: () => void = () => {};

/// The first whole document of a page's load, timed for the log.
const load = { connected: 0, received: 0, size: 0, unpacked: 0, drawn: 0, logged: false };

/// A frame's name, and whether the hub packed it (`z:<name>,` and the frame in
/// raw deflate).
function frameName(data: ArrayBuffer): { name: string; packed: boolean } {
  const head = new Uint8Array(data, 0, Math.min(40, data.byteLength));
  const packed = head[0] === Z && head[1] === COLON;
  const comma = head.indexOf(COMMA, packed ? 2 : 0);
  const name = comma < 0 ? "" : dec.decode(head.subarray(packed ? 2 : 0, comma));
  return { name, packed };
}

async function unpack(data: ArrayBuffer): Promise<ArrayBuffer> {
  const { packed } = frameName(data);
  if (!packed) {
    return data;
  }
  const comma = new Uint8Array(data, 0, Math.min(40, data.byteLength)).indexOf(COMMA, 2);
  const stream = new Blob([new Uint8Array(data, comma + 1)])
    .stream()
    .pipeThrough(new DecompressionStream("deflate-raw"));
  return await new Response(stream).arrayBuffer();
}

/// A whole document sent as a change (`diff-v1,`, by a preview that had just
/// compiled) as a whole one (`new,`): the payload is the same.
function asNew(data: ArrayBuffer): ArrayBuffer {
  const { name } = frameName(data);
  if (name !== "diff-v1") {
    return data;
  }
  const payload = new Uint8Array(data, name.length + 1);
  const out = new Uint8Array(4 + payload.length);
  out.set([0x6e, 0x65, 0x77, COMMA]);
  out.set(payload, 4);
  return out.buffer;
}

function randomId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/// Connects the page to the hub, and again whenever the connection is lost and
/// the page is on screen. Returns what closes it for good.
export function hubConnect(url: string, events: HubLinkEvents): () => void {
  // in memory only: a page loaded again is a new page, with nothing to go on
  const id = randomId();
  let have = 0;
  let socket: WebSocket | undefined;
  let ready = false;
  let disposed = false;
  let attempts = 0;
  let retryTimer = 0;
  let pongTimer = 0;
  let lost: { at: number; how: string } | undefined;
  let hiddenAt = document.visibilityState === "hidden" ? performance.now() : 0;
  let awayFor = 0;
  let chain = Promise.resolve();
  const logs = logQueue;

  hubLogger = (text) => {
    if (socket && ready && socket.readyState === WebSocket.OPEN) {
      socket.send(`hub-log ${text}`);
    } else if (logs.length < 20) {
      logs.push(text);
    }
  };

  function connect() {
    clearTimeout(retryTimer);
    if (disposed || socket) {
      return;
    }
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    socket = ws;
    ws.onopen = () => ws.send(`hub-hello ${id} ${have}`);
    ws.onmessage = (event) => receive(ws, event.data);
    ws.onclose = (event) => {
      drop(`closed (${event.code})`);
      retry();
    };
  }

  /// Lets a connection go; it says nothing more.
  function drop(how: string) {
    const ws = socket;
    if (!ws) {
      return;
    }
    socket = undefined;
    ready = false;
    ws.onopen = ws.onmessage = ws.onclose = null;
    try {
      ws.close();
    } catch (_) {
      // already closed
    }
    clearTimeout(pongTimer);
    lost ??= { at: performance.now(), how };
    events.closed();
  }

  function retry() {
    // off screen it waits: the page connects once it is shown again
    if (disposed || document.visibilityState !== "visible") {
      return;
    }
    attempts += 1;
    if (attempts >= 3) {
      probe();
    }
    const delay = attempts === 1 ? 250 : Math.min(1000 * 2 ** (attempts - 2), 10000);
    retryTimer = window.setTimeout(connect, delay);
  }

  function probe() {
    fetch(location.href, { cache: "no-store", method: "HEAD" })
      .then((res) => (res.ok ? hubStatus("Connecting to the Mac again…") : hubProbeFailed(res)))
      .catch(() => hubProbeFailed(undefined));
  }

  /// On screen again. A connection that looks open may be dead (the phone
  /// slept): one that does not answer at once is dropped for a new one.
  function onScreen() {
    if (disposed) {
      return;
    }
    if (hiddenAt) {
      awayFor = performance.now() - hiddenAt;
      hiddenAt = 0;
    }
    if (!socket) {
      attempts = 0;
      connect();
      return;
    }
    if (!ready) {
      return;
    }
    socket.send("hub-ping");
    clearTimeout(pongTimer);
    pongTimer = window.setTimeout(() => {
      drop("did not answer");
      attempts = 0;
      connect();
    }, 3000);
  }

  function receive(ws: WebSocket, data: ArrayBuffer | string) {
    if (typeof data !== "string" && frameName(data).name === "hub") {
      const what = dec.decode(new Uint8Array(data, 4));
      if (what === "pong") {
        clearTimeout(pongTimer);
        return;
      }
      const fresh = what === "fresh";
      ready = true;
      attempts = 0;
      hubStatus(null);
      if (fresh) {
        have = 0;
        if (docShown) {
          // what waited belongs to the session that ended; the document
          // comes again whole
          held.length = 0;
          heldVersion = "";
          heldWhole = false;
          lastDoc = "";
          updateButton();
          freshWithDoc = true;
        }
      }
      if (lost) {
        const away = awayFor ? `, off screen ${secs(awayFor)}` : "";
        hubLog(
          `connected again: the connection ${lost.how} ${secs(performance.now() - lost.at)} ago${away}; ${fresh ? "a new session" : "the session goes on"}`,
        );
        lost = undefined;
        awayFor = 0;
      } else if (!load.connected) {
        load.connected = performance.now();
      }
      for (const text of logs.splice(0)) {
        ws.send(`hub-log ${text}`);
      }
      events.opened(ws, fresh);
      return;
    }
    have += 1;
    ws.send(`hub-ack ${have}`);
    if (typeof data === "string") {
      chain = chain.then(() => events.deliver(data as any));
      return;
    }
    const { name } = frameName(data);
    if ((name === "new" || name === "diff-v1") && !load.received) {
      load.received = performance.now();
      load.size = data.byteLength;
    }
    chain = chain
      .then(() => take(data, name))
      .catch((err) => console.error("hub: cannot take a frame", err));
  }

  /// The whole document coming again on a page that shows one.
  let freshWithDoc = false;

  async function take(data: ArrayBuffer, name: string) {
    if (name === "doc-version") {
      versions = true;
      const version = dec.decode(new Uint8Array(data, "doc-version,".length)).trim();
      if (lastDoc === "held" && !held.some(isDocFrame)) {
        // what it belongs to went in with a tap in the meantime
        lastDoc = "shown";
      }
      if (lastDoc === "shown") {
        shownVersion = version;
      } else if (lastDoc === "held") {
        heldVersion = version;
        if (wholeHeld) {
          wholeHeld = false;
          heldWhole = true;
          hubLog(
            version === shownVersion
              ? "a new session, the same document: the one on screen stays"
              : "a new session, a changed document: it waits for the button",
          );
        } else if (version === shownVersion && !heldWhole) {
          // changes that add up to nothing: in at once, nothing to see
          await deliver(held.splice(0), version);
        }
        updateButton();
      }
      lastDoc = "";
      return;
    }
    const isDoc = name === "new" || name === "diff-v1";
    if (isDoc && freshWithDoc) {
      freshWithDoc = false;
      wholeHeld = true;
      hold(data);
      return;
    }
    // the outline goes with the document it belongs to, which may come after it
    if ((isDoc || name === "outline") && docShown) {
      hold(data);
      return;
    }
    const frame = await unpack(data);
    if (isDoc) {
      load.unpacked ||= performance.now();
      docShown = true;
      lastDoc = "shown";
    }
    events.deliver(frame);
  }

  function hold(data: ArrayBuffer) {
    held.push(data);
    if (isDocFrame(data)) {
      lastDoc = "held";
      heldVersion = "";
    }
    updateButton();
  }

  /// `whole`: the first document frame is a new session's whole document,
  /// which the page's renderer takes afresh (as `new`), not on top of what it
  /// has.
  async function deliver(frames: ArrayBuffer[], version: string, whole = false) {
    if (version) {
      shownVersion = version;
    }
    if (heldVersion === version) {
      heldVersion = "";
    }
    let afresh = whole;
    for (const frame of frames) {
      const data = await unpack(frame);
      if (afresh && isDocFrame(frame)) {
        afresh = false;
        events.deliver(asNew(data));
      } else {
        events.deliver(data);
      }
    }
  }

  showHeld = () => {
    if (held.length === 0) {
      return;
    }
    const frames = held.splice(0);
    const version = heldVersion;
    const whole = heldWhole;
    wholeHeld = false;
    heldWhole = false;
    updateButton();
    const point = svgDocRef?.impl.captureViewPoint();
    const started = performance.now();
    pendingPlace = point ? { point, started, count: frames.length } : undefined;
    if (!point) {
      hubLog(`showing ${frames.length} held frames; no place to keep`);
    }
    chain = chain.then(() => deliver(frames, version, whole));
  };

  const onVisibility = () => {
    if (document.visibilityState === "visible") {
      onScreen();
    } else {
      hiddenAt = performance.now();
      saveLastView();
    }
  };
  const onPageShow = (event: PageTransitionEvent) => event.persisted && onScreen();
  document.addEventListener("visibilitychange", onVisibility);
  window.addEventListener("pageshow", onPageShow);
  window.addEventListener("online", onScreen);

  const nav = performance.getEntriesByType("navigation")[0] as
    | PerformanceNavigationTiming
    | undefined;
  hubLog(`opened (${nav?.type ?? "unknown"}), ${window.innerWidth}×${window.innerHeight}`);
  connect();

  return () => {
    disposed = true;
    document.removeEventListener("visibilitychange", onVisibility);
    window.removeEventListener("pageshow", onPageShow);
    window.removeEventListener("online", onScreen);
    drop("closed by the page");
  };
}

function isDocFrame(data: ArrayBuffer): boolean {
  const { name } = frameName(data);
  return name === "new" || name === "diff-v1";
}

let updateElem: HTMLButtonElement | undefined;
/// The button that shows the held changes: there while some change the
/// document (with versions known, once the held ones' version has come and
/// differs from the one on screen).
function updateButton() {
  const change =
    held.some(isDocFrame) && (!versions || (heldVersion !== "" && heldVersion !== shownVersion));
  if (!change) {
    updateElem?.remove();
    updateElem = undefined;
    return;
  }
  if (updateElem) {
    return;
  }
  updateElem = document.createElement("button");
  updateElem.id = "typst-hub-update";
  updateElem.title = "The document has changed: show it";
  updateElem.innerHTML =
    '<svg viewBox="0 0 16 16"><path d="M13 8.5A5 5 0 1 1 11.3 4.2M13 2.5v3h-3" /></svg>';
  updateElem.addEventListener("click", () => showHeld());
  document.body.append(updateElem);
}

/// After held changes went in: the view back at the place it showed.
function placeBack() {
  const place = pendingPlace;
  if (!place || !svgDocRef) {
    return;
  }
  if (svgDocRef.impl.isRendering) {
    setTimeout(placeBack, 50);
    return;
  }
  const done = svgDocRef.impl.scrollToViewPoint(place.point);
  if (done || performance.now() - place.started > 5000) {
    pendingPlace = undefined;
    hubLog(`showed ${place.count} held frames in ${secs(performance.now() - place.started)}`);
  } else {
    setTimeout(placeBack, 100);
  }
}

// Every page's server hears every click on its document, so a page follows a
// link only if it was clicked itself, a moment ago.
let lastClick = 0;
if (hub) {
  window.addEventListener("click", () => (lastClick = Date.now()), { capture: true });
}

/// `navigate,` from the server: a link into another document was clicked.
export function hubNavigate(json: string) {
  if (!hub || Date.now() - lastClick > 5000) {
    return;
  }
  lastClick = 0;
  const { filepath, label } = JSON.parse(json) as { filepath: string; label?: string };
  const prefix = hub.root.endsWith("/") ? hub.root : `${hub.root}/`;
  if (!filepath.startsWith(prefix)) {
    hubStatus("That document is outside the documents folder.", 4000);
    return;
  }
  location.assign(hubAddress(filepath.slice(prefix.length), label));
}

let statusElem: HTMLElement | undefined;
let statusTimer = 0;
/// A line at the bottom of the page; `null` takes it away.
export function hubStatus(text: string | null, ms = 0) {
  if (!hub) {
    return;
  }
  clearTimeout(statusTimer);
  if (text === null) {
    statusElem?.remove();
    statusElem = undefined;
    return;
  }
  if (!statusElem) {
    statusElem = document.createElement("div");
    statusElem.id = "typst-hub-status";
    document.body.append(statusElem);
  }
  statusElem.textContent = text;
  if (ms > 0) {
    statusTimer = window.setTimeout(() => hubStatus(null), ms);
  }
}

/// Why the page cannot connect.
export function hubProbeFailed(res: Response | undefined) {
  hubStatus(
    res === undefined
      ? "Can't reach the Mac — asleep, offline, or Tailscale is off. Trying again…"
      : `The Mac answers, the preview hub does not (HTTP ${res.status}). Trying again…`,
  );
}

/// The home button, and the jumps in the browser's history.
export function installHubPage(windowElem: TypstDomWindowElement): Subscription[] {
  if (!hub) {
    return [];
  }
  document.body.classList.add("typst-hub");

  const home = document.createElement("a");
  home.id = "typst-hub-home";
  home.href = "/";
  home.title = "Documents";
  home.innerHTML =
    '<svg viewBox="0 0 16 16"><path d="M2.5 7.5 8 3l5.5 4.5M4 6.5V13h3v-3.5h2V13h3V6.5" /></svg>';
  document.body.append(home);

  // A jump (a link inside the document, an entry of the contents) is a step
  // in the history; back and forward put the reading point back.
  const points: (string | undefined)[] = [];
  let step: number = typeof history.state?.typstStep === "number" ? history.state.typstStep : 0;
  history.replaceState({ ...history.state, typstStep: step }, "");
  const jump = (window as any).handleTypstLocation;
  (window as any).handleTypstLocation = (elem: Element, page: number, x: number, y: number) => {
    points[step] = readingPoint(windowElem);
    step += 1;
    points.length = step;
    history.pushState({ typstStep: step }, "");
    return jump(elem, page, x, y);
  };

  return [
    fromEvent<PopStateEvent>(window, "popstate").subscribe((event) => {
      const to = event.state?.typstStep;
      if (typeof to !== "number") {
        return;
      }
      points[step] = readingPoint(windowElem);
      step = to;
      const point = points[step]?.split(" ").map(Number);
      const rootElem = document.getElementById("typst-app")?.firstElementChild;
      if (point && point.length === 3 && point.every(Number.isFinite) && rootElem) {
        windowElem.handleTypstLocation(rootElem, point[0], point[1], point[2]);
      }
    }),
  ];
}

/// A document with a few wide sheets (A3 drawings among A4 pages) fits its
/// widest page to the screen, which on a phone leaves the ordinary pages half
/// as wide. The first time a document opens on a narrow screen, it is zoomed
/// so that its commonest page fits instead, and the zoom is kept with the
/// place from then on (ws.ts).
export function hubFitCommonPage(svgDoc: TypstDocument) {
  if (!hub) {
    return;
  }
  const main = document.getElementById("typst-container-main");
  let tries = 0;
  const attempt = () => {
    const svg = document.querySelector("#typst-app > svg");
    const pages = svg?.querySelectorAll(":scope > g.typst-page");
    // a phone can take many seconds over its first render
    if (!main || !svg || !pages?.length || svgDoc.impl.isRendering) {
      if (++tries < 600) {
        setTimeout(attempt, 100);
      }
      return;
    }
    const docWidth = Number.parseFloat(svg.getAttribute("data-width") || "0");
    const counts = new Map<number, number>();
    for (const page of pages) {
      const width = Number.parseFloat(page.getAttribute("data-page-width") || "0");
      counts.set(width, (counts.get(width) || 0) + 1);
    }
    const [common] = [...counts.entries()].sort((a, b) => b[1] - a[1])[0];
    const ratio = docWidth / common;
    if (!(common > 0) || !(ratio > 1.2) || (main.clientWidth * common) / docWidth >= 600) {
      return;
    }
    // the middle of the view's top edge stays put: the top of the page, or
    // the heading a label landed on, stays in sight
    const view = main.getBoundingClientRect();
    svgDoc.impl.captureZoomAnchor(view.left + view.width / 2, view.top);
    svgDoc.impl.setScaleRatio(Math.min(ratio, 10));
    svgDoc.addViewportChange();
  };
  attempt();
}

// A phone cannot hold a long document. construction-details as one page of
// svg is 250,000 elements (164,000 glyphs alone), which took Safari to 1.6 GB
// and iOS killed it; the drawings, the text layers and the renderer's own
// memory were the lesser part. So a hub page keeps in the document only the
// pages within two screens of the view: the renderer still renders the whole
// document, every time, and the page keeps each page's svg as text (by its
// data-tid) and puts it in, or takes it out, as the view moves. Nothing is
// asked of the server. The paper of every page stays (typst-dom draws it
// outside the pages), so a page being filled is white, never missing.
//
// A render after the first sends only what changed. A page it marks
// `data-reuse-from` its own tid is unchanged and stays as it is; one marked
// with another page's tid is that page changed, and its svg is a patch of
// that page's elements: that page is filled before typst-dom patches it, and
// the result is kept as the changed page's text afterwards.
const PAGE = /<g\s[^>]*class="typst-page"[^>]*>/g;
const G = /<g(?:\s[^>]*?)?(\/)?>|<\/g>/g;
const pageText = new Map<string, string>();
/// pages a render changed: their text is taken from the document after it
const changed = new Set<string>();
let windowed: TypstDomWindowElement | undefined;
let svgDocRef: TypstDocument | undefined;

function livePages(): SVGGElement[] {
  const svg = document.querySelector("#typst-app > svg");
  return svg ? [...svg.querySelectorAll<SVGGElement>(":scope > g.typst-page")] : [];
}

function fill(page: SVGGElement): boolean {
  const text = pageText.get(page.getAttribute("data-tid") || "");
  if (!text) {
    return false;
  }
  const range = document.createRange();
  range.selectNodeContents(page);
  page.append(range.createContextualFragment(text));
  windowed?.initTypstSvg(page);
  return true;
}

/// Every render's svg, before typst-dom puts it in: a new page's content is
/// kept here and the page goes in empty; a changed page goes in whole, onto
/// its previous version, filled for it.
function emptyPages(svg: string): string {
  const parts: string[] = [];
  let copied = 0;
  let live: SVGGElement[] | undefined;
  PAGE.lastIndex = 0;
  for (let open = PAGE.exec(svg); open; open = PAGE.exec(svg)) {
    if (open[0].endsWith("/>")) {
      continue;
    }
    const start = open.index + open[0].length;
    let depth = 1;
    G.lastIndex = start;
    let tag = G.exec(svg);
    for (; tag; tag = G.exec(svg)) {
      if (tag[0] === "</g>") {
        depth -= 1;
      } else if (!tag[1]) {
        depth += 1;
      }
      if (depth === 0) {
        break;
      }
    }
    const tid = /data-tid="([^"]*)"/.exec(open[0])?.[1];
    if (!tag || tid === undefined) {
      // not what was expected: the page goes in whole
      continue;
    }
    PAGE.lastIndex = tag.index;
    const from = /data-reuse-from="([^"]*)"/.exec(open[0])?.[1];
    if (from === undefined) {
      pageText.set(tid, svg.slice(start, tag.index));
      parts.push(svg.slice(copied, start));
      copied = tag.index;
    } else if (from !== tid) {
      live ??= livePages();
      const previous = live.find((p) => p.getAttribute("data-tid") === from);
      if (previous && previous.childElementCount === 0) {
        fill(previous);
      }
      changed.add(tid);
    }
  }
  if (parts.length === 0) {
    return svg;
  }
  parts.push(svg.slice(copied));
  return parts.join("");
}

/// After a render: the changed pages' text is taken, what no page has any
/// more is dropped, and the window is set again.
function afterRender() {
  const live = livePages();
  const tids = new Set<string>();
  for (const page of live) {
    const tid = page.getAttribute("data-tid") || "";
    tids.add(tid);
    if (changed.has(tid) && page.childElementCount > 0) {
      pageText.set(tid, page.innerHTML);
    }
  }
  changed.clear();
  for (const tid of pageText.keys()) {
    if (!tids.has(tid)) {
      pageText.delete(tid);
    }
  }
  windowPages();
}

/// Fills the pages near the view and empties the others.
function windowPages() {
  const main = document.getElementById("typst-container-main");
  const svg = document.querySelector("#typst-app > svg") as SVGElement | null;
  const dataHeight = Number.parseFloat(svg?.getAttribute("data-height") || "0");
  if (!windowed || !main || !svg || !(dataHeight > 0)) {
    return;
  }
  const scale = svg.getBoundingClientRect().height / dataHeight;
  if (!(scale > 0)) {
    return;
  }
  const top = main.scrollTop / scale;
  const bottom = (main.scrollTop + main.clientHeight) / scale;
  const margin = 2 * (bottom - top);
  for (const page of livePages()) {
    const y = Number.parseFloat(page.getAttribute("data-y") || "NaN");
    const height = Number.parseFloat(page.getAttribute("data-page-height") || "NaN");
    const near = !(y + height < top - margin || y > bottom + margin);
    if (near && page.childElementCount === 0) {
      fill(page);
    } else if (!near && page.childElementCount > 0) {
      page.replaceChildren();
    }
  }
}

/// Keeps a hub page's document to the pages near the view.
export function hubWindowPages(svgDoc: TypstDocument, windowElem: TypstDomWindowElement) {
  if (!hub) {
    return;
  }
  windowed = windowElem;
  svgDocRef = svgDoc;
  svgDoc.impl.svgFilter = emptyPages;
  const main = document.getElementById("typst-container-main");
  main?.addEventListener("scroll", scheduleWindow, { passive: true });
  main?.addEventListener("scroll", () => scheduleSave(), { passive: true });
  window.addEventListener("resize", scheduleWindow);
}

let frame = 0;
function scheduleWindow() {
  if (frame === 0) {
    frame = requestAnimationFrame(() => {
      frame = 0;
      windowPages();
    });
  }
}

let renders = 0;
/// After every render (typst.ts): the pages the view needs are filled, now
/// and once more after typst-dom has fitted the document to the view.
export function hubAfterRender(docRoot: Element) {
  if (!windowed || docRoot.tagName.toLowerCase() !== "svg") {
    return;
  }
  afterRender();
  scheduleWindow();
  renders += 1;
  if (load.unpacked && !load.drawn) {
    load.drawn = performance.now();
    logLoad();
  }
  if (pendingPlace) {
    requestAnimationFrame(placeBack);
  }
  if (lastView && placeReady) {
    removeLastView();
  }
  scheduleSave();
}

// ------------------------------------------------------------- the last view

// A page loaded again, which iOS does after it killed one for memory, or a
// document opened again, shows at once the pages it showed last, as they were,
// until its document has come and rendered at the same place. The pages on
// screen are kept on the phone (IndexedDB) a moment after every scroll and
// render, and as the page goes off screen.

/// What a kept view is made of; one kept by another make of the page is left.
const VIEW_FORMAT = 1;

interface LastView {
  format: number;
  /// The width of the view it was taken at: another width, another layout.
  width: number;
  html: string;
  /// Where the document's svg sat in the view.
  x: number;
  y: number;
  invert: boolean;
  dark: boolean;
  /// What it shows: the pages and their place, to save only a change.
  sig: string;
}

const DB = "typst-hub";
const STORE = "last-views";
const KEPT = 8;
let lastView: HTMLElement | undefined;
/// The page has put the view back where it was (ws.ts): from then on, the view
/// is the page's own, and is kept.
let placeReady = false;
let saveTimer = 0;
let lastSaved = "";

function idb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function idbDo<T>(
  mode: IDBTransactionMode,
  work: (store: IDBObjectStore) => IDBRequest<T>,
): Promise<T> {
  const db = await idb();
  try {
    return await new Promise<T>((resolve, reject) => {
      const req = work(db.transaction(STORE, mode).objectStore(STORE));
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  } finally {
    db.close();
  }
}

/// The ids an element and its descendants refer to: `href="#…"`, `url(#…)`.
function refsOf(elem: Element, into: string[]) {
  for (const e of [elem, ...elem.querySelectorAll("*")]) {
    for (const attr of e.attributes) {
      const value = attr.value;
      if (value.startsWith("#")) {
        into.push(value.slice(1));
      } else if (!value.startsWith("data:") && value.includes("url(#")) {
        for (const m of value.matchAll(/url\(["']?#([^)"']+)["']?\)/g)) {
          into.push(m[1]);
        }
      }
    }
  }
}

let skipped = "";
/// Says once why the last view is not kept.
function skip(why: string): undefined {
  if (skipped !== why) {
    skipped = why;
    hubLog(`the last view not kept: ${why}`);
  }
  return undefined;
}

/// The pages on screen, with the definitions they use, as one svg.
function captureLastView(): LastView | undefined {
  const main = document.getElementById("typst-container-main");
  const app = document.getElementById("typst-app");
  const svg = app?.querySelector(":scope > svg");
  if (!main || !app || !svg) {
    return skip("no document");
  }
  const view = main.getBoundingClientRect();
  const margin = view.height / 4;
  const pages = livePages().filter((p) => {
    const r = p.getBoundingClientRect();
    return r.bottom > view.top - margin && r.top < view.bottom + margin;
  });
  if (pages.length === 0) {
    return skip("no page on screen");
  }
  if (pages.some((p) => p.childElementCount === 0)) {
    return skip("a page on screen not filled yet");
  }
  const at = svg.getBoundingClientRect();
  const sig = `${pages.map((p) => p.getAttribute("data-tid")).join(" ")} ${Math.round(at.left)} ${Math.round(at.top)} ${Math.round(at.width)}`;
  if (sig === lastSaved) {
    return undefined;
  }
  const wanted = new Set<Element>(pages);
  const clone = svg.cloneNode(false) as Element;
  const used: string[] = [];
  for (const child of svg.children) {
    if (child.matches("g.typst-page")) {
      if (wanted.has(child)) {
        const copy = child.cloneNode(true) as Element;
        for (const layer of copy.querySelectorAll("foreignObject")) {
          layer.remove();
        }
        refsOf(copy, used);
        clone.append(copy);
      }
    } else {
      clone.append(child.cloneNode(true));
    }
  }
  // What the pages use from outside the document's svg (the glyphs are in
  // typst-dom's own resources svg), and what that uses.
  const defs = document.createElementNS("http://www.w3.org/2000/svg", "defs");
  const seen = new Set<string>();
  while (used.length > 0) {
    const id = used.pop()!;
    if (seen.has(id)) {
      continue;
    }
    seen.add(id);
    const target = document.getElementById(id);
    if (target && !target.closest("#typst-app") && !target.closest("#typst-hub-last-view")) {
      const copy = target.cloneNode(true) as Element;
      refsOf(copy, used);
      defs.append(copy);
    }
  }
  clone.prepend(defs);
  return {
    format: VIEW_FORMAT,
    width: main.clientWidth,
    html: clone.outerHTML,
    x: at.left - view.left,
    y: at.top - view.top,
    invert: app.classList.contains("invert-colors"),
    dark: matchMedia("(prefers-color-scheme: dark)").matches,
    sig,
  };
}

function scheduleSave(ms = 1500) {
  clearTimeout(saveTimer);
  saveTimer = window.setTimeout(saveLastView, ms);
}

async function saveLastView() {
  clearTimeout(saveTimer);
  if (!hub) {
    return;
  }
  if (!placeReady || lastView) {
    skip(placeReady ? "the last view is still shown" : "the place is not back yet");
    return;
  }
  const view = captureLastView();
  if (!view) {
    return;
  }
  if (view.html.length > 40e6) {
    skip(`${(view.html.length / 1e6).toFixed(0)} MB of pages on screen`);
    return;
  }
  lastSaved = view.sig;
  const key = hub.path;
  try {
    await idbDo("readwrite", (store) => store.put(view, key));
    // the documents seen last keep theirs
    const order: string[] = JSON.parse(localStorage.getItem(`${DB} ${STORE}`) || "[]");
    const next = [key, ...order.filter((k) => k !== key)];
    for (const old of next.splice(KEPT)) {
      await idbDo("readwrite", (store) => store.delete(old));
    }
    localStorage.setItem(`${DB} ${STORE}`, JSON.stringify(next));
    if (!kept) {
      kept = true;
      hubLog(`kept the last view (${(view.html.length / 1e6).toFixed(1)} MB)`);
    }
  } catch (err) {
    lastSaved = "";
    if (!saveFailed) {
      saveFailed = true;
      hubLog(`cannot keep the last view: ${err}`);
    }
  }
}
let saveFailed = false;
let kept = false;

/// Shows the pages this document showed last, if they were taken at this
/// width, until the document itself is there.
let lastViewTried = false;
export async function hubShowLastView() {
  if (!hub || location.hash.length > 1 || lastViewTried) {
    return;
  }
  lastViewTried = true;
  let view: LastView | undefined;
  const asked = performance.now();
  try {
    // never long: a slow read must not hold the document up
    const late = new Promise<undefined>((resolve) => setTimeout(resolve, 400));
    view = await Promise.race([(storedView ??= readStoredView()), late]);
  } catch (err) {
    hubLog(`cannot read the last view: ${err}`);
    return;
  }
  const main = document.getElementById("typst-container-main");
  if (!view || view.format !== VIEW_FORMAT || !main) {
    return;
  }
  const late = placeReady || renders > 0;
  if (late || Math.abs(main.clientWidth - view.width) > 2) {
    const why = late ? `the document was there first (${secs(performance.now() - asked)} to read it)` : `taken at width ${view.width}, now ${main.clientWidth}`;
    hubLog(`the last view not shown: ${why}`);
    return;
  }
  const rect = main.getBoundingClientRect();
  const elem = document.createElement("div");
  elem.id = "typst-hub-last-view";
  // inverted then by the dark appearance, inverted now if it is dark now
  const dark = matchMedia("(prefers-color-scheme: dark)").matches;
  if (view.invert === view.dark ? dark : view.invert) {
    elem.classList.add("invert-colors");
  }
  elem.style.left = `${rect.left}px`;
  elem.style.top = `${rect.top}px`;
  elem.style.width = `${rect.width}px`;
  elem.style.height = `${rect.height}px`;
  for (let at: HTMLElement | null = main; at; at = at.parentElement) {
    const background = getComputedStyle(at).backgroundColor;
    if (background && background !== "transparent" && !background.endsWith(", 0)")) {
      elem.style.background = background;
      break;
    }
  }
  const inner = document.createElement("div");
  inner.style.position = "absolute";
  inner.style.left = `${view.x}px`;
  inner.style.top = `${view.y}px`;
  inner.innerHTML = view.html;
  elem.append(inner);
  document.body.append(elem);
  lastView = elem;
  lastSaved = view.sig;
  hubLog(`showed the last view at ${secs(performance.now())}`);
  // never in the way for long
  setTimeout(removeLastView, 120000);
  // painted before the renderer's loading holds the page
  await new Promise((resolve) => requestAnimationFrame(() => setTimeout(resolve, 0)));
}

/// The last view, read as soon as the page's script runs.
let storedView: Promise<LastView | undefined> | undefined;
function readStoredView(): Promise<LastView | undefined> {
  return idbDo("readonly", (store) => store.get(hub!.path));
}
if (hub && location.hash.length <= 1) {
  storedView = readStoredView();
  storedView.catch(() => {});
}

function removeLastView() {
  if (lastView) {
    lastView.remove();
    lastView = undefined;
  }
}

/// The page has put its view back where it was (ws.ts).
export function hubViewRestored() {
  if (!hub || placeReady) {
    return;
  }
  placeReady = true;
  if (renders > 0) {
    // the document at its place is painted by the next frame
    requestAnimationFrame(() => requestAnimationFrame(removeLastView));
  }
  logLoad();
}

function logLoad() {
  if (load.logged || !load.drawn || !placeReady) {
    return;
  }
  load.logged = true;
  const t = (ms: number) => (ms ? secs(ms) : "–");
  hubLog(
    `whole document, ${(load.size / 1e6).toFixed(1)} MB packed: connected ${t(load.connected)}, received ${t(load.received)}, unpacked ${t(load.unpacked)}, drawn ${t(load.drawn)}, at its place ${t(performance.now())}`,
  );
}
