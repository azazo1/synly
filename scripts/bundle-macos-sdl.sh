#!/usr/bin/env bash
set -euo pipefail

# 只处理包内副本. 许可路径必须与调用者选择的构建依赖对应.
if [[ $# -ne 1 && $# -ne 2 && $# -ne 4 ]]; then
    printf '用法: %s APP_BUNDLE [SDL2_LICENSE [SDL3_LIBRARY SDL3_LICENSE]]\n' "$0" >&2
    exit 2
fi
app="$1"
binary="$app/Contents/MacOS/synly"
frameworks="$app/Contents/Frameworks"
notices="$app/Contents/Resources/audio-licenses"
fail() { printf '%s\n' "$*" >&2; exit 1; }
[[ -f "$binary" && ! -L "$binary" ]] || fail '缺少包内可执行文件或它是符号链接'
listing="$(otool -L "$binary")"
sdl2=''
while IFS= read -r line; do
    case "$line" in
        *libSDL2*.dylib*|*SDL2.framework/*)
            [[ -z "$sdl2" ]] || fail '发现多个 SDL2 依赖'
            sdl2="${line#"${line%%[![:space:]]*}"}"
            sdl2="${sdl2%% \(*}"
            ;;
    esac
done <<< "$listing"
if [[ -z "$sdl2" ]]; then
    printf '[audio] 原生播放构建, 无需携带 SDL2\n'
    exit 0
fi
case "$sdl2" in /*.dylib) ;; *) fail "暂不支持此 SDL2 输入链接路径: $sdl2" ;; esac
[[ $# -ge 2 && -s "$2" && -f "$sdl2" ]] || fail 'SDL2 库或对应许可文件缺失'
[[ ! -e "$frameworks/libSDL2.dylib" && ! -L "$frameworks/libSDL2.dylib" ]] || fail '目标 SDL2 库已存在, 请重新组装应用包'
compat=0
symbols="$(strings "$sdl2")"
case "$symbols" in *sdl2-compat:*) compat=1 ;; esac
if [[ "$compat" == 1 ]]; then
    [[ $# -eq 4 && -f "$3" && -s "$4" ]] || fail 'sdl2-compat 还需要 SDL3_LIBRARY 和 SDL3_LICENSE'
    case "$symbols" in *'@loader_path/libSDL3.dylib'*) ;; *) fail '无法确认兼容库支持同目录加载 SDL3' ;; esac
    [[ ! -e "$frameworks/libSDL3.dylib" && ! -L "$frameworks/libSDL3.dylib" ]] || fail '目标 SDL3 库已存在'
fi
architectures="$(lipo -archs "$binary")"
# 不递归复制未知第三方依赖. 首个依赖行是 dylib 自身的 install name.
validate_library() {
    local library="$1" deps line dependency first=1 architecture
    for architecture in $architectures; do
        lipo "$library" -verify_arch "$architecture"
    done
    deps="$(otool -L "$library")"
    while IFS= read -r line; do
        case "$line" in *' (compatibility version '*) ;; *) continue ;; esac
        if [[ "$first" == 1 ]]; then first=0; continue; fi
        dependency="${line#"${line%%[![:space:]]*}"}"
        dependency="${dependency%% \(*}"
        case "$dependency" in
            /System/Library/*|/usr/lib/*) ;;
            *) fail "SDL 库还有未处理的非系统依赖: $dependency" ;;
        esac
    done <<< "$deps"
}
printf '[audio] 检查 SDL 架构及传递依赖\n'
validate_library "$sdl2"
if [[ "$compat" == 1 ]]; then validate_library "$3"; fi
mkdir -p "$frameworks" "$notices"
printf '[audio] 复制 SDL2 和对应许可\n'
cp -L "$sdl2" "$frameworks/libSDL2.dylib"
chmod u+w "$frameworks/libSDL2.dylib"
cp "$2" "$notices/SDL2-LICENSE.txt"
install_name_tool -id '@loader_path/libSDL2.dylib' "$frameworks/libSDL2.dylib"
if [[ "$compat" == 1 ]]; then
    printf '[audio] 复制兼容层的运行时 SDL3 依赖和许可\n'
    cp -L "$3" "$frameworks/libSDL3.dylib"
    chmod u+w "$frameworks/libSDL3.dylib"
    cp "$4" "$notices/SDL3-LICENSE.txt"
    install_name_tool -id '@loader_path/libSDL3.dylib' "$frameworks/libSDL3.dylib"
    codesign --force --sign - "$frameworks/libSDL3.dylib"
    codesign --verify --strict "$frameworks/libSDL3.dylib"
fi
install_name_tool -change "$sdl2" '@executable_path/../Frameworks/libSDL2.dylib' "$binary"
codesign --force --sign - "$frameworks/libSDL2.dylib"
codesign --force --sign - "$binary"
codesign --verify --strict "$frameworks/libSDL2.dylib"
codesign --verify --strict "$binary"
printf '[audio] 包内 SDL 库复制和临时签名完成; 发布签名及公证仍由发布流程执行\n'
