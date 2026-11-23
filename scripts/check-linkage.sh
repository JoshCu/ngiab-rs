#!/bin/sh
# Fail if a release binary dynamically links anything beyond the platform's
# system libraries (netcdf, hdf5 and zlib must be linked statically).
#
#   scripts/check-linkage.sh <target-triple> <binary>
#
# Linux:   ldd; only glibc (libc, libm, libpthread, libdl, librt, libutil; the
#          last is portable-pty's openpty on glibc < 2.34), libgcc_s and the
#          dynamic loader are allowed (no GTK: rfd must use xdg-portal).
# macOS:   otool -L; only libSystem, libc++, libiconv, libobjc and
#          /System/Library/Frameworks/* are allowed.
# Windows: objdump -p (if on PATH) or dumpbin /dependents (after
#          ilammy/msvc-dev-cmd); only DLLs that ship in System32, API sets and
#          the VC runtime are allowed, and never hdf5/netcdf/zlib.

set -eu

[ $# -eq 2 ] || {
    echo "usage: $0 <target-triple> <binary>" >&2
    exit 2
}
target=$1
bin=$2
[ -f "$bin" ] || {
    echo "error: $bin not found" >&2
    exit 1
}

bad=
check() { # check <dependency> <allowed: 0|1>
    if [ "$2" = 1 ]; then
        printf '  ok   %s\n' "$1"
    else
        printf '  BAD  %s\n' "$1"
        bad="$bad $1"
    fi
}

echo "dynamic dependencies of $bin ($target):"
case "$target" in
    *-linux-*)
        deps=$(ldd "$bin" | awk '{print $1}' | sed 's|.*/||')
        for d in $deps; do
            case "$d" in
                linux-vdso.so.* | ld-linux-*.so.* | libc.so.* | libm.so.* | libpthread.so.* | \
                    libdl.so.* | librt.so.* | libutil.so.* | libgcc_s.so.*) check "$d" 1 ;;
                *) check "$d" 0 ;;
            esac
        done
        ;;
    *-apple-*)
        deps=$(otool -L "$bin" | tail -n +2 | awk '{print $1}')
        for d in $deps; do
            case "$d" in
                /usr/lib/libSystem.* | /usr/lib/libc++.* | /usr/lib/libc++abi.* | /usr/lib/libiconv.* | \
                    /usr/lib/libobjc.* | /System/Library/Frameworks/*) check "$d" 1 ;;
                *) check "$d" 0 ;;
            esac
        done
        ;;
    *-windows-*)
        if command -v objdump >/dev/null 2>&1 && objdump -p "$bin" >/dev/null 2>&1; then
            deps=$(objdump -p "$bin" | sed -n 's/^[[:space:]]*DLL Name:[[:space:]]*//p' | tr -d '\r')
        else
            deps=$(MSYS_NO_PATHCONV=1 dumpbin /nologo /dependents "$bin" | tr -d '\r' |
                sed -n 's/^[[:space:]]*\([^[:space:]]*\.[dD][lL][lL]\)[[:space:]]*$/\1/p')
        fi
        sysroot=${SYSTEMROOT:-C:/Windows}
        if command -v cygpath >/dev/null 2>&1; then sysroot=$(cygpath -u "$sysroot"); fi
        for d in $deps; do
            lower=$(printf '%s' "$d" | tr '[:upper:]' '[:lower:]')
            case "$lower" in
                *hdf5* | *netcdf* | zlib*.dll | libz*.dll | libgcc* | libstdc++* | libwinpthread*) check "$d" 0 ;;
                api-ms-win-* | ext-ms-* | vcruntime140*.dll | msvcp140*.dll | ucrtbase.dll) check "$d" 1 ;;
                *) if [ -f "$sysroot/System32/$d" ]; then check "$d" 1; else check "$d" 0; fi ;;
            esac
        done
        ;;
    *)
        echo "error: unsupported target $target" >&2
        exit 2
        ;;
esac

[ -n "${deps:-}" ] || {
    echo "error: could not list dependencies of $bin" >&2
    exit 1
}
if [ -n "$bad" ]; then
    echo "error: unexpected dynamic dependencies:$bad" >&2
    exit 1
fi
echo "linkage ok"
