#!/bin/sh
# ScratchPost installer.
#
#   curl -sSL https://raw.githubusercontent.com/saksham-sin/scratchpost/main/install.sh | sh
#
# Downloads the latest release binary for this platform and installs it as `sp`.
# No sudo, no package manager, no build toolchain required.
#
# Environment overrides:
#   SP_VERSION      release tag to install (default: latest)
#   SP_INSTALL_DIR  destination directory (default: /usr/local/bin if writable,
#                   else ~/.local/bin)
#   SP_REPO         owner/name to install from (default: saksham-sin/scratchpost)
#   SP_BASE_URL     release host (default: https://github.com); point this at an
#                   internal mirror for air-gapped or rate-limited installs
#   SP_REQUIRE_CHECKSUM  set to 1 to refuse installing when the release publishes
#                   no SHA-256 checksum (default: warn and continue)

set -eu

REPO="${SP_REPO:-saksham-sin/scratchpost}"
BASE_URL="${SP_BASE_URL:-https://github.com}"
VERSION="${SP_VERSION:-latest}"
BIN_NAME="sp"

# ---------------------------------------------------------------- presentation

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$(printf '\033[1m')
    DIM=$(printf '\033[2m')
    GREEN=$(printf '\033[32m')
    RED=$(printf '\033[31m')
    RESET=$(printf '\033[0m')
else
    BOLD='' DIM='' GREEN='' RED='' RESET=''
fi

say() { printf '%s\n' "$*"; }
ok() { printf '  %s%s%s %s\n' "$GREEN" "ok" "$RESET" "$*"; }
fail() {
    printf '%sinstall failed:%s %s\n' "$RED" "$RESET" "$*" >&2
    exit 1
}

# ------------------------------------------------------------------- platform

detect_target() {
    os=$(uname -s 2>/dev/null || echo unknown)
    arch=$(uname -m 2>/dev/null || echo unknown)

    case "$os" in
        Darwin) os_part="apple-darwin" ;;
        Linux) os_part="unknown-linux-gnu" ;;
        *) fail "unsupported operating system: $os (macOS and Linux only)" ;;
    esac

    case "$arch" in
        x86_64 | amd64) arch_part="x86_64" ;;
        arm64 | aarch64) arch_part="aarch64" ;;
        *) fail "unsupported architecture: $arch (x86_64 and aarch64 only)" ;;
    esac

    printf '%s-%s' "$arch_part" "$os_part"
}

# ------------------------------------------------------------------ download

have() { command -v "$1" >/dev/null 2>&1; }

fetch() {
    # fetch <url> <destination>
    if have curl; then
        curl -fsSL --retry 3 --connect-timeout 10 -o "$2" "$1"
    elif have wget; then
        wget -q -O "$2" "$1"
    else
        fail "neither curl nor wget is available"
    fi
}

sha256_of() {
    # sha256_of <file> -> lowercase hex digest on stdout
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif have openssl; then
        openssl dgst -sha256 -r "$1" | cut -d' ' -f1
    else
        return 1
    fi
}

# ------------------------------------------------------------------- install

choose_install_dir() {
    if [ -n "${SP_INSTALL_DIR:-}" ]; then
        printf '%s' "$SP_INSTALL_DIR"
        return
    fi
    # Prefer a system-wide location only when we can actually write to it.
    if [ -w /usr/local/bin ] 2>/dev/null; then
        printf '/usr/local/bin'
        return
    fi
    printf '%s/.local/bin' "$HOME"
}

path_hint() {
    dir="$1"
    case ":${PATH:-}:" in
        *":$dir:"*) return 0 ;;
    esac
    printf '\n  %s%s is not on your PATH.%s Add it with:\n\n' "$DIM" "$dir" "$RESET"
    shell_name=$(basename "${SHELL:-sh}")
    case "$shell_name" in
        fish) printf '    fish_add_path %s\n' "$dir" ;;
        zsh) printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.zshrc\n' "$dir" ;;
        *) printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.profile\n' "$dir" ;;
    esac
}

main() {
    target=$(detect_target)
    say ""
    say "${BOLD}ScratchPost${RESET} ${DIM}installer${RESET}"
    say "  ${DIM}platform${RESET} $target"

    if [ "$VERSION" = "latest" ]; then
        url="$BASE_URL/$REPO/releases/latest/download/sp-$target"
    else
        url="$BASE_URL/$REPO/releases/download/$VERSION/sp-$target"
    fi
    sum_url="$url.sha256"

    tmp=$(mktemp "${TMPDIR:-/tmp}/sp-install.XXXXXX") || fail "cannot create a temporary file"
    sum_tmp=$(mktemp "${TMPDIR:-/tmp}/sp-install-sum.XXXXXX") || fail "cannot create a temporary file"
    # shellcheck disable=SC2064
    trap "rm -f '$tmp' '$sum_tmp'" EXIT INT TERM

    say "  ${DIM}download${RESET} $url"
    fetch "$url" "$tmp" || fail "could not download $url
  No release asset exists for $target yet. Build from source instead:
    git clone $BASE_URL/$REPO && cd scratchpost && cargo build --release"

    [ -s "$tmp" ] || fail "downloaded file is empty"

    # A truncated download is the most common failure; refuse to install one.
    # od pads each byte with spaces, so compare the whitespace-stripped hex.
    magic=$(head -c 4 "$tmp" | od -An -tx1 | tr -d ' \n')
    if [ "$magic" != "cffaedfe" ] && [ "$magic" != "7f454c46" ]; then
        fail "downloaded file is not a Mach-O or ELF executable (truncated download?)"
    fi

    # The magic bytes only prove the file is an executable, not that it is the
    # executable we published. Verify the SHA-256 the release ships alongside it.
    if ! fetch "$sum_url" "$sum_tmp" 2>/dev/null || [ ! -s "$sum_tmp" ]; then
        if [ "${SP_REQUIRE_CHECKSUM:-0}" = "1" ]; then
            fail "no checksum published at $sum_url and SP_REQUIRE_CHECKSUM=1"
        fi
        say "  ${DIM}checksum${RESET} not published for this release; skipping verification"
    else
        expected=$(cut -d' ' -f1 <"$sum_tmp" | tr 'A-F' 'a-f' | tr -d ' \n')
        case "$expected" in
            *[!0-9a-f]* | '')
                fail "malformed checksum at $sum_url"
                ;;
        esac
        if [ "${#expected}" -ne 64 ]; then
            fail "malformed checksum at $sum_url (expected 64 hex characters)"
        fi
        if ! actual=$(sha256_of "$tmp"); then
            if [ "${SP_REQUIRE_CHECKSUM:-0}" = "1" ]; then
                fail "no sha256sum, shasum or openssl available to verify the download"
            fi
            say "  ${DIM}checksum${RESET} no SHA-256 tool available; skipping verification"
        elif [ "$actual" != "$expected" ]; then
            fail "checksum mismatch for $url
  expected $expected
  actual   $actual
  The download is corrupt or has been tampered with. Nothing was installed."
        else
            ok "checksum verified ${expected}"
        fi
    fi

    dir=$(choose_install_dir)
    mkdir -p "$dir" || fail "cannot create $dir"

    dest="$dir/$BIN_NAME"
    # Install by renaming a fresh file over the destination rather than copying
    # into it. Overwriting in place mutates the inode, and macOS caches code
    # signing state per inode: the stale cache makes the kernel SIGKILL the new
    # binary on first exec. A rename swaps the inode and sidesteps the cache.
    staged="$dir/.$BIN_NAME.new.$$"
    if [ -w "$dir" ]; then
        cp "$tmp" "$staged" || fail "cannot write to $dir"
        chmod 755 "$staged" || fail "cannot make $staged executable"
        mv -f "$staged" "$dest" || fail "cannot install $dest"
    else
        say "  ${DIM}escalating${RESET} $dir is not writable"
        sudo mkdir -p "$dir" || fail "cannot create $dir"
        sudo cp "$tmp" "$staged" || fail "cannot write to $dir"
        sudo chmod 755 "$staged" || fail "cannot make $staged executable"
        sudo mv -f "$staged" "$dest" || fail "cannot install $dest"
    fi

    ok "installed $dest"

    if ! "$dest" --version >/dev/null 2>&1; then
        fail "$dest does not run on this machine"
    fi
    ok "verified $("$dest" --version 2>/dev/null | head -1)"

    path_hint "$dir"

    say ""
    say "${BOLD}Next${RESET}"
    say "  cd into your project's master folder and run:"
    say ""
    say "    ${GREEN}sp init${RESET}"
    say ""
    say "  Then paste ${BOLD}scratchpost-agent-prompt.md${RESET} into your agent's chat."
    say ""
}

main "$@"
