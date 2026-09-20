#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$root/.tmp"
work="$(mktemp -d "$root/.tmp/audio-notices-test.XXXXXX")"
files=(README.md sunshine-GPL-3.0.txt moonlight-common-GPL-3.0.txt moonlight-qt-GPL-3.0.txt)
printf '[1/3] 检查 macOS 资源布局与许可逐字复制\n'
bash "$root/scripts/package-audio-notices.sh" "$work/Synly.app/Contents/Resources/audio-licenses"
for name in "${files[@]}"; do
    cmp "$root/licenses/audio/$name" "$work/Synly.app/Contents/Resources/audio-licenses/$name"
done
# 对固定上游全文进行字节哈希断言, 不基于说明文案测试.
expected_sunshine=3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986
expected_moonlight=589ed823e9a84c56feb95ac58e7cf384626b9cbf4fda2a907bc36e103de1bad2
hash_file() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}
[[ "$(hash_file "$root/licenses/audio/sunshine-GPL-3.0.txt")" == "$expected_sunshine "* ]]
for name in moonlight-common-GPL-3.0.txt moonlight-qt-GPL-3.0.txt; do
    [[ "$(hash_file "$root/licenses/audio/$name")" == "$expected_moonlight "* ]]
done

printf '[2/3] 缺失, 空文件, 符号链接与旧目标必须失败\n'
mkdir -p "$work/repo/scripts" "$work/repo/licenses/audio"
cp "$root/scripts/package-audio-notices.sh" "$work/repo/scripts/"
for name in "${files[@]}"; do cp "$root/licenses/audio/$name" "$work/repo/licenses/audio/"; done
expect_failure() {
    if bash "$@"; then printf '预期失败但操作成功\n' >&2; exit 1; fi
}
expect_failure "$root/scripts/package-audio-notices.sh" "$work/Synly.app/Contents/Resources/audio-licenses"
for name in "${files[@]}"; do
    rm "$work/repo/licenses/audio/$name"
    expect_failure "$work/repo/scripts/package-audio-notices.sh" "$work/missing-$name"
    [[ ! -e "$work/missing-$name" ]]
    : > "$work/repo/licenses/audio/$name"
    expect_failure "$work/repo/scripts/package-audio-notices.sh" "$work/empty-$name"
    [[ ! -e "$work/empty-$name" ]]
    rm "$work/repo/licenses/audio/$name"
    ln -s "$root/licenses/audio/$name" "$work/repo/licenses/audio/$name"
    expect_failure "$work/repo/scripts/package-audio-notices.sh" "$work/link-$name"
    [[ ! -e "$work/link-$name" ]]
    rm "$work/repo/licenses/audio/$name"
    cp "$root/licenses/audio/$name" "$work/repo/licenses/audio/$name"
done

printf '[3/3] 执行 Linux 安装包脚本并解包校验, 不执行占位二进制\n'
cp "$root/scripts/package-linux.sh" "$root/scripts/installer-linux.sh" "$work/repo/scripts/"
mkdir -p "$work/repo/target/release" "$work/repo/assets/linux"
# 只用于 file 格式识别的 ELF 头, 不编译或运行 Linux 程序.
printf '\177ELF\002\001\001\000\000\000\000\000\000\000\000\000\002\000\076\000' > "$work/repo/target/release/synly"
chmod 755 "$work/repo/target/release/synly"
cp "$root/assets/linux/synly-256.png" "$root/assets/linux/synly-512.png" "$work/repo/assets/linux/"
(
    cd "$work/repo"
    bash scripts/package-linux.sh test-only x86_64-unknown-linux-gnu 'dist with spaces'
)
archive="$work/repo/dist with spaces/synly-test-only-linux-x86_64-setup.tar.gz"
mkdir "$work/unpacked"
tar -xzf "$archive" -C "$work/unpacked"
[[ -x "$work/unpacked/install.sh" ]]
cmp "$work/repo/target/release/synly" "$work/unpacked/payload/synly"
for name in "${files[@]}"; do cmp "$root/licenses/audio/$name" "$work/unpacked/payload/audio-licenses/$name"; done
# 暂存目录应全部回收, 只留下最终归档.
shopt -s nullglob
leftovers=("$work/repo/dist with spaces"/audio-notices.* "$work/repo/dist with spaces"/linux-stage.*)
[[ ${#leftovers[@]} -eq 0 ]]
printf '音频许可随附测试通过, 测试产物保留于 %s\n' "$work"
