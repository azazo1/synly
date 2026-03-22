alias r := run

run *args:
    cargo run -- {{ args }}

up:
    docker compose up -d

cross-build:
    cargo zigbuild --target {{ arch() }}-unknown-linux-musl --release
    cd target && ln -sf {{ arch() }}-unknown-linux-musl/release/synly synly-cross

vhs-join:
    PATH={{ join(justfile_directory(), "tapes", "join") }}:$PATH vhs tapes/join/join.tape

vhs-host:
    PATH={{ join(justfile_directory(), "tapes", "host") }}:$PATH vhs tapes/host/host.tape

[parallel]
vhs: vhs-join vhs-host

record: up cross-build vhs
