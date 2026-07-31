[private]
default:
    @just --list

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

# just input-screen-mock --edge right
# 使用 Slint 虚拟屏幕验证当前平台的输入捕获和返回.
input-screen-mock *args:
    cargo run --features input-screen-mock --bin input-screen-mock -- {{ args }}

# 完全捕获 macOS trackpad 事件并输出诊断日志, 按任意键退出.
input-macos-trackpad-debug:
    cargo run --features input-macos-trackpad-debug --bin input-macos-trackpad-debug

# just input-receiver-mock receive --listen 0.0.0.0:59679
# 使用真实被控端和 mock 控制端验证系统输入注入.
input-receiver-mock *args:
    cargo run --features input-receiver-mock --bin input-receiver-mock -- {{ args }}

# 构建 Android 核心动态库并生成 Kotlin 绑定, 产物进入 android/app/src/main.
android-core:
    rustup target add aarch64-linux-android
    cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build --release --features uniffi -p synly-core
    uniffi-bindgen generate --library android/app/src/main/jniLibs/arm64-v8a/libsynly_core.so --language kotlin --out-dir android/app/src/main/java --no-format

# 构建 Android debug APK, 自动先构建核心库.
[windows]
android-build: android-core
    cd android && gradlew.bat assembleDebug

[linux]
android-build: android-core
    cd android && ./gradlew assembleDebug

[macos]
android-build: android-core
    cd android && ./gradlew assembleDebug
