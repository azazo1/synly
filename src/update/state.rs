use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    ReadyToRestart,
    HandedOff,
    DmgOpened,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UpdateSnapshot {
    pub phase: UpdatePhase,
    pub current_version: String,
    pub latest_tag: Option<String>,
    pub latest_display: Option<String>,
    pub release_notes: String,
    pub release_url: Option<String>,
    pub error_text: String,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub auto_check: bool,
    pub apply_message: String,
}

impl UpdateSnapshot {
    pub fn idle(current_version: String, auto_check: bool) -> Self {
        Self {
            phase: UpdatePhase::Idle,
            current_version,
            latest_tag: None,
            latest_display: None,
            release_notes: String::new(),
            release_url: None,
            error_text: String::new(),
            received_bytes: 0,
            total_bytes: None,
            auto_check,
            apply_message: String::new(),
        }
    }

    pub fn status_bar_text(&self) -> String {
        match self.phase {
            UpdatePhase::Available
            | UpdatePhase::Downloading
            | UpdatePhase::ReadyToRestart
            | UpdatePhase::HandedOff
            | UpdatePhase::DmgOpened => {
                if let Some(version) = &self.latest_display {
                    format!("新版本 {version} 可用")
                } else {
                    "新版本可用".to_string()
                }
            }
            _ => self.current_version.clone(),
        }
    }

    pub fn status_bar_is_link(&self) -> bool {
        matches!(
            self.phase,
            UpdatePhase::Available
                | UpdatePhase::Downloading
                | UpdatePhase::ReadyToRestart
                | UpdatePhase::HandedOff
                | UpdatePhase::DmgOpened
        )
    }

    pub fn progress(&self) -> f32 {
        match (self.received_bytes, self.total_bytes) {
            (received, Some(total)) if total > 0 => (received as f32 / total as f32).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AvailableRelease {
    pub tag: String,
    pub display: String,
    pub notes: String,
    pub html_url: String,
    pub archive_name: String,
    pub archive_url: String,
    pub checksums_url: String,
}

#[derive(Clone, Debug)]
pub enum InstallOutcome {
    #[allow(dead_code)]
    ReadyToRestart { exe: PathBuf },
    #[allow(dead_code)]
    HandedOff,
    #[allow(dead_code)]
    DmgOpened,
}

#[derive(Clone, Debug)]
pub enum RestartAction {
    Relaunch { exe: PathBuf },
    QuitOnly,
}
