#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo-ndk >/dev/null 2>&1; then
    echo "缺少 cargo-ndk, 请先运行 cargo install cargo-ndk --locked" >&2
    exit 1
fi
if ! command -v uniffi-bindgen >/dev/null 2>&1; then
    echo "缺少 uniffi-bindgen, 请先运行 cargo install uniffi --version 0.31.2 --features cli --locked" >&2
    exit 1
fi

if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    for root in "${ANDROID_HOME:-}/ndk" "$HOME/Android/Sdk/ndk" "$HOME/Library/Android/sdk/ndk"; do
        if [[ -d "$root" ]]; then
            version="$(ls "$root" | sort -V | tail -n 1)"
            if [[ -n "$version" ]]; then
                export ANDROID_NDK_HOME="$root/$version"
                break
            fi
        fi
    done
fi
if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    echo "ANDROID_NDK_HOME 未设置, 且未在默认位置找到 Android SDK NDK" >&2
    exit 1
fi

echo "ANDROID_NDK_HOME=$ANDROID_NDK_HOME"
rustup target add aarch64-linux-android
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build --release --features uniffi -p synly-core
uniffi-bindgen generate --library android/app/src/main/jniLibs/arm64-v8a/libsynly_core.so --language kotlin --out-dir android/app/src/main/java --no-format

