#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -f "$work/otool" "$work/binary"; rmdir "$work"' EXIT
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "binary:" "    ${TEST_DEPENDENCY} (compatibility version 1.0.0)"\n' > "$work/otool"
chmod +x "$work/otool"
touch "$work/binary"
export PATH="$work:$PATH"
export TEST_DEPENDENCY='/System/Library/Frameworks/CoreAudio.framework/CoreAudio'
printf '[1/2] 系统依赖允许打包\n'
bash "$root/scripts/check-macos-audio-linkage.sh" "$work/binary"
printf '[2/2] 未处理的 SDL2 依赖必须阻止打包\n'
for dependency in /opt/homebrew/lib/libSDL2-2.0.0.dylib '@rpath/libSDL2.dylib' '@executable_path/../Frameworks/SDL2.framework/SDL2'; do
    export TEST_DEPENDENCY="$dependency"
    if bash "$root/scripts/check-macos-audio-linkage.sh" "$work/binary"; then
        printf '错误: 未处理的 SDL2 依赖被放行\n' >&2
        exit 1
    fi
done
printf 'macOS 音频链接打包门禁测试通过\n'
