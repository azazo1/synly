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

# 构建当前平台的可分发 release 产物.
[windows]
dist: build
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/package-windows.ps1 -Binary target/release/synly.exe -OutputDir dist

[macos]
dist: build
    bash scripts/package-macos.sh

[linux]
dist: build
    mkdir -p dist
    cp target/release/synly dist/synly

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
