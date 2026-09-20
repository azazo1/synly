#!/usr/bin/env bash
set -euo pipefail

# 在最终打包前验证音频动态库没有依赖开发机的安装路径.
if [[ $# -ne 1 || ! -f "$1" ]]; then
    printf '用法: %s MACH_O_EXECUTABLE\n' "$0" >&2
    exit 2
fi
printf '[audio] 检查 SDL2 动态依赖\n'
binary="$1"
binary_dir="$(cd "$(dirname "$binary")" && pwd)"
listing="$(otool -L "$binary")"
failed=0
sdl2=''
while IFS= read -r line; do
    # otool 首行为文件名, 依赖行以空白开头. SDL 名称不包含空格.
    case "$line" in
        *libSDL2*.dylib*|*SDL2.framework/SDL2*)
            dependency="${line#"${line%%[![:space:]]*}"}"
            dependency="${dependency%% \(*}"
            if [[ -n "$sdl2" ]]; then
                printf '发现多个 SDL2 依赖: %s\n' "$dependency" >&2
                failed=1
                continue
            fi
            sdl2="$dependency"
            ;;
    esac
done <<< "$listing"

if [[ -z "$sdl2" ]]; then
    printf '[audio] 未发现未支持的 SDL2 动态依赖\n'
    exit 0
fi

if [[ "$sdl2" != '@executable_path/../Frameworks/libSDL2.dylib' ]]; then
    printf 'SDL2 依赖尚未随包验证: %s\n' "$sdl2" >&2
    printf '拒绝生成不完整的应用包. 请先实现 SDL2 库随包复制, install name 修正, 许可和签名验证; 不要要求普通用户安装开发库.\n' >&2
    exit 1
fi

bundled="$binary_dir/../Frameworks/libSDL2.dylib"
if [[ ! -f "$bundled" || -L "$bundled" ]]; then
    printf '包内缺少 SDL2 运行库: %s\n' "$bundled" >&2
    failed=1
fi
if [[ "$failed" -eq 0 ]]; then
    symbols="$(strings "$bundled")"
    case "$symbols" in
        *sdl2-compat:*)
            sdl3="$binary_dir/../Frameworks/libSDL3.dylib"
            if [[ ! -f "$sdl3" || -L "$sdl3" ]]; then
                printf 'sdl2-compat 包内缺少 SDL3 运行库: %s\n' "$sdl3" >&2
                failed=1
            fi
            ;;
    esac
fi
if [[ "$failed" -ne 0 ]]; then
    printf '拒绝生成不完整的应用包.\n' >&2
    exit 1
fi
printf '[audio] SDL2 已改为包内加载路径\n'
