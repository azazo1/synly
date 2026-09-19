#!/usr/bin/env bash
set -euo pipefail

# 显式传入 nanors 源码目录和输出可执行文件, 不下载或修改上游源码.
if [[ $# -ne 2 ]]; then
    printf '用法: bash %s <nanors-source-dir> <output-executable>\n' "$0" >&2
    exit 2
fi
upstream=$1
output=$2
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
for source in rs.c rs.h deps/obl/oblas_lite.c deps/obl/oblas_common.c; do
    if [[ ! -f "$upstream/$source" ]]; then
        printf '缺少上游源码: %s/%s\n' "$upstream" "$source" >&2
        exit 2
    fi
done
printf '编译独立音频 FEC 生成器: %s\n' "$output" >&2
clang -std=c11 -O2 -Wall -Wextra \
    -I "$upstream" -I "$upstream/deps/obl" \
    "$script_dir/audio-fec-vectors.c" \
    "$upstream/rs.c" "$upstream/deps/obl/oblas_lite.c" \
    "$upstream/deps/obl/oblas_common.c" -o "$output"
printf '生成器已就绪: %s\n' "$output" >&2
