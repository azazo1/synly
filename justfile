[private]
default:
    @just --list

import? 'local.just'

# 启动 Slint GUI.
run:
    cargo run --

# just headless host
# 以无界面模式启动会话, 子命令可选 host/join, 例如 just headless connect demo-device.
headless *args:
    cargo run -- --headless {{ args }}

# 运行全部测试.
test:
    cargo test

# 运行音频编解码, 队列及 UDP 流水线测试.
audio-test:
    cargo test --offline --bin synly audio::
    cargo test --offline -p synly-core protocol::tests::audio_offer

# 验证音频许可全文, 失败拒绝行为与归档随附, 不运行应用.
[unix]
audio-notices-test:
    bash scripts/tests/audio-notices.sh

# 验证音频许可全文, 失败拒绝行为与 Windows ZIP 随附, 不运行应用.
[windows]
audio-notices-test:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/tests/audio-notices.ps1

# just audio-fec-vectors /path/to/nanors
# 编译上游独立 FEC 生成器并验证全部单片和双片丢失组合.
[unix]
audio-fec-vectors upstream:
    mkdir -p target/audio-tests
    bash native/tests/build-audio-fec-vectors.sh '{{upstream}}' target/audio-tests/audio-fec-vectors
    target/audio-tests/audio-fec-vectors

# just audio-queue-vectors /path/to/moonlight-common-c /path/to/nanors
# 编译原始 Moonlight 队列, 重现并逐字节比较固定接收轨迹.
[unix]
audio-queue-vectors moonlight nanors:
    mkdir -p target/audio-tests
    bash native/tests/build-audio-queue-vectors.sh '{{moonlight}}' '{{nanors}}' target/audio-tests/audio-queue-vectors
    target/audio-tests/audio-queue-vectors > target/audio-tests/audio-queue-vectors.tsv
    cmp native/tests/audio-queue-vectors.tsv target/audio-tests/audio-queue-vectors.tsv
    cargo test --offline --bin synly audio::receiver::upstream_tests

# just audio-renderer-test /path/to/moonlight-qt
# 编译未经修改的上游 SDL renderer, 使用内存设备验证水位和失败路径.
[unix]
audio-renderer-test moonlight:
    mkdir -p target/audio-tests
    bash native/tests/build-audio-renderer-tests.sh '{{moonlight}}' target/audio-tests/audio-renderer-tests
    target/audio-tests/audio-renderer-tests

# 使用内存检查器验证 macOS 原生故障路径和真实重采样, 不打开音频设备.
[macos]
audio-native-test:
    mkdir -p target/audio-tests
    clang -fobjc-arc -fsanitize=address,undefined -g native/tests/macos-audio-tests.m -framework Foundation -framework AudioToolbox -framework CoreAudio -o target/audio-tests/macos-audio-tests
    target/audio-tests/macos-audio-tests
    clang -fsanitize=address,undefined -g native/tests/macos-conversion-tests.c -framework AudioToolbox -framework CoreAudio -o target/audio-tests/macos-conversion-tests
    target/audio-tests/macos-conversion-tests
    clang -std=c11 -fsanitize=address,undefined -fno-sanitize-recover=all -g native/tests/macos-capture-ring-tests.c -o target/audio-tests/macos-capture-ring-tests
    target/audio-tests/macos-capture-ring-tests
    clang -std=c11 -fsanitize=address,undefined -fno-sanitize-recover=all -g native/tests/macos-playback-ring-tests.c -o target/audio-tests/macos-playback-ring-tests
    target/audio-tests/macos-playback-ring-tests
    clang -fobjc-arc -fsanitize=address,undefined -fno-sanitize-recover=all -g native/tests/macos-audio-lifecycle-tests.m -framework Foundation -framework AudioToolbox -framework CoreAudio -o target/audio-tests/macos-audio-lifecycle-tests
    target/audio-tests/macos-audio-lifecycle-tests
    clang -std=c11 -fsanitize=address,undefined -fno-sanitize-recover=all -g native/tests/macos-capture-health-tests.c -o target/audio-tests/macos-capture-health-tests
    target/audio-tests/macos-capture-health-tests

# 使用 ThreadSanitizer 验证双向 SPSC 和创建/销毁串行边界, 不打开音频设备.
[macos]
audio-ring-race-test:
    mkdir -p target/audio-tests
    clang -std=c11 -fsanitize=thread -g native/tests/macos-capture-ring-tests.c -o target/audio-tests/macos-capture-ring-race-tests
    target/audio-tests/macos-capture-ring-race-tests
    clang -std=c11 -fsanitize=thread -g native/tests/macos-playback-ring-tests.c -o target/audio-tests/macos-playback-ring-race-tests
    target/audio-tests/macos-playback-ring-race-tests
    clang -fobjc-arc -fsanitize=thread -g native/tests/macos-audio-lifecycle-tests.m -framework Foundation -framework AudioToolbox -framework CoreAudio -o target/audio-tests/macos-audio-lifecycle-race-tests
    target/audio-tests/macos-audio-lifecycle-race-tests
    clang -std=c11 -fsanitize=thread -g native/tests/macos-capture-health-tests.c -o target/audio-tests/macos-capture-health-race-tests
    target/audio-tests/macos-capture-health-race-tests

# 运行全部 target 和 feature 的 clippy.
clippy:
    cargo clippy --all-targets --all-features

# 构建 release 产物.
build:
    cargo build --release

# 启动隔离数据目录的调试实例, 完整日志写入隔离目录.
[unix]
debug:
    mkdir -p target/synly-debug
    SYNLY_DATA_DIR=target/synly-debug SYNLY_LOG_FILE=target/synly-debug/synly.log RUST_LOG=synly=trace cargo run

# 启动隔离数据目录的调试实例, 完整日志写入隔离目录.
[windows]
[script('powershell.exe', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File')]
debug:
    $ErrorActionPreference = 'Stop'
    New-Item -ItemType Directory -Force -Path target/synly-debug | Out-Null
    $env:SYNLY_DATA_DIR = 'target/synly-debug'
    $env:SYNLY_LOG_FILE = 'target/synly-debug/synly.log'
    $env:RUST_LOG = 'synly=trace'
    cargo run
    exit $LASTEXITCODE

# 打印当前应嵌入二进制的构建版本.
[unix]
build-version:
    bash scripts/build-version.sh

[windows]
build-version:
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-version.ps1

# 构建当前平台的可分发 release 产物.
[windows]
[script('powershell.exe', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File')]
dist:
    $ErrorActionPreference = 'Stop'
    $version = (powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-version.ps1 | Select-Object -Last 1).Trim()
    if ([string]::IsNullOrWhiteSpace($version)) { throw 'Unable to resolve the build version' }
    $env:SYNLY_BUILD_VERSION = $version
    cargo build --release
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/package-windows.ps1 -Binary target/release/synly.exe -OutputDir dist
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

[macos]
dist:
    SYNLY_BUILD_VERSION="$(bash scripts/build-version.sh)" cargo build --release
    bash scripts/package-macos.sh

[linux]
dist:
    SYNLY_BUILD_VERSION="$(bash scripts/build-version.sh)" cargo build --release
    bash scripts/package-linux.sh

# 产出用于自动更新测试的 fake 构建, 版本固定为 v0.0.0.
[windows]
[script('powershell.exe', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File')]
fake-dist:
    $ErrorActionPreference = 'Stop'
    $env:SYNLY_BUILD_VERSION = 'v0.0.0'
    $env:SYNLY_FAKE_DIST = '1'
    cargo build --release
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/package-windows.ps1 -Binary target/release/synly.exe -OutputDir dist -Version v0.0.0 -Fake
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

[macos]
fake-dist:
    SYNLY_BUILD_VERSION=v0.0.0 SYNLY_FAKE_DIST=1 cargo build --release
    SYNLY_FAKE_DIST=1 bash scripts/package-macos.sh v0.0.0 "$(rustc -vV | sed -n 's/^host: //p')" dist

[linux]
fake-dist:
    SYNLY_BUILD_VERSION=v0.0.0 SYNLY_FAKE_DIST=1 cargo build --release
    SYNLY_FAKE_DIST=1 bash scripts/package-linux.sh v0.0.0 "$(rustc -vV | sed -n 's/^host: //p')" dist

# just gradlew testDebugUnitTest
# 自动发现 JDK 与 Android SDK 后运行指定 Gradle 任务, 参数原样透传.
[windows]
gradlew *args:
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/android-gradle.ps1 {{ args }}

[linux]
gradlew *args:
    bash scripts/android-gradle.sh {{ args }}

[macos]
gradlew *args:
    bash scripts/android-gradle.sh {{ args }}

# 使用 Slint 虚拟屏幕验证当前平台的输入捕获和返回.
input-screen-mock:
    RUST_LOG=synly=debug cargo run --features input-screen-mock --bin input-screen-mock

# 完全捕获 macOS trackpad 事件并输出诊断日志, 按任意键退出.
input-macos-trackpad-debug:
    cargo run --features input-macos-trackpad-debug --bin input-macos-trackpad-debug

# just input-receiver-mock receive --listen 0.0.0.0:59679
# 使用真实被控端和 mock 控制端验证系统输入注入.
input-receiver-mock *args:
    cargo run --features input-receiver-mock --bin input-receiver-mock -- {{ args }}

# 构建 Android 核心动态库并生成 Kotlin 绑定, 产物进入 android/app/src/main.
[windows]
android-core:
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/android-build-core.ps1

[linux]
android-core:
    bash scripts/android-build-core.sh

[macos]
android-core:
    bash scripts/android-build-core.sh

# just android-build
# just android-build release
# 构建 Android APK, 自动先构建核心库; 默认根据签名环境变量选择 debug/release, 也可显式指定.
[windows]
android-build mode='auto': android-core
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/android-build.ps1 {{ mode }}

[linux]
android-build mode='auto': android-core
    bash scripts/android-build.sh {{ mode }}

[macos]
android-build mode='auto': android-core
    bash scripts/android-build.sh {{ mode }}
