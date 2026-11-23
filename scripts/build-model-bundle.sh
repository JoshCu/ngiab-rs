#!/bin/sh
# Build the relocatable ngiab-rs model bundle from an NGIAB base image.
#
#   scripts/build-model-bundle.sh <base-image> <release-name> <output-dir>
#
#   e.g. scripts/build-model-bundle.sh awiciroh/ngiab:v1.9.0 models-2026.11.23 dist/
#
# Produces in <output-dir>:
#   ngiab-models-x86_64-unknown-linux-gnu.tar.gz   the bundle (manifest.json at its root)
#   ngiab-models-x86_64-unknown-linux-gnu.json     the manifest plus "tar_sha256"
#   SHA256SUMS                                     covers the two files above
#   bundle-report.md                               gate results and report-only findings
#
# The bundle holds only /dmod/datasets, /dmod/shared_libs (+ libgfortran and
# libquadmath) and the Rust LSTM weights. Every .so gets RPATH=$ORIGIN and the
# LSTM config.yml files get relative run_dir/train_dir/img_log_dir values.
#
# The script exits non-zero if any gate fails:
#   - a Python artefact (.venv, __pycache__, site-packages, *.py, *.pyc) remains
#   - a text file still contains /ngen/ or /dmod/
#   - a symlink dangles
#   - the tarball is larger than 1 GiB
#
# Requirements: Linux, GNU tar/find/grep, docker (or podman via
# CONTAINER_ENGINE=podman), patchelf, jq, readelf, strings, ldd, sha256sum.
#
# Environment:
#   CONTAINER_ENGINE  docker (default) or podman
#   PULL              1 = always pull the image; 0 = never pull; unset = pull
#                     only if no linux/amd64 copy of the image is present
#   KEEP_STAGE        1 = keep the staging directory and print its path

set -eu

TARGET=x86_64-unknown-linux-gnu
NAME="ngiab-models-$TARGET"
MAX_BYTES=1073741824 # 1 GiB
LSTM_REL=ngen/ngen/extern/lstm/trained_neuralhydrology_models

usage() {
    echo "usage: $0 <base-image> <release-name> <output-dir>" >&2
    exit 2
}

log() { printf '==> %s\n' "$*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

[ $# -eq 3 ] || usage
IMAGE=$1
RELEASE=$2
OUT=$3
[ -n "$IMAGE" ] && [ -n "$RELEASE" ] && [ -n "$OUT" ] || usage

ENGINE=${CONTAINER_ENGINE:-docker}
for tool in "$ENGINE" patchelf jq readelf strings ldd sha256sum tar find grep; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"
done

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/ngiab-bundle.XXXXXX")
STAGE="$WORK/stage"
CID=

cleanup() {
    if [ -n "$CID" ]; then "$ENGINE" rm "$CID" >/dev/null 2>&1 || true; fi
    if [ "${KEEP_STAGE:-0}" = 1 ]; then
        log "staging directory kept at $STAGE"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

REPORT="$OUT/bundle-report.md"
: >"$REPORT"
report() { printf '%s\n' "$*" >>"$REPORT"; }
# Turn stdin lines into a markdown bullet list of code spans.
# shellcheck disable=SC2016 # literal backticks
md_list() { sed 's/`/'"'"'/g; s/^/  - `/; s/$/`/' >>"$REPORT"; }

# ---------------------------------------------------------------- pull
image_arch() { "$ENGINE" image inspect --format '{{.Architecture}}' "$IMAGE" 2>/dev/null || true; }
case "${PULL:-auto}" in
    1) do_pull=1 ;;
    0) do_pull=0 ;;
    *) if [ "$(image_arch)" = amd64 ]; then do_pull=0; else do_pull=1; fi ;;
esac
if [ "$do_pull" = 1 ]; then
    log "pulling $IMAGE (linux/amd64)"
    "$ENGINE" pull --platform linux/amd64 "$IMAGE" >&2
fi
[ "$(image_arch)" = amd64 ] || die "$IMAGE is not available as linux/amd64 (got '$(image_arch)')"

# Prefer the RepoDigest of the repository we were asked for; images that were
# re-tagged from another repository carry several.
repo=${IMAGE%@*}
case "${repo##*/}" in *:*) repo=${repo%:*} ;; esac
digests=$("$ENGINE" image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$IMAGE")
digest_ref=$(printf '%s\n' "$digests" | grep -E "^(docker\.io/)?(library/)?${repo#docker.io/}@" | head -n 1 || true)
[ -n "$digest_ref" ] || digest_ref=$(printf '%s\n' "$digests" | grep @ | head -n 1 || true)
SOURCE_DIGEST=${digest_ref#*@}
[ -n "$SOURCE_DIGEST" ] || log "warning: no RepoDigest for $IMAGE (locally built image?)"

# ---------------------------------------------------------------- extract
log "extracting from $IMAGE"
CID=$("$ENGINE" create --platform linux/amd64 "$IMAGE")
mkdir -p "$STAGE/dmod" "$STAGE/${LSTM_REL%/*}"
"$ENGINE" cp "$CID:/dmod/datasets" "$STAGE/dmod/datasets"
"$ENGINE" cp "$CID:/dmod/shared_libs" "$STAGE/dmod/shared_libs"
"$ENGINE" cp "$CID:/$LSTM_REL" "$STAGE/$LSTM_REL"
# GCC runtime libraries for the Fortran models (GCC runtime library exception).
# Never bundle libc or libstdc++.
for lib in libgfortran.so.5 libquadmath.so.0; do
    "$ENGINE" cp -L "$CID:/usr/lib64/$lib" "$STAGE/dmod/shared_libs/$lib"
done
"$ENGINE" rm "$CID" >/dev/null
CID=
chmod -R u+rwX,go+rX "$STAGE"

# ---------------------------------------------------------------- prune
log "pruning Python artefacts"
find "$STAGE" \( -name .venv -o -name __pycache__ -o -name site-packages \
    -o -name '*.py' -o -name '*.pyc' \) -prune -exec rm -rf {} +
# Stale lock files left behind by an interrupted weight conversion.
find "$STAGE" -name .conversion.lock -type f -exec rm -f {} +

# ---------------------------------------------------------------- relocate
log "setting RPATH=\$ORIGIN on shared libraries"
find "$STAGE/dmod/shared_libs" -type f -name '*.so*' | sort | while IFS= read -r so; do
    # shellcheck disable=SC2016 # literal $ORIGIN is intended
    patchelf --set-rpath '$ORIGIN' "$so"
done

# rust-lstm resolves a relative run_dir against <runs>/.. (current releases)
# or by replacing ".." with <runs>/.. (the librust_lstm_1025.so in older
# images). "../trained_neuralhydrology_models/<run>" works with both.
log "making LSTM config.yml paths relative"
for cfg in "$STAGE/$LSTM_REL"/*/config.yml; do
    [ -f "$cfg" ] || continue
    run=$(basename "$(dirname "$cfg")")
    rel="../trained_neuralhydrology_models/$run"
    tmp="$cfg.tmp"
    awk -v rel="$rel" '
        function base(v) { sub(/[ \t]+$/, "", v); sub(/\/+$/, "", v); n = split(v, p, "/"); return p[n] }
        /^run_dir:/     { print "run_dir: " rel; next }
        /^train_dir:/   { v = $0; sub(/^train_dir:[ \t]*/, "", v);   print "train_dir: " rel "/" base(v); next }
        /^img_log_dir:/ { v = $0; sub(/^img_log_dir:[ \t]*/, "", v); print "img_log_dir: " rel "/" base(v); next }
        { print }
    ' "$cfg" >"$tmp"
    mv "$tmp" "$cfg"
    grep -q "^run_dir: $rel\$" "$cfg" || die "no run_dir in $cfg"
done

# ---------------------------------------------------------------- gates
log "checking gates"
report "# Model bundle report: $RELEASE"
report ""
report "- source image: \`$IMAGE\`"
report "- source digest: \`${SOURCE_DIGEST:-unknown}\`"
report ""
report "## Gates"
report ""
failed=0
gate() { # gate <name> <offending lines...>
    name=$1
    shift
    if [ -n "$*" ]; then
        failed=1
        report "- FAIL: $name"
        printf '%s\n' "$@" | sed "s|$STAGE/||" | md_list
        log "GATE FAILED: $name"
        printf '%s\n' "$@" >&2
    else
        report "- pass: $name"
    fi
}
gate "no Python artefacts" "$(find "$STAGE" \( -name .venv -o -name __pycache__ -o -name site-packages \
    -o -name '*.py' -o -name '*.pyc' \) -print)"
gate "no /ngen/ or /dmod/ in text files" "$(grep -rIlE '/ngen/|/dmod/' "$STAGE" || true)"
gate "no dangling symlinks" "$(find "$STAGE" -xtype l -print)"

# ---------------------------------------------------------------- report only
libs=$(find "$STAGE/dmod/shared_libs" -type f -name '*.so*' | sort)
# The loops below split $libs on newlines only, so a space in $STAGE (TMPDIR) or
# in a library name does not break the paths.
IFS_SAVED=$IFS
IFS='
'

report ""
report "## Report only"
report ""
report "### Absolute paths compiled into shared libraries (cannot be rewritten)"
report ""
hits=0
for so in $libs; do
    n=$(strings -n 5 "$so" | grep -cE '/dmod|/ngen' || true)
    if [ "$n" -gt 0 ]; then
        hits=1
        report "- \`$(basename "$so")\`: $n string(s), e.g."
        strings -n 5 "$so" | grep -E '/dmod|/ngen' | sort -u | head -n 5 | md_list
    fi
done
[ "$hits" = 1 ] || report "- none"

report ""
report "### Unresolved dependencies (ldd with LD_LIBRARY_PATH=dmod/shared_libs)"
report ""
hits=0
for so in $libs; do
    missing=$(LD_LIBRARY_PATH="$STAGE/dmod/shared_libs" ldd "$so" 2>&1 | grep 'not found' |
        awk '{print $1}' | sort -u | tr '\n' ' ' || true)
    if [ -n "$missing" ]; then
        hits=1
        report "- \`$(basename "$so")\`: ${missing% }"
    fi
done
[ "$hits" = 1 ] || report "- none"

report ""
report "### Symbol versions"
report ""
glibc_of() { readelf -V "$1" 2>/dev/null | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sed 's/^GLIBC_//' | sort -uV | tail -n 1; }
MIN_GLIBC=$(for so in $libs; do glibc_of "$so"; done | sort -uV | tail -n 1)
[ -n "$MIN_GLIBC" ] || die "could not determine the required glibc version"
for so in $libs; do
    v=$(glibc_of "$so")
    [ "$v" = "$MIN_GLIBC" ] && report "- glibc $MIN_GLIBC required by \`$(basename "$so")\`"
done
cxx=$(for so in $libs; do readelf -V "$so" 2>/dev/null | grep -oE 'GLIBCXX_[0-9]+(\.[0-9]+)+' || true; done |
    sed 's/^GLIBCXX_//' | sort -uV | tail -n 1)
report "- highest GLIBCXX (host libstdc++): ${cxx:-none}"
report "- **min_glibc: $MIN_GLIBC**"
IFS=$IFS_SAVED

if [ "$failed" = 1 ]; then
    report ""
    report "**One or more gates failed; no bundle was written.**"
    die "gates failed, see $REPORT"
fi

# ---------------------------------------------------------------- manifest
# Written after the text gate: the manifest itself names the /dmod and /ngen prefixes.
log "writing manifest"
CREATED=$(date -u +%Y-%m-%dT%H:%M:%SZ)
find "$STAGE/dmod/shared_libs" \( -type f -o -type l \) -name '*.so*' -exec basename {} \; | sort |
    jq -R . | jq -s \
        --arg release "$RELEASE" \
        --arg image "$IMAGE" \
        --arg digest "$SOURCE_DIGEST" \
        --arg created "$CREATED" \
        --arg target "$TARGET" \
        --arg glibc "$MIN_GLIBC" \
        '{schema: 1, release: $release, source_image: $image, source_digest: $digest,
          created: $created, target: $target, min_glibc: $glibc,
          prefixes: ["/dmod", "/ngen/ngen/extern"], libs: .}' >"$STAGE/manifest.json"

# ---------------------------------------------------------------- package
log "packaging"
TARBALL="$OUT/$NAME.tar.gz"
rm -f "$TARBALL" "$OUT/$NAME.json" "$OUT/SHA256SUMS"
tar --sort=name --owner=0 --group=0 --numeric-owner -C "$STAGE" -czf "$TARBALL" .
size=$(wc -c <"$TARBALL" | tr -d ' ')
tar_sha=$(sha256sum "$TARBALL" | awk '{print $1}')
jq --arg sha "$tar_sha" '. + {tar_sha256: $sha}' "$STAGE/manifest.json" >"$OUT/$NAME.json"
(cd "$OUT" && sha256sum "$NAME.tar.gz" "$NAME.json" >SHA256SUMS)

stray=$(tar tzf "$TARBALL" | grep -vE '^\./($|manifest\.json$|dmod/$|dmod/(datasets|shared_libs)(/|$)|ngen/$|ngen/ngen/$|ngen/ngen/extern/$|ngen/ngen/extern/lstm/$|ngen/ngen/extern/lstm/trained_neuralhydrology_models(/|$))' || true)
if [ -n "$stray" ]; then
    report ""
    report "- FAIL: unexpected tarball entries:"
    printf '%s\n' "$stray" | md_list
    rm -f "$TARBALL"
    die "unexpected entries in the tarball: $stray"
fi

if [ "$size" -gt "$MAX_BYTES" ]; then
    report ""
    report "- FAIL: tarball is $size bytes (limit $MAX_BYTES)"
    rm -f "$TARBALL"
    die "tarball is $size bytes, over the 1 GiB limit"
fi

report ""
report "## Bundle"
report ""
report "- \`$NAME.tar.gz\`: $size bytes, sha256 \`$tar_sha\`"
report "- files: $(tar tzf "$TARBALL" | grep -vc '/$')"

log "done: $TARBALL ($size bytes, min_glibc $MIN_GLIBC)"
