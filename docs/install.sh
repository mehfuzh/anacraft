#!/usr/bin/env bash
set -euo pipefail

REPO="smartloop-ai/anacraft"
BINARY="anacraft"
GITHUB="https://github.com"

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux*)  os="unknown-linux-musl" ;;
        Darwin*) os="apple-darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="pc-windows-msvc" ;;
        *) echo "unsupported OS: $os" >&2; exit 1 ;;
    esac

    case "$arch" in
        x86_64|amd64)   arch="x86_64" ;;
        aarch64|arm64)   arch="aarch64" ;;
        armv7l|armhf)    arch="armv7" ;;
        *) echo "unsupported arch: $arch" >&2; exit 1 ;;
    esac

    # rust target convention
    echo "${arch}-${os}"
}

main() {
    local target
    target="$(detect_platform)"
    local ext=""
    if [[ "$target" == *"windows"* ]]; then
        ext=".exe"
    fi

    local tag="${VERSION:-latest}"
    local url
    if [ "$tag" = "latest" ]; then
        url="${GITHUB}/${REPO}/releases/latest/download/${BINARY}-${target}.tar.gz"
    else
        url="${GITHUB}/${REPO}/releases/download/${tag}/${BINARY}-${target}.tar.gz"
    fi

    local tmpdir
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    printf "⛏  downloading %s (%s)...\n" "$BINARY" "$target"
    curl -fsSL "$url" -o "$tmpdir/archive.tar.gz"

    printf "⛏  extracting...\n"
    tar -xzf "$tmpdir/archive.tar.gz" -C "$tmpdir"

    local dest="${INSTALL_DIR:-/usr/local/bin}"
    mkdir -p "$dest"
    install -m 755 "$tmpdir/${BINARY}${ext}" "$dest/${BINARY}${ext}"

    printf "\n  ✓ installed %s to %s/%s\n" "$BINARY" "$dest" "$BINARY"
    printf "\n  run %s to start the dashboard\n\n" "$BINARY"
}

main "$@"
