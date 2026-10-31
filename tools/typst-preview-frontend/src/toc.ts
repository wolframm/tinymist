import { Subscription, fromEvent } from "rxjs";
import type { TypstDomWindowElement } from "typst-dom/index.preview.mjs";

// The document's contents in a panel at the left. `o` (or the button at the
// top left) opens it with its filter focused: type a few letters of a heading
// or its number, arrows to choose, Enter to go there; a click goes there too.
// The heading being read is marked as the document scrolls. The server sends
// the outline with every compile (`outline,` and its JSON). The panel's right
// edge drags to another width. Whether it is open, and its width, are kept per
// document, like the view (ws.ts). On a narrow screen (a phone) the panel lies
// over the document instead, and goes once a heading is chosen or the
// document is tapped.

interface OutlineItem {
  title: string;
  numbers?: string;
  position?: { page_no: number; x: number; y: number };
  children: OutlineItem[];
}

interface Entry {
  elem: HTMLElement;
  /// number and title, folded for the filter
  text: string;
  parent?: Entry;
  page: number;
  x: number;
  y: number;
}

export interface Toc {
  update(outline: { items: OutlineItem[] }): void;
  toggle(): void;
  dispose(): void;
}

export interface TocOptions {
  /// Go to a heading, as a link in the document does.
  jump(page: number, x: number, y: number): void;
  /// The panel took or gave back width: fit the document to it.
  refit(): void;
  storageKey: string;
}

const narrow = () => window.matchMedia("(max-width: 700px)").matches;

const fold = (s: string) =>
  s
    .normalize("NFD")
    .replace(/\p{M}/gu, "")
    .toLowerCase();

export function installToc(windowElem: TypstDomWindowElement, opts: TocOptions): Toc {
  const main = document.getElementById("typst-container-main")!;
  // Keyboard scrolling follows focus: after the panel, the document has it.
  main.tabIndex = -1;

  const panel = document.createElement("div");
  panel.id = "typst-toc";
  const filter = document.createElement("input");
  filter.id = "typst-toc-filter";
  filter.type = "text";
  filter.placeholder = "Go to section";
  filter.spellcheck = false;
  filter.autocomplete = "off";
  const list = document.createElement("div");
  list.id = "typst-toc-list";
  panel.append(filter, list);

  const button = document.createElement("div");
  button.id = "typst-toc-button";
  button.title = "Contents (o)";
  button.innerHTML =
    '<svg viewBox="0 0 16 16"><path d="M2 3.5h12M5 6.5h9M5 9.5h9M2 12.5h12" /></svg>';
  document.body.append(panel, button);

  let entries: Entry[] = [];
  let lastOutline = "";
  let current: Entry | undefined;
  let selected: Entry | undefined;

  function isOpen() {
    return windowElem.classList.contains("toc-open");
  }

  function setOpen(open: boolean, focus: boolean) {
    if (open !== isOpen()) {
      windowElem.classList.toggle("toc-open", open);
      document.body.classList.toggle("toc-open", open);
      try {
        localStorage.setItem(opts.storageKey, open ? "1" : "0");
      } catch (_) {
        // storage unavailable: the panel starts closed next time
      }
      if (!narrow()) {
        opts.refit();
      }
    }
    if (open) {
      markCurrent(true);
      if (focus) {
        filter.focus();
        filter.select();
      }
    } else {
      setFilter("");
      main.focus({ preventScroll: true });
    }
  }

  function update(outline: { items: OutlineItem[] }) {
    const json = JSON.stringify(outline.items);
    if (json === lastOutline) {
      return;
    }
    lastOutline = json;
    const scrollTop = list.scrollTop;
    entries = [];
    current = selected = undefined;
    list.replaceChildren();
    const add = (item: OutlineItem, depth: number, parent?: Entry) => {
      const elem = document.createElement("div");
      elem.className = `typst-toc-entry level-${Math.min(depth, 4)}`;
      elem.style.setProperty("--depth", `${depth - 1}`);
      const title = document.createElement("span");
      title.className = "typst-toc-title";
      title.textContent = item.title;
      if (item.numbers) {
        const num = document.createElement("span");
        num.className = "typst-toc-num";
        num.textContent = item.numbers;
        elem.append(num);
      } else {
        title.classList.add("unnumbered");
      }
      elem.append(title);
      const pos = item.position;
      const entry: Entry = {
        elem,
        text: fold(`${item.numbers || ""} ${item.title}`),
        parent,
        page: pos?.page_no || 0,
        x: pos?.x || 0,
        y: pos?.y || 0,
      };
      elem.addEventListener("click", () => go(entry));
      list.append(elem);
      entries.push(entry);
      for (const child of item.children) {
        add(child, depth + 1, entry);
      }
    };
    for (const item of outline.items) {
      add(item, 1);
    }
    list.scrollTop = scrollTop;
    applyFilter();
  }

  function go(entry: Entry) {
    if (entry.page < 1) {
      return;
    }
    opts.jump(entry.page, entry.x, entry.y);
    if (narrow()) {
      setOpen(false, false);
    } else {
      main.focus({ preventScroll: true });
    }
  }

  // ---------------------------------------------------------------- filter

  function visible() {
    return entries.filter((e) => !e.elem.classList.contains("filtered-out"));
  }

  function select(entry: Entry | undefined) {
    selected?.elem.classList.remove("selected");
    selected = entry;
    if (entry) {
      entry.elem.classList.add("selected");
      reveal(entry, false);
    }
  }

  function setFilter(value: string) {
    if (filter.value !== value) {
      filter.value = value;
      applyFilter();
    }
  }

  // Every word of the query in the number or the title; the headings above a
  // match stay, dimmed, to show where it is.
  function applyFilter() {
    const words = fold(filter.value).split(/\s+/).filter((w) => w.length > 0);
    list.classList.toggle("filtering", words.length > 0);
    const matches = new Set<Entry>();
    const context = new Set<Entry>();
    for (const e of entries) {
      if (words.every((w) => e.text.includes(w))) {
        matches.add(e);
        for (let p = e.parent; p && !context.has(p); p = p.parent) {
          context.add(p);
        }
      }
    }
    for (const e of entries) {
      e.elem.classList.toggle("filtered-out", !matches.has(e) && !context.has(e));
      e.elem.classList.toggle("context", words.length > 0 && !matches.has(e));
    }
    if (words.length === 0) {
      select(undefined);
      markCurrent(false);
    } else {
      select(entries.find((e) => matches.has(e)));
    }
  }

  function onFilterKey(event: KeyboardEvent) {
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp": {
        const shown = visible();
        const from = selected ?? current;
        let i = from ? shown.indexOf(from) : -1;
        i = event.key === "ArrowDown" ? Math.min(i + 1, shown.length - 1) : Math.max(i - 1, 0);
        select(shown[i]);
        break;
      }
      case "Enter":
        if (selected) {
          const target = selected;
          setFilter("");
          select(undefined);
          go(target);
        } else {
          main.focus({ preventScroll: true });
        }
        break;
      case "Escape":
        if (filter.value) {
          setFilter("");
        } else {
          main.focus({ preventScroll: true });
        }
        break;
      default:
        return;
    }
    event.preventDefault();
  }

  // ---------------------------------------------------------------- current

  /// Keep `entry` in sight in the list; `center` when the panel has just
  /// opened.
  function reveal(entry: Entry, center: boolean) {
    const top = entry.elem.offsetTop;
    const bottom = top + entry.elem.offsetHeight;
    if (center || top < list.scrollTop || bottom > list.scrollTop + list.clientHeight) {
      list.scrollTop = top - list.clientHeight / 3;
    }
  }

  // The heading being read: the last one above the line a jump puts its target
  // on (38.2 % down the view, see `scrollTo` in typst-dom).
  function markCurrent(center: boolean) {
    if (!isOpen() || entries.length === 0) {
      return;
    }
    const svg = document.getElementById("typst-app")?.firstElementChild;
    const dataHeight = Number.parseFloat(svg?.getAttribute("data-height") || "0");
    if (!svg || !(dataHeight > 0)) {
      return;
    }
    // vertical, as the jump measures (the width is rounded to whole pixels)
    const scale = svg.getBoundingClientRect().height / dataHeight;
    const line = (main.scrollTop + windowElem.clientHeight * 0.382 + 4) / scale;
    const pageY = new Map<number, number>();
    for (const page of svg.querySelectorAll(":scope > g.typst-page")) {
      const n = Number.parseInt(page.getAttribute("data-page-number") || "-1");
      pageY.set(n + 1, Number.parseFloat(page.getAttribute("data-y") || "NaN"));
    }
    let at: Entry | undefined;
    for (const e of entries) {
      const y = (pageY.get(e.page) ?? NaN) + e.y;
      if (y <= line) {
        at = e;
      } else if (y > line) {
        break;
      }
    }
    if (at !== current) {
      current?.elem.classList.remove("current");
      current = at;
      current?.elem.classList.add("current");
    }
    if (current && !list.classList.contains("filtering") && !list.matches(":hover")) {
      reveal(current, center);
    }
  }

  // ---------------------------------------------------------------- width

  // Only the panel follows the drag, over the document: the document fits
  // itself to the new width once, on release (a refit repaints the whole
  // view). A double click puts back the default width.
  const widthKey = `${opts.storageKey} width`;
  const grip = document.createElement("div");
  grip.id = "typst-toc-grip";
  panel.append(grip);

  const clampWidth = (px: number) => Math.round(Math.max(160, Math.min(px, window.innerWidth * 0.6)));

  function setWidth(px: number | undefined) {
    const root = document.documentElement.style;
    try {
      if (px === undefined) {
        root.removeProperty("--typst-toc-width");
        localStorage.removeItem(widthKey);
      } else {
        root.setProperty("--typst-toc-width", `${px}px`);
        localStorage.setItem(widthKey, `${px}`);
      }
    } catch (_) {
      // storage unavailable: the width lasts until the page goes
    }
    if (isOpen() && !narrow()) {
      opts.refit();
    }
  }

  function onGripDown(event: PointerEvent) {
    if (event.button !== 0) {
      return;
    }
    event.preventDefault();
    grip.setPointerCapture(event.pointerId);
    document.body.classList.add("toc-resizing");
    const startX = event.clientX;
    const startWidth = panel.getBoundingClientRect().width;
    let width = startWidth;
    const move = (e: PointerEvent) => {
      width = clampWidth(startWidth + e.clientX - startX);
      panel.style.width = `${width}px`;
    };
    const up = () => {
      grip.removeEventListener("pointermove", move);
      grip.removeEventListener("pointerup", up);
      grip.removeEventListener("pointercancel", up);
      document.body.classList.remove("toc-resizing");
      panel.style.removeProperty("width");
      if (width !== startWidth) {
        setWidth(width);
      }
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", up);
    grip.addEventListener("pointercancel", up);
  }

  let frame = 0;
  const subscriptions: Subscription[] = [
    fromEvent(main, "scroll").subscribe(() => {
      if (frame === 0 && isOpen()) {
        frame = requestAnimationFrame(() => {
          frame = 0;
          markCurrent(false);
        });
      }
    }),
    fromEvent<KeyboardEvent>(filter, "keydown").subscribe(onFilterKey),
    fromEvent(filter, "input").subscribe(applyFilter),
    fromEvent(button, "click").subscribe(() => setOpen(!isOpen(), false)),
    fromEvent<PointerEvent>(grip, "pointerdown").subscribe(onGripDown),
    fromEvent(grip, "dblclick").subscribe(() => setWidth(undefined)),
    fromEvent(main, "click").subscribe(() => {
      if (narrow() && isOpen()) {
        setOpen(false, false);
      }
    }),
  ];

  let open = false;
  try {
    open = localStorage.getItem(opts.storageKey) === "1";
    const width = Number.parseFloat(localStorage.getItem(widthKey) || "");
    if (width > 0) {
      document.documentElement.style.setProperty("--typst-toc-width", `${clampWidth(width)}px`);
    }
  } catch (_) {
    // storage unavailable
  }
  if (open && !narrow()) {
    setOpen(true, false);
  }

  return {
    update,
    // `o`: open with the filter focused, or close
    toggle: () => setOpen(!isOpen(), true),
    dispose() {
      subscriptions.forEach((s) => s.unsubscribe());
      panel.remove();
      button.remove();
    },
  };
}
