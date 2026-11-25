#!/bin/sh
# ngiab-rs installer for Linux and macOS.
#
#   curl -LsSf https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.sh | sh
#   curl -LsSf https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.sh | sh -s -- --backends docker -y
#
# Installs the ngiab-rs binary into ~/.local/bin, then optionally sets up one or
# more execution backends (local, docker, podman). Run with --help for options.
# Prompts are read from /dev/tty; without a terminal it never waits for input.
#
# Everything is inside functions and `main` is called on the last line, so the
# whole script has been read before anything runs (safe with `curl | sh`).

set -u

REPO="${NGIAB_RS_REPO:-JoshCu/ngiab-rs}"
IMAGE_REPO="docker.io/joshcu/ngiab-rs"
MIN_GLIBC="2.34"
# hdf5-metno-src (statically built HDF5) needs CMake >= 3.26. Older distros
# (Ubuntu 22.04: 3.22, Debian 12: 3.25) get this Kitware build in a temp dir.
MIN_CMAKE="3.26"
KITWARE_CMAKE="3.31.10"
KITWARE_CMAKE_SHA256="3cb3dd247b6a1de2d0f4b20c6fd4326c9024e894cebc9dc8699758887e566ca7"
PATH_MARKER="# ngiab-rs"

say() { printf '%s\n' "$*" >&2; }
warn() { printf 'warning: %s\n' "$*" >&2; }
err() { printf 'error: %s\n' "$*" >&2; }
die() {
    err "$*"
    exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

# "1", "true", "yes", "y", "on" (any case) -> 1, anything else -> 0.
truthy() {
    case "$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')" in
    1 | true | yes | y | on) echo 1 ;;
    *) echo 0 ;;
    esac
}

usage() {
    cat >&2 <<'EOF'
ngiab-rs installer

USAGE:
    install.sh [OPTIONS]

OPTIONS (each also settable with the environment variable shown; a flag wins):
    --version X.Y.Z         Release to install (default: latest)    NGIAB_RS_VERSION
    --install-dir DIR       Where to put ngiab-rs (default: ~/.local/bin)
                                                                    NGIAB_RS_INSTALL_DIR
    --backends LIST         Backends to set up, comma separated: local, docker,
                            podman, all or none. Skips the menu.    NGIAB_RS_BACKENDS
    --skip-binary           Only set up backends, keep the installed binary
                                                                    NGIAB_RS_SKIP_BINARY=1
    --no-modify-path        Don't edit shell startup files          NGIAB_RS_NO_MODIFY_PATH=1
    -y, --yes               Accept every prompt, including sudo / package installs
                                                                    NGIAB_RS_YES=1
    -h, --help              Show this help

Other environment variables:
    NGIAB_RS_REPO           GitHub repository (default: JoshCu/ngiab-rs)
    NGIAB_RS_IMAGE          Container image to pull (default: docker.io/joshcu/ngiab-rs:<version>)
    NGIAB_RS_DOWNLOAD_URL   Fetch release assets (and TOOL_VERSIONS) from this base URL
                            instead of GitHub: for mirrors and testing.
EOF
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------

parse_args() {
    VERSION="${NGIAB_RS_VERSION:-}"
    INSTALL_DIR="${NGIAB_RS_INSTALL_DIR:-}"
    BACKENDS="${NGIAB_RS_BACKENDS:-}"
    SKIP_BINARY="$(truthy "${NGIAB_RS_SKIP_BINARY:-}")"
    NO_MODIFY_PATH="$(truthy "${NGIAB_RS_NO_MODIFY_PATH:-}")"
    YES="$(truthy "${NGIAB_RS_YES:-}")"
    DOWNLOAD_URL="${NGIAB_RS_DOWNLOAD_URL:-}"

    while [ $# -gt 0 ]; do
        case "$1" in
        --version | --install-dir | --backends)
            [ $# -ge 2 ] || {
                err "$1 needs a value"
                usage
                exit 2
            }
            set_opt "$1" "$2"
            shift 2
            continue
            ;;
        --version=* | --install-dir=* | --backends=*) set_opt "${1%%=*}" "${1#*=}" ;;
        --skip-binary) SKIP_BINARY=1 ;;
        --no-modify-path) NO_MODIFY_PATH=1 ;;
        -y | --yes) YES=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            err "unknown option: $1"
            usage
            exit 2
            ;;
        esac
        shift
    done

    VERSION="${VERSION#v}"
    if [ -z "$INSTALL_DIR" ]; then
        [ -n "${HOME:-}" ] || die "HOME is not set; pass --install-dir"
        INSTALL_DIR="$HOME/.local/bin"
    fi
    DOWNLOAD_URL="${DOWNLOAD_URL%/}"
}

set_opt() {
    case "$1" in
    --version) VERSION="$2" ;;
    --install-dir) INSTALL_DIR="$2" ;;
    --backends) BACKENDS="$2" ;;
    esac
}

# ---------------------------------------------------------------------------
# Terminal and prompts
# ---------------------------------------------------------------------------

detect_tty() {
    # Opening /dev/tty fails (ENXIO) when there is no controlling terminal, e.g.
    # under CI, `docker run` without -t, or ssh without -t.
    if (: </dev/tty) 2>/dev/null; then HAVE_TTY=1; else HAVE_TTY=0; fi
}

# ask "question" -> sets ANSWER. Empty answer on EOF or without a terminal.
ask() {
    ANSWER=""
    [ "$HAVE_TTY" = 1 ] || return 1
    printf '%s ' "$1" >&2
    if ! IFS= read -r ANSWER </dev/tty; then
        # EOF (Ctrl-D): treat as "no answer", which callers take as no.
        ANSWER=""
        printf '\n' >&2
        return 1
    fi
    return 0
}

# confirm "question" [default y|n]. -y accepts. Without a terminal the answer is no.
confirm() {
    if [ "$YES" = 1 ]; then
        say "$1 [yes: -y]"
        return 0
    fi
    if [ "$HAVE_TTY" != 1 ]; then
        say "$1 [no: no terminal to ask on; re-run with -y to accept]"
        return 1
    fi
    if [ "${2:-y}" = y ]; then hint="[Y/n]"; else hint="[y/N]"; fi
    ask "$1 $hint" || return 1
    case "$ANSWER" in
    [Yy] | [Yy][Ee][Ss]) return 0 ;;
    [Nn] | [Nn][Oo]) return 1 ;;
    "") [ "${2:-y}" = y ] ;;
    *) return 1 ;;
    esac
}

# ---------------------------------------------------------------------------
# Privileged commands: always echoed, and confirmed unless -y
# ---------------------------------------------------------------------------

is_root() { [ "$(id -u)" = 0 ]; }

# Prefix shown to the user for a privileged command.
priv_prefix() {
    if is_root; then
        echo ""
    elif have sudo; then
        echo "sudo "
    elif have doas; then
        echo "doas "
    else
        echo "sudo "
    fi
}

# Run one command as root (no confirmation; callers confirm first).
as_root() {
    if is_root; then
        "$@" </dev/null
    elif have sudo; then
        # Without a terminal sudo can't ask for a password: fail instead of waiting.
        if [ "$HAVE_TTY" = 1 ]; then sudo "$@" </dev/null; else sudo -n "$@" </dev/null; fi
    elif have doas; then
        if [ "$HAVE_TTY" = 1 ]; then doas "$@" </dev/null; else doas -n "$@" </dev/null; fi
    else
        err "need root for: $*  (neither sudo nor doas is installed)"
        return 1
    fi
}

# Show a list of commands (one per argument, already prefixed) and ask once.
confirm_cmds() {
    question="$1"
    shift
    say ""
    say "$question"
    for c in "$@"; do say "    $c"; done
    confirm "Run $( [ $# -gt 1 ] && echo "these commands" || echo "this command")?" y
}

# ---------------------------------------------------------------------------
# Downloads
# ---------------------------------------------------------------------------

# download URL DEST
download() {
    if have curl; then
        if [ -n "$DOWNLOAD_URL" ]; then
            curl -fsSL --retry 3 -o "$2" "$1" </dev/null
        else
            curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1" </dev/null
        fi
    elif have wget; then
        if [ -n "$DOWNLOAD_URL" ]; then
            wget -q -O "$2" "$1" </dev/null
        else
            wget --https-only -q -O "$2" "$1" </dev/null
        fi
    else
        err "need curl or wget to download files"
        return 1
    fi
}

sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | awk '{print $1}'
    elif have shasum; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif have openssl; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        return 1
    fi
}

# verify FILE NAME SUMS_FILE
verify_sha256() {
    expected="$(awk -v n="$2" '$2 == n || $2 == "*" n {print $1; exit}' "$3")"
    [ -n "$expected" ] || {
        err "$2 is not listed in SHA256SUMS"
        return 1
    }
    actual="$(sha256_of "$1")" || {
        err "need sha256sum, shasum or openssl to verify the download"
        return 1
    }
    if [ "$expected" != "$actual" ]; then
        err "checksum mismatch for $2"
        err "  expected $expected"
        err "  got      $actual"
        return 1
    fi
}

release_base() {
    if [ -n "$DOWNLOAD_URL" ]; then
        echo "$DOWNLOAD_URL"
    elif [ -n "$VERSION" ]; then
        echo "https://github.com/$REPO/releases/download/v$VERSION"
    else
        echo "https://github.com/$REPO/releases/latest/download"
    fi
}

# ---------------------------------------------------------------------------
# Platform
# ---------------------------------------------------------------------------

# version_ge A B: A >= B for dotted numeric versions.
version_ge() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        na = split(a, x, "."); nb = split(b, y, ".");
        n = na > nb ? na : nb;
        for (i = 1; i <= n; i++) {
            xi = (i <= na) ? x[i] + 0 : 0; yi = (i <= nb) ? y[i] + 0 : 0;
            if (xi > yi) exit 0; if (xi < yi) exit 1;
        }
        exit 0 }'
}

glibc_version() {
    v="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
    if [ -z "$v" ] && have ldd; then
        v="$(ldd --version 2>&1 | head -n 1 | awk '/GLIBC|GNU libc/ {print $NF}')"
    fi
    echo "$v"
}

detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"
    TARGET=""
    NO_BINARY_REASON=""
    LOCAL_REASON=""
    GLIBC=""
    case "$OS" in
    Linux)
        case "$ARCH" in
        x86_64 | amd64)
            GLIBC="$(glibc_version)"
            if [ -z "$GLIBC" ]; then
                NO_BINARY_REASON="this Linux does not use glibc (musl?), and the prebuilt binary needs it"
                LOCAL_REASON="needs glibc >= $MIN_GLIBC"
            else
                TARGET="x86_64-unknown-linux-gnu"
                if ! version_ge "$GLIBC" "$MIN_GLIBC"; then
                    LOCAL_REASON="glibc $GLIBC is older than $MIN_GLIBC; use docker or podman"
                fi
            fi
            ;;
        *)
            NO_BINARY_REASON="there is no prebuilt binary for Linux $ARCH"
            LOCAL_REASON="Linux x86_64 only (model libraries are x86_64)"
            ;;
        esac
        ;;
    Darwin)
        LOCAL_REASON="Linux x86_64 only (model libraries are Linux ELF)"
        case "$ARCH" in
        arm64 | aarch64) TARGET="aarch64-apple-darwin" ;;
        x86_64)
            # An Intel shell under Rosetta on Apple silicon: use the native build.
            if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
                TARGET="aarch64-apple-darwin"
            else
                TARGET="x86_64-apple-darwin"
            fi
            ;;
        *) NO_BINARY_REASON="there is no prebuilt binary for macOS $ARCH" ;;
        esac
        ;;
    *)
        NO_BINARY_REASON="there is no prebuilt binary for $OS $ARCH (Windows: use install.ps1)"
        LOCAL_REASON="Linux x86_64 only"
        ;;
    esac
}

no_binary_help() {
    err "$NO_BINARY_REASON."
    say ""
    say "Alternatives:"
    say "  - run it in a container (docker or podman):"
    say "      docker run -it -v \"\$PWD:/data\" joshcu/ngiab-rs"
    say "  - build it from source with Rust:"
    say "      cargo install --git https://github.com/$REPO --locked"
}

# ---------------------------------------------------------------------------
# Binary
# ---------------------------------------------------------------------------

install_binary() {
    asset="ngiab-rs-$TARGET.tar.gz"
    base="$(release_base)"
    say "Downloading $asset from $base"
    download "$base/$asset" "$TMP/$asset" || die "download failed: $base/$asset"
    download "$base/SHA256SUMS" "$TMP/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
    verify_sha256 "$TMP/$asset" "$asset" "$TMP/SHA256SUMS" || die "refusing to install an unverified binary"

    mkdir -p "$TMP/x"
    tar -xzf "$TMP/$asset" -C "$TMP/x" || die "could not extract $asset"
    [ -f "$TMP/x/ngiab-rs" ] || die "$asset does not contain ngiab-rs"

    mkdir -p "$INSTALL_DIR" || die "could not create $INSTALL_DIR"
    # Copy next to the target, then rename: replaces a running binary safely.
    install -m 755 "$TMP/x/ngiab-rs" "$INSTALL_DIR/.ngiab-rs.tmp.$$" || die "could not write to $INSTALL_DIR"
    mv -f "$INSTALL_DIR/.ngiab-rs.tmp.$$" "$INSTALL_DIR/ngiab-rs" || die "could not install into $INSTALL_DIR"
    BIN="$INSTALL_DIR/ngiab-rs"
    if [ "$OS" = Darwin ]; then
        xattr -d com.apple.quarantine "$BIN" 2>/dev/null || true
    fi
    "$BIN" --version >/dev/null 2>&1 </dev/null || die "the installed binary does not run: $BIN --version"
    say "Installed $("$BIN" --version </dev/null) to $BIN"
}

find_binary() {
    if [ -x "$INSTALL_DIR/ngiab-rs" ]; then
        BIN="$INSTALL_DIR/ngiab-rs"
    elif have ngiab-rs; then
        BIN="$(command -v ngiab-rs)"
    else
        BIN=""
    fi
}

# ---------------------------------------------------------------------------
# PATH
# ---------------------------------------------------------------------------

on_path() {
    case ":${PATH:-}:" in
    *":$1:"*) return 0 ;;
    *) return 1 ;;
    esac
}

modify_path() {
    PATH_NOTE=""
    on_path "$INSTALL_DIR" && return 0
    if [ "$NO_MODIFY_PATH" = 1 ]; then
        PATH_NOTE="$INSTALL_DIR is not on PATH (--no-modify-path): add it yourself."
        return 0
    fi
    case "$INSTALL_DIR" in
    *"'"* | *'
'*)
        PATH_NOTE="$INSTALL_DIR contains a quote or newline; add it to PATH yourself."
        return 0
        ;;
    esac

    line="case \":\$PATH:\" in *':$INSTALL_DIR:'*) ;; *) export PATH='$INSTALL_DIR':\"\$PATH\" ;; esac $PATH_MARKER"
    edited=""
    found=0
    for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
        [ -f "$rc" ] || continue
        found=1
        add_rc_line "$rc" "$line" && edited="$edited $rc"
    done
    if [ "$found" = 0 ]; then
        add_rc_line "$HOME/.profile" "$line" && edited="$edited $HOME/.profile"
    fi
    if have fish || [ -d "$HOME/.config/fish" ]; then
        fish_file="$HOME/.config/fish/conf.d/ngiab-rs.fish"
        if mkdir -p "$HOME/.config/fish/conf.d" 2>/dev/null &&
            printf '%s\n' "if not contains '$INSTALL_DIR' \$PATH; set -gx PATH '$INSTALL_DIR' \$PATH; end $PATH_MARKER" >"$fish_file"; then
            edited="$edited $fish_file"
        fi
    fi
    if [ -n "$edited" ]; then
        say "Added $INSTALL_DIR to PATH in:$edited"
    fi
    PATH_NOTE="Open a new shell, or run:  export PATH=\"$INSTALL_DIR:\$PATH\""
}

# add_rc_line FILE LINE: append unless a marked line for this dir is already there.
add_rc_line() {
    if [ -f "$1" ] && grep -F "$PATH_MARKER" "$1" 2>/dev/null | grep -qF "'$INSTALL_DIR'"; then
        return 1
    fi
    printf '\n%s\n' "$2" >>"$1" 2>/dev/null || {
        warn "could not write to $1"
        return 1
    }
}

# ---------------------------------------------------------------------------
# Package managers
# ---------------------------------------------------------------------------

detect_pkg_manager() {
    PKG=""
    if [ "$OS" = Darwin ]; then
        have brew && PKG=brew
        return 0
    fi
    for p in apt-get dnf yum pacman zypper; do
        if have "$p"; then
            PKG="$p"
            return 0
        fi
    done
}

# pkg_install "why" PACKAGES... (Linux package managers; echoed and confirmed)
pkg_install() {
    why="$1"
    shift
    pfx="$(priv_prefix)"
    case "$PKG" in
    apt-get)
        confirm_cmds "$why" "${pfx}apt-get update" "${pfx}apt-get install -y $*" || return 1
        as_root apt-get update || return 1
        as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y "$@"
        ;;
    dnf | yum)
        confirm_cmds "$why" "${pfx}$PKG install -y $*" || return 1
        as_root "$PKG" install -y "$@"
        ;;
    pacman)
        confirm_cmds "$why" "${pfx}pacman -S --needed --noconfirm $*" || return 1
        as_root pacman -S --needed --noconfirm "$@"
        ;;
    zypper)
        confirm_cmds "$why" "${pfx}zypper --non-interactive install $*" || return 1
        as_root zypper --non-interactive install "$@"
        ;;
    *)
        err "no supported package manager found (apt-get, dnf, yum, pacman, zypper)"
        return 1
        ;;
    esac
}

# ---------------------------------------------------------------------------
# Backend results
# ---------------------------------------------------------------------------

# result KIND STATUS NOTE   (STATUS: ok | action | failed | unsupported | skipped)
result() {
    eval "RES_$1=\$2"
    eval "NOTE_$1=\$3"
}

# ---------------------------------------------------------------------------
# Backend: local
# ---------------------------------------------------------------------------

find_cargo() {
    CARGO=""
    if have cargo; then
        CARGO="$(command -v cargo)"
    elif [ -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]; then
        CARGO="${CARGO_HOME:-$HOME/.cargo}/bin/cargo"
    fi
    [ -n "$CARGO" ]
}

ensure_cargo() {
    find_cargo && return 0
    say ""
    say "Rust (cargo) is needed to build bmi-driver and rs-route."
    say "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal"
    confirm "Install Rust with rustup (into ~/.cargo, no sudo)?" y || return 1
    download "https://sh.rustup.rs" "$TMP/rustup-init.sh" || return 1
    if [ "$NO_MODIFY_PATH" = 1 ]; then
        sh "$TMP/rustup-init.sh" -y --profile minimal --no-modify-path </dev/null || return 1
    else
        sh "$TMP/rustup-init.sh" -y --profile minimal </dev/null || return 1
    fi
    find_cargo
}

# A C and C++ compiler, gfortran (rs_route's build.rs compiles a Fortran kernel),
# cmake and make. Python headers are not needed.
ensure_build_tools() {
    missing=""
    have cc || have gcc || missing="$missing cc"
    have c++ || have g++ || missing="$missing c++"
    have gfortran || missing="$missing gfortran"
    have cmake || missing="$missing cmake"
    have make || missing="$missing make"
    if [ -n "$missing" ]; then
        say ""
        say "Missing build tools:$missing"
        case "$PKG" in
        apt-get) pkgs="build-essential gfortran cmake" ;;
        dnf | yum) pkgs="gcc gcc-c++ gcc-gfortran make cmake" ;;
        pacman) pkgs="base-devel gcc-fortran cmake" ;;
        zypper) pkgs="gcc gcc-c++ gcc-fortran make cmake" ;;
        *)
            err "install a C/C++ compiler, gfortran, cmake and make, then re-run"
            return 1
            ;;
        esac
        # shellcheck disable=SC2086 # word splitting of the package list is intended
        pkg_install "Install the compilers and build tools:" $pkgs || return 1
        for t in cc c++ gfortran make; do
            have "$t" || {
                err "$t is still missing"
                return 1
            }
        done
    fi
    ensure_cmake
}

# Sets CMAKE_DIR to a directory to put first on PATH when the system cmake is
# missing or too old; empty when the system one is fine.
ensure_cmake() {
    CMAKE_DIR=""
    if have cmake; then
        cv="$(cmake --version 2>/dev/null | awk 'NR == 1 {print $3}')"
        if [ -n "$cv" ] && version_ge "$cv" "$MIN_CMAKE"; then
            return 0
        fi
        say ""
        say "cmake ${cv:-?} is older than $MIN_CMAKE, which the static HDF5 build needs."
    else
        say ""
        say "cmake is missing."
    fi
    name="cmake-$KITWARE_CMAKE-linux-x86_64"
    url="https://github.com/Kitware/CMake/releases/download/v$KITWARE_CMAKE/$name.tar.gz"
    say "    $url"
    confirm "Download CMake $KITWARE_CMAKE (about 50 MB) into a temporary directory for this build (no sudo)?" y ||
        return 1
    download "$url" "$TMP/$name.tar.gz" || return 1
    printf '%s  %s\n' "$KITWARE_CMAKE_SHA256" "$name.tar.gz" >"$TMP/cmake.sha256"
    verify_sha256 "$TMP/$name.tar.gz" "$name.tar.gz" "$TMP/cmake.sha256" || return 1
    tar -xzf "$TMP/$name.tar.gz" -C "$TMP" || return 1
    CMAKE_DIR="$TMP/$name/bin"
    "$CMAKE_DIR/cmake" --version >/dev/null 2>&1 </dev/null || {
        err "the downloaded cmake does not run"
        return 1
    }
}

# Reads BMI_DRIVER_VERSION / RS_ROUTE_VERSION from the release's TOOL_VERSIONS.
# The file is parsed, never sourced.
fetch_tool_versions() {
    if [ -n "$DOWNLOAD_URL" ]; then
        url="$DOWNLOAD_URL/TOOL_VERSIONS"
    else
        url="https://raw.githubusercontent.com/$REPO/v$INSTALLED_VERSION/TOOL_VERSIONS"
    fi
    download "$url" "$TMP/TOOL_VERSIONS" || {
        err "could not download $url"
        return 1
    }
    BMI_DRIVER_VERSION="$(sed -n 's/^BMI_DRIVER_VERSION=\([0-9A-Za-z.+-]*\)[[:space:]]*$/\1/p' "$TMP/TOOL_VERSIONS" | head -n 1)"
    RS_ROUTE_VERSION="$(sed -n 's/^RS_ROUTE_VERSION=\([0-9A-Za-z.+-]*\)[[:space:]]*$/\1/p' "$TMP/TOOL_VERSIONS" | head -n 1)"
    [ -n "$BMI_DRIVER_VERSION" ] && [ -n "$RS_ROUTE_VERSION" ] || {
        err "$url does not define BMI_DRIVER_VERSION and RS_ROUTE_VERSION"
        return 1
    }
}

setup_local() {
    say ""
    say "== local =="
    if [ -n "$LOCAL_REASON" ]; then
        result local unsupported "$LOCAL_REASON"
        return
    fi
    ensure_cargo || {
        result local failed "Rust is not installed (https://rustup.rs)"
        return
    }
    ensure_build_tools || {
        result local failed "missing C/C++ compiler, gfortran, make or cmake >= $MIN_CMAKE"
        return
    }
    fetch_tool_versions || {
        result local failed "could not read TOOL_VERSIONS for v$INSTALLED_VERSION"
        return
    }
    say ""
    say "Building bmi-driver $BMI_DRIVER_VERSION and rs_route $RS_ROUTE_VERSION with cargo."
    say "This compiles netCDF/HDF5 from source and takes about 5-15 minutes."
    # cmake >= 4 refuses the old cmake_minimum_required of the bundled C libraries.
    for crate in "bmi-driver:$BMI_DRIVER_VERSION" "rs_route:$RS_ROUTE_VERSION"; do
        name="${crate%%:*}"
        ver="${crate#*:}"
        say "+ cargo install $name --version $ver --locked --features static"
        if ! PATH="${CMAKE_DIR:+$CMAKE_DIR:}$PATH" \
            CMAKE_POLICY_VERSION_MINIMUM="${CMAKE_POLICY_VERSION_MINIMUM:-3.5}" \
            "$CARGO" install "$name" --version "$ver" --locked --features static </dev/null; then
            result local failed "cargo install $name $ver failed (see the output above)"
            return
        fi
    done
    say ""
    say "Downloading the model files (about 30-80 MB) into ~/.ngiab/container"
    say "+ ngiab-rs models update -y"
    if ! "$BIN" models update -y </dev/null; then
        result local failed "tools built, but the model download failed; retry with: ngiab-rs models update"
        return
    fi
    result local ok "bmi-driver $BMI_DRIVER_VERSION, rs-route $RS_ROUTE_VERSION, model files installed"
}

# ---------------------------------------------------------------------------
# Backends: docker and podman
# ---------------------------------------------------------------------------

os_release_ids() {
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        (. /etc/os-release && echo "${ID:-} ${ID_LIKE:-}")
    fi
}

# Pull the image; on failure records the result and returns 1.
pull_image() {
    rt="$1"
    shift
    say "+ $* $rt pull $IMAGE"
    if "$@" "$rt" pull "$IMAGE" </dev/null; then
        return 0
    fi
    result "$rt" failed "could not pull $IMAGE"
    return 1
}

# systemd is PID 1 (not the case in most containers and WSL1).
has_systemd() { have systemctl && [ -d /run/systemd/system ]; }

install_docker_linux() {
    ids=" $(os_release_ids) "
    pfx="$(priv_prefix)"
    case "$ids" in
    *" ubuntu "* | *" debian "* | *" raspbian "* | *" fedora "* | *" centos "* | *" rhel "*)
        confirm_cmds "Install Docker Engine with Docker's convenience script:" \
            "curl -fsSL https://get.docker.com | ${pfx}sh" || return 1
        download "https://get.docker.com" "$TMP/get-docker.sh" || return 1
        as_root sh "$TMP/get-docker.sh" || return 1
        ;;
    *" arch "*)
        pkg_install "Install Docker from the Arch repositories:" docker || return 1
        ;;
    *" opensuse"* | *" suse "* | *" sles "*)
        pkg_install "Install Docker from the openSUSE repositories:" docker || return 1
        ;;
    *)
        err "no automatic Docker install for this distribution; see https://docs.docker.com/engine/install/"
        return 1
        ;;
    esac
    # The convenience script starts the service on systemd hosts; the packages don't.
    if has_systemd && ! docker info >/dev/null 2>&1 </dev/null; then
        DOCKER_START_ASKED=1
        confirm_cmds "Start the Docker service now and at boot:" "${pfx}systemctl enable --now docker" &&
            as_root systemctl enable --now docker
    fi
    have docker
}

setup_docker() {
    say ""
    say "== docker =="
    pfx="$(priv_prefix)"
    if ! have docker; then
        if [ "$OS" = Darwin ]; then
            say "+ brew install --cask docker"
            if [ "$PKG" = brew ] && confirm "Install Docker Desktop with Homebrew?" y &&
                brew install --cask docker </dev/null; then
                result docker action "launch Docker Desktop once (open -a Docker) and accept the licence; the image is pulled on first run"
            else
                result docker failed "install Docker Desktop: https://www.docker.com/products/docker-desktop/"
            fi
            return
        fi
        install_docker_linux || {
            result docker failed "Docker is not installed; see https://docs.docker.com/engine/install/"
            return
        }
    fi

    out="$(docker info 2>&1 </dev/null)"
    st=$?
    use_root=""
    note=""
    if [ $st -ne 0 ]; then
        case "$out" in
        *[Pp]"ermission denied"*)
            user="$(id -un)"
            if is_root; then
                result docker failed "permission denied on the Docker socket even as root: $(printf '%s' "$out" | tail -n 1)"
                return
            elif id -nG "$user" 2>/dev/null | tr ' ' '\n' | grep -qx docker; then
                # Already in the group (per /etc/group), just not in this login session.
                note="you are in the docker group, but it only applies after you log out and back in"
                use_root=1
            elif confirm_cmds "You can't use the Docker daemon yet. Add $user to the docker group:" \
                "${pfx}usermod -aG docker $user" && as_root usermod -aG docker "$user"; then
                note="log out and back in (or reboot) so the docker group applies"
                use_root=1
            else
                result docker failed "permission denied on the Docker socket; run: ${pfx}usermod -aG docker $user, then log in again"
                return
            fi
            ;;
        *)
            if [ "$OS" = Darwin ]; then
                result docker action "Docker is not running: open Docker Desktop (open -a Docker); the image is pulled on first run"
                return
            fi
            if has_systemd && [ -z "${DOCKER_START_ASKED:-}" ] &&
                confirm_cmds "The Docker daemon is not running. Start it now and at boot:" \
                "${pfx}systemctl enable --now docker" && as_root systemctl enable --now docker &&
                docker info >/dev/null 2>&1 </dev/null; then
                :
            else
                result docker failed "the Docker daemon is not running; start it (e.g. ${pfx}systemctl start docker), then run: ngiab-rs backend install docker"
                return
            fi
            ;;
        esac
    fi

    if [ -n "$use_root" ] && ! is_root; then
        # Group membership only applies after a new login; pull as root meanwhile.
        if confirm_cmds "Pull the image as root until then:" "${pfx}docker pull $IMAGE"; then
            if as_root docker pull "$IMAGE"; then
                result docker action "$note"
            else
                result docker action "$note; then run: docker pull $IMAGE"
            fi
        else
            result docker action "$note; then run: docker pull $IMAGE"
        fi
        return
    fi
    pull_image docker || return
    result docker ok "$IMAGE"
}

setup_podman() {
    say ""
    say "== podman =="
    if ! have podman; then
        if [ "$OS" = Darwin ]; then
            if [ "$PKG" != brew ]; then
                result podman failed "install Podman: https://podman.io/docs/installation"
                return
            fi
            say "+ brew install podman"
            if ! confirm "Install Podman with Homebrew?" y || ! brew install podman </dev/null; then
                result podman failed "install Podman: https://podman.io/docs/installation"
                return
            fi
        else
            pkg_install "Install Podman:" podman || {
                result podman failed "Podman is not installed; see https://podman.io/docs/installation"
                return
            }
        fi
    fi
    if [ "$OS" = Darwin ] && ! podman info >/dev/null 2>&1 </dev/null; then
        if [ -z "$(podman machine list --format '{{.Name}}' 2>/dev/null </dev/null)" ]; then
            say "+ podman machine init"
            podman machine init </dev/null || {
                result podman failed "podman machine init failed"
                return
            }
        fi
        say "+ podman machine start"
        podman machine start </dev/null || {
            result podman failed "podman machine start failed"
            return
        }
    fi
    if ! out="$(podman info 2>&1 </dev/null)"; then
        printf '%s\n' "$out" | tail -n 5 >&2
        result podman failed "podman info failed (see above)"
        return
    fi
    pull_image podman || return
    result podman ok "$IMAGE"
}

# ---------------------------------------------------------------------------
# Backend selection
# ---------------------------------------------------------------------------

recommended_backends() {
    if [ -z "$LOCAL_REASON" ] && find_cargo; then
        echo local
    elif have docker && docker info >/dev/null 2>&1 </dev/null; then
        echo docker
    elif have podman && podman info >/dev/null 2>&1 </dev/null; then
        echo podman
    else
        echo docker
    fi
}

# normalize_backends "LIST" -> sets SELECTED (space separated, ordered, unique) or returns 1.
normalize_backends() {
    SELECTED=""
    want_local=0
    want_docker=0
    want_podman=0
    for tok in $(printf '%s' "$1" | tr ',' ' ' | tr '[:upper:]' '[:lower:]'); do
        case "$tok" in
        1 | local) want_local=1 ;;
        2 | docker) want_docker=1 ;;
        3 | podman) want_podman=1 ;;
        all) want_local=1 want_docker=1 want_podman=1 ;;
        none | 0) ;;
        *)
            err "unknown backend: $tok (expected local, docker, podman, all or none)"
            return 1
            ;;
        esac
    done
    [ $want_local = 1 ] && SELECTED="$SELECTED local"
    [ $want_docker = 1 ] && SELECTED="$SELECTED docker"
    [ $want_podman = 1 ] && SELECTED="$SELECTED podman"
    SELECTED="${SELECTED# }"
    return 0
}

menu() {
    rec="$1"
    if [ -n "$LOCAL_REASON" ]; then
        local_line="unavailable: $LOCAL_REASON"
    else
        local_line="runs bmi-driver and rs-route natively, no containers; fastest.
                Installs Rust if needed, compilers (C/C++, gfortran, cmake, make) via
                your package manager, builds both tools (5-15 min) and downloads the
                model files (30-80 MB) into ~/.ngiab/container."
    fi
    cat >&2 <<EOF

Which backends should ngiab-rs use? (choose any number)
  1) local   - $local_line
  2) docker  - runs each tool in the $IMAGE image.
                Installs Docker if missing (asks first), checks the daemon, pulls the image.
  3) podman  - like docker but rootless and daemonless.
                Installs Podman if missing (asks first), pulls the image.

EOF
    tries=0
    while [ $tries -lt 3 ]; do
        tries=$((tries + 1))
        ask "Enter numbers or names (e.g. \"1 3\", \"all\", \"none\"; Enter = $rec):" || break
        [ -n "$ANSWER" ] || ANSWER="$rec"
        if normalize_backends "$ANSWER" 2>/dev/null; then
            return 0
        fi
        say "Didn't understand \"$ANSWER\"."
    done
    say "No valid choice; not setting up any backend."
    SELECTED=""
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

summary() {
    [ -n "$SELECTED" ] || return 0
    say ""
    say "Backend summary"
    printf '  %-8s %-12s %s\n' BACKEND RESULT NOTES >&2
    for k in $SELECTED; do
        r="" n=""
        eval "r=\${RES_$k:-skipped}"
        eval "n=\${NOTE_$k:-}"
        case "$r" in
        ok) label="ok" ;;
        action) label="needs action" ;;
        unsupported) label="unsupported" ;;
        failed) label="FAILED" ;;
        *) label="$r" ;;
        esac
        printf '  %-8s %-12s %s\n' "$k" "$label" "$n" >&2
    done
}

main() {
    parse_args "$@"
    detect_tty
    detect_platform
    detect_pkg_manager

    # Validate --backends before downloading anything.
    REQUESTED=""
    if [ -n "$BACKENDS" ]; then
        normalize_backends "$BACKENDS" || exit 2
        REQUESTED="$SELECTED"
    fi

    TMP="$(mktemp -d 2>/dev/null || mktemp -d -t ngiab-rs)" || die "could not create a temporary directory"
    trap 'rm -rf "$TMP"' EXIT
    trap 'rm -rf "$TMP"; exit 130' INT TERM

    if [ "$SKIP_BINARY" = 1 ]; then
        find_binary
        [ -n "$BIN" ] || die "--skip-binary: ngiab-rs is not installed in $INSTALL_DIR or on PATH"
    else
        if [ -n "$NO_BINARY_REASON" ]; then
            no_binary_help
            exit 1
        fi
        install_binary
        modify_path
    fi

    INSTALLED_VERSION="$("$BIN" --version </dev/null 2>/dev/null | awk '{print $2; exit}')"
    INSTALLED_VERSION="${INSTALLED_VERSION#v}"
    [ -n "$INSTALLED_VERSION" ] || die "could not read the version from $BIN --version"
    IMAGE="${NGIAB_RS_IMAGE:-$IMAGE_REPO:$INSTALLED_VERSION}"

    SELECTED="$REQUESTED"
    if [ -n "$BACKENDS" ]; then
        :
    elif [ "$YES" = 1 ]; then
        SELECTED="$(recommended_backends)"
        say "Setting up the recommended backend: $SELECTED (-y)"
    elif [ "$HAVE_TTY" = 1 ]; then
        menu "$(recommended_backends)"
    else
        rec="$(recommended_backends)"
        say ""
        say "No terminal to ask on, so no backend was set up. To set one up later, run:"
        say "    ngiab-rs backend install $rec"
        say "or re-run this installer with, e.g.:"
        say "    curl -LsSf https://github.com/$REPO/releases/latest/download/install.sh | sh -s -- --skip-binary --backends $rec -y"
    fi

    for k in $SELECTED; do
        case "$k" in
        local) setup_local ;;
        docker) setup_docker ;;
        podman) setup_podman ;;
        esac
    done

    # Record what works (or will after a re-login / first launch) in install.json.
    record=""
    failed=0
    for k in $SELECTED; do
        eval "r=\${RES_$k:-skipped}"
        case "$r" in
        ok | action) record="$record $k" ;;
        failed) failed=1 ;;
        esac
    done
    if [ -n "$record" ]; then
        # shellcheck disable=SC2086 # one argument per backend
        "$BIN" backend record $record </dev/null ||
            warn "could not record the backends in install.json ($BIN backend record$record)"
    fi

    summary
    say ""
    [ "$SKIP_BINARY" = 1 ] || say "ngiab-rs $INSTALLED_VERSION is installed at $BIN"
    [ -z "${PATH_NOTE:-}" ] || say "$PATH_NOTE"
    say "Get started:  ngiab-rs /path/to/data_folder   (ngiab-rs --help for more)"
    [ "$failed" = 0 ] || exit 1
}

main "$@"
