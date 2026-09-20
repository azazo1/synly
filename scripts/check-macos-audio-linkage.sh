#!/usr/bin/env bash
set -euo pipefail

# 在最终打包前验证音频动态库没有依赖开发机的安装路径.
if [[ $# -ne 1 || ! -f "$1" ]]; then
    printf '用法: %s MACH_O_EXECUTABLE\n' "$0" >&2
    exit 2
fi
printf '[audio] 检查 SDL2 动态依赖\n'
# 不使用管道子 shell, 必须把检测到的失败传回调用方.
listing="$(otool -L "$1")"
failed=0
while IFS= read -r line; do
    # otool 首行为文件名, 依赖行以空白开头. SDL 名称不包含空格.
    case "$line" in
        *libSDL2*.dylib*|*SDL2.framework/SDL2*)
            dependency="${line#"${line%%[![:space:]]*}"}"
            dependency="${dependency%% \(*}"
            printf 'SDL2 依赖尚未随包验证: %s\n' "$dependency" >&2
            failed=1
            ;;
    esac
done <<< "$listing"
if [[ "$failed" -ne 0 ]]; then
    printf '拒绝生成不完整的应用包. 请先实现 SDL2 库随包复制, install name 修正, 许可和签名验证; 不要要求普通用户安装开发库.\n' >&2
    exit 1
fi
printf '[audio] 未发现未支持的 SDL2 动态依赖\n'
