#!/bin/zsh
# rebuild.sh — build and install the tinymist flavor: an upstream release tag plus
# the fixes on the `flavor` branch. Everything (Rust toolchain, cargo registry, npm
# and yarn caches, build output) stays inside this folder, in `.nosync` folders: those
# iCloud Drive leaves on this Mac, and the Documents mirror skips them too.
#
#   ./rebuild.sh            rebase `flavor` onto the newest upstream release, build, install
#   ./rebuild.sh v0.15.8    the same onto a named release tag
#   ./rebuild.sh --build    no rebase: build and install what `flavor` is now
#
# Installs to ~/.local/bin/tinymist, which precedes /opt/homebrew/bin on PATH, so
# PyCharm's Typst plugin picks it up. Homebrew's copy stays untouched.
set -euo pipefail

C=${0:A:h}
T=$C/.toolchain.nosync
export RUSTUP_HOME=$T/rustup CARGO_HOME=$T/cargo PATH=$T/cargo/bin:$PATH
export CARGO_TARGET_DIR=$C/target.nosync
export npm_config_cache=$T/npm-cache YARN_CACHE_FOLDER=$T/yarn-cache
cd "$C"

if [[ ! -x $T/cargo/bin/cargo ]]; then
  echo "no toolchain in $T — install with:" >&2
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o $T/rustup-init.sh" >&2
  echo "  RUSTUP_HOME=$T/rustup CARGO_HOME=$T/cargo sh $T/rustup-init.sh -y --no-modify-path --profile minimal" >&2
  exit 1
fi

[[ $(git branch --show-current) == flavor ]] || git checkout -q flavor
[[ -z $(git status --porcelain --untracked-files=no) ]] || { echo "flavor has uncommitted changes" >&2; exit 1; }

# ---------------------------------------------------------------- rebase
if [[ ${1:-} != --build ]]; then
  git fetch --quiet --tags upstream
  old=$(cat .flavor-base)
  new=${1:-$(git tag --list 'v*' | grep -v -- '-rc' | sort -V | tail -1)}
  if [[ $new != $old ]]; then
    echo "rebasing flavor: $old -> $new"
    # commits whose patch already sits upstream drop out of the rebase by themselves
    git rebase --quiet --onto "$new" "$old" flavor
    echo "$new" > .flavor-base
    git commit -q -m "flavor: base on $new" -- .flavor-base
  else
    echo "flavor already on $old"
  fi
  echo "flavor commits on top of $(cat .flavor-base):"
  git log --format='  %h %s' "$(cat .flavor-base)..flavor" | grep -v 'flavor: base on'
fi

# ---------------------------------------------------------------- frontend
if [[ ! -d node_modules || yarn.lock -nt node_modules/.yarn-integrity ]]; then
  npx -y yarn@1 install --frozen-lockfile --non-interactive
fi
npx -y yarn@1 build:preview
cp locales/tinymist-rt.toml crates/tinymist-assets/src/tinymist-rt.toml
grep -q 'e.metaKey || e.ctrlKey || e.altKey' crates/tinymist-assets/src/typst-preview.html \
  || { echo "built preview page lacks the modifier-key guard" >&2; exit 1; }

# ---------------------------------------------------------------- binary
# The flavor branch un-comments the `tinymist-assets = { path = ... }` patch in
# Cargo.toml, so this bundles the page built above rather than the crates.io one.
cargo build --release -p tinymist-cli
bin=$CARGO_TARGET_DIR/release/tinymist
# (grep -c, not -q: under pipefail an early exit of grep -q fails the pipeline)
[[ $(strings -a "$bin" | grep -c 'waitForServerThenReload') -gt 0 ]] \
  || { echo "binary lacks the flavor preview page" >&2; exit 1; }

# ---------------------------------------------------------------- install
mkdir -p ~/.local/bin
cp "$bin" ~/.local/bin/tinymist.new && mv ~/.local/bin/tinymist.new ~/.local/bin/tinymist
echo "installed: $(~/.local/bin/tinymist -V) (flavor on $(cat .flavor-base))"
echo "homebrew:  $(/opt/homebrew/bin/tinymist -V 2>/dev/null || echo none)"
echo "restart the language server (PyCharm's status-bar widget, or PyCharm) to run the new binary"
