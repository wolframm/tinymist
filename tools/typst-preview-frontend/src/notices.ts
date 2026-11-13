import { Subscription, fromEvent } from "rxjs";

// Notices from the editor, at the top right of the page: something happened to
// the document while it was being read — an answer to a comment went in, say.
// The editor sends its whole list (`notices,` and JSON) whenever it changes,
// and the server keeps it for a page that connects later, so a reload shows
// the same ones. A notice stays until it is clicked or closed with its ×; the
// page reports either (`notice open <id>`, `notice close <id>`) and what a
// click does is the editor's business. Above five, the older ones fold into
// one line that unfolds them.

interface Notice {
  id: string;
  icon?: string;
  tone?: string;
  title?: string;
  text?: string;
  note?: string;
}

export interface Notices {
  update(items: Notice[]): void;
  subscription: Subscription;
}

const SHOWN = 5;

export function installNotices(send: (message: string) => void): Notices {
  const box = document.createElement("div");
  box.id = "typst-notices";
  document.body.append(box);

  let items: Notice[] = [];
  let unfolded = false;
  // Gone here, and not yet gone from the editor's list.
  const dismissed = new Set<string>();

  function card(item: Notice): HTMLElement {
    const elem = document.createElement("div");
    elem.className = `typst-notice tone-${item.tone || "info"}`;
    elem.dataset.id = item.id;
    const icon = document.createElement("span");
    icon.className = "typst-notice-icon";
    icon.textContent = item.icon || "";
    const body = document.createElement("div");
    body.className = "typst-notice-body";
    if (item.note) {
      const note = document.createElement("div");
      note.className = "typst-notice-note";
      note.textContent = item.note;
      body.append(note);
    }
    const title = document.createElement("div");
    title.className = "typst-notice-title";
    title.textContent = item.title || "";
    body.append(title);
    if (item.text) {
      const text = document.createElement("div");
      text.className = "typst-notice-text";
      text.textContent = item.text;
      body.append(text);
    }
    const close = document.createElement("span");
    close.className = "typst-notice-close";
    close.textContent = "×";
    close.title = "Close";
    elem.append(icon, body, close);
    return elem;
  }

  function render() {
    const shown = items.filter((item) => !dismissed.has(item.id));
    // newest first; the editor's list is oldest first
    const ordered = shown.slice().reverse();
    const visible = unfolded ? ordered : ordered.slice(0, SHOWN);
    const children: HTMLElement[] = visible.map(card);
    if (ordered.length > visible.length) {
      const more = document.createElement("div");
      more.className = "typst-notice-more";
      more.textContent = `+${ordered.length - visible.length} more`;
      children.push(more);
    }
    if (ordered.length <= SHOWN) {
      unfolded = false;
    }
    box.replaceChildren(...children);
  }

  function update(next: Notice[]) {
    items = Array.isArray(next) ? next.filter((item) => item && typeof item.id === "string") : [];
    for (const id of [...dismissed]) {
      if (!items.some((item) => item.id === id)) {
        dismissed.delete(id);
      }
    }
    render();
  }

  // On the window, capture phase, and installed before the page's other click
  // handlers: a click on a notice is not a click on the document under it.
  const onClick = (event: MouseEvent) => {
    const target = event.target as Element | null;
    if (!target || !box.contains(target)) {
      return;
    }
    event.stopImmediatePropagation();
    event.preventDefault();
    if (target.closest(".typst-notice-more")) {
      unfolded = true;
      render();
      return;
    }
    const elem = target.closest<HTMLElement>(".typst-notice");
    const id = elem?.dataset.id;
    if (!id) {
      return;
    }
    dismissed.add(id);
    render();
    send(`notice ${target.closest(".typst-notice-close") ? "close" : "open"} ${id}`);
  };
  const swallow = (event: Event) => {
    if (box.contains(event.target as Node)) {
      event.stopImmediatePropagation();
    }
  };

  const subscription = new Subscription(() => box.remove());
  subscription.add(fromEvent<MouseEvent>(window, "click", { capture: true }).subscribe(onClick));
  for (const kind of ["mousedown", "mouseup", "dblclick"]) {
    subscription.add(fromEvent(window, kind, { capture: true }).subscribe(swallow));
  }
  return { update, subscription };
}
