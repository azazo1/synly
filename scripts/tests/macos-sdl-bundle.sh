#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "binary:" "    ${TEST_DEPENDENCY} (compatibility version 1.0.0)"\n' > "$work/otool"
chmod +x "$work/otool"
export PATH="$work:$PATH"
app="$work/Synly.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/audio-licenses"
touch "$app/Contents/MacOS/synly"

printf '[1/2] 原生构建不复制 SDL\n'
export TEST_DEPENDENCY='/System/Library/Frameworks/CoreAudio.framework/CoreAudio'
bash "$root/scripts/bundle-macos-sdl.sh" "$app"
[[ ! -e "$app/Contents/Frameworks/libSDL2.dylib" ]]

printf '[2/2] 链接了 SDL2 但库文件缺失必须失败\n'
export TEST_DEPENDENCY="$work/missing-libSDL2-2.0.0.dylib"
if bash "$root/scripts/bundle-macos-sdl.sh" "$app"; then
    printf '错误: 缺失 SDL2 库时随包成功\n' >&2
    exit 1
fi
printf 'macOS SDL 随包脚本测试通过\n'
