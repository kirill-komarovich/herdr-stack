#!/bin/sh
set -eu

cd "$(dirname "$0")/.."

repo=kirill-komarovich/herdr-stack
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' herdr-plugin.toml | head -n 1)
dest=target/release/herdr-stack

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  *) target= ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
  sum=sha256sum
else
  sum="shasum -a 256"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

asset=herdr-stack-$target.tar.gz
base=https://github.com/$repo/releases/download/v$version

# A checkout of an untagged commit has no release to fetch, so it builds instead.
if [ -n "$target" ] &&
  curl -fsSL -o "$tmp/$asset" "$base/$asset" &&
  curl -fsSL -o "$tmp/$asset.sha256" "$base/$asset.sha256"; then
  (cd "$tmp" && $sum -c "$asset.sha256" >/dev/null)
  tar -xzf "$tmp/$asset" -C "$tmp"
  mkdir -p "$(dirname "$dest")"
  mv "$tmp/herdr-stack" "$dest"
  chmod +x "$dest"
  echo "herdr-stack: installed v$version for $target"
  exit 0
fi

if command -v cargo >/dev/null 2>&1; then
  echo "herdr-stack: no v$version release for ${target:-$(uname -sm)}, building from source" >&2
  cargo build --release --locked --target-dir target
  exit 0
fi

echo "herdr-stack: no v$version release for ${target:-$(uname -sm)} and no cargo to build one" >&2
exit 1
