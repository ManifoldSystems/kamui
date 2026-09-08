#!/bin/sh
set -eu

install_dir="${KAMUI_INSTALL_DIR:-$HOME/.local/bin}"
release_url="${KAMUI_RELEASE_URL:-https://is3.cloudhost.id/orvix/kamui-releases/latest}"
os=$(uname -s)
arch=$(uname -m)

printf '\n  KAMUI\n'
printf '  Repository-aware coding agent for the terminal\n\n'

case "$os-$arch" in
  Linux-x86_64|Linux-amd64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64|Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-x86_64|Darwin-amd64) target="x86_64-apple-darwin" ;;
  Darwin-arm64|Darwin-aarch64) target="aarch64-apple-darwin" ;;
  *) echo "Unsupported platform: $os $arch" >&2; exit 1 ;;
esac

archive="kamui-$target.tar.gz"
temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT INT TERM

printf '  Platform   %s %s\n' "$os" "$arch"
printf '  Target     %s\n' "$target"
printf '  Install    %s/kamui\n\n' "$install_dir"

download() {
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fL --progress-bar "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget --show-progress -q "$1" -O "$2"
  else
    echo "curl or wget is required." >&2
    exit 1
  fi
}

printf '  Downloading %s\n' "$archive"
download "$release_url/$archive" "$temp_dir/$archive"
printf '  Downloading checksum\n'
download "$release_url/$archive.sha256" "$temp_dir/$archive.sha256"

printf '  Verifying SHA-256... '
expected=$(awk '{print $1}' "$temp_dir/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$temp_dir/$archive" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$temp_dir/$archive" | awk '{print $1}')
fi

if [ "$expected" != "$actual" ]; then
  printf 'FAILED\n' >&2
  echo "Checksum verification failed." >&2
  exit 1
fi
printf 'OK\n'

printf '  Installing binary... '
mkdir -p "$install_dir"
tar -xzf "$temp_dir/$archive" -C "$temp_dir"
install -m 755 "$temp_dir/kamui" "$install_dir/kamui"
printf 'OK\n'

version=$($install_dir/kamui --version 2>/dev/null || true)
printf '\n  Installed %s\n' "${version:-Kamui}"
printf '  Location  %s/kamui\n' "$install_dir"
case ":$PATH:" in
  *":$install_dir:"*) printf '\n  Run: kamui\n\n' ;;
  *)
    printf '\n  %s is not currently in PATH.\n' "$install_dir"
    printf '  Add this line to your shell profile:\n\n'
    printf '    export PATH="%s:$PATH"\n\n' "$install_dir"
    ;;
esac
