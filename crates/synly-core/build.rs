//! 读取 `SYNLY_BUILD_VERSION` 供 Android uniffi 的 build_version 接口展示.
//! 未设置时为 `dev-build`. 发布路径由 `scripts/build-version.sh` 或 CI 注入.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SYNLY_BUILD_VERSION");
    let version = std::env::var("SYNLY_BUILD_VERSION")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "dev-build".to_string());
    println!("cargo:rustc-env=SYNLY_BUILD_VERSION={version}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=native/bluetooth.h");
        println!("cargo:rerun-if-changed=native/bluetooth.m");
        cc::Build::new()
            .file("native/bluetooth.m")
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .flag("-mmacosx-version-min=14.0")
            .flag("-Wall")
            .flag("-Wextra")
            .flag("-Werror")
            .compile("synly_bluetooth");
        for framework in ["Foundation", "CoreFoundation", "CoreBluetooth", "IOBluetooth"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}
