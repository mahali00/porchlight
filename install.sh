#!/bin/sh
set -eu

repo="mahali00/porchlight"
dir="${PORCHLIGHT_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
  echo "✗ $1" >&2
  exit 1
}

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-musl" ;;
  *) fail "porchlight runs on macOS and Linux" ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) arch="aarch64" ;;
  x86_64 | amd64) arch="x86_64" ;;
  *) fail "porchlight has no build for $(uname -m)" ;;
esac

if [ "$os" = "apple-darwin" ] && [ "$arch" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = "1" ]; then
  arch="aarch64"
fi

file="porchlight-$arch-$os.tar.gz"
url="https://github.com/$repo/releases/latest/download/$file"
temp="$(mktemp -d)"
trap 'rm -rf "$temp"' EXIT

echo "· downloading $file"
curl -fsSL "$url" -o "$temp/$file" || fail "couldn't download $url"
curl -fsSL "$url.sha256" -o "$temp/$file.sha256" || fail "couldn't download the checksum"

expected="$(cut -d ' ' -f 1 "$temp/$file.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$temp/$file" | cut -d ' ' -f 1)"
else
  actual="$(shasum -a 256 "$temp/$file" | cut -d ' ' -f 1)"
fi
[ "$expected" = "$actual" ] || fail "the download doesn't match its checksum"

tar -xzf "$temp/$file" -C "$temp"
mkdir -p "$dir"
rm -f "$dir/porchlight"
mv "$temp/porchlight" "$dir/porchlight"
chmod +x "$dir/porchlight"

echo "✓ installed porchlight to $dir/porchlight"

case ":$PATH:" in
  *":$dir:"*) echo "  run porchlight to start" ;;
  *)
    echo "  $dir isn't on your PATH. Add this to your shell's startup file:"
    echo "    export PATH=\"$dir:\$PATH\""
    echo "  or run $dir/porchlight now"
    ;;
esac
