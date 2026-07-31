$ErrorActionPreference = 'Stop'

if (-not (Get-Command cargo-ndk -ErrorAction SilentlyContinue)) {
    throw '缺少 cargo-ndk, 请先运行 cargo install cargo-ndk --locked'
}
if (-not (Get-Command uniffi-bindgen -ErrorAction SilentlyContinue)) {
    throw '缺少 uniffi-bindgen, 请先运行 cargo install uniffi --version 0.31.2 --features cli --locked'
}

if (-not $env:ANDROID_NDK_HOME) {
    $ndkRoot = Join-Path $env:LOCALAPPDATA 'Android\Sdk\ndk'
    if (Test-Path $ndkRoot) {
        $ndk = Get-ChildItem $ndkRoot -Directory |
            Sort-Object Name -Descending |
            Select-Object -First 1
        if ($ndk) {
            $env:ANDROID_NDK_HOME = $ndk.FullName
        }
    }
}
if (-not $env:ANDROID_NDK_HOME) {
    throw 'ANDROID_NDK_HOME 未设置, 且未在默认位置找到 Android SDK NDK'
}
if (-not $env:ANDROID_HOME) {
    $sdkRoot = Join-Path $env:LOCALAPPDATA 'Android\Sdk'
    if (Test-Path $sdkRoot) {
        $env:ANDROID_HOME = $sdkRoot
    }
}

Write-Host "ANDROID_NDK_HOME=$env:ANDROID_NDK_HOME"
rustup target add aarch64-linux-android
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build --release --features uniffi -p synly-core
uniffi-bindgen generate --library android/app/src/main/jniLibs/arm64-v8a/libsynly_core.so --language kotlin --out-dir android/app/src/main/java --no-format

