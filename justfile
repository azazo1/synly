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
debug:
    powershell -NoProfile -ExecutionPolicy Bypass -Command "New-Item -ItemType Directory -Force -Path target/synly-debug | Out-Null; $env:SYNLY_DATA_DIR='target/synly-debug'; $env:SYNLY_LOG_FILE='target/synly-debug/synly.log'; $env:RUST_LOG='synly=trace'; cargo run"

# 打印当前应嵌入二进制的构建版本.
[unix]
build-version:
    bash scripts/build-version.sh

[windows]
build-version:
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-version.ps1

# 构建当前平台的可分发 release 产物.
[windows]
dist:
    powershell -NoProfile -ExecutionPolicy Bypass -Command "$env:SYNLY_BUILD_VERSION = (powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-version.ps1).Trim(); cargo build --release; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }"
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/package-windows.ps1 -Binary target/release/synly.exe -OutputDir dist

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
fake-dist:
    powershell -NoProfile -ExecutionPolicy Bypass -Command "$env:SYNLY_BUILD_VERSION='v0.0.0'; $env:SYNLY_FAKE_DIST='1'; cargo build --release; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }"
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/package-windows.ps1 -Binary target/release/synly.exe -OutputDir dist -Version v0.0.0 -Fake

[macos]
fake-dist:
    SYNLY_BUILD_VERSION=v0.0.0 SYNLY_FAKE_DIST=1 cargo build --release
    SYNLY_FAKE_DIST=1 bash scripts/package-macos.sh v0.0.0 "$(rustc -vV | sed -n 's/^host: //p')" dist

[linux]
fake-dist:
    SYNLY_BUILD_VERSION=v0.0.0 SYNLY_FAKE_DIST=1 cargo build --release
    SYNLY_FAKE_DIST=1 bash scripts/package-linux.sh v0.0.0 "$(rustc -vV | sed -n 's/^host: //p')" dist

# 安装当前工作树中的 Synly.
install:
    cargo install --path .

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
