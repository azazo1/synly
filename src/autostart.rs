use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn apply(enabled: bool) -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate Synly executable")?;
    apply_platform(enabled, executable)
}

#[cfg(target_os = "macos")]
fn apply_platform(enabled: bool, executable: PathBuf) -> Result<()> {
    let directory = dirs::home_dir()
        .context("unable to determine home directory")?
        .join("Library")
        .join("LaunchAgents");
    let target = directory.join("dev.azazo.synly.plist");
    if !enabled {
        remove_if_exists(&target)?;
        return Ok(());
    }
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create autostart directory {}", directory.display()))?;
    let executable = xml_escape(&executable.to_string_lossy());
    let contents = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>dev.azazo.synly</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{executable}</string>\n  </array>\n  <key>RunAtLoad</key>\n  <true/>\n</dict>\n</plist>\n"
    );
    std::fs::write(&target, contents)
        .with_context(|| format!("failed to write autostart file {}", target.display()))
}

#[cfg(target_os = "linux")]
fn apply_platform(enabled: bool, executable: PathBuf) -> Result<()> {
    let directory = dirs::config_dir()
        .context("unable to determine config directory")?
        .join("autostart");
    let target = directory.join("synly.desktop");
    if !enabled {
        remove_if_exists(&target)?;
        return Ok(());
    }
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create autostart directory {}", directory.display()))?;
    let executable = desktop_exec_escape(&executable.to_string_lossy());
    let contents = format!(
        "[Desktop Entry]\nType=Application\nName=Synly\nExec=\"{executable}\"\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    std::fs::write(&target, contents)
        .with_context(|| format!("failed to write autostart file {}", target.display()))
}

#[cfg(windows)]
fn apply_platform(enabled: bool, executable: PathBuf) -> Result<()> {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
    };

    let key_path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let value_name = wide("Synly");
    let mut key = std::ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        anyhow::bail!("failed to open Windows autostart registry key: {status}");
    }
    let result = if enabled {
        let command = wide(&format!("\"{}\"", executable.display()));
        let bytes = unsafe {
            std::slice::from_raw_parts(
                command.as_ptr().cast::<u8>(),
                command.len() * std::mem::size_of::<u16>(),
            )
        };
        let status = unsafe {
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        };
        (status == 0)
            .then_some(())
            .ok_or_else(|| anyhow::anyhow!("failed to set Windows autostart value: {status}"))
    } else {
        let status = unsafe { RegDeleteValueW(key, value_name.as_ptr()) };
        if status == 0 || status == 2 {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "failed to remove Windows autostart value: {status}"
            ))
        }
    };
    unsafe {
        RegCloseKey(key);
    }
    result
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn apply_platform(_enabled: bool, _executable: PathBuf) -> Result<()> {
    anyhow::bail!("login autostart is not supported on this platform")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn remove_if_exists(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove autostart file {}", path.display())),
    }
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "linux")]
fn desktop_exec_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
