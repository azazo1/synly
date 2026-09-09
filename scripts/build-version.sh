#!/usr/bin/env bash
# 计算应嵌入二进制的构建版本号, 打印到 stdout.
# 已设置 SYNLY_BUILD_VERSION 时原样输出; 否则按 git describe 生成.
# 精确 tag 输出该 tag; 非 tag 追加 - 和 7 位短 hash; 脏工作区改用 ^.
set -euo pipefail

if [[ -n "${SYNLY_BUILD_VERSION:-}" ]]; then
    printf '%s\n' "$SYNLY_BUILD_VERSION"
    exit 0
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
fallback="$(cargo pkgid -p synly 2>/dev/null | sed -n 's/.*@//p')"
fallback="${fallback:-unknown}"

if [[ -z "$repo_root" ]]; then
    printf '%s\n' "$fallback"
    exit 0
fi

describe="$(git -C "$repo_root" describe --tags --always --abbrev=7 2>/dev/null || true)"
head="$(git -C "$repo_root" rev-parse --short=7 HEAD 2>/dev/null || true)"
dirty=0
if [[ -n "$(git -C "$repo_root" status --porcelain 2>/dev/null || true)" ]]; then
    dirty=1
fi

if [[ -z "$describe" ]]; then
    printf '%s\n' "$fallback"
    exit 0
fi

if [[ "$dirty" -eq 1 ]]; then
    separator="^"
else
    separator="-"
fi

if [[ "$describe" =~ ^(.+)-([0-9]+)-g([0-9a-fA-F]{7,})$ ]]; then
    printf '%s%s%s\n' "${BASH_REMATCH[1]}" "$separator" "${BASH_REMATCH[3]}"
    exit 0
fi

if [[ "$describe" =~ ^[0-9a-fA-F]{7,}$ ]]; then
    printf '%s%s%s\n' "$fallback" "$separator" "${describe:0:7}"
    exit 0
fi

if [[ "$dirty" -eq 1 && -n "$head" ]]; then
    printf '%s^%s\n' "$describe" "$head"
    exit 0
fi

printf '%s\n' "$describe"
