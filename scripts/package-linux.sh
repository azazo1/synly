#!/usr/bin/env bash
# 打包 Linux 安装版: synly-<version>-linux-<arch>-setup.tar.gz
#
# 归档结构:
#   install.sh          安装, 升级与卸载脚本
#   payload/            程序文件, 由 install.sh 整目录交换到 <prefix>/opt/synly
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
archive="$output_dir/synly-$version-linux-$arch-setup$suffix.tar.gz"

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
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stage="$(mktemp -d "$output_dir/linux-stage.XXXXXX")"
payload="$stage/payload"
cleanup() {
    # stage 是本脚本用 mktemp 创建的中间目录, 只删除它自己.
    case "$stage" in
        "$output_dir"/*) rm -rf -- "$stage" ;;
    esac
}
trap cleanup EXIT

mkdir -p "$payload/icons"
cp "$binary" "$payload/synly"
chmod 755 "$payload/synly"
# audio-licenses 必须由许可脚本自己创建, 它拒绝写入已存在的目标.
bash "$root/scripts/package-audio-notices.sh" "$payload/audio-licenses"
cp "$root/assets/linux/synly-256.png" "$payload/icons/synly-256.png"
cp "$root/assets/linux/synly-512.png" "$payload/icons/synly-512.png"

cat >"$payload/install-manifest.txt" <<MANIFEST
name=synly
display_name=Synly
version=$version
form=installer
MANIFEST

cp "$root/scripts/installer-linux.sh" "$stage/install.sh"
chmod 755 "$stage/install.sh"

bundle_linux_sdl() {
    local listing line name path
    if ! command -v ldd >/dev/null; then
        return 0
    fi
    if ! listing="$(ldd "$payload/synly" 2>/dev/null)"; then
        return 0
    fi
    if ! grep -q 'libSDL2' <<<"$listing"; then
        printf '[audio] 原生播放构建, 无需携带 SDL2\n'
        return 0
    fi
    printf '[package] 随附 SDL2 运行库\n'
    [[ -s "$root/licenses/audio/SDL2-LICENSE.txt" ]] || {
        printf '缺少 SDL2 许可全文\n' >&2
        exit 1
    }
    cp "$root/licenses/audio/SDL2-LICENSE.txt" "$payload/audio-licenses/SDL2-LICENSE.txt"
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
                cp -L "$path" "$payload/$name"
                if [[ "$name" == *libSDL3* ]]; then
                    [[ -s "$root/licenses/audio/SDL3-LICENSE.txt" ]] || {
                        printf '缺少 SDL3 许可全文\n' >&2
                        exit 1
                    }
                    cp "$root/licenses/audio/SDL3-LICENSE.txt" "$payload/audio-licenses/SDL3-LICENSE.txt"
                fi
                ;;
        esac
    done <<<"$listing"
    if command -v readelf >/dev/null; then
        if ! readelf -d "$payload/synly" | grep -E 'RPATH|RUNPATH' | grep -q '\$ORIGIN'; then
            if command -v patchelf >/dev/null; then
                printf '[package] 为 SDL2 构建补写 $ORIGIN rpath\n'
                patchelf --set-rpath '$ORIGIN' "$payload/synly"
            else
                printf 'SDL2 构建缺少 $ORIGIN rpath, 请设置 SYNLY_BUNDLE_SDL=1 重新编译或安装 patchelf\n' >&2
                exit 1
            fi
        fi
    fi
}

bundle_linux_sdl

rm -f "$archive"
tar -czf "$archive" -C "$stage" install.sh payload
test -f "$archive"
printf '[package] completed %s\n' "$archive"
