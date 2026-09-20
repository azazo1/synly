//! 分发形态: 构建期注入的形态常量与运行期安装位置复核.
//!
//! 形态决定更新时匹配哪种 release 资产, 也决定落地方式: 安装版把整个程序目录交给平台
//! 安装器, 便携形态才会去替换自身可执行文件. 本项目只发布安装版, 便携形态仅用于
//! `cargo run` 一类直接从构建目录启动的场景, 它们不会参与更新.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 构建期注入的形态值.
const BUILT_FORM: &str = env!("SYNLY_DISTRIBUTION_FORM");

/// 应用显示名, 同时是安装目录名.
#[cfg(windows)]
pub const APP_DISPLAY_NAME: &str = "Synly";

/// 可执行文件名, 不含平台扩展名.
#[cfg(not(any(windows, target_os = "macos")))]
pub const APP_EXECUTABLE: &str = "synly";

/// Windows 安装器在 HKCU 下登记的卸载项 AppId, 必须与 `scripts/installer-windows.iss` 一致.
#[cfg(windows)]
pub const WINDOWS_UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{B354AB28-E96A-4AF4-9988-253DA25F421F}_is1";

/// 安装版落在程序目录里的清单文件, 用于运行期确认当前程序确实由安装器安装.
#[cfg(not(any(windows, target_os = "macos")))]
pub const INSTALL_MANIFEST: &str = "install-manifest.txt";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DistributionForm {
    Installer,
    Portable,
}

impl DistributionForm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installer => "installer",
            Self::Portable => "portable",
        }
    }
}

/// 编译时确定的形态.
pub fn built_form() -> DistributionForm {
    match BUILT_FORM {
        "installer" => DistributionForm::Installer,
        _ => DistributionForm::Portable,
    }
}

/// 运行期复核后的形态, 只在首次调用时探测一次.
///
/// 构建期声明为安装版, 但可执行文件并不在安装位置时 (例如用户把安装目录里的程序复制到
/// 别处运行), 按便携形态处理并记 warn, 避免对着错误的目录做整目录替换.
pub fn effective_form() -> DistributionForm {
    static FORM: OnceLock<DistributionForm> = OnceLock::new();
    *FORM.get_or_init(|| {
        let built = built_form();
        if built == DistributionForm::Portable {
            return built;
        }
        match installed_location() {
            Some(_) => DistributionForm::Installer,
            None => {
                tracing::warn!(
                    "构建声明为安装版, 但当前可执行文件不在安装位置, 按便携形态处理, 自动更新不会就地替换"
                );
                DistributionForm::Portable
            }
        }
    })
}

/// 当前程序所在的安装目录, 不是安装版时为 `None`.
pub fn installer_root() -> Option<PathBuf> {
    (effective_form() == DistributionForm::Installer)
        .then(installed_location)
        .flatten()
}

fn installed_location() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let root = exe.parent()?.to_path_buf();
    let expected = expected_install_root()?;
    if !same_path(&root, &expected) {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        // macOS 的安装实体是 app bundle 本身, 由更新模块的 bundle 探测负责.
        super::macos::bundle_root(&exe).map(|_| root)
    }
    #[cfg(windows)]
    {
        uninstall_entry_exists().then_some(root)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        root.join(INSTALL_MANIFEST).is_file().then_some(root)
    }
}

/// 平台约定下的安装目录.
fn expected_install_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let local = dirs::data_local_dir()?;
        Some(local.join("Programs").join(APP_DISPLAY_NAME))
    }
    #[cfg(target_os = "macos")]
    {
        None
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let home = crate::path_expand::home_dir()?;
        Some(Path::new(&home).join(".local/opt").join(APP_EXECUTABLE))
    }
}

#[cfg(windows)]
fn uninstall_entry_exists() -> bool {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW,
    };

    let subkey: Vec<u16> = WINDOWS_UNINSTALL_KEY
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut handle = std::ptr::null_mut();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut handle) };
    if status != 0 {
        return false;
    }
    unsafe { RegCloseKey(handle) };
    true
}

#[cfg(windows)]
fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(right.to_string_lossy().trim_end_matches(['\\', '/']))
}

#[cfg(not(windows))]
fn same_path(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_form_matches_injected_value() {
        let form = built_form();
        assert!(matches!(
            form,
            DistributionForm::Installer | DistributionForm::Portable
        ));
        assert_eq!(built_form(), form);
    }

    #[cfg(windows)]
    #[test]
    fn uninstall_key_is_a_user_hive_subkey() {
        assert!(WINDOWS_UNINSTALL_KEY.starts_with(r"Software\Microsoft\Windows\CurrentVersion"));
        assert!(WINDOWS_UNINSTALL_KEY.ends_with("_is1"));
    }
}
