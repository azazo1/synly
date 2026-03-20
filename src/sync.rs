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

const SYNLY_INTERNAL_DIR: &str = ".synly";
const SYNLY_DELETED_DIR: &str = "deleted";

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchTarget {
    pub path: PathBuf,
    pub recursive: bool,
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

pub fn watch_targets(spec: &OutgoingSpec) -> Result<Vec<WatchTarget>> {
    let mut targets = BTreeMap::<PathBuf, bool>::new();

    match spec {
        OutgoingSpec::RootContents { root } => {
            targets.insert(root.clone(), true);
        }
        OutgoingSpec::SelectedItems { items } => {
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

pub fn delete_paths(root: &Path, wire_paths: &[String]) -> Result<()> {
    for wire_path in wire_paths {
        let path = resolve_incoming_path(root, wire_path)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => archive_deleted_path(root, wire_path, &path)?,
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

    #[test]
    fn root_contents_watch_target_is_recursive() {
        let root = PathBuf::from("/tmp/workspace");
        let targets = watch_targets(&OutgoingSpec::RootContents { root: root.clone() }).unwrap();

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

        let snapshot = build_snapshot(&OutgoingSpec::RootContents { root: root.clone() }).unwrap();

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
    fn delete_paths_moves_entries_into_synly_deleted_without_collisions() {
        let root = test_dir("archives-delete");
        fs::create_dir_all(&root).unwrap();

        fs::write(root.join("sample.txt"), "first").unwrap();
        delete_paths(&root, &["sample.txt".to_string()]).unwrap();

        fs::write(root.join("sample.txt"), "second").unwrap();
        delete_paths(&root, &["sample.txt".to_string()]).unwrap();

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
