#!/usr/bin/env bash
#
# install.sh — one-line installer for neenee-code.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ming2k/neenee/main/install.sh | bash
#
# Or, to pin a version:
#   NEENEE_VERSION=0.9.1 curl -fsSL .../install.sh | bash
#
# Installs the `neenee-code` binary into ~/.local/bin (or $INSTALL_DIR if set).
# Detects OS + architecture and pulls the matching release tarball from GitHub.

set -euo pipefail

# --- config -------------------------------------------------------------

REPO="ming2k/neenee"
# Binary name as published inside the release tarball (matches `[[bin]]` in
# crates/neenee-code/Cargo.toml).
BIN_NAME="neenee-code"
# Where the binary lands. Honour an explicit override, otherwise ~/.local/bin
# (no sudo needed; create it if missing).
INSTALL_DIR="${INSTALL_DIR:-${HOME}/.local/bin}"
# Pin a version with NEENEE_VERSION="0.9.1". Empty means "latest release".
NEENEE_VERSION="${NEENEE_VERSION:-}"

# --- pretty printing ----------------------------------------------------

if [[ -n "${NO_COLOR:-}" ]] || [[ ! -t 1 ]]; then
    fmt_reset=""; fmt_bold=""; fmt_green=""; fmt_red=""; fmt_yellow=""; fmt_blue=""
else
    fmt_reset=$'\033[0m'; fmt_bold=$'\033[1m'
    fmt_green=$'\033[32m'; fmt_red=$'\033[31m'
    fmt_yellow=$'\033[33m'; fmt_blue=$'\033[34m'
fi

info()  { printf "${fmt_blue}›${fmt_reset} %s\n" "$*"; }
good()  { printf "${fmt_green}✓${fmt_reset} %s\n" "$*"; }
warn()  { printf "${fmt_yellow}!${fmt_reset} %s\n" "$*" >&2; }
abort() { printf "${fmt_red}✗${fmt_reset} %s\n" "$*" >&2; exit 1; }

# --- prerequisites ------------------------------------------------------

need() { command -v "$1" >/dev/null 2>&1 || abort "Required command not found: $1"; }
need uname
need tar

# Pick an HTTP fetcher. Prefer curl (matches the documented pipe-to-bash flow).
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }        # to stdout
    fetch_to() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
    fetch_to() { wget -qO "$2" "$1"; }
else
    abort "Neither curl nor wget is installed. Please install one and retry."
fi

# --- detect platform ----------------------------------------------------

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Darwin) target_os="apple-darwin" ;;
    Linux)  target_os="unknown-linux-gnu" ;;
    *)      abort "Unsupported OS: $os (only macOS and Linux are packaged)." ;;
esac

case "$arch" in
    x86_64|amd64)    target_arch="x86_64" ;;
    aarch64|arm64)   target_arch="aarch64" ;;
    *)               abort "Unsupported architecture: $arch." ;;
esac

# Alpine (musl) gets the static build so the binary isn't pinned to a glibc.
if [[ "$target_os" == "unknown-linux-gnu" && "$target_arch" == "x86_64" ]] \
   && { [[ -f /etc/alpine-release ]] || ldd --version 2>&1 | grep -qi musl; }; then
    target_os="unknown-linux-musl"
fi

target="${target_arch}-${target_os}"
info "Detected ${fmt_bold}${target}${fmt_reset}"

# --- resolve version ----------------------------------------------------

if [[ -z "$NEENEE_VERSION" ]]; then
    info "Looking up the latest release…"
    NEENEE_VERSION="$(fetch "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep -m1 '"tag_name"' | sed -E 's/.*"v?([^"]+)".*/\1/')"
    [[ -n "$NEENEE_VERSION" ]] || abort "Could not determine the latest release."
fi
# Allow the user to pass either "0.9.1" or "v0.9.1".
version="${NEENEE_VERSION#v}"
info "Installing ${fmt_bold}neenee-code v${version}${fmt_reset}"

# --- download + extract -------------------------------------------------

tarball="neenee-${version}-${target}.tar.gz"
url="https://github.com/${REPO}/releases/download/v${version}/${tarball}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

info "Downloading $url"
fetch_to "$url" "${tmpdir}/${tarball}"

info "Extracting…"
tar -xzf "${tmpdir}/${tarball}" -C "$tmpdir"

# The tarball contains a top-level dir `neenee-<version>-<target>/`; locate
# the binary by name so we don't hard-code the exact directory.
src="$(find "$tmpdir" -type f -name "$BIN_NAME" -perm -u+x | head -n1)"
[[ -n "$src" ]] || abort "Binary '$BIN_NAME' not found inside the archive."

# --- install ------------------------------------------------------------

mkdir -p "$INSTALL_DIR"
dest="${INSTALL_DIR%/}/${BIN_NAME}"
install -m 0755 "$src" "$dest"

good "Installed ${fmt_bold}${dest}${fmt_reset}"

# --- PATH sanity check --------------------------------------------------

case ":${PATH:-}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        warn "$INSTALL_DIR is not on your PATH."
        printf "  Add this to your shell profile (~/.bashrc, ~/.zshrc, …):\n"
        printf '    export PATH="%s:$PATH"\n' "$INSTALL_DIR"
        ;;
esac

# Shell-completion hint: the binary can print its own completions, but that
# is left to the user. Finish with a friendly next-step.
printf "\n"
good "Done! Run ${fmt_bold}neenee-code${fmt_reset} to start."
printf "  First launch: press ${fmt_bold}Ctrl+M${fmt_reset} to pick a provider.\n"
