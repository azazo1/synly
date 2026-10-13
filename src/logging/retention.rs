//! 只识别当前日志族的归档, 按时间, 数量与容量清理.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::LogPolicy;

struct LogNames {
    current: String,
    stem: String,
    suffix: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ArchiveKey {
    date: String,
    index: u64,
}

impl LogNames {
    fn from_path(path: &Path) -> io::Result<Self> {
        let current = path.file_name().and_then(|name| name.to_str());
        let stem = path.file_stem().and_then(|name| name.to_str());
        let (Some(current), Some(stem)) = (current, stem) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "日志文件名必须是 UTF-8",
            ));
        };
        let suffix = path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        Ok(Self {
            current: current.to_owned(),
            stem: stem.to_owned(),
            suffix,
        })
    }

    fn archive_key(&self, name: &str) -> Option<ArchiveKey> {
        // 接收旧 tracing-appender 的按日文件, 如 service.trace.log.2026-01-01.
        if let Some(date) = name.strip_prefix(&format!("{}.", self.current))
            && valid_date(date)
        {
            return Some(ArchiveKey {
                date: date.to_owned(),
                index: 0,
            });
        }
        let value = name
            .strip_prefix(&format!("{}.", self.stem))?
            .strip_suffix(&self.suffix)?;
        let (date, index) = value.rsplit_once('.')?;
        if !valid_date(date) {
            return None;
        }
        let index = if index == "overflow" {
            u64::MAX
        } else {
            if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            index.parse::<u64>().ok().filter(|index| *index > 0)?
        };
        Some(ArchiveKey {
            date: date.to_owned(),
            index,
        })
    }
}

pub(super) fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

pub(super) fn archive_path(path: &Path, date: &str) -> io::Result<PathBuf> {
    let names = LogNames::from_path(path)?;
    let dir = parent_dir(path);
    let mut last_index = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(key) = entry.file_name().to_str().and_then(|name| names.archive_key(name))
            && key.date == date
            && key.index != u64::MAX
        {
            last_index = last_index.max(key.index);
        }
    }
    let index = last_index.checked_add(1).ok_or_else(|| {
        io::Error::other("日志归档编号已耗尽")
    })?;
    Ok(dir.join(format!("{}.{date}.{index:06}{}", names.stem, names.suffix)))
}

pub(super) fn prune(path: &Path, policy: LogPolicy, now: SystemTime) -> io::Result<()> {
    let names = LogNames::from_path(path)?;
    let mut files = Vec::new();
    let mut first_error = None;
    for entry in fs::read_dir(parent_dir(path))? {
        let result = (|| -> io::Result<_> {
            let entry = entry?;
            let Some(key) = entry.file_name().to_str().and_then(|name| names.archive_key(name)) else {
                return Ok(None);
            };
            // 不跟随符号链接, 不删除目录或相似前缀的其他文件.
            if !entry.file_type()?.is_file() {
                return Ok(None);
            }
            let metadata = entry.metadata()?;
            Ok(Some((key, entry.path(), metadata.len())))
        })();
        match result {
            Ok(Some(file)) => files.push(file),
            Ok(None) => {}
            Err(error) => { first_error.get_or_insert(error); }
        }
    }
    // 编号按数值排序, 防止同一天的第 10 个归档排在第 2 个之前.
    files.sort_by(|left, right| {
        (&left.0.date, left.0.index).cmp(&(&right.0.date, right.0.index))
            .then_with(|| left.1.cmp(&right.1))
    });
    let cutoff = date_string(now.checked_sub(policy.max_age).unwrap_or(UNIX_EPOCH));
    // 先移除过期或超大的归档, 避免旧大文件挤掉仍有用的新日志.
    files.retain(|(key, archive, bytes)| {
        if key.date >= cutoff && *bytes <= policy.max_file_bytes {
            return true;
        }
        match remove_archive(archive) {
            Ok(()) => false,
            Err(error) => {
                first_error.get_or_insert(error);
                true
            }
        }
    });
    let mut remaining = files.len();
    let mut total_bytes = files
        .iter()
        .fold(0_u64, |total, file| total.saturating_add(file.2));
    // 为当前文件预留完整容量, 写入时不必逐条扫描目录.
    let archive_budget = policy
        .max_file_bytes
        .saturating_mul((policy.max_files - 1) as u64);
    for (key, archive, bytes) in files {
        if key.date < cutoff || bytes > policy.max_file_bytes {
            // 本轮已尝试删除但失败, 仍保留在数量与容量统计中.
            continue;
        }
        if remaining < policy.max_files && total_bytes <= archive_budget {
            continue;
        }
        match remove_archive(&archive) {
            Ok(()) => {
                remaining -= 1;
                total_bytes = total_bytes.saturating_sub(bytes);
            }
            Err(error) => {
                // 删除失败的文件仍计入占用, 并继续尝试其他旧归档.
                first_error.get_or_insert(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn remove_archive(path: &Path) -> io::Result<()> {
    fs::remove_file(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("删除日志 {} 失败: {error}", path.display()),
        )
    })
}

pub(super) fn date_string(time: SystemTime) -> String {
    let days = time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn valid_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !bytes.iter().enumerate().all(|(index, byte)| {
        index == 4 || index == 7 || byte.is_ascii_digit()
    }) {
        return false;
    }
    let year = date[..4].parse::<u32>().unwrap_or(0);
    let month = date[5..7].parse::<u32>().unwrap_or(0);
    let day = date[8..].parse::<u32>().unwrap_or(0);
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let max_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    (1..=max_day).contains(&day)
}

fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::{DEFAULT_POLICY, tests::TestDir};

    #[test]
    fn cleanup_keeps_newest_numeric_archives_and_unrelated_files() {
        let dir = TestDir::new();
        let path = dir.0.join("synly.log");
        let date = date_string(SystemTime::now());
        for index in [1, 2, 10] {
            fs::write(dir.0.join(format!("synly.{date}.{index}.log")), b"log").unwrap();
        }
        fs::write(&path, b"active").unwrap();
        let unrelated = dir.0.join("synly-settings.json");
        fs::write(&unrelated, b"keep").unwrap();
        let policy = LogPolicy { max_files: 2, ..DEFAULT_POLICY };
        prune(&path, policy, SystemTime::now()).unwrap();
        assert!(dir.0.join(format!("synly.{date}.10.log")).exists());
        assert!(!dir.0.join(format!("synly.{date}.2.log")).exists());
        assert_eq!(fs::read(&path).unwrap(), b"active");
        assert!(unrelated.exists());
        assert!(archive_path(&path, &date).unwrap().ends_with(format!("synly.{date}.000011.log")));
    }

    #[test]
    fn cleanup_removes_legacy_expired_and_oversized_logs() {
        let dir = TestDir::new();
        let path = dir.0.join("service.trace.log");
        let date = date_string(SystemTime::now());
        let expired = dir.0.join("service.trace.log.2000-01-01");
        let large = dir.0.join(format!("service.trace.{date}.2.log"));
        let legacy = dir.0.join(format!("service.trace.log.{date}"));
        let recent = dir.0.join(format!("service.trace.{date}.1.log"));
        fs::write(&expired, b"old").unwrap();
        fs::write(&large, [b'x'; 100]).unwrap();
        fs::write(&legacy, b"old").unwrap();
        fs::write(&recent, b"new").unwrap();
        let policy = LogPolicy { max_file_bytes: 4, ..DEFAULT_POLICY };
        prune(&path, policy, SystemTime::now()).unwrap();
        assert!(!expired.exists());
        assert!(!large.exists());
        assert!(legacy.exists());
        assert!(recent.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_symlinks_and_directories() {
        let dir = TestDir::new();
        let path = dir.0.join("synly.log");
        let target = dir.0.join("keep.txt");
        let link = dir.0.join("synly.2000-01-01.1.log");
        let directory = dir.0.join("synly.2000-01-01.2.log");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        fs::create_dir(&directory).unwrap();
        prune(&path, DEFAULT_POLICY, SystemTime::now()).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(directory.is_dir());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }
}
