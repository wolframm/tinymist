# tinymist flavor

Arne's build of tinymist: the upstream release tag plus the fixes below, installed as
`~/.local/bin/tinymist`, which precedes Homebrew's copy on PATH. nvim's typst-preview
and the LSP both run it.

## Layout

- `flavor` — the branch. Base tag in `.flavor-base`; the fixes are the commits after it.
  Each fix is also a PR upstream where one makes sense; when upstream merges one, the
  rebase drops it by itself.
- `origin` = github.com/wolframm/tinymist (the fork), `upstream` = Myriad-Dreamin/tinymist.
- `.toolchain/` — rustup, cargo home, npm and yarn caches. Nothing of the build lives
  outside this folder except the installed binary. Excluded from git via `.git/info/exclude`.
- `rebuild.sh` — rebase onto the newest release, build the preview page, build the
  binary with that page bundled, install. `./rebuild.sh --build` skips the rebase.

## Fixes on the branch

| commit | what | upstream |
|---|---|---|
| fix(preview): let modifier shortcuts and arrow keys reach the browser | the preview page's key handler ignored modifiers, so Cmd+T toggled the theme and Cmd+↑ / arrows / Space were swallowed | PR #2727 |
| fix(preview): jump from a heading lands on the heading, not its outline entry | a span found on several pages jumped to the first — the Contents entry; for a cursor inside a heading the last occurrence is taken | to file |
| fix(preview): resolve a cursor that sits before its text | `jump_from_cursor` only matched the leaf BEFORE the cursor, so a cursor on a line's first character never synced the preview | to file |
| feat(preview): hand `file:` link clicks to the editor, leave other links to the browser | `jump_from_click` skipped links outright; the page now prevents the (impossible) navigation of a `file:` link and lets the click reach the editor, and stops other link clicks before the source handler | flavor |
| feat(preview): reload the page when its server comes back | the old reconnect targeted the data-plane port, which changes on restart; now the page waits for its own address and reloads, restoring the scroll position | flavor |
| build: bundle the locally built preview page | un-comments the `tinymist-assets = { path = ... }` patch in `Cargo.toml` | flavor only |

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
strings -a "$(which tinymist)" | grep -c 'e.metaKey || e.ctrlKey || e.altKey'   # 1 = flavor page
```

A running nvim keeps the tinymist it started; restart nvim, or `:TypstPreviewStop` and
`:TypstPreview`, to pick up a new binary (the LSP restarts with nvim).
