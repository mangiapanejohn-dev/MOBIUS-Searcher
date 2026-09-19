#!/bin/sh
# Install the prebuilt mobius-searcher binary (macOS / Linux).
#   curl -fsSL https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/scripts/install.sh | sh
# Options (environment): MOBIUS_VERSION=v0.1.0 (default: latest release),
#                        MOBIUS_INSTALL_DIR=~/.local/bin,
#                        MOBIUS_DOWNLOAD_BASE=<mirror URL holding the release assets>
set -eu

repo="mangiapanejohn-dev/MOBIUS-Searcher"
dir="${MOBIUS_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  *)
    echo "no prebuilt binary for $(uname -s) $(uname -m); build from source:" >&2
    echo "  cargo install --git https://github.com/$repo mobius-searcher --locked" >&2
    exit 1
    ;;
esac

if [ -n "${MOBIUS_DOWNLOAD_BASE:-}" ]; then
  base="$MOBIUS_DOWNLOAD_BASE"
elif [ -n "${MOBIUS_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/$MOBIUS_VERSION"
else
  base="https://github.com/$repo/releases/latest/download"
fi
asset="mobius-searcher-$target.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $asset"
curl -fsSL --retry 3 -o "$tmp/$asset" "$base/$asset"
curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

want="$(awk -v a="$asset" '$2 == a {print $1}' "$tmp/SHA256SUMS")"
if command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
else
  got="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  echo "checksum mismatch for $asset (expected ${want:-none}, got $got)" >&2
  exit 1
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/mobius-searcher-$target/mobius-searcher" "$dir/mobius-searcher"
echo "installed $dir/mobius-searcher"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "add it to your PATH:  export PATH=\"$dir:\$PATH\"" ;;
esac
echo "next:  mobius-searcher --doctor"
