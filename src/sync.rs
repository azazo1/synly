use crate::cli::SyncMode;
use anyhow::{Context, Result, bail};
use filetime::{FileTime, set_file_mtime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::{DirEntry, WalkDir};

#[derive(Clone, Debug)]
pub struct WorkspaceSpec {
    pub mode: SyncMode,
    pub outgoing: Option<OutgoingSpec>,
    pub incoming_root: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub enum OutgoingSpec {
    RootContents { root: PathBuf },
    SelectedItems { items: Vec<NamedItem> },
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
}

impl WorkspaceSpec {
    pub fn for_send(paths: Vec<PathBuf>) -> Result<Self> {
        if paths.is_empty() {
            bail!("at least one path is required for send mode");
        }

        let canonical_paths = canonicalize_all(paths)?;
        let outgoing = if canonical_paths.len() == 1 && canonical_paths[0].is_dir() {
            OutgoingSpec::RootContents {
                root: canonical_paths[0].clone(),
            }
        } else {
            OutgoingSpec::SelectedItems {
                items: build_named_items(canonical_paths)?,
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
            outgoing: Some(OutgoingSpec::RootContents { root: root.clone() }),
            incoming_root: Some(root),
        })
    }

    pub fn for_auto(root: PathBuf) -> Result<Self> {
        let root = ensure_directory(root)?;
        Ok(Self {
            mode: SyncMode::Auto,
            outgoing: Some(OutgoingSpec::RootContents { root: root.clone() }),
            incoming_root: Some(root),
        })
    }

    pub fn summary(&self) -> WorkspaceSummary {
        let (send_description, send_layout, send_items) = match &self.outgoing {
            Some(OutgoingSpec::RootContents { root }) => (
                Some(format!("同步目录内容: {}", root.display())),
                Some(SnapshotLayout::RootContents),
                Vec::new(),
            ),
            Some(OutgoingSpec::SelectedItems { items }) => (
                Some(format!("同步 {} 个指定文件/文件夹", items.len())),
                Some(SnapshotLayout::SelectedItems),
                items.iter().map(|item| item.name.clone()).collect(),
            ),
            None => (None, None, Vec::new()),
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
        }
    }

    pub fn local_human_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("模式: {}", self.mode.label())];

        match &self.outgoing {
            Some(OutgoingSpec::RootContents { root }) => {
                lines.push(format!("发送目录: {}", root.display()));
            }
            Some(OutgoingSpec::SelectedItems { items }) => {
                for item in items {
                    lines.push(format!("发送条目: {}", item.path.display()));
                }
            }
            None => {}
        }

        if let Some(root) = &self.incoming_root {
            lines.push(format!("接收目录: {}", root.display()));
        }

        lines
    }
}

impl WorkspaceSummary {
    pub fn human_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("模式: {}", self.mode.label())];

        if let Some(description) = &self.send_description {
            lines.push(format!("发送: {}", description));
        }
        if !self.send_items.is_empty() {
            lines.push(format!("发送条目: {}", self.send_items.join(", ")));
        }
        if let Some(root) = &self.receive_root {
            lines.push(format!("接收目录: {}", root));
        }

        lines
    }
}

pub fn build_snapshot(spec: &OutgoingSpec) -> Result<ManifestSnapshot> {
    let mut entries = BTreeMap::new();
    match spec {
        OutgoingSpec::RootContents { root } => {
            for entry in WalkDir::new(root)
                .min_depth(1)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(should_keep_entry)
            {
                let entry = entry?;
                add_entry(root, entry.path(), None, &mut entries)?;
            }

            Ok(ManifestSnapshot {
                layout: SnapshotLayout::RootContents,
                entries,
            })
        }
        OutgoingSpec::SelectedItems { items } => {
            for item in items {
                if item.is_dir {
                    for entry in WalkDir::new(&item.path)
                        .min_depth(0)
                        .sort_by_file_name()
                        .into_iter()
                        .filter_entry(should_keep_entry)
                    {
                        let entry = entry?;
                        add_entry(&item.path, entry.path(), Some(&item.name), &mut entries)?;
                    }
                } else {
                    add_entry(&item.path, &item.path, Some(&item.name), &mut entries)?;
                }
            }

            Ok(ManifestSnapshot {
                layout: SnapshotLayout::SelectedItems,
                entries,
            })
        }
    }
}

pub fn build_incoming_snapshot(root: &Path) -> Result<ManifestSnapshot> {
    build_snapshot(&OutgoingSpec::RootContents {
        root: root.to_path_buf(),
    })
}

pub fn build_apply_plan(
    remote: &ManifestSnapshot,
    local: &ManifestSnapshot,
    delete_policy: DeletePolicy,
) -> ApplyPlan {
    let file_requests = remote
        .entries
        .iter()
        .filter_map(|(path, remote_entry)| {
            if remote_entry.kind != EntryKind::File {
                return None;
            }

            match local.entries.get(path) {
                Some(local_entry)
                    if local_entry.kind == EntryKind::File
                        && local_entry.hash == remote_entry.hash
                        && local_entry.size == remote_entry.size
                        && local_entry.modified_ms == remote_entry.modified_ms
                        && local_entry.executable == remote_entry.executable =>
                {
                    None
                }
                _ => Some(path.clone()),
            }
        })
        .collect::<Vec<_>>();

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
    }
}

pub fn resolve_outgoing_path(spec: &OutgoingSpec, wire_path: &str) -> Result<PathBuf> {
    let relative = wire_to_relative_path(wire_path)?;
    match spec {
        OutgoingSpec::RootContents { root } => Ok(root.join(relative)),
        OutgoingSpec::SelectedItems { items } => {
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

pub fn resolve_incoming_path(root: &Path, wire_path: &str) -> Result<PathBuf> {
    Ok(root.join(wire_to_relative_path(wire_path)?))
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
        if let Ok(metadata) = fs::metadata(&directory)
            && metadata.is_file()
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

pub fn delete_paths(root: &Path, wire_paths: &[String]) -> Result<()> {
    for wire_path in wire_paths {
        let path = resolve_incoming_path(root, wire_path)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    fs::remove_dir_all(&path).with_context(|| {
                        format!("failed to remove directory {}", path.display())
                    })?;
                } else {
                    fs::remove_file(&path)
                        .with_context(|| format!("failed to remove file {}", path.display()))?;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to inspect path {}", path.display()));
            }
        }
    }

    Ok(())
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

    paths.sort_by(|left, right| right.1.cmp(&left.1));
    paths.into_iter().map(|(path, _)| path).collect()
}

fn remote_selected_scopes(snapshot: &ManifestSnapshot) -> BTreeSet<String> {
    snapshot
        .entries
        .keys()
        .filter_map(|path| path.split('/').next())
        .map(|item| item.to_string())
        .collect()
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

fn should_keep_entry(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    name != ".git" && !name.ends_with(".synly.part")
}

fn modified_time_ms(time: &Option<SystemTime>) -> u64 {
    time.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn path_depth(path: &str) -> usize {
    path.split('/').count()
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

    #[test]
    fn wire_path_validation_rejects_parent_segments() {
        assert!(wire_to_relative_path("../etc/passwd").is_err());
        assert!(wire_to_relative_path("a/../../b").is_err());
        assert!(wire_to_relative_path("safe/path").is_ok());
    }

    #[test]
    fn selected_items_delete_scope_stays_inside_shared_items() {
        let remote = ManifestSnapshot {
            layout: SnapshotLayout::SelectedItems,
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
    fn metadata_change_requires_resync() {
        let remote = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
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
    }
}
