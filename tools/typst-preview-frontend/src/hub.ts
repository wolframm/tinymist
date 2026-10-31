import { Subscription, fromEvent } from "rxjs";
import type {
  TypstPreviewDocument as TypstDocument,
  TypstDomWindowElement,
} from "typst-dom/index.preview.mjs";
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

// The hub compresses large frames (a whole document: 42 MB of drawings become
// 6), as `z,` and the frame in raw deflate. Frames stay in order: each waits
// for the one before it.
const Z = 0x7a;
const COMMA = 0x2c;
export function hubInflater(next: (data: ArrayBuffer) => void): (data: ArrayBuffer) => void {
  if (!hub) {
    return next;
  }
  let chain = Promise.resolve();
  return (data) => {
    chain = chain
      .then(async () => {
        if (data instanceof ArrayBuffer && data.byteLength > 2) {
          const head = new Uint8Array(data, 0, 2);
          if (head[0] === Z && head[1] === COMMA) {
            const stream = new Blob([new Uint8Array(data, 2)])
              .stream()
              .pipeThrough(new DecompressionStream("deflate-raw"));
            data = await new Response(stream).arrayBuffer();
          }
        }
        next(data);
      })
      .catch((err) => console.error("hub: cannot inflate a frame", err));
  };
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

/// The page's own when its connection is gone: why it waits.
export function hubProbeFailed(res: Response | undefined) {
  hubStatus(
    res === undefined
      ? "Can't reach the Mac — asleep, offline, or Tailscale is off. Trying again…"
      : `The Mac answers, the preview hub does not (HTTP ${res.status}). Trying again…`,
  );
}

/// The home button, the jumps in the browser's history, and a page that the
/// browser brings back from its cache reconnects.
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
    fromEvent<PageTransitionEvent>(window, "pageshow").subscribe((event) => {
      if (event.persisted) {
        location.reload();
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
  svgDoc.impl.svgFilter = emptyPages;
  document.getElementById("typst-container-main")?.addEventListener("scroll", scheduleWindow, {
    passive: true,
  });
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

/// After every render (typst.ts): the pages the view needs are filled, now
/// and once more after typst-dom has fitted the document to the view.
export function hubAfterRender(docRoot: Element) {
  if (windowed && docRoot.tagName.toLowerCase() === "svg") {
    afterRender();
    scheduleWindow();
  }
}
