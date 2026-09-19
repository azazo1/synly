#!/usr/bin/env bash
set -euo pipefail

# 原始 sdlaud.cpp/sdl.h/renderer.h 保持不变, 仅外部 SDL/Qt/Moonlight API 使用替身.
if [[ $# -ne 2 ]]; then
    printf '用法: bash %s <moonlight-qt-dir> <output-executable>\n' "$0" >&2
    exit 2
fi
moonlight=$1
output=$2
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
renderers="$moonlight/app/streaming/audio/renderers"
for name in sdlaud.cpp sdl.h renderer.h; do
    if [[ ! -f "$renderers/$name" ]]; then
        printf '缺少上游 renderer 源码: %s\n' "$renderers/$name" >&2
        exit 2
    fi
done
printf '编译原始 Moonlight SDL renderer 行为测试: %s\n' "$output" >&2
clang++ -std=c++17 -g -Wall -Wextra -Werror \
    -fsanitize=address,undefined -fno-sanitize-recover=all \
    -I "$renderers" -I "$script_dir/audio-renderer-shim" \
    "$script_dir/audio-renderer-tests.cpp" "$renderers/sdlaud.cpp" -o "$output"
printf '原始 renderer 测试已就绪, 不链接 SDL 或 Qt 库: %s\n' "$output" >&2
