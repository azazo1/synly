#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf '用法: %s DESTINATION\n' "$0" >&2
    exit 2
fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_dir="$root/licenses/audio"
destination="$1"
files=(README.md sunshine-GPL-3.0.txt moonlight-common-GPL-3.0.txt moonlight-qt-GPL-3.0.txt)

# 验证完成前不创建目标, 缺许可时不允许静默打出不完整产物.
for name in "${files[@]}"; do
    if [[ ! -f "$source_dir/$name" || ! -s "$source_dir/$name" || -L "$source_dir/$name" ]]; then
        printf '[package] 缺少有效的音频许可文件: %s\n' "$source_dir/$name" >&2
        exit 1
    fi
done
if [[ -e "$destination" || -L "$destination" ]]; then
    printf '[package] 音频许可目标必须为新目录: %s\n' "$destination" >&2
    exit 1
fi
printf '[package] 随附音频来源说明和 GPL 全文\n'
mkdir -p "$(dirname "$destination")"
mkdir "$destination"
for name in "${files[@]}"; do
    cp "$source_dir/$name" "$destination/$name"
    cmp "$source_dir/$name" "$destination/$name"
done
