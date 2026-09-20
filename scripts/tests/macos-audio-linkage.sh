#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "binary:" "    ${TEST_DEPENDENCY} (compatibility version 1.0.0)"\n' > "$work/otool"
chmod +x "$work/otool"
export PATH="$work:$PATH"

printf '[1/4] 系统依赖允许打包\n'
touch "$work/native"
export TEST_DEPENDENCY='/System/Library/Frameworks/CoreAudio.framework/CoreAudio'
bash "$root/scripts/check-macos-audio-linkage.sh" "$work/native"

printf '[2/4] 未处理的 SDL2 依赖必须阻止打包\n'
touch "$work/unbundled"
for dependency in /opt/homebrew/lib/libSDL2-2.0.0.dylib '@rpath/libSDL2.dylib' '@executable_path/../Frameworks/SDL2.framework/SDL2'; do
    export TEST_DEPENDENCY="$dependency"
    if bash "$root/scripts/check-macos-audio-linkage.sh" "$work/unbundled"; then
        printf '错误: 未处理的 SDL2 依赖被放行\n' >&2
        exit 1
    fi
done

printf '[3/4] 声明包内路径但缺少库文件必须失败\n'
mkdir -p "$work/app/Contents/MacOS" "$work/app/Contents/Frameworks"
touch "$work/app/Contents/MacOS/synly"
export TEST_DEPENDENCY='@executable_path/../Frameworks/libSDL2.dylib'
if bash "$root/scripts/check-macos-audio-linkage.sh" "$work/app/Contents/MacOS/synly"; then
    printf '错误: 缺少包内 SDL2 时被放行\n' >&2
    exit 1
fi

printf '[4/4] 包内 SDL2 路径且库文件存在时允许打包\n'
touch "$work/app/Contents/Frameworks/libSDL2.dylib"
bash "$root/scripts/check-macos-audio-linkage.sh" "$work/app/Contents/MacOS/synly"
printf 'macOS 音频链接打包门禁测试通过\n'
