import { Subscription, fromEvent } from "rxjs";
import type { TypstDomWindowElement } from "typst-dom/index.preview.mjs";

// Back and forward through links: Cmd+[ and Cmd+] (Ctrl elsewhere), in place
// of the browser's history, which a preview page does not use. The history is
// the editor's: it spans documents, and one document's tab cannot raise
// another's. The page reports each step with where it was reading — a click on
// a link inside the document (`jump`), on a link into another document
// (`link`), Cmd+[ (`back`), Cmd+] (`forward`) — and the editor moves the pages.

/// The document point at the place a jump puts its target (see `scrollTo` in
/// typst-dom: 7 % from the left, 38.2 % from the top of the view), as
/// `page x y` in pt — what `panelScrollByPosition` puts back there, whatever
/// the zoom by then.
function readingPoint(windowElem: TypstDomWindowElement): string {
  const main = document.getElementById("typst-container-main");
  const svg = document.getElementById("typst-app")?.firstElementChild;
  const dataWidth = Number.parseFloat(svg?.getAttribute("data-width") || "0");
  if (!main || !svg || !(dataWidth > 0)) {
    return "";
  }
  const scale = svg.getBoundingClientRect().width / dataWidth;
  const x = (main.scrollLeft + windowElem.clientWidth * 0.07) / scale;
  const y = (main.scrollTop + windowElem.clientHeight * 0.382) / scale;
  let at: Element | undefined;
  let atY = -Infinity;
  for (const page of svg.querySelectorAll(":scope > g.typst-page")) {
    const pageY = Number.parseFloat(page.getAttribute("data-y") || "NaN");
    if (pageY <= y && pageY > atY) {
      at = page;
      atY = pageY;
    }
  }
  if (!at) {
    return "";
  }
  const pageNo = Number.parseInt(at.getAttribute("data-page-number")!) + 1;
  const pageX = Number.parseFloat(at.getAttribute("data-x") || "0");
  return `${pageNo} ${x - pageX} ${y - atY}`;
}

export function installLinkHistory(
  windowElem: TypstDomWindowElement,
  send: (message: string) => void,
): Subscription[] {
  const report = (kind: string) => send(`nav ${kind} ${readingPoint(windowElem)}`.trimEnd());

  // A link inside the document calls the global handleTypstLocation from its
  // onclick; the editor's scrolls call windowElem's directly and stay out.
  const scrollToLink = windowElem.handleTypstLocation;
  (window as any).handleTypstLocation = (elem: Element, page: number, x: number, y: number) => {
    report("jump");
    return scrollToLink.call(windowElem, elem, page, x, y);
  };

  const isMac = navigator.platform.toUpperCase().indexOf("MAC") !== -1;
  return [
    // A link into another document is a `file:` link (see typst-dom's click
    // handler, which hands it to the editor). Capture phase: the report goes
    // out before the click does.
    fromEvent<MouseEvent>(window, "click", { capture: true }).subscribe((event) => {
      const anchor = document
        .elementsFromPoint(event.clientX, event.clientY)
        .map((el) => el.closest("a"))
        .find((a) => a !== null);
      const href = anchor?.getAttribute("href") || anchor?.getAttribute("xlink:href") || "";
      if (href.startsWith("file:")) {
        report("link");
      }
    }),
    fromEvent<KeyboardEvent>(window, "keydown").subscribe((event) => {
      if (!(isMac ? event.metaKey : event.ctrlKey) || event.shiftKey || event.altKey) {
        return;
      }
      if (event.key === "[" || event.key === "]") {
        event.preventDefault();
        report(event.key === "[" ? "back" : "forward");
      }
    }),
  ];
}
