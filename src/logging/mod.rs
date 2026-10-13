//! 桌面主程序与 Windows 辅助进程共用的有界日志文件.

mod retention;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use retention::{archive_path, date_string, parent_dir, prune};

#[derive(Clone, Copy)]
struct LogPolicy {
    max_file_bytes: u64,
    max_files: usize,
    max_age: Duration,
}

const DEFAULT_POLICY: LogPolicy = LogPolicy {
    max_file_bytes: 1024 * 1024,
    max_files: 5,
    max_age: Duration::from_secs(14 * 86_400),
};
const CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);
const ERROR_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// 每个日志族最多 5 个文件, 单文件最多 1 MiB, 归档保留不超过 14 天.
///
/// 交给 tracing-appender 的工作线程写入, 启动与轮转时立即清理,
/// 持续写入期间每小时再检查一次. 不扫描或清理其他日志族.
pub struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    date: String,
    policy: LogPolicy,
    last_cleanup: Instant,
    last_error_report: Option<Instant>,
}

impl RotatingFile {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        Self::open_with_policy(path, DEFAULT_POLICY)
    }

    fn open_with_policy(path: PathBuf, policy: LogPolicy) -> io::Result<Self> {
        fs::create_dir_all(parent_dir(&path))?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let metadata = file.metadata()?;
        let date = date_string(metadata.modified().unwrap_or_else(|_| SystemTime::now()));
        let mut writer = Self {
            path,
            file: Some(file),
            written: metadata.len(),
            date,
            policy,
            last_cleanup: Instant::now(),
            last_error_report: None,
        };
        if writer.written > 0
            && (writer.written >= policy.max_file_bytes
                || writer.date != date_string(SystemTime::now()))
        {
            writer.rotate()?;
        } else {
            writer.cleanup();
        }
        Ok(writer)
    }

    fn reopen(&mut self) -> io::Result<()> {
        let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.written = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if self.written > 0 {
            let archived = archive_path(&self.path, &self.date)?;
            self.flush()?;
            // Windows 也必须先关闭旧句柄, 再归档并打开新的当前文件.
            drop(self.file.take());
            if let Err(error) = fs::rename(&self.path, &archived) {
                // 归档失败时保留原文件, 恢复句柄以便后续写入重试.
                self.reopen()?;
                return Err(error);
            }
        }
        self.written = 0;
        self.date = date_string(SystemTime::now());
        self.reopen()?;
        self.cleanup();
        Ok(())
    }

    fn cleanup(&mut self) {
        self.last_cleanup = Instant::now();
        if let Err(error) = prune(&self.path, self.policy, SystemTime::now()) {
            self.report_error("清理", &error);
        }
    }

    fn report_error(&mut self, operation: &str, error: &io::Error) {
        if self.last_error_report.is_some_and(|last| last.elapsed() < ERROR_REPORT_INTERVAL) {
            return;
        }
        self.last_error_report = Some(Instant::now());
        // 日志工作线程内不能再次向 tracing 写日志, 避免递归或堵塞队列.
        eprintln!("日志{operation}失败 ({}): {error}", self.path.display());
    }

    fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.file.is_none() {
            self.reopen()?;
        }
        let incoming = bytes.len() as u64;
        if self.written > 0
            && (self.date != date_string(SystemTime::now())
                || self.written >= self.policy.max_file_bytes
                || (incoming <= self.policy.max_file_bytes
                    && incoming > self.policy.max_file_bytes - self.written))
        {
            self.rotate()?;
        }
        if self.last_cleanup.elapsed() >= CLEANUP_INTERVAL {
            self.cleanup();
        }
        if self.written == 0 {
            self.date = date_string(SystemTime::now());
        }
        // 超大单条日志分片写入, write_all 会继续写剩余内容并触发轮转.
        let available = self.policy.max_file_bytes - self.written;
        let length = incoming.min(available) as usize;
        let file = self.file.as_mut().ok_or_else(|| io::Error::other("日志文件未打开"))?;
        let written = file.write(&bytes[..length])?;
        self.written += written as u64;
        Ok(written)
    }
}

impl Write for RotatingFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.write_chunk(bytes);
        if let Err(error) = &result {
            self.report_error("写入或轮转", error);
        }
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) struct TestDir(pub(super) PathBuf);

    impl TestDir {
        pub(super) fn new() -> Self {
            let path = std::env::temp_dir().join(format!("synly-log-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn oversized_write_is_split_without_losing_bytes() {
        let dir = TestDir::new();
        let path = dir.0.join("synly.log");
        let policy = LogPolicy { max_file_bytes: 4, ..DEFAULT_POLICY };
        let mut writer = RotatingFile::open_with_policy(path.clone(), policy).unwrap();
        writer.write_all(b"abcdefghijk").unwrap();
        writer.flush().unwrap();
        let mut archives = fs::read_dir(&dir.0).unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|candidate| candidate != &path)
            .collect::<Vec<_>>();
        archives.sort();
        let mut contents = Vec::new();
        for archive in archives {
            let bytes = fs::read(archive).unwrap();
            assert!(bytes.len() <= 4);
            contents.extend(bytes);
        }
        let current = fs::read(&path).unwrap();
        assert!(current.len() <= 4);
        contents.extend(current);
        assert_eq!(contents, b"abcdefghijk");
    }

    #[test]
    fn startup_and_rotation_enforce_file_count() {
        let dir = TestDir::new();
        let path = dir.0.join("input-agent.trace.log");
        let date = date_string(SystemTime::now());
        for index in 1..=8 {
            fs::write(dir.0.join(format!("input-agent.trace.{date}.{index}.log")), b"old").unwrap();
        }
        fs::write(&path, [b'x'; 30]).unwrap();
        let policy = LogPolicy { max_file_bytes: 4, ..DEFAULT_POLICY };
        let mut writer = RotatingFile::open_with_policy(path, policy).unwrap();
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 5);
        writer.write_all(b"123456789012345678901234567890").unwrap();
        writer.flush().unwrap();
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 5);
        for entry in fs::read_dir(&dir.0).unwrap() {
            assert!(entry.unwrap().metadata().unwrap().len() <= 4);
        }
    }

    #[test]
    fn date_change_archives_previous_days_contents() {
        let dir = TestDir::new();
        let path = dir.0.join("synly.log");
        let mut writer = RotatingFile::open(path.clone()).unwrap();
        writer.write_all(b"before").unwrap();
        let yesterday = date_string(SystemTime::now() - Duration::from_secs(86_400));
        writer.date = yesterday.clone();
        writer.write_all(b"after").unwrap();
        writer.flush().unwrap();
        assert_eq!(fs::read(dir.0.join(format!("synly.{yesterday}.000001.log"))).unwrap(), b"before");
        assert_eq!(fs::read(&path).unwrap(), b"after");
    }
}
