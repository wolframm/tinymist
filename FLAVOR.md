# tinymist flavor

Arne's build of tinymist: the upstream release tag plus the fixes below, installed as
`~/.local/bin/tinymist`, which precedes Homebrew's copy (installed, unused) on an
interactive shell's PATH. The PyCharm Typst plugin
(`~/Documents/code/typst-pycharm`) runs it as the language server, which hosts every preview.

## Layout

- `flavor` — the branch. Base tag in `.flavor-base`; the fixes are the commits after it.
  Each fix is also a PR upstream where one makes sense; when upstream merges one, the
  rebase drops it by itself.
- `origin` = github.com/wolframm/tinymist (the fork), `upstream` = Myriad-Dreamin/tinymist.
- `.toolchain.nosync/` — rustup, cargo home, npm and yarn caches; `target.nosync/` — cargo's
  output. Nothing of the build lives outside this folder except the installed binary. The
  `.nosync` suffix keeps them out of iCloud Drive (Documents is synced) and the Documents mirror
  skips them; both are excluded from git via `.git/info/exclude`.
- `rebuild.sh` — rebase onto the newest release, build the preview page, build the
  binary with that page bundled, install. `./rebuild.sh --build` skips the rebase.

## Fixes on the branch

| commit | what | upstream |
|---|---|---|
| fix(preview): let modifier shortcuts and arrow keys reach the browser | the preview page's key handler ignored modifiers, so Cmd+T toggled the theme and Cmd+↑ / arrows / Space were swallowed | PR #2727 |
| fix(preview): jump from a heading lands on the heading, not its outline entry | a span found on several pages jumped to the first — the Contents entry; for a cursor inside a heading the last occurrence is taken | to file |
| fix(preview): resolve a cursor that sits before its text | `jump_from_cursor` only matched the leaf BEFORE the cursor, so a cursor on a line's first character never synced the preview | to file |
| feat(preview): hand `file:` link clicks to the editor, leave other links to the browser | `jump_from_click` skipped links outright; the page's click-to-source handler now returns for any non-`file:` link (browser or the link's own onclick act) and prevents the impossible navigation of a `file:` link before sending the click to the editor | flavor |
| feat(preview): reload the page when its server comes back | the old reconnect targeted the data-plane port, which changes on restart; now the page waits for its own address and reloads, restoring the scroll position | flavor |
| feat(preview): `--stable-static-port` | with a `:0` static host the server picks a per-document port (path hash → 23700–23799, next free on a clash), so the address survives a restart; the editor plugin's own port retry is not used. Applies to previews hosted by the language server too | flavor |
| feat(preview): cross-document links land in the editor and the target's preview | a click on a link into another document — a call whose first argument is a `.typ` path string and which may carry `label: <l>`, the house `#doclink` — is resolved on the syntax tree to the labelled heading of the target and reaches the editor flagged `cross_document`; `tinymist.resolveCrossDocumentLink [path, {line, character}]` gives the same resolution for a source position | flavor |
| feat(preview): a scroll requested before the page connected is kept | the server holds a `panelScrollTo` / `panelScrollByPosition` that no page received and replays it to the next page once that page has its first document; the page retries a scroll until the page it names has rendered; a hidden page (inactive tab, covered window) scrolls instantly, since Chrome drops smooth scrolls there | flavor |
| fix(lsp): `tinymist.doKillPreview []` kills all previews | the argument was read before the "no argument" check, so the documented kill-all always failed with "expect String at args[0]" | flavor |
| fix(preview): a click reaches the editor once; a port that cannot be bound is an error, not a panic | every connected page has a renderer and each resolved the same click, so with two pages a click arrived twice (and started two previews of a cross-document target on one port); the editor actor drops a jump repeated within 300 ms. `make_http_server` returns the bind error instead of panicking — in the language server a panic took every preview and the server down | flavor |
| feat(preview): a page lands on the label its address names; a closed page lets go of the broadcasts | a cross-document jump also carries the link's `label`; the page reads `…/#label` on connecting and on every fragment change, asks its own server (`jump-label`) for the element carrying the label, gets an ordinary jump and clears the fragment. The render and outline actors of a page end when the page does — before, one that outlived its page took a scroll meant for the next page and dropped it | flavor |
| fix(preview): zoom keeps its point in place | Cmd+= / Cmd+- kept the scroll position, so the document grew from its top-left corner; Ctrl+scroll and pinch scrolled before the document had grown, so the scroll was clamped to the old width and the horizontal part was lost. A zoom now records the document point under the pointer (keys: the middle of the view), as page-local y so the scale-dependent page gaps do not move it, and scrolls it back there once the new size is drawn. A portrait page in a document with landscape pages slid right by 300 px in two steps | to file |
| feat(preview): back and forward through links | Cmd+[ / Cmd+] (Ctrl elsewhere) in the page, instead of the browser's history. The page reports each step to the editor — a click on a link inside the document (`jump`), on a link into another document (`link`, sent before the click), the keys (`back`, `forward`) — with the document point where it was reading, as `nav <kind> <page> <x> <y>`; the server passes it on as `tinymist/preview/navigate` with the task id. The editor keeps the history across documents and moves pages with `panelScrollByPosition`, the point landing where it was | flavor |
| feat(preview): a reload keeps the zoom and the place | a reload — the browser's, or the page's own when its server comes back — reset the zoom to the window's width and put the page at its top (the server-restart reload kept a raw scroll offset, which lands elsewhere at any zoom but 100 %). The page saves its zoom and the document point at the top left of the view, held to its page as the zoom anchor holds it, in localStorage under the document's title (the origin is the document's port), as they change and when the page goes; its first render puts both back. A label in the address or a scroll from the editor wins over the place | flavor |
| fix(preview): a long contents page no longer freezes the page | bringing a table of contents into view froze the page for 5–15 s (Chrome's `PaintArtifactCompositor::Update`): every text item carries its own selection layer, and a contents page's leader dots are one item per dot. The layers of a fill — three or more items in a row with the same single sign — are hidden (10.8 s → 0.5 s). A scroll no longer re-renders the whole document 0.5 s later when partial rendering is off; Cmd+↑ / Cmd+↓, Home / End and any jump over more than two screens are instant | flavor |
| feat(preview): a contents panel | `o` or the button at the top left opens the outline at the left of the page, the document narrowing beside it: numbers and titles from the server (`outline,` on the data plane, with each heading's displayed number), a filter focused on `o` (words of a number or title, ↑ ↓, Enter), a click or Enter jumps as a link does (Cmd+[ comes back), the heading being read is marked; the right edge drags to another width (the document refits on release, a double click resets it); open or closed and the width are kept per document | flavor |
| feat(hub): the previews of a documents folder, for a phone | `tinymist hub` serves a home page (favourite folders, the documents changed last, a search over the favourites, a browser of the folder) and `/v/<path>`, the preview of one document: it starts a `tinymist preview` for it on first use, on loopback ports, passes the page's connection through and compresses large frames (a 42 MB document arrives as 7 MB). At most four previews run; one stops 15 min after its last page. A request must name the published host and carry a permitted `Tailscale-User-Login`. Previews run with `--navigate-in-page`: a click on a link into another document makes that page open the target at its label; other clicks go nowhere. On a narrow screen the contents panel lies over the document, jumps are browser history steps, and a document with a few wide sheets first fits its commonest page | flavor only |
| fix(hub): a phone keeps only the pages near the view | construction-details took Safari on iOS to 1.6 GB (250,000 svg elements, 164,000 of them glyphs) and iOS killed the page. A hub page now keeps in the document only the pages within two screens of the view and each page's svg as text, filled in and emptied as the view moves (typst-dom's new `svgFilter` hook); a changed page in an update is filled before typst-dom patches it. 1.6 GB → 0.57 GB (peak 0.9 GB) in the iOS simulator. Pages on the Mac are unchanged | flavor only |
| fix(preview): a page's whole document is its own; a page without an editor learns each render's version | `current` (a page connecting) made every page of the document take the whole document again: the render request is a broadcast to all pages' renderers, and now names the page that asked. With `--navigate-in-page` (the hub) every render is followed by `doc-version,<hash>`, a hash over what the pages draw without their source positions: an edit undone, or a changed comment, gives the same version, in any preview process | flavor only |
| feat(hub): a phone's page keeps its document | iOS closes a page's connection whenever the screen locks or Safari leaves the screen, and the page reloaded the whole document (52 MB, 15–30 s blank on the phone). The hub now keeps a session per page: its own connection to the preview, kept for two hours while the page is away, its frames numbered until the page confirms them (`hub-hello <id> <have>`, `hub-ack`, `hub-ping`); a page back on screen reconnects and gets only what it missed. A page holds the document's changes while it is read: a blue button under the home button shows them in place, at the same reading point; changes that draw nothing new go in at once. A page loaded again shows the pages it showed last (kept in IndexedDB) until its document is drawn. The page logs why it connected again, how long a whole document took, and what it did with held changes, to the hub's log (`hub-log`) | flavor only |
| fix(hub): the buttons stay in the corner of a zoomed page | Safari's pinch zooms the whole page, things fixed to the window with it: the buttons grew and left the screen. The contents, home and update buttons, the contents panel and the status line sit in one box (`#typst-hub-chrome`) laid over what the screen shows (the visual viewport), at the scale that undoes the zoom: at any zoom they keep their corner and their size, and the panel spans the screen's height | flavor only |
| feat(preview): notices from the editor | the editor sends its list of notices (`tinymist.scrollPreview [task, {event: "notices", items: [{id, icon, tone, title, text, note}]}]`, the whole list each time); every page of the preview shows them at its top right, newest on top, outside the colour inversion, until one is clicked or closed with its ×; above 5 the older ones fold into `+n more`. The server keeps the list for a page that connects later, so a reload shows the same ones. The page reports a click or a close (`notice open <id>`, `notice close <id>`) and the server passes it on as `tinymist/preview/notice {taskId, action, id}`; what a click does is the editor's business (the PyCharm plugin: watchdoc's answers) | flavor |
| feat(preview): a scroll to the text nearest a source position | `panelScrollTo` takes `nearest: true`: where nothing is rendered for the position (a comment, a call's arguments, a blank line) the page goes to the text nearest to it in the source, the one before it when two are as near (`jump_near_cursor`), instead of nowhere. Without the flag a scroll is as before. The PyCharm plugin sets it for a click on a notice: the place a comment was taken out of has no text of its own | flavor |
| build: bundle the locally built preview page | un-comments the `tinymist-assets = { path = ... }` patch in `Cargo.toml` | flavor only |

## The hub: previews on a phone

`tinymist hub` runs as the launch agent `com.user.typst-hub`
(`~/Library/LaunchAgents/com.user.typst-hub.plist`, `bash -c exec …`). macOS treats tinymist
as its own program for the Documents permission, whatever starts it, and an ad-hoc signed
build is a new program to it. `rebuild.sh` therefore signs every build with the self-signed
code-signing certificate "tinymist local" (login keychain) and the identifier `tinymist`, so
the one grant holds across builds. Without the certificate it warns, macOS asks again after
the install ("tinymist would like to access files in your Documents folder"), and the hub's
lists and previews wait until the answer. It listens on `127.0.0.1:23690`; `tailscale serve --bg --https=8443
http://127.0.0.1:23690` publishes it inside the tailnet only (never `funnel`). The plist names
the host (`<mac>.<tailnet>.ts.net:8443`) and the Tailscale login a request must carry.
Favourites: `~/Library/Application Support/tinymist-hub/favorites.json`; log:
`~/Library/Logs/typst-hub.log`. `rebuild.sh` restarts the agent after an install.
The phone reaches the hub only while the Mac is awake; on battery the Mac sleeps
after 30 min.

A blank page, missing documents or an old page after a fresh build: look at
`ps -ef | grep 'tinymist hub'` before reading any code. A second hub beside the
agent's (`launchctl print gui/$(id -u)/com.user.typst-hub` names its pid) holds
the port, and one started before the installed binary's date (`ps -p <pid> -o
lstart=` against `stat -f '%Sm' ~/.local/bin/tinymist`) runs the old build:
`kill <pid>` it, then `launchctl kickstart -k gui/$(id -u)/com.user.typst-hub`.

A page's connection to the hub is a session (`hub.rs`, `Session`). The page names a random id
of its own (in memory: a page loaded again is a new page) and the frames it has; the hub keeps
the session's connection to the preview while the page is away, numbers what the preview sends
and drops it once the page confirms it. A session ends two hours after its page went, when
256 MB pile up for it, or with its preview; a page back after that gets a new session and the
whole document, which waits like any change and goes in afresh with the next change shown
(the new session numbers its fonts its own way). A frame of 64 KiB or more goes
compressed, as `z:<name>,` and the frame in raw deflate. What the page does (`hub.ts`) is in the hub's log as `the page: …`.

## Rebuilding for a new upstream release

```
./rebuild.sh            # newest release tag
./rebuild.sh v0.16.0    # a named one
```

It prints which flavor commits survived the rebase. If a rebase conflicts, resolve it,
`git rebase --continue`, then `./rebuild.sh --build`.

## Checking a binary

```
tinymist -V
strings -a "$(which tinymist)" | grep -c waitForServerThenReload   # above 0: the flavor's page
pgrep -fl tinymist                           # the processes running now
ps -p <pid> -o lstart=                       # when one started
stat -f '%Sm' ~/.local/bin/tinymist          # when the binary was installed
```

`rebuild.sh` makes the `strings` check itself before it installs. One `tinymist lsp`
per open PyCharm project and one `tinymist hub` are what runs. A running language
server keeps the tinymist it started: one started before the binary's date runs the
old build, until it is restarted from PyCharm's status-bar widget or with PyCharm.

## Colour inversion

With `--invert-colors auto` the page inverts its document itself where the system
appearance is dark (`invert(0.933333) hue-rotate(180deg)` on `.typst-doc`,
`typst.css`); `<meta name="darkreader-lock" />` keeps Dark Reader off it. An image is
inverted with the page and shows its own colours under the pointer. The setting is
plain `auto`, never `{ rest = "auto", image = "never" }`, which keeps every image as
drawn: an embedded SVG drawing then stays white paper on a dark page. A preview takes
the setting when it starts: after a change, Stop Preview (`⌃⌥S`) and Preview (`⌃⌥P`).

## Measuring a page

- **A slow page, without Arne's Chrome**: headless Chrome over CDP with tracing
  (`--headless=new --remote-debugging-port`, a `--user-data-dir` of its own in a
  `.nosync` folder, `--use-mock-keychain --password-store=basic`), the main thread's
  trace events summed by name (`RasterTask`, `ImageDecodeTask`, layout). Long-task
  timings taken inside the page mislead.
- **Safari's memory on iOS, without a device**: `footprint -p <pid>` on the iOS
  Simulator's `com.apple.WebKit.WebContent` process (`phys_footprint`, `IOSurface`,
  `JS JIT`).
