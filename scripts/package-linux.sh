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
stage="$(mktemp -d "$output_dir/linux-stage.XXXXXX")"
notices_stage="$(mktemp -d "$output_dir/audio-notices.XXXXXX")"
cleanup() {
    rm -f "$stage/synly"
    rm -f "$stage"/libSDL2-2.0.so.0 "$stage"/libSDL3.so.0
    if [[ -d "$stage/audio-licenses" ]]; then
        for name in README.md sunshine-GPL-3.0.txt moonlight-common-GPL-3.0.txt moonlight-qt-GPL-3.0.txt SDL2-LICENSE.txt SDL3-LICENSE.txt; do
            rm -f "$stage/audio-licenses/$name"
        done
        rmdir "$stage/audio-licenses" || true
    fi
    rmdir "$stage" || true
    for name in README.md sunshine-GPL-3.0.txt moonlight-common-GPL-3.0.txt moonlight-qt-GPL-3.0.txt; do
        rm -f "$notices_stage/audio-licenses/$name"
    done
    if [[ -d "$notices_stage/audio-licenses" ]]; then rmdir "$notices_stage/audio-licenses"; fi
    rmdir "$notices_stage" || true
}
trap cleanup EXIT

cp "$binary" "$stage/synly"
chmod 755 "$stage/synly"
bash "$(dirname "${BASH_SOURCE[0]}")/package-audio-notices.sh" "$notices_stage/audio-licenses"
cp -R "$notices_stage/audio-licenses" "$stage/audio-licenses"

bundle_linux_sdl() {
    local listing line name path
    if ! command -v ldd >/dev/null; then
        return 0
    fi
    if ! listing="$(ldd "$stage/synly" 2>/dev/null)"; then
        return 0
    fi
    if ! grep -q 'libSDL2' <<<"$listing"; then
        printf '[audio] 原生播放构建, 无需携带 SDL2\n'
        return 0
    fi
    printf '[package] 随附 SDL2 运行库\n'
    local root
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    [[ -s "$root/licenses/audio/SDL2-LICENSE.txt" ]] || {
        printf '缺少 SDL2 许可全文\n' >&2
        exit 1
    }
    cp "$root/licenses/audio/SDL2-LICENSE.txt" "$stage/audio-licenses/SDL2-LICENSE.txt"
    while IFS= read -r line; do
        case "$line" in
            *libSDL2*|*libSDL3*)
                name="${line%% =>*}"
                name="${name#"${name%%[![:space:]]*}"}"
                path="${line#*=> }"
                path="${path%% (*}"
                path="${path%"${path##*[![:space:]]}"}"
                if [[ "$path" == "not found" || -z "$path" ]]; then
                    printf '找不到运行时库 %s\n' "$name" >&2
                    exit 1
                fi
                cp -L "$path" "$stage/$name"
                if [[ "$name" == *libSDL3* ]]; then
                    [[ -s "$root/licenses/audio/SDL3-LICENSE.txt" ]] || {
                        printf '缺少 SDL3 许可全文\n' >&2
                        exit 1
                    }
                    cp "$root/licenses/audio/SDL3-LICENSE.txt" "$stage/audio-licenses/SDL3-LICENSE.txt"
                fi
                ;;
        esac
    done <<<"$listing"
    if command -v readelf >/dev/null; then
        if ! readelf -d "$stage/synly" | grep -E 'RPATH|RUNPATH' | grep -q '\$ORIGIN'; then
            if command -v patchelf >/dev/null; then
                printf '[package] 为 SDL2 构建补写 \$ORIGIN rpath\n'
                patchelf --set-rpath '$ORIGIN' "$stage/synly"
            else
                printf 'SDL2 构建缺少 \$ORIGIN rpath, 请设置 SYNLY_BUNDLE_SDL=1 重新编译或安装 patchelf\n' >&2
                exit 1
            fi
        fi
    fi
}

bundle_linux_sdl

archive_files=(synly audio-licenses)
for extra in libSDL2-2.0.so.0 libSDL3.so.0; do
    if [[ -e "$stage/$extra" ]]; then
        archive_files+=("$extra")
    fi
done
rm -f "$archive"
tar -czf "$archive" -C "$stage" "${archive_files[@]}"
test -f "$archive"
printf '[package] completed %s\n' "$archive"
