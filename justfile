alias r := run

run *args:
    cargo run -- {{ args }}

install:
    cargo install --path .

# just input-macos-mock --edge right
# 使用 GUI mock 验证 macOS 输入捕获和虚拟屏幕切换.
input-macos-mock *args:
    cargo run --features input-macos-mock --bin input-macos-mock -- {{ args }}

down:
    docker compose down

up:
    docker compose up -d

cross-build:
    cargo zigbuild --target {{ arch() }}-unknown-linux-musl --release
    cd target && ln -sf {{ arch() }}-unknown-linux-musl/release/synly synly-cross

vhs-join:
    sleep 0.5s
    PATH={{ join(justfile_directory(), "tapes", "join") }}:$PATH vhs tapes/join/join.tape

vhs-host:
    PATH={{ join(justfile_directory(), "tapes", "host") }}:$PATH vhs tapes/host/host.tape

[parallel]
vhs: vhs-join vhs-host

alias rec := record

record: cross-build down up vhs
