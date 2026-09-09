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
}
