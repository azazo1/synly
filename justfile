[private]
default:
    @just --list

# 启动 Slint GUI.
run:
    cargo run --

# just headless --host --fs off --trusted-only
# 使用显式参数启动无界面模式.
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

# 安装当前工作树中的 Synly.
install:
    cargo install --path .

# just input-macos-mock --edge right
# 使用 GUI mock 验证 macOS 输入捕获和虚拟屏幕切换.
input-macos-mock *args:
    cargo run --features input-macos-mock --bin input-macos-mock -- {{ args }}

# 完全捕获 macOS trackpad 事件并输出诊断日志, 按任意键退出.
input-macos-trackpad-debug:
    cargo run --features input-macos-trackpad-debug --bin input-macos-trackpad-debug

# just input-receiver-mock receive --listen 0.0.0.0:59679
# 使用真实被控端和 mock 控制端验证系统输入注入.
input-receiver-mock *args:
    cargo run --features input-receiver-mock --bin input-receiver-mock -- {{ args }}
