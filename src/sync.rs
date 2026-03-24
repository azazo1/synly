use crate::cli::{ClipboardMode, SyncMode};
use anyhow::{Context, Result, bail};
use filetime::{FileTime, set_file_mtime};
use ignore::gitignore::Gitignore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SYNLY_INTERNAL_DIR: &str = ".synly";
const SYNLY_DELETED_DIR: &str = "deleted";
const SYNLY_IGNORE_FILE: &str = ".synlyignore";

#[derive(Clone, Debug)]
pub struct WorkspaceSpec {
    pub mode: SyncMode,
    pub outgoing: Option<OutgoingSpec>,
    pub incoming_root: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub enum OutgoingSpec {
    RootContents {
        root: PathBuf,
        max_folder_depth: Option<usize>,
    },
    SelectedItems {
        items: Vec<NamedItem>,
        max_folder_depth: Option<usize>,
    },
}

#[derive(Clone, Debug)]
pub struct NamedItem {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub mode: SyncMode,
    pub send_description: Option<String>,
    pub send_layout: Option<SnapshotLayout>,
    pub send_items: Vec<String>,
    pub receive_root: Option<String>,
    #[serde(default)]
    pub max_folder_depth: Option<usize>,
    #[serde(default)]
    pub clipboard_mode: ClipboardMode,
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
    #[serde(default)]
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

#[derive(Clone, Copy, Debug)]
pub enum DeletePolicy {
    Never,
    MirrorAll,
    MirrorSelectedItems,
}

#[derive(Clone, Debug)]
pub struct ApplyPlan {
    pub file_requests: Vec<String>,
    pub delete_paths: Vec<String>,
    pub skipped_newer_paths: Vec<String>,
    pub unreliable_timestamp_paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TimestampComparisonContext {
    pub remote_clock_delta_ms: i64,
    pub local_now_ms: Option<u64>,
    pub remote_now_ms: Option<u64>,
    pub skew_tolerance_ms: u64,
    pub future_guard_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct DeleteReport {
    pub archived_count: usize,
    pub failures: Vec<DeleteFailure>,
}

#[derive(Clone, Debug)]
pub struct DeleteFailure {
    pub wire_path: String,
    pub local_path: Option<PathBuf>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchTarget {
    pub path: PathBuf,
    pub recursive: bool,
}

#[derive(Debug)]
struct SynlyIgnoreMatcher {
    root: PathBuf,
    matchers: Vec<ScopedIgnoreMatcher>,
}

#[derive(Debug)]
struct ScopedIgnoreMatcher {
    directory: PathBuf,
    matcher: Gitignore,
}

impl WorkspaceSpec {
    pub fn for_off() -> Self {
        Self {
            mode: SyncMode::Off,
            outgoing: None,
            incoming_root: None,
        }
    }

    pub fn for_send(paths: Vec<PathBuf>) -> Result<Self> {
        if paths.is_empty() {
            bail!("at least one path is required for send mode");
        }

        let canonical_paths = canonicalize_all(paths)?;
        let outgoing = if canonical_paths.len() == 1 && canonical_paths[0].is_dir() {
            OutgoingSpec::RootContents {
                root: canonical_paths[0].clone(),
                max_folder_depth: None,
            }
        } else {
            OutgoingSpec::SelectedItems {
                items: build_named_items(canonical_paths)?,
                max_folder_depth: None,
            }
        };

        Ok(Self {
            mode: SyncMode::Send,
            outgoing: Some(outgoing),
            incoming_root: None,
        })
    }

    pub fn for_receive(root: PathBuf) -> Result<Self> {
        let root = ensure_directory(root)?;
        Ok(Self {
            mode: SyncMode::Receive,
            outgoing: None,
            incoming_root: Some(root),
        })
    }

    pub fn for_both(root: PathBuf) -> Result<Self> {
        let root = ensure_directory(root)?;
        Ok(Self {
            mode: SyncMode::Both,
            outgoing: Some(OutgoingSpec::RootContents {
                root: root.clone(),
                max_folder_depth: None,
            }),
            incoming_root: Some(root),
        })
    }

    pub fn for_auto(root: PathBuf) -> Result<Self> {
        let root = ensure_directory(root)?;
        Ok(Self {
            mode: SyncMode::Auto,
            outgoing: Some(OutgoingSpec::RootContents {
                root: root.clone(),
                max_folder_depth: None,
            }),
            incoming_root: Some(root),
        })
    }

    pub fn with_max_folder_depth(mut self, max_folder_depth: Option<usize>) -> Self {
        if let Some(outgoing) = &mut self.outgoing {
            outgoing.set_max_folder_depth(max_folder_depth);
        }
        self
    }

    pub fn summary(&self, clipboard_mode: ClipboardMode) -> WorkspaceSummary {
        let (send_description, send_layout, send_items, max_folder_depth) = match &self.outgoing {
            Some(OutgoingSpec::RootContents {
                root,
                max_folder_depth,
            }) => (
                Some(format!("同步目录内容: {}", root.display())),
                Some(SnapshotLayout::RootContents),
                Vec::new(),
                *max_folder_depth,
            ),
            Some(OutgoingSpec::SelectedItems {
                items,
                max_folder_depth,
            }) => (
                Some(format!("同步 {} 个指定文件/文件夹", items.len())),
                Some(SnapshotLayout::SelectedItems),
                items.iter().map(|item| item.name.clone()).collect(),
                *max_folder_depth,
            ),
            None => (None, None, Vec::new(), None),
        };

        WorkspaceSummary {
            mode: self.mode,
            send_description,
            send_layout,
            send_items,
            receive_root: self
                .incoming_root
                .as_ref()
                .map(|path| path.display().to_string()),
            max_folder_depth,
            clipboard_mode,
        }
    }

    pub fn can_send_files(&self) -> bool {
        self.outgoing.is_some()
    }

    pub fn can_receive_files(&self) -> bool {
        self.incoming_root.is_some()
    }

    pub fn file_sync_enabled(&self) -> bool {
        self.can_send_files() || self.can_receive_files()
    }

    pub fn local_human_lines(&self, clipboard_mode: ClipboardMode) -> Vec<String> {
        let mut lines = vec![format!("文件同步模式: {}", self.mode.label())];

        if !self.file_sync_enabled() {
            lines.push("文件同步: 关闭".to_string());
        }

        match &self.outgoing {
            Some(OutgoingSpec::RootContents {
                root,
                max_folder_depth,
            }) => {
                lines.push(format!("发送目录: {}", root.display()));
                if let Some(max_folder_depth) = max_folder_depth {
                    lines.push(format!("发送最大目录深度: {}", max_folder_depth));
                }
            }
            Some(OutgoingSpec::SelectedItems {
                items,
                max_folder_depth,
            }) => {
                for item in items {
                    lines.push(format!("发送条目: {}", item.path.display()));
                }
                if let Some(max_folder_depth) = max_folder_depth {
                    lines.push(format!("发送最大目录深度: {}", max_folder_depth));
                }
            }
            None => {}
        }

        if let Some(root) = &self.incoming_root {
            lines.push(format!("接收目录: {}", root.display()));
        }
        lines.push(format!("剪贴板同步: {}", clipboard_mode.label()));

        lines
    }
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

    pub fn human_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("文件同步模式: {}", self.mode.label())];

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
        lines.push(format!("剪贴板同步: {}", self.clipboard_mode.label()));

        lines
    }
}

impl OutgoingSpec {
    pub fn set_max_folder_depth(&mut self, max_folder_depth: Option<usize>) {
        match self {
            OutgoingSpec::RootContents {
                max_folder_depth: depth,
                ..
            }
            | OutgoingSpec::SelectedItems {
                max_folder_depth: depth,
                ..
            } => *depth = max_folder_depth,
        }
    }
}

pub fn build_snapshot(spec: &OutgoingSpec) -> Result<ManifestSnapshot> {
    let mut entries = BTreeMap::new();
    match spec {
        OutgoingSpec::RootContents {
            root,
            max_folder_depth,
        } => {
            add_walk_entries(root, None, 1, *max_folder_depth, &mut entries)?;
            Ok(ManifestSnapshot {
                layout: SnapshotLayout::RootContents,
                max_folder_depth: *max_folder_depth,
                entries,
            })
        }
        OutgoingSpec::SelectedItems {
            items,
            max_folder_depth,
        } => {
            for item in items {
                if item.is_dir {
                    add_walk_entries(
                        &item.path,
                        Some(&item.name),
                        0,
                        *max_folder_depth,
                        &mut entries,
                    )?;
                } else if should_keep_path(&item.path) {
                    add_entry(&item.path, &item.path, Some(&item.name), &mut entries)?;
                }
            }

            Ok(ManifestSnapshot {
                layout: SnapshotLayout::SelectedItems,
                max_folder_depth: *max_folder_depth,
                entries,
            })
        }
    }
}

pub fn build_incoming_snapshot(root: &Path) -> Result<ManifestSnapshot> {
    build_snapshot(&OutgoingSpec::RootContents {
        root: root.to_path_buf(),
        max_folder_depth: None,
    })
}

pub fn filter_snapshot_for_incoming_root(
    root: &Path,
    snapshot: &ManifestSnapshot,
) -> Result<ManifestSnapshot> {
    SynlyIgnoreMatcher::discover(root)?.filter_snapshot(snapshot)
}

pub fn filter_snapshot_by_folder_depth(
    snapshot: &ManifestSnapshot,
    layout: SnapshotLayout,
    max_folder_depth: Option<usize>,
) -> ManifestSnapshot {
    let Some(max_folder_depth) = max_folder_depth else {
        return snapshot.clone();
    };

    let entries = snapshot
        .entries
        .iter()
        .filter(|(path, _)| snapshot_folder_depth(layout, path) <= max_folder_depth)
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect();

    ManifestSnapshot {
        layout: snapshot.layout,
        max_folder_depth: snapshot.max_folder_depth,
        entries,
    }
}

pub fn watch_targets(spec: &OutgoingSpec) -> Result<Vec<WatchTarget>> {
    let mut targets = BTreeMap::<PathBuf, bool>::new();

    match spec {
        OutgoingSpec::RootContents { root, .. } => {
            targets.insert(root.clone(), true);
        }
        OutgoingSpec::SelectedItems { items, .. } => {
            for item in items {
                let (path, recursive) = if item.is_dir {
                    (item.path.clone(), true)
                } else {
                    let parent = item.path.parent().with_context(|| {
                        format!(
                            "shared file {} has no parent directory",
                            item.path.display()
                        )
                    })?;
                    (parent.to_path_buf(), false)
                };

                targets
                    .entry(path)
                    .and_modify(|existing| *existing |= recursive)
                    .or_insert(recursive);
            }
        }
    }

    Ok(targets
        .into_iter()
        .map(|(path, recursive)| WatchTarget { path, recursive })
        .collect())
}

pub fn build_apply_plan(
    remote: &ManifestSnapshot,
    local: &ManifestSnapshot,
    delete_policy: DeletePolicy,
) -> ApplyPlan {
    build_apply_plan_with_time(
        remote,
        local,
        delete_policy,
        TimestampComparisonContext::default(),
    )
}

pub fn build_apply_plan_with_time(
    remote: &ManifestSnapshot,
    local: &ManifestSnapshot,
    delete_policy: DeletePolicy,
    time_context: TimestampComparisonContext,
) -> ApplyPlan {
    let mut file_requests = Vec::new();
    let mut skipped_newer_paths = Vec::new();
    let mut unreliable_timestamp_paths = Vec::new();

    for (path, remote_entry) in &remote.entries {
        if remote_entry.kind != EntryKind::File {
            continue;
        }

        match local.entries.get(path) {
            Some(local_entry)
                if local_entry.kind == EntryKind::File
                    && local_entry.hash == remote_entry.hash
                    && local_entry.size == remote_entry.size
                    && local_entry.modified_ms == remote_entry.modified_ms
                    && local_entry.executable == remote_entry.executable => {}
            Some(local_entry) if local_entry.kind == EntryKind::File => {
                match classify_timestamp_conflict(local_entry, remote_entry, time_context) {
                    TimestampConflict::LocalNewer => skipped_newer_paths.push(path.clone()),
                    TimestampConflict::Unreliable => {
                        unreliable_timestamp_paths.push(path.clone());
                        file_requests.push(path.clone());
                    }
                    TimestampConflict::RemoteNewerOrEqual => file_requests.push(path.clone()),
                }
            }
            _ => file_requests.push(path.clone()),
        }
    }

    let delete_paths = match delete_policy {
        DeletePolicy::Never => Vec::new(),
        DeletePolicy::MirrorAll => compute_delete_paths(remote, local, |_, _| true),
        DeletePolicy::MirrorSelectedItems => {
            let scopes = remote_selected_scopes(remote);
            compute_delete_paths(remote, local, |path, _| {
                path.split('/')
                    .next()
                    .is_some_and(|top| scopes.contains(top))
            })
        }
    };

    ApplyPlan {
        file_requests,
        delete_paths,
        skipped_newer_paths,
        unreliable_timestamp_paths,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimestampConflict {
    LocalNewer,
    RemoteNewerOrEqual,
    Unreliable,
}

fn classify_timestamp_conflict(
    local_entry: &ManifestEntry,
    remote_entry: &ManifestEntry,
    time_context: TimestampComparisonContext,
) -> TimestampConflict {
    if timestamp_is_suspicious_future(
        local_entry.modified_ms,
        time_context.local_now_ms,
        time_context.future_guard_ms,
    ) || timestamp_is_suspicious_future(
        remote_entry.modified_ms,
        time_context.remote_now_ms,
        time_context.future_guard_ms,
    ) {
        return TimestampConflict::Unreliable;
    }

    let normalized_remote_ms =
        normalize_remote_modified_ms(remote_entry.modified_ms, time_context.remote_clock_delta_ms);
    let local_newer_threshold = normalized_remote_ms.saturating_add(time_context.skew_tolerance_ms);

    if local_entry.modified_ms > local_newer_threshold {
        TimestampConflict::LocalNewer
    } else {
        TimestampConflict::RemoteNewerOrEqual
    }
}

fn timestamp_is_suspicious_future(
    modified_ms: u64,
    current_ms: Option<u64>,
    future_guard_ms: u64,
) -> bool {
    current_ms.is_some_and(|current_ms| modified_ms > current_ms.saturating_add(future_guard_ms))
}

fn normalize_remote_modified_ms(remote_modified_ms: u64, remote_clock_delta_ms: i64) -> u64 {
    let normalized = i128::from(remote_modified_ms) - i128::from(remote_clock_delta_ms);
    normalized.clamp(0, i128::from(u64::MAX)) as u64
}

pub fn resolve_outgoing_path(spec: &OutgoingSpec, wire_path: &str) -> Result<PathBuf> {
    let relative = wire_to_relative_path(wire_path)?;
    match spec {
        OutgoingSpec::RootContents { root, .. } => Ok(root.join(relative)),
        OutgoingSpec::SelectedItems { items, .. } => {
            let mut components = relative.components();
            let first = match components.next() {
                Some(Component::Normal(component)) => component.to_string_lossy().to_string(),
                _ => bail!("wire path `{}` is not within selected items", wire_path),
            };

            let item = items
                .iter()
                .find(|candidate| candidate.name == first)
                .with_context(|| {
                    format!(
                        "requested path `{}` is not part of the shared selection",
                        wire_path
                    )
                })?;

            let rest = components.collect::<PathBuf>();
            if !item.is_dir && !rest.as_os_str().is_empty() {
                bail!("requested path `{}` points inside a shared file", wire_path);
            }

            Ok(item.path.join(rest))
        }
    }
}

pub fn snapshot_contains_file(snapshot: &ManifestSnapshot, wire_path: &str) -> Result<bool> {
    let normalized = path_to_wire(&wire_to_relative_path(wire_path)?)?;
    Ok(snapshot
        .entries
        .get(&normalized)
        .is_some_and(|entry| entry.kind == EntryKind::File))
}

pub fn resolve_incoming_path(root: &Path, wire_path: &str) -> Result<PathBuf> {
    let relative = wire_to_relative_path(wire_path)?;
    ensure_no_symlink_ancestors(root, &relative)?;
    Ok(root.join(relative))
}

pub fn ensure_directories(root: &Path, snapshot: &ManifestSnapshot) -> Result<()> {
    let mut directories = snapshot
        .entries
        .iter()
        .filter_map(|(path, entry)| (entry.kind == EntryKind::Dir).then_some(path.clone()))
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| path_depth(path));

    for wire_path in directories {
        let directory = resolve_incoming_path(root, &wire_path)?;
        if let Ok(metadata) = fs::symlink_metadata(&directory)
            && (metadata.file_type().is_symlink() || metadata.is_file())
        {
            fs::remove_file(&directory).with_context(|| {
                format!("failed to remove conflicting file {}", directory.display())
            })?;
        }
        fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create directory {}", directory.display()))?;
    }

    Ok(())
}

pub fn delete_paths_best_effort(root: &Path, wire_paths: &[String]) -> DeleteReport {
    let mut report = DeleteReport::default();

    for wire_path in wire_paths {
        match try_delete_path(root, wire_path) {
            Ok(DeleteDisposition::Archived) => {
                report.archived_count += 1;
            }
            Ok(DeleteDisposition::NotFound) => {}
            Err((local_path, err)) => {
                report.failures.push(DeleteFailure {
                    wire_path: wire_path.clone(),
                    local_path,
                    reason: format!("{err:#}"),
                });
            }
        }
    }

    report
}

pub fn apply_file_metadata(path: &Path, modified_ms: u64, executable: bool) -> Result<()> {
    let secs = (modified_ms / 1_000) as i64;
    let nanos = ((modified_ms % 1_000) * 1_000_000) as u32;
    set_file_mtime(path, FileTime::from_unix_time(secs, nanos))
        .with_context(|| format!("failed to set file time for {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?
            .permissions();
        let mode = if executable { 0o755 } else { 0o644 };
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to set permissions for {}", path.display()))?;
    }

    Ok(())
}

fn compute_delete_paths<F>(
    remote: &ManifestSnapshot,
    local: &ManifestSnapshot,
    scope_predicate: F,
) -> Vec<String>
where
    F: Fn(&str, &ManifestEntry) -> bool,
{
    let mut paths = local
        .entries
        .iter()
        .filter_map(|(path, entry)| {
            (!remote.entries.contains_key(path) && scope_predicate(path, entry))
                .then_some((path.clone(), path_depth(path)))
        })
        .collect::<Vec<_>>();

    paths.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));

    let mut collapsed = Vec::<String>::new();
    for (path, _) in paths {
        if !collapsed
            .iter()
            .any(|ancestor| is_ancestor_path(ancestor, &path))
        {
            collapsed.push(path);
        }
    }

    collapsed
}

enum DeleteDisposition {
    Archived,
    NotFound,
}

fn try_delete_path(
    root: &Path,
    wire_path: &str,
) -> std::result::Result<DeleteDisposition, (Option<PathBuf>, anyhow::Error)> {
    let path = match resolve_incoming_path(root, wire_path) {
        Ok(path) => path,
        Err(err) => return Err((None, err)),
    };

    match fs::symlink_metadata(&path) {
        Ok(_) => archive_deleted_path(root, wire_path, &path)
            .map(|_| DeleteDisposition::Archived)
            .map_err(|err| (Some(path), err)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DeleteDisposition::NotFound),
        Err(err) => Err((
            Some(path.clone()),
            anyhow::Error::from(err).context(format!("failed to inspect path {}", path.display())),
        )),
    }
}

fn remote_selected_scopes(snapshot: &ManifestSnapshot) -> BTreeSet<String> {
    snapshot
        .entries
        .keys()
        .filter_map(|path| path.split('/').next())
        .map(|item| item.to_string())
        .collect()
}

fn add_walk_entries(
    base: &Path,
    prefix: Option<&str>,
    min_depth: usize,
    max_folder_depth: Option<usize>,
    entries: &mut BTreeMap<String, ManifestEntry>,
) -> Result<()> {
    if !should_keep_path(base) {
        return Ok(());
    }

    if min_depth == 0 {
        add_entry(base, base, prefix, entries)?;
    }

    let mut active_matchers = Vec::new();
    visit_snapshot_directory(
        base,
        base,
        prefix,
        0,
        max_folder_depth,
        &mut active_matchers,
        entries,
    )?;
    Ok(())
}

fn add_entry(
    base: &Path,
    actual_path: &Path,
    prefix: Option<&str>,
    entries: &mut BTreeMap<String, ManifestEntry>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(actual_path)
        .with_context(|| format!("failed to read metadata for {}", actual_path.display()))?;

    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    let wire_path = build_wire_path(base, actual_path, prefix)?;
    let entry = if metadata.is_dir() {
        ManifestEntry {
            kind: EntryKind::Dir,
            size: 0,
            modified_ms: modified_time_ms(&metadata.modified().ok()),
            hash: None,
            executable: false,
        }
    } else if metadata.is_file() {
        ManifestEntry {
            kind: EntryKind::File,
            size: metadata.len(),
            modified_ms: modified_time_ms(&metadata.modified().ok()),
            hash: Some(hash_file(actual_path)?),
            executable: is_executable(&metadata),
        }
    } else {
        return Ok(());
    };

    entries.insert(wire_path, entry);
    Ok(())
}

fn build_wire_path(base: &Path, actual_path: &Path, prefix: Option<&str>) -> Result<String> {
    match prefix {
        Some(prefix_name) => {
            if actual_path == base {
                Ok(prefix_name.to_string())
            } else {
                let relative = actual_path.strip_prefix(base).with_context(|| {
                    format!(
                        "failed to compute relative path from {} to {}",
                        base.display(),
                        actual_path.display()
                    )
                })?;
                let suffix = path_to_wire(relative)?;
                Ok(format!("{}/{}", prefix_name, suffix))
            }
        }
        None => {
            let relative = actual_path.strip_prefix(base).with_context(|| {
                format!(
                    "failed to compute relative path from {} to {}",
                    base.display(),
                    actual_path.display()
                )
            })?;
            path_to_wire(relative)
        }
    }
}

fn path_to_wire(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(normal) => {
                let part = normal
                    .to_str()
                    .with_context(|| format!("non-utf8 path segment in {}", path.display()))?;
                if part.contains('/') || part.contains('\\') {
                    bail!("unsupported path segment `{}`", part);
                }
                parts.push(part.to_string());
            }
            _ => bail!("unsupported path component in {}", path.display()),
        }
    }
    Ok(parts.join("/"))
}

fn wire_to_relative_path(wire_path: &str) -> Result<PathBuf> {
    let mut relative = PathBuf::new();
    for segment in wire_path.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains('\\')
            || segment.contains(':')
        {
            bail!("invalid wire path `{}`", wire_path);
        }
        relative.push(segment);
    }
    Ok(relative)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn canonicalize_all(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    paths
        .into_iter()
        .map(|path| {
            fs::canonicalize(&path)
                .with_context(|| format!("failed to access path {}", path.display()))
        })
        .collect()
}

fn build_named_items(paths: Vec<PathBuf>) -> Result<Vec<NamedItem>> {
    let mut seen = BTreeSet::new();
    let mut items = Vec::new();

    for path in paths {
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .with_context(|| format!("failed to get file name for {}", path.display()))?
            .to_string();
        if !seen.insert(name.clone()) {
            bail!(
                "duplicate shared item name `{}`; please rename or share fewer paths",
                name
            );
        }
        let metadata =
            fs::metadata(&path).with_context(|| format!("failed to inspect {}", path.display()))?;
        items.push(NamedItem {
            name,
            path,
            is_dir: metadata.is_dir(),
        });
    }

    items.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(items)
}

fn ensure_directory(path: PathBuf) -> Result<PathBuf> {
    let absolute = if path.exists() {
        fs::canonicalize(&path)
            .with_context(|| format!("failed to access directory {}", path.display()))?
    } else {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create parent directory {}", parent.display())
            })?;
        }
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create directory {}", path.display()))?;
        fs::canonicalize(&path)
            .with_context(|| format!("failed to access directory {}", path.display()))?
    };

    let metadata = fs::metadata(&absolute)
        .with_context(|| format!("failed to inspect {}", absolute.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", absolute.display());
    }
    Ok(absolute)
}

impl SynlyIgnoreMatcher {
    fn discover(root: &Path) -> Result<Self> {
        let mut matchers = Vec::new();
        let mut active_indices = Vec::new();
        collect_ignore_matchers(root, &mut active_indices, &mut matchers)?;

        Ok(Self {
            root: root.to_path_buf(),
            matchers,
        })
    }

    fn filter_snapshot(&self, snapshot: &ManifestSnapshot) -> Result<ManifestSnapshot> {
        let mut entries = BTreeMap::new();
        for (path, entry) in &snapshot.entries {
            let is_dir = entry.kind == EntryKind::Dir;
            if !self.is_ignored_wire_path(path, is_dir)? {
                entries.insert(path.clone(), entry.clone());
            }
        }

        Ok(ManifestSnapshot {
            layout: snapshot.layout,
            max_folder_depth: snapshot.max_folder_depth,
            entries,
        })
    }

    fn is_ignored_wire_path(&self, wire_path: &str, is_dir: bool) -> Result<bool> {
        let relative = wire_to_relative_path(wire_path)?;
        Ok(self.is_ignored_path(&self.root.join(relative), is_dir))
    }

    fn is_ignored_path(&self, path: &Path, is_dir: bool) -> bool {
        let mut decision = None;
        for matcher in &self.matchers {
            if !path.starts_with(&matcher.directory) {
                continue;
            }

            let matched = matcher.matcher.matched_path_or_any_parents(path, is_dir);
            if matched.is_ignore() {
                decision = Some(true);
            } else if matched.is_whitelist() {
                decision = Some(false);
            }
        }
        decision.unwrap_or(false)
    }
}

fn visit_snapshot_directory(
    directory: &Path,
    base: &Path,
    prefix: Option<&str>,
    current_depth: usize,
    max_folder_depth: Option<usize>,
    active_matchers: &mut Vec<ScopedIgnoreMatcher>,
    entries: &mut BTreeMap<String, ManifestEntry>,
) -> Result<()> {
    let previous_len = active_matchers.len();
    if let Some(matcher) = load_directory_ignore_matcher(directory)? {
        active_matchers.push(matcher);
    }

    for child in sorted_directory_children(directory)? {
        let metadata = fs::symlink_metadata(&child)
            .with_context(|| format!("failed to inspect {}", child.display()))?;
        if metadata.file_type().is_symlink() || !should_keep_path(&child) {
            continue;
        }

        let is_dir = metadata.is_dir();
        if is_ignored_by_matchers(&child, is_dir, active_matchers) {
            continue;
        }

        add_entry(base, &child, prefix, entries)?;
        if is_dir && should_descend(current_depth, max_folder_depth) {
            visit_snapshot_directory(
                &child,
                base,
                prefix,
                current_depth + 1,
                max_folder_depth,
                active_matchers,
                entries,
            )?;
        }
    }

    active_matchers.truncate(previous_len);
    Ok(())
}

fn collect_ignore_matchers(
    directory: &Path,
    active_indices: &mut Vec<usize>,
    matchers: &mut Vec<ScopedIgnoreMatcher>,
) -> Result<()> {
    let previous_len = active_indices.len();
    if let Some(matcher) = load_directory_ignore_matcher(directory)? {
        matchers.push(matcher);
        active_indices.push(matchers.len() - 1);
    }

    for child in sorted_directory_children(directory)? {
        let metadata = fs::symlink_metadata(&child)
            .with_context(|| format!("failed to inspect {}", child.display()))?;
        if metadata.file_type().is_symlink() || !should_keep_path(&child) {
            continue;
        }

        let is_dir = metadata.is_dir();
        if is_ignored_by_active_indices(&child, is_dir, active_indices, matchers) {
            continue;
        }

        if is_dir {
            collect_ignore_matchers(&child, active_indices, matchers)?;
        }
    }

    active_indices.truncate(previous_len);
    Ok(())
}

fn load_directory_ignore_matcher(directory: &Path) -> Result<Option<ScopedIgnoreMatcher>> {
    let path = directory.join(SYNLY_IGNORE_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            let (matcher, error) = Gitignore::new(&path);
            if let Some(error) = error {
                return Err(error).with_context(|| format!("failed to parse {}", path.display()));
            }
            Ok(Some(ScopedIgnoreMatcher {
                directory: directory.to_path_buf(),
                matcher,
            }))
        }
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn sorted_directory_children(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut children = fs::read_dir(directory)
        .with_context(|| format!("failed to read directory {}", directory.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .with_context(|| format!("failed to read entry under {}", directory.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    children.sort();
    Ok(children)
}

fn is_ignored_by_matchers(path: &Path, is_dir: bool, matchers: &[ScopedIgnoreMatcher]) -> bool {
    let mut decision = None;
    for matcher in matchers {
        if !path.starts_with(&matcher.directory) {
            continue;
        }

        let matched = matcher.matcher.matched_path_or_any_parents(path, is_dir);
        if matched.is_ignore() {
            decision = Some(true);
        } else if matched.is_whitelist() {
            decision = Some(false);
        }
    }
    decision.unwrap_or(false)
}

fn is_ignored_by_active_indices(
    path: &Path,
    is_dir: bool,
    active_indices: &[usize],
    matchers: &[ScopedIgnoreMatcher],
) -> bool {
    let mut decision = None;
    for &index in active_indices {
        let matcher = &matchers[index];
        if !path.starts_with(&matcher.directory) {
            continue;
        }

        let matched = matcher.matcher.matched_path_or_any_parents(path, is_dir);
        if matched.is_ignore() {
            decision = Some(true);
        } else if matched.is_whitelist() {
            decision = Some(false);
        }
    }
    decision.unwrap_or(false)
}

fn should_keep_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    !is_ignored_name(&name)
}

fn is_ignored_name(name: &str) -> bool {
    name == ".git"
        || name == SYNLY_INTERNAL_DIR
        || name == ".DS_Store"
        || name.eq_ignore_ascii_case("desktop.ini")
        || name.ends_with(".synly.part")
}

fn ensure_no_symlink_ancestors(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();

    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }

        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "incoming path {} escapes through symlink ancestor {}",
                    relative.display(),
                    current.display()
                );
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }

    Ok(())
}

fn archive_deleted_path(root: &Path, wire_path: &str, path: &Path) -> Result<()> {
    let deleted_root = deleted_archive_root(root);
    fs::create_dir_all(&deleted_root).with_context(|| {
        format!(
            "failed to create deleted archive {}",
            deleted_root.display()
        )
    })?;

    let bucket = unique_deleted_bucket(&deleted_root)?;
    let archive_path = bucket.join(wire_to_relative_path(wire_path)?);
    if let Some(parent) = archive_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create deleted archive directory {}",
                parent.display()
            )
        })?;
    }

    fs::rename(path, &archive_path).with_context(|| {
        format!(
            "failed to archive {} into {}",
            path.display(),
            archive_path.display()
        )
    })?;
    Ok(())
}

fn deleted_archive_root(root: &Path) -> PathBuf {
    root.join(SYNLY_INTERNAL_DIR).join(SYNLY_DELETED_DIR)
}

fn unique_deleted_bucket(deleted_root: &Path) -> Result<PathBuf> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    for attempt in 0..10_000u32 {
        let candidate = deleted_root.join(format!("{timestamp_ms}-{attempt:04}"));
        if !candidate.exists() {
            fs::create_dir_all(&candidate).with_context(|| {
                format!(
                    "failed to create deleted archive bucket {}",
                    candidate.display()
                )
            })?;
            return Ok(candidate);
        }
    }

    bail!(
        "failed to allocate a unique deleted archive bucket under {}",
        deleted_root.display()
    );
}

fn is_ancestor_path(ancestor: &str, path: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn modified_time_ms(time: &Option<SystemTime>) -> u64 {
    time.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn path_depth(path: &str) -> usize {
    path.split('/').count()
}

fn snapshot_folder_depth(layout: SnapshotLayout, path: &str) -> usize {
    match layout {
        SnapshotLayout::RootContents => path_depth(path).saturating_sub(1),
        SnapshotLayout::SelectedItems => path_depth(path).saturating_sub(2),
    }
}

fn should_descend(current_depth: usize, max_folder_depth: Option<usize>) -> bool {
    max_folder_depth.is_none_or(|max_folder_depth| current_depth < max_folder_depth)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use uuid::Uuid;
    use walkdir::WalkDir;

    fn file_entry(hash: &str) -> ManifestEntry {
        ManifestEntry {
            kind: EntryKind::File,
            size: 1,
            modified_ms: 1,
            hash: Some(hash.to_string()),
            executable: false,
        }
    }

    fn dir_entry() -> ManifestEntry {
        ManifestEntry {
            kind: EntryKind::Dir,
            size: 0,
            modified_ms: 1,
            hash: None,
            executable: false,
        }
    }

    #[test]
    fn wire_path_validation_rejects_parent_segments() {
        assert!(wire_to_relative_path("../etc/passwd").is_err());
        assert!(wire_to_relative_path("a/../../b").is_err());
        assert!(wire_to_relative_path("safe/path").is_ok());
    }

    #[test]
    fn ignores_platform_metadata_files() {
        assert!(is_ignored_name(".DS_Store"));
        assert!(is_ignored_name("desktop.ini"));
        assert!(is_ignored_name("Desktop.ini"));
        assert!(!is_ignored_name("desktop.ini.backup"));
        assert!(!is_ignored_name("notes.txt"));
    }

    #[test]
    fn selected_items_delete_scope_stays_inside_shared_items() {
        let remote = ManifestSnapshot {
            layout: SnapshotLayout::SelectedItems,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "docs/readme.txt".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 1,
                    modified_ms: 1,
                    hash: Some("a".into()),
                    executable: false,
                },
            )]),
        };

        let local = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([
                (
                    "docs/old.txt".to_string(),
                    ManifestEntry {
                        kind: EntryKind::File,
                        size: 1,
                        modified_ms: 1,
                        hash: Some("b".into()),
                        executable: false,
                    },
                ),
                (
                    "notes/private.txt".to_string(),
                    ManifestEntry {
                        kind: EntryKind::File,
                        size: 1,
                        modified_ms: 1,
                        hash: Some("c".into()),
                        executable: false,
                    },
                ),
            ]),
        };

        let plan = build_apply_plan(&remote, &local, DeletePolicy::MirrorSelectedItems);
        assert_eq!(plan.delete_paths, vec!["docs/old.txt".to_string()]);
    }

    #[test]
    fn file_off_workspace_reports_file_sync_disabled() {
        let workspace = WorkspaceSpec::for_off();

        assert!(!workspace.file_sync_enabled());
        assert!(!workspace.can_send_files());
        assert!(!workspace.can_receive_files());

        let lines = workspace.local_human_lines(ClipboardMode::Both);
        assert!(lines.iter().any(|line| line.contains("文件同步: 关闭")));

        let summary = workspace.summary(ClipboardMode::Both);
        assert!(!summary.file_sync_enabled());
        let remote_lines = summary.human_lines();
        assert!(
            remote_lines
                .iter()
                .any(|line| line.contains("文件同步: 关闭"))
        );
    }

    #[test]
    fn metadata_change_requires_resync() {
        let remote = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "bin/tool".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 42,
                    modified_ms: 200,
                    hash: Some("same".into()),
                    executable: true,
                },
            )]),
        };

        let local = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "bin/tool".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 42,
                    modified_ms: 100,
                    hash: Some("same".into()),
                    executable: false,
                },
            )]),
        };

        let plan = build_apply_plan(&remote, &local, DeletePolicy::Never);
        assert_eq!(plan.file_requests, vec!["bin/tool".to_string()]);
        assert!(plan.skipped_newer_paths.is_empty());
        assert!(plan.unreliable_timestamp_paths.is_empty());
    }

    #[test]
    fn newer_local_file_skips_overwrite_request() {
        let remote = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "bin/tool".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 42,
                    modified_ms: 100,
                    hash: Some("remote".into()),
                    executable: false,
                },
            )]),
        };

        let local = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "bin/tool".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 24,
                    modified_ms: 200,
                    hash: Some("local".into()),
                    executable: false,
                },
            )]),
        };

        let plan = build_apply_plan(&remote, &local, DeletePolicy::Never);
        assert!(plan.file_requests.is_empty());
        assert_eq!(plan.skipped_newer_paths, vec!["bin/tool".to_string()]);
        assert!(plan.unreliable_timestamp_paths.is_empty());
    }

    #[test]
    fn clock_delta_is_applied_before_deciding_local_is_newer() {
        let remote = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "bin/tool".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 42,
                    modified_ms: 3_601_000,
                    hash: Some("remote".into()),
                    executable: false,
                },
            )]),
        };

        let local = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "bin/tool".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 24,
                    modified_ms: 1_500,
                    hash: Some("local".into()),
                    executable: false,
                },
            )]),
        };

        let plan = build_apply_plan_with_time(
            &remote,
            &local,
            DeletePolicy::Never,
            TimestampComparisonContext {
                remote_clock_delta_ms: 3_600_000,
                local_now_ms: Some(10_000),
                remote_now_ms: Some(3_610_000),
                skew_tolerance_ms: 100,
                future_guard_ms: 60_000,
            },
        );

        assert!(plan.file_requests.is_empty());
        assert_eq!(plan.skipped_newer_paths, vec!["bin/tool".to_string()]);
        assert!(plan.unreliable_timestamp_paths.is_empty());
    }

    #[test]
    fn suspicious_future_timestamp_disables_pause_on_local_newer() {
        let remote = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "bin/tool".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 42,
                    modified_ms: 100,
                    hash: Some("remote".into()),
                    executable: false,
                },
            )]),
        };

        let local = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "bin/tool".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 24,
                    modified_ms: 1_000_000,
                    hash: Some("local".into()),
                    executable: false,
                },
            )]),
        };

        let plan = build_apply_plan_with_time(
            &remote,
            &local,
            DeletePolicy::Never,
            TimestampComparisonContext {
                remote_clock_delta_ms: 0,
                local_now_ms: Some(1_000),
                remote_now_ms: Some(1_000),
                skew_tolerance_ms: 100,
                future_guard_ms: 10_000,
            },
        );

        assert_eq!(plan.file_requests, vec!["bin/tool".to_string()]);
        assert!(plan.skipped_newer_paths.is_empty());
        assert_eq!(
            plan.unreliable_timestamp_paths,
            vec!["bin/tool".to_string()]
        );
    }

    #[test]
    fn root_contents_watch_target_is_recursive() {
        let root = PathBuf::from("/tmp/workspace");
        let targets = watch_targets(&OutgoingSpec::RootContents {
            root: root.clone(),
            max_folder_depth: None,
        })
        .unwrap();

        assert_eq!(
            targets,
            vec![WatchTarget {
                path: root,
                recursive: true,
            }]
        );
    }

    #[test]
    fn selected_items_watch_targets_use_parent_for_files() {
        let targets = watch_targets(&OutgoingSpec::SelectedItems {
            items: vec![
                NamedItem {
                    name: "workspace".to_string(),
                    path: PathBuf::from("/tmp/workspace"),
                    is_dir: true,
                },
                NamedItem {
                    name: "notes.txt".to_string(),
                    path: PathBuf::from("/tmp/workspace/notes.txt"),
                    is_dir: false,
                },
                NamedItem {
                    name: "todo.txt".to_string(),
                    path: PathBuf::from("/tmp/workspace/todo.txt"),
                    is_dir: false,
                },
            ],
            max_folder_depth: None,
        })
        .unwrap();

        assert_eq!(
            targets,
            vec![WatchTarget {
                path: PathBuf::from("/tmp/workspace"),
                recursive: true,
            }]
        );
    }

    #[test]
    fn snapshot_file_lookup_rejects_unadvertised_paths() {
        let snapshot = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([(
                "docs/readme.txt".to_string(),
                ManifestEntry {
                    kind: EntryKind::File,
                    size: 1,
                    modified_ms: 1,
                    hash: Some("x".into()),
                    executable: false,
                },
            )]),
        };

        assert!(snapshot_contains_file(&snapshot, "docs/readme.txt").unwrap());
        assert!(!snapshot_contains_file(&snapshot, "docs/../../etc/passwd").unwrap_or(false));
        assert!(!snapshot_contains_file(&snapshot, "docs/private.txt").unwrap());
    }

    #[test]
    fn root_contents_snapshot_ignores_synly_directory() {
        let root = test_dir("ignores-synly");
        fs::create_dir_all(root.join(".synly/deleted")).unwrap();
        fs::write(root.join(".synly/deleted/archived.txt"), "old").unwrap();
        fs::write(root.join("keep.txt"), "new").unwrap();

        let snapshot = build_snapshot(&OutgoingSpec::RootContents {
            root: root.clone(),
            max_folder_depth: None,
        })
        .unwrap();

        assert!(snapshot.entries.contains_key("keep.txt"));
        assert!(
            !snapshot
                .entries
                .keys()
                .any(|path| path.starts_with(".synly"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn root_contents_snapshot_respects_synlyignore() {
        let root = test_dir("root-synlyignore");
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join(".synlyignore"), "dist/\n*.tmp\n!keep.tmp\n").unwrap();
        fs::write(root.join("dist/bundle.js"), "bundle").unwrap();
        fs::write(root.join("docs/readme.txt"), "readme").unwrap();
        fs::write(root.join("drop.tmp"), "drop").unwrap();
        fs::write(root.join("keep.tmp"), "keep").unwrap();

        let snapshot = build_snapshot(&OutgoingSpec::RootContents {
            root: root.clone(),
            max_folder_depth: None,
        })
        .unwrap();

        assert!(snapshot.entries.contains_key(".synlyignore"));
        assert!(snapshot.entries.contains_key("docs/readme.txt"));
        assert!(snapshot.entries.contains_key("keep.tmp"));
        assert!(!snapshot.entries.contains_key("drop.tmp"));
        assert!(!snapshot.entries.keys().any(|path| path.starts_with("dist")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nested_synlyignore_is_respected() {
        let root = test_dir("nested-synlyignore");
        fs::create_dir_all(root.join("docs/sub")).unwrap();
        fs::write(root.join("docs/.synlyignore"), "draft.txt\nsub/\n").unwrap();
        fs::write(root.join("docs/draft.txt"), "draft").unwrap();
        fs::write(root.join("docs/keep.txt"), "keep").unwrap();
        fs::write(root.join("docs/sub/hidden.txt"), "hidden").unwrap();

        let snapshot = build_snapshot(&OutgoingSpec::RootContents {
            root: root.clone(),
            max_folder_depth: None,
        })
        .unwrap();

        assert!(snapshot.entries.contains_key("docs/.synlyignore"));
        assert!(snapshot.entries.contains_key("docs/keep.txt"));
        assert!(!snapshot.entries.contains_key("docs/draft.txt"));
        assert!(
            !snapshot
                .entries
                .keys()
                .any(|path| path.starts_with("docs/sub"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn root_contents_snapshot_honors_max_folder_depth() {
        let root = test_dir("depth-limited-root");
        fs::create_dir_all(root.join("nested/deeper")).unwrap();
        fs::write(root.join("top.txt"), "top").unwrap();
        fs::write(root.join("nested/child.txt"), "child").unwrap();
        fs::write(root.join("nested/deeper/grandchild.txt"), "grandchild").unwrap();

        let snapshot = build_snapshot(&OutgoingSpec::RootContents {
            root: root.clone(),
            max_folder_depth: Some(0),
        })
        .unwrap();

        assert_eq!(snapshot.max_folder_depth, Some(0));
        assert!(snapshot.entries.contains_key("top.txt"));
        assert!(snapshot.entries.contains_key("nested"));
        assert!(!snapshot.entries.contains_key("nested/child.txt"));
        assert!(!snapshot.entries.contains_key("nested/deeper"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn selected_item_depth_filter_preserves_only_visible_levels() {
        let snapshot = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([
                ("docs".to_string(), dir_entry()),
                ("docs/readme.txt".to_string(), file_entry("a")),
                ("docs/sub".to_string(), dir_entry()),
                ("docs/sub/deep.txt".to_string(), file_entry("b")),
            ]),
        };

        let filtered =
            filter_snapshot_by_folder_depth(&snapshot, SnapshotLayout::SelectedItems, Some(0));

        assert!(filtered.entries.contains_key("docs"));
        assert!(filtered.entries.contains_key("docs/readme.txt"));
        assert!(filtered.entries.contains_key("docs/sub"));
        assert!(!filtered.entries.contains_key("docs/sub/deep.txt"));
    }

    #[test]
    fn incoming_filter_respects_synlyignore_for_requests_and_deletes() {
        let root = test_dir("incoming-synlyignore");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".synlyignore"), "secret.txt\ncache/\n").unwrap();
        fs::write(root.join("secret.txt"), "local-only").unwrap();

        let remote = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([
                (".synlyignore".to_string(), file_entry("ignore")),
                ("cache".to_string(), dir_entry()),
                ("cache/data.bin".to_string(), file_entry("cache")),
                ("keep.txt".to_string(), file_entry("keep")),
                ("secret.txt".to_string(), file_entry("secret")),
            ]),
        };

        let filtered_remote = filter_snapshot_for_incoming_root(&root, &remote).unwrap();
        let local = build_incoming_snapshot(&root).unwrap();
        let plan = build_apply_plan(&filtered_remote, &local, DeletePolicy::MirrorAll);

        assert!(filtered_remote.entries.contains_key(".synlyignore"));
        assert!(filtered_remote.entries.contains_key("keep.txt"));
        assert!(!filtered_remote.entries.contains_key("secret.txt"));
        assert!(
            !filtered_remote
                .entries
                .keys()
                .any(|path| path.starts_with("cache"))
        );
        assert_eq!(plan.file_requests, vec!["keep.txt".to_string()]);
        assert_eq!(plan.skipped_newer_paths, vec![".synlyignore".to_string()]);
        assert!(plan.unreliable_timestamp_paths.is_empty());
        assert!(plan.delete_paths.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_paths_moves_entries_into_synly_deleted_without_collisions() {
        let root = test_dir("archives-delete");
        fs::create_dir_all(&root).unwrap();

        fs::write(root.join("sample.txt"), "first").unwrap();
        assert!(matches!(
            try_delete_path(&root, "sample.txt"),
            Ok(DeleteDisposition::Archived)
        ));

        fs::write(root.join("sample.txt"), "second").unwrap();
        assert!(matches!(
            try_delete_path(&root, "sample.txt"),
            Ok(DeleteDisposition::Archived)
        ));

        assert!(!root.join("sample.txt").exists());

        let archived = WalkDir::new(root.join(".synly/deleted"))
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.file_name() == "sample.txt")
            .count();
        assert_eq!(archived, 2);

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn incoming_path_rejects_symlink_ancestor() {
        use std::os::unix::fs::symlink;

        let root = test_dir("symlink-ancestor");
        fs::create_dir_all(&root).unwrap();
        let outside = test_dir("symlink-outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();

        assert!(resolve_incoming_path(&root, "escape/file.txt").is_err());

        let _ = fs::remove_file(root.join("escape"));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    fn test_dir(prefix: &str) -> PathBuf {
        env::temp_dir().join(format!("synly-{prefix}-{}", Uuid::new_v4()))
    }
}
