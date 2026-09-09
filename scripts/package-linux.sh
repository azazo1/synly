#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 0 ]]; then
    version="$(cargo metadata --locked --no-deps --format-version 1 | jq -er '.packages[] | select(.name == "synly") | .version')"
    target="$(rustc -vV | sed -n 's/^host: //p')"
    output_dir="dist"
elif [[ $# -eq 3 ]]; then
    version="$1"
    target="$2"
    output_dir="$3"
else
    printf 'usage: %s [VERSION TARGET OUTPUT_DIR]\n' "$0" >&2
    exit 2
fi
if [[ -x "target/$target/release/synly" ]]; then
    binary="target/$target/release/synly"
else
    binary="target/release/synly"
fi

case "$target" in
    x86_64-unknown-linux-gnu|x86_64-unknown-linux-musl) arch="x86_64" ;;
    aarch64-unknown-linux-gnu|aarch64-unknown-linux-musl) arch="aarch64" ;;
    *)
        printf 'unsupported Linux target: %s\n' "$target" >&2
        exit 1
        ;;
esac

suffix=""
if [[ "${SYNLY_FAKE_DIST:-}" == "1" || "${SYNLY_FAKE_DIST:-}" == "true" ]]; then
    suffix="-fake"
fi
archive="$output_dir/synly-$version-linux-$arch$suffix.tar.gz"

if [[ ! -x "$binary" ]]; then
    printf 'missing executable: %s\n' "$binary" >&2
    exit 1
fi

printf '[package] checking Linux binary for %s\n' "$target"
if ! file "$binary" | grep -q 'ELF'; then
    printf 'unexpected binary format: %s\n' "$binary" >&2
    exit 1
fi

printf '[package] creating %s\n' "$archive"
mkdir -p "$output_dir"
rm -f "$archive"
tar -C "$(dirname "$binary")" -czf "$archive" "$(basename "$binary")"
test -f "$archive"
printf '[package] completed %s\n' "$archive"
