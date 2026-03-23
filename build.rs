use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpusLinkPreference {
    PreferStatic,
    PreferDynamic,
}

#[derive(Debug)]
struct ResolvedOpusLib {
    dir: PathBuf,
    lib_name: String,
    link_statically: bool,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for key in [
        "OPUS_DIR",
        "OPUS_LIB_DIR",
        "OPUS_LIB_NAME",
        "OPUS_STATIC",
        "VCPKG_ROOT",
        "VCPKG_INSTALLATION_ROOT",
        "VCPKG_DEFAULT_TRIPLET",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    let target = env::var("TARGET").unwrap_or_default();
    link_opus(&target, opus_link_preference());
}

fn opus_link_preference() -> OpusLinkPreference {
    match env::var("OPUS_STATIC") {
        Ok(value) if is_disabled_env_value(&value) => OpusLinkPreference::PreferDynamic,
        Ok(_) | Err(_) => OpusLinkPreference::PreferStatic,
    }
}

fn is_disabled_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn link_opus(target: &str, preference: OpusLinkPreference) {
    if let Some(resolved) = manual_opus_link(target, preference) {
        emit_opus_link(&resolved);
        return;
    }

    if target.contains("windows") {
        link_opus_windows(target, preference);
    } else {
        link_opus_pkg_config(preference);
    }
}

fn emit_opus_link(resolved: &ResolvedOpusLib) {
    println!("cargo:rustc-link-search=native={}", resolved.dir.display());
    emit_opus_link_name(&resolved.lib_name, resolved.link_statically);
}

fn emit_opus_link_name(lib_name: &str, link_statically: bool) {
    if link_statically {
        println!("cargo:rustc-link-lib=static={lib_name}");
    } else {
        println!("cargo:rustc-link-lib={lib_name}");
    }
}

fn manual_opus_link(target: &str, preference: OpusLinkPreference) -> Option<ResolvedOpusLib> {
    let lib_name = env::var("OPUS_LIB_NAME").unwrap_or_else(|_| "opus".to_string());

    if let Some(dir) = env::var_os("OPUS_LIB_DIR") {
        let candidate = PathBuf::from(dir);
        if candidate.is_dir() {
            return Some(resolve_manual_opus_dir(
                candidate,
                lib_name,
                target,
                preference,
            ));
        }

        println!(
            "cargo:warning=synly ignored OPUS_LIB_DIR because it does not exist: {}",
            candidate.display()
        );
    }

    if let Some(opus_dir) = env::var_os("OPUS_DIR") {
        let base = PathBuf::from(opus_dir);
        for candidate in [base.join("lib"), base.join("lib64"), base.clone()] {
            if candidate.is_dir() {
                return Some(resolve_manual_opus_dir(
                    candidate,
                    lib_name.clone(),
                    target,
                    preference,
                ));
            }
        }

        println!(
            "cargo:warning=synly ignored OPUS_DIR because it does not contain a usable library directory: {}",
            base.display()
        );
    }

    None
}

fn resolve_manual_opus_dir(
    dir: PathBuf,
    lib_name: String,
    target: &str,
    preference: OpusLinkPreference,
) -> ResolvedOpusLib {
    let link_statically = matches!(preference, OpusLinkPreference::PreferStatic)
        && manual_static_link_available(&dir, &lib_name, target);

    if matches!(preference, OpusLinkPreference::PreferStatic) && !link_statically {
        println!(
            "cargo:warning=synly found Opus in {} but could not confirm a static library; falling back to the default linker mode",
            dir.display()
        );
    }

    ResolvedOpusLib {
        dir,
        lib_name,
        link_statically,
    }
}

fn manual_static_link_available(dir: &Path, lib_name: &str, target: &str) -> bool {
    if target.contains("windows") {
        return windows_dir_looks_static(dir)
            || candidate_windows_static_lib_names(lib_name)
                .into_iter()
                .any(|name| dir.join(format!("{name}.lib")).is_file());
    }

    [format!("lib{lib_name}.a"), format!("{lib_name}.a")]
        .into_iter()
        .any(|name| dir.join(name).is_file())
}

fn windows_dir_looks_static(dir: &Path) -> bool {
    dir.to_string_lossy()
        .replace('\\', "/")
        .contains("windows-static")
}

fn candidate_windows_static_lib_names(default_lib_name: &str) -> Vec<String> {
    let mut names = vec![
        format!("{default_lib_name}_static"),
        format!("lib{default_lib_name}_static"),
    ];
    names.dedup();
    names
}

fn link_opus_windows(target: &str, preference: OpusLinkPreference) {
    let default_lib_name = env::var("OPUS_LIB_NAME").unwrap_or_else(|_| "opus".to_string());
    if let Some(resolved) = find_vcpkg_opus(target, &default_lib_name, preference) {
        emit_opus_link(&resolved);
        return;
    }

    emit_opus_link_name(&default_lib_name, false);
    println!(
        "cargo:warning=synly could not locate an Opus library automatically; set OPUS_LIB_DIR/OPUS_LIB_NAME or VCPKG_ROOT if link fails"
    );
}

fn find_vcpkg_opus(
    target: &str,
    default_lib_name: &str,
    preference: OpusLinkPreference,
) -> Option<ResolvedOpusLib> {
    let root = env::var_os("VCPKG_ROOT")
        .or_else(|| env::var_os("VCPKG_INSTALLATION_ROOT"))
        .map(PathBuf::from)?;

    let target_features = env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let mut triplets =
        candidate_vcpkg_triplets(target, target_features.contains("crt-static"), preference);
    if let Ok(explicit) = env::var("VCPKG_DEFAULT_TRIPLET") {
        triplets.insert(0, explicit);
    }
    triplets.dedup();

    for triplet in triplets {
        let installed = root.join("installed").join(&triplet);
        let link_statically = triplet.contains("-static");
        let mut search_dirs = vec![installed.join("lib")];

        if !target.contains("windows-msvc") {
            search_dirs.push(installed.join("debug").join("lib"));
        }

        for dir in search_dirs {
            if !dir.is_dir() {
                continue;
            }

            for lib_name in candidate_windows_lib_names(default_lib_name, link_statically) {
                let file = dir.join(format!("{lib_name}.lib"));
                if file.is_file() {
                    return Some(ResolvedOpusLib {
                        dir,
                        lib_name,
                        link_statically,
                    });
                }
            }
        }
    }

    None
}

fn candidate_vcpkg_triplets(
    target: &str,
    crt_static: bool,
    preference: OpusLinkPreference,
) -> Vec<String> {
    let default_dynamic = default_vcpkg_triplet(target, false);
    let default_static = default_vcpkg_triplet(target, true);
    let mut triplets = Vec::new();

    match preference {
        OpusLinkPreference::PreferStatic => {
            triplets.push(default_static);
            triplets.push(default_dynamic);
            triplets.push("x64-windows-static".to_string());
            triplets.push("x64-windows".to_string());
        }
        OpusLinkPreference::PreferDynamic => {
            triplets.push(default_dynamic);
            triplets.push(default_static);
            triplets.push("x64-windows".to_string());
            triplets.push("x64-windows-static".to_string());
        }
    }

    if crt_static {
        triplets.insert(0, default_vcpkg_triplet(target, true));
    }

    triplets.dedup();
    triplets
}

fn candidate_windows_lib_names(default_lib_name: &str, prefer_static_names: bool) -> Vec<String> {
    let base_names = vec![
        default_lib_name.to_string(),
        format!("lib{default_lib_name}"),
        format!("{default_lib_name}d"),
        format!("lib{default_lib_name}d"),
    ];
    let static_names = candidate_windows_static_lib_names(default_lib_name);
    let mut names = Vec::new();

    if prefer_static_names {
        names.extend(static_names);
        names.extend(base_names);
    } else {
        names.extend(base_names);
        names.extend(static_names);
    }

    names.dedup();
    names
}

fn default_vcpkg_triplet(target: &str, crt_static: bool) -> String {
    let arch = if target.starts_with("x86_64") {
        "x64"
    } else if target.starts_with("aarch64") {
        "arm64"
    } else if target.starts_with("i686") || target.starts_with("i586") {
        "x86"
    } else {
        "x64"
    };

    if crt_static {
        format!("{arch}-windows-static")
    } else {
        format!("{arch}-windows")
    }
}

fn link_opus_pkg_config(preference: OpusLinkPreference) {
    if matches!(preference, OpusLinkPreference::PreferStatic) && try_link_opus_pkg_config(true) {
        return;
    }

    if try_link_opus_pkg_config(false) {
        if matches!(preference, OpusLinkPreference::PreferStatic) {
            println!(
                "cargo:warning=synly could not obtain static pkg-config flags for opus; falling back to the default linker mode"
            );
        }
        return;
    }

    panic!("pkg-config failed for opus");
}

fn try_link_opus_pkg_config(link_opus_statically: bool) -> bool {
    let mut command = Command::new("pkg-config");
    if link_opus_statically {
        command.arg("--static");
    }

    let output = match command.args(["--libs", "--cflags", "opus"]).output() {
        Ok(output) => output,
        Err(err) => {
            if link_opus_statically {
                println!(
                    "cargo:warning=synly failed to invoke pkg-config --static for opus ({err}); retrying without --static"
                );
                return false;
            }
            panic!("failed to invoke pkg-config for opus: {err}");
        }
    };

    if !output.status.success() {
        return false;
    }

    let flags = String::from_utf8(output.stdout).expect("pkg-config output was not valid UTF-8");
    parse_pkg_config_flags(&flags, link_opus_statically);
    true
}

fn parse_pkg_config_flags(flags: &str, link_opus_statically: bool) {
    for token in flags.split_whitespace() {
        if let Some(path) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else if let Some(lib) = token.strip_prefix("-l") {
            emit_opus_link_name(lib, link_opus_statically && is_opus_lib_name(lib));
        }
    }
}

fn is_opus_lib_name(lib_name: &str) -> bool {
    matches!(
        lib_name,
        "opus" | "libopus" | "opus_static" | "libopus_static"
    )
}
