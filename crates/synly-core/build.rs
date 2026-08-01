//! 为 synly-core 生成与桌面端一致的构建版本号.
//!
//! Android 端通过 uniffi 的 build_version 接口读取该值并显示在界面中.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=SYNLY_BUILD_VERSION={}",
        synly_build_version::build_version_string()
    );
    synly_build_version::emit_git_rerun_if_changed();
}
