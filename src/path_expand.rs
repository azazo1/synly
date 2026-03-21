use anyhow::{Context, Result, bail};
use std::borrow::Cow;
use std::env;
use std::path::PathBuf;

pub fn expand_path_string(raw: &str) -> Result<PathBuf> {
    expand_with(raw, resolve_required_env_var)
}

pub fn expand_config_path_string(raw: &str) -> Result<PathBuf> {
    expand_with(raw, resolve_env_var_or_empty)
}

fn expand_with<F>(raw: &str, mut resolve_env_var: F) -> Result<PathBuf>
where
    F: FnMut(&str) -> Result<String>,
{
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("路径不能为空");
    }

    if starts_with_expandable_tilde(trimmed) && home_dir().is_none() {
        bail!("无法展开 `~`，因为当前环境没有可用的 home 目录");
    }

    let normalized = normalize_percent_env_vars(trimmed);
    let expanded = shellexpand::full_with_context(&normalized, home_dir, |name| {
        resolve_env_var(name).map(Some)
    })
    .map_err(|err| err.cause)?;

    Ok(PathBuf::from(expanded.into_owned()))
}

fn resolve_required_env_var(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("环境变量 `{name}` 未定义"))
}

fn resolve_env_var_or_empty(name: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(String::new()),
        Err(err) => Err(err).with_context(|| format!("环境变量 `{name}` 未定义")),
    }
}

fn starts_with_expandable_tilde(raw: &str) -> bool {
    if !raw.starts_with('~') {
        return false;
    }

    let rest = &raw[1..];
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')
}

fn home_dir() -> Option<String> {
    #[cfg(windows)]
    {
        if let Ok(profile) = env::var("USERPROFILE")
            && !profile.trim().is_empty()
        {
            return Some(profile);
        }

        let drive = env::var("HOMEDRIVE").ok()?;
        let path = env::var("HOMEPATH").ok()?;
        if !drive.trim().is_empty() && !path.trim().is_empty() {
            return Some(format!("{drive}{path}"));
        }
    }

    env::var("HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn normalize_percent_env_vars(raw: &str) -> Cow<'_, str> {
    if !raw.contains('%') {
        return Cow::Borrowed(raw);
    }

    let chars = raw.char_indices().collect::<Vec<_>>();
    let mut output = String::with_capacity(raw.len());
    let mut index = 0usize;
    let mut changed = false;

    while index < chars.len() {
        let (byte_index, ch) = chars[index];
        if ch != '%' {
            output.push(ch);
            index += 1;
            continue;
        }

        let mut end = index + 1;
        while end < chars.len() && chars[end].1 != '%' {
            end += 1;
        }

        if end >= chars.len() {
            output.push('%');
            index += 1;
            continue;
        }

        let name_start = byte_index + ch.len_utf8();
        let name_end = chars[end].0;
        if name_start == name_end {
            output.push('%');
            index += 1;
            continue;
        }

        output.push_str("${");
        output.push_str(&raw[name_start..name_end]);
        output.push('}');
        index = end + 1;
        changed = true;
    }

    if changed {
        Cow::Owned(output)
    } else {
        Cow::Borrowed(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_config_path_string, expand_path_string};
    use std::env;
    use std::path::PathBuf;

    #[test]
    fn expands_shell_style_env_var() {
        let path = expand_path_string("$PATH").unwrap();
        assert_eq!(path, PathBuf::from(env::var("PATH").unwrap()));
    }

    #[test]
    fn expands_braced_env_var() {
        let path = expand_path_string("${PATH}/bin").unwrap();
        assert_eq!(
            path,
            PathBuf::from(format!("{}/bin", env::var("PATH").unwrap()))
        );
    }

    #[test]
    fn expands_percent_env_var_when_closed() {
        let path = expand_path_string("%PATH%/bin").unwrap();
        assert_eq!(
            path,
            PathBuf::from(format!("{}/bin", env::var("PATH").unwrap()))
        );
    }

    #[test]
    fn expands_tilde_prefix() {
        let home = env::var("HOME")
            .or_else(|_| env::var("USERPROFILE"))
            .expect("home-like env var should exist during tests");
        let path = expand_path_string("~/demo").unwrap();
        assert_eq!(path, PathBuf::from(format!("{home}/demo")));
    }

    #[test]
    fn expands_combined_path() {
        let home = env::var("HOME")
            .or_else(|_| env::var("USERPROFILE"))
            .expect("home-like env var should exist during tests");
        let path = expand_path_string("~/$PATH").unwrap();
        assert_eq!(
            path,
            PathBuf::from(format!("{home}/{}", env::var("PATH").unwrap()))
        );
    }

    #[test]
    fn cli_expansion_errors_when_env_var_is_missing() {
        let err = expand_path_string("$SYNLY_ENV_SHOULD_NOT_EXIST_4F6CC5D6").unwrap_err();
        assert!(
            err.to_string()
                .contains("环境变量 `SYNLY_ENV_SHOULD_NOT_EXIST_4F6CC5D6` 未定义")
        );
    }

    #[test]
    fn config_expansion_uses_empty_string_for_missing_env_var() {
        let path = expand_config_path_string("cache-$SYNLY_ENV_SHOULD_NOT_EXIST_4F6CC5D6").unwrap();
        assert_eq!(path, PathBuf::from("cache-"));
    }
}
