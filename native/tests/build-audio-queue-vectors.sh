#!/usr/bin/env bash
set -euo pipefail

# 只编译显式传入的上游源码, 不下载, 不改写任何上游文件.
if [[ $# -ne 3 ]]; then
    printf '用法: bash %s <moonlight-common-c-dir> <nanors-dir> <output-executable>\n' "$0" >&2
    exit 2
fi
moonlight=$1
nanors=$2
output=$3
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
for source in "$moonlight/src/RtpAudioQueue.c" "$nanors/rs.c" "$nanors/deps/obl/oblas_lite.c" "$nanors/deps/obl/oblas_common.c"; do
    if [[ ! -f "$source" ]]; then
        printf '缺少上游源码: %s\n' "$source" >&2
        exit 2
    fi
done
printf '编译独立 Moonlight 队列轨迹生成器: %s\n' "$output" >&2
clang -std=c11 -g -fsanitize=address,undefined -fno-sanitize-recover=all -DLC_DEBUG -DLC_FUZZING \
    -I "$moonlight/src" -I "$script_dir/audio-queue-shim" -I "$nanors" -I "$nanors/deps/obl" \
    "$script_dir/audio-queue-vectors.c" "$moonlight/src/RtpAudioQueue.c" \
    "$nanors/rs.c" "$nanors/deps/obl/oblas_lite.c" "$nanors/deps/obl/oblas_common.c" -o "$output"
printf '生成器已就绪: %s\n' "$output" >&2
