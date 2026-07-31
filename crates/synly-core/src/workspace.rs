use crate::input::InputMode;
use crate::settings::{AudioMode, ClipboardMode, FileSyncMode, InitialSyncMode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub file_sync_mode: FileSyncMode,
    pub send_description: Option<String>,
    pub send_layout: Option<SnapshotLayout>,
    pub send_items: Vec<String>,
    pub receive_root: Option<String>,
    pub initial_sync: Option<InitialSyncMode>,
    pub max_folder_depth: Option<usize>,
    pub clipboard_mode: ClipboardMode,
    pub audio_mode: AudioMode,
    pub input_mode: InputMode,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotLayout {
    RootContents,
    SelectedItems,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestSnapshot {
    pub layout: SnapshotLayout,
    pub max_folder_depth: Option<usize>,
    pub entries: BTreeMap<String, ManifestEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    pub kind: EntryKind,
    pub size: u64,
    pub modified_ms: u64,
    pub hash: Option<String>,
    pub executable: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
}

impl WorkspaceSummary {
    pub fn can_send_files(&self) -> bool {
        self.send_layout.is_some()
    }

    pub fn can_receive_files(&self) -> bool {
        self.receive_root.is_some()
    }

    pub fn file_sync_enabled(&self) -> bool {
        self.can_send_files() || self.can_receive_files()
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("文件同步模式: {}", self.file_sync_mode.label())];

        if !self.file_sync_enabled() {
            lines.push("文件同步: 关闭".to_string());
        }

        if let Some(description) = &self.send_description {
            lines.push(format!("发送: {}", description));
        }
        if !self.send_items.is_empty() {
            lines.push(format!("发送条目: {}", self.send_items.join(", ")));
        }
        if let Some(max_folder_depth) = self.max_folder_depth {
            lines.push(format!("发送最大目录深度: {}", max_folder_depth));
        }
        if let Some(root) = &self.receive_root {
            lines.push(format!("接收目录: {}", root));
        }
        if let Some(initial_sync) = self.initial_sync {
            lines.push(format!("初始状态: {}", initial_sync.label()));
        }
        lines.push(format!("剪贴板同步: {}", self.clipboard_mode.label()));
        lines.push(format!("音频同步: {}", self.audio_mode.label()));
        lines.push(format!("鼠标键盘同步: {}", self.input_mode.label()));

        lines
    }
}
