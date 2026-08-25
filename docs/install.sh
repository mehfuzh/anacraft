#!/usr/bin/env bash
set -euo pipefail

REPO="mehfuzh/anacraft"
# The command is `craft`. Archives keep the anacraft- prefix, matching the repo
# and every release before 0.5.0. `anacraft` is installed alongside as an alias
# so existing scripts and muscle memory keep working.
PACKAGE="anacraft"
BINARY="craft"
ALIAS="anacraft"
GITHUB="https://github.com"
API="https://api.github.com"

# Set by main() before the cleanup trap can fire. Declared up here because the
# trap runs outside the function, where a `local` would be out of scope and
# `set -u` would abort on the expansion.
tmpdir=""
cleanup() { [ -n "$tmpdir" ] && rm -rf "$tmpdir"; }
trap cleanup EXIT

die() { printf "\n  ⛏  %s\n\n" "$1" >&2; exit 1; }

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux*)  os="unknown-linux-musl" ;;
        Darwin*) os="apple-darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="pc-windows-msvc" ;;
        *) die "unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) die "unsupported architecture: $arch" ;;
    esac

    # rust target convention
    echo "${arch}-${os}"
}

# Release assets carry the tag in their name, so "latest" has to be resolved to
# a concrete tag before a download URL can be built.
resolve_tag() {
    local tag="${VERSION:-latest}"
    if [ "$tag" != "latest" ]; then
        echo "$tag"
        return
    fi
    tag="$(curl -fsSL "${API}/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name" *: *"\([^"]*\)".*/\1/p' \
        | head -n 1)"
    [ -n "$tag" ] || die "could not work out the latest release — set VERSION=v0.1.0 to pin one"
    echo "$tag"
}

# Somewhere the user can actually write, so a piped install never stops to ask
# for a password. INSTALL_DIR wins if it is set. Otherwise /usr/local/bin is
# used only when it is genuinely writable (Homebrew leaves it that way on Intel
# macOS); on a stock box it is root-owned, so we fall back to ~/.local/bin.
#
# Note that `mkdir -p` exits 0 on a directory that already exists but is not
# writable, so it cannot double as the permission test.
pick_dest() {
    local candidate
    if [ -n "${INSTALL_DIR:-}" ]; then
        mkdir -p "$INSTALL_DIR" 2>/dev/null \
            || die "cannot create $INSTALL_DIR"
        [ -w "$INSTALL_DIR" ] || die "cannot write to $INSTALL_DIR"
        echo "$INSTALL_DIR"
        return
    fi

    for candidate in /usr/local/bin "$HOME/.local/bin"; do
        [ -d "$candidate" ] || mkdir -p "$candidate" 2>/dev/null || continue
        if [ -w "$candidate" ]; then
            echo "$candidate"
            return
        fi
    done

    die "found nowhere writable to install to — set INSTALL_DIR to a directory you own"
}

main() {
    local target tag stem archive url dest
    target="$(detect_platform)"
    tag="$(resolve_tag)"

    # Windows ships a zip; everything else a tarball. Both unpack to a
    # directory named after the release, with the binary inside it.
    stem="${PACKAGE}-${tag}-${target}"
    if [[ "$target" == *windows* ]]; then
        archive="${stem}.zip"
    else
        archive="${stem}.tar.gz"
    fi
    url="${GITHUB}/${REPO}/releases/download/${tag}/${archive}"

    tmpdir="$(mktemp -d)"

    printf "⛏  downloading %s %s (%s)...\n" "$PACKAGE" "$tag" "$target"
    curl -fsSL "$url" -o "$tmpdir/$archive" \
        || die "no build for $target in $tag — see ${GITHUB}/${REPO}/releases"

    printf "⛏  extracting...\n"
    if [[ "$archive" == *.zip ]]; then
        command -v unzip >/dev/null 2>&1 || die "unzip is needed to install on Windows"
        unzip -q "$tmpdir/$archive" -d "$tmpdir"
    else
        tar -xzf "$tmpdir/$archive" -C "$tmpdir"
    fi

    local ext="" src
    [[ "$target" == *windows* ]] && ext=".exe"
    src="$tmpdir/$stem/${BINARY}${ext}"
    [ -f "$src" ] || die "the archive did not contain ${BINARY}${ext}"

    dest="$(pick_dest)"
    install -m 755 "$src" "$dest/${BINARY}${ext}" \
        || die "could not write to $dest — set INSTALL_DIR to a directory you own"

    # Keep the old name working. A symlink on unix; Windows has no reliable
    # symlink without elevation, so copy the exe there instead.
    if [ -n "$ext" ]; then
        cp -f "$dest/${BINARY}${ext}" "$dest/${ALIAS}${ext}" 2>/dev/null || true
    else
        ln -sf "${BINARY}" "$dest/${ALIAS}" 2>/dev/null || true
    fi

    printf "\n  ✓ installed %s %s to %s/%s\n" "$PACKAGE" "$tag" "$dest" "$BINARY"
    printf "    %s also works, as an alias\n" "$ALIAS"

    # ~/.local/bin is frequently absent from PATH, so say exactly what to add
    # and where, rather than leaving an installed binary nobody can run.
    case ":$PATH:" in
        *":$dest:"*)
            printf "\n  run %s to start the dashboard\n\n" "$BINARY"
            ;;
        *)
            printf "\n  %s is not on your PATH yet — add it:\n\n" "$dest"
            case "$(basename "${SHELL:-sh}")" in
                fish)
                    printf "    fish_add_path %s\n\n" "$dest"
                    ;;
                zsh)
                    printf "    echo 'export PATH=\"%s:\$PATH\"' >> ~/.zshrc && exec zsh\n\n" "$dest"
                    ;;
                bash)
                    printf "    echo 'export PATH=\"%s:\$PATH\"' >> ~/.bashrc && exec bash\n\n" "$dest"
                    ;;
                *)
                    printf "    export PATH=\"%s:\$PATH\"\n\n" "$dest"
                    ;;
            esac
            printf "  then run %s\n\n" "$BINARY"
            ;;
    esac
}

main "$@"
