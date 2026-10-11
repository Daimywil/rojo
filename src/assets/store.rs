//! The server side of asset sync: an index of every asset file in each asset
//! directory, and the operations clients use to keep it in sync with Studio.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs, io,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::{bail, Context};
use memofs::Vfs;
use rayon::prelude::*;
use rbx_dom_weak::types::Ref;

use crate::{message_queue::MessageQueue, snapshot::RojoTree};

use super::{
    cache::{self, FileEntry},
    compare,
    names::{self, is_under, keys_at_or_under},
    plan::{decide, AssetSides, Decision, PlanAction, PlanReason},
    rbxm::{self, ChunkCompression},
    state::{BaseState, Group, Hash, ShortHash},
};

/// A change to an asset file that clients should know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetChange {
    pub container: Ref,
    pub path: String,
    pub removed: bool,

    /// The client whose request caused this change, if any. That client
    /// already has the change, so it can ignore it.
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetMessage {
    Change(AssetChange),

    /// The set of asset directories changed, for example because the project
    /// file was edited. Clients should start their asset sync over.
    ContainersChanged,
}

/// Public description of an asset directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    /// The instance the assets are synced into.
    pub id: Ref,

    /// The asset directory, relative to the project folder.
    pub key: String,

    pub root: PathBuf,
}

#[derive(Debug)]
struct Container {
    info: ContainerInfo,

    /// The asset directory, canonicalized the same way the VFS canonicalizes
    /// paths of filesystem events. Only known once the directory exists.
    canonical_root: Option<PathBuf>,

    /// Every asset file in the directory by path, or `None` if the directory
    /// has not been scanned yet.
    files: Option<BTreeMap<String, FileEntry>>,
}

/// Studio's version of an asset, uploaded while planning a sync.
struct StagedAsset {
    raw: Vec<u8>,
    raw_hash: Hash,
    canonical_hash: Hash,
}

/// A file operation requested by a client during live sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    Write { path: String, data: Vec<u8> },
    Remove { path: String },
    Move { from: String, to: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub path: String,
    pub action: PlanAction,
    pub reason: Option<PlanReason>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PlanResult {
    /// Paths whose Studio contents have to be uploaded before the plan can be
    /// final.
    pub need_upload: Vec<String>,

    /// Every asset that is not in sync and what to do about it.
    pub actions: Vec<PlanEntry>,

    /// Whether this place has never synced this container before, meaning
    /// nothing is known about which side changed what.
    pub first_sync: bool,
}

/// An asset file read for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadEntry {
    pub path: String,
    pub data: Vec<u8>,
    pub hash: Hash,
}

/// An asset that a place synced before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedAsset {
    pub path: String,
    /// What Studio serialized the asset to when it was last synced.
    pub studio: Hash,
    /// What Studio serialized it to in earlier syncs.
    pub older: Vec<ShortHash>,
}

/// What's known about the assets of a container, which the plugin uses to
/// give assets the same file names they had before.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KnownAssets {
    /// Every asset the place synced before, including ones whose files were
    /// deleted since.
    pub synced: Vec<SyncedAsset>,
    /// Every asset file on disk.
    pub files: Vec<String>,
    /// Groups of assets that were in sync when the place last started
    /// syncing.
    pub groups: Vec<Group>,
}

/// Records that a client applied a file to Studio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckEntry {
    pub path: String,
    /// Hash of the file contents the client applied.
    pub fs_hash: Hash,
    /// Hash of what Studio serialized the asset to after applying it.
    pub studio_hash: Hash,
}

struct Inner {
    project_folder: PathBuf,
    containers: Vec<Container>,
    state: BaseState,
    staging: HashMap<(String, Ref), HashMap<String, StagedAsset>>,
    vfs: Option<Arc<Vfs>>,

    /// The index of every asset directory as of the last run, by key. Taken
    /// when the directory is first scanned.
    cached_index: BTreeMap<String, BTreeMap<String, FileEntry>>,

    /// Whether an index changed since the cache was last saved.
    index_changed: bool,
}

/// Keeps track of the asset directories of a project.
pub struct AssetStore {
    inner: Mutex<Inner>,
    messages: MessageQueue<AssetMessage>,
}

pub fn hash_bytes(bytes: &[u8]) -> Hash {
    *blake3::hash(bytes).as_bytes()
}

/// Hashes the canonical form of a binary model without allocating when the
/// model is already canonical, which every asset file written by Rojo is.
fn canonical_hash(contents: &[u8]) -> Result<Hash, rbxm::RbxmError> {
    if rbxm::is_canonical(contents)? {
        Ok(hash_bytes(contents))
    } else {
        Ok(hash_bytes(&rbxm::canonicalize(contents)?))
    }
}

/// Whether an asset file only differs from Studio's version of it by rounding
/// in some floats. The file must still have the contents it was indexed with,
/// so that a file that's being changed on disk isn't mistaken for either.
fn file_only_differs_by_noise(root: &Path, path: &str, entry: &FileEntry, studio: &[u8]) -> bool {
    let Ok(contents) = fs::read(absolute_path(root, path)) else {
        return false;
    };

    canonical_hash(&contents).is_ok_and(|hash| hash == entry.hash)
        && compare::same_except_float_noise(&contents, studio)
}

/// The result of reading an asset file from disk.
enum ReadOutcome {
    Valid(FileEntry),
    Missing,

    /// The file exists but couldn't be read or isn't a valid model right now.
    /// This happens while another program is still writing it, since git and
    /// most other tools don't write files atomically. Treating this as a
    /// deletion would delete the asset from Studio, so these files keep
    /// whatever was last known about them until they can be read.
    Unreadable,
}

fn read_entry(path: &Path) -> ReadOutcome {
    let read = fs::read(path).and_then(|contents| Ok((contents, fs::metadata(path)?)));

    let (contents, metadata) = match read {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return ReadOutcome::Missing,
        Err(err) => {
            log::debug!("Could not read asset file {}: {err}", path.display());
            return ReadOutcome::Unreadable;
        }
    };

    match canonical_hash(&contents) {
        Ok(hash) => ReadOutcome::Valid(FileEntry {
            hash,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }),
        Err(err) => {
            log::debug!(
                "Asset file {} is not a valid Roblox model right now: {err}",
                path.display()
            );
            ReadOutcome::Unreadable
        }
    }
}

/// Whether a file looks unchanged since it was indexed, which saves reading
/// and hashing it again.
fn metadata_matches(metadata: &fs::Metadata, entry: &FileEntry) -> bool {
    metadata.len() == entry.len
        && metadata
            .modified()
            .is_ok_and(|modified| Some(modified) == entry.modified)
}

fn stat_matches(path: &Path, entry: &FileEntry) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata_matches(&metadata, entry))
}

/// Reads a file unless it looks unchanged since it was indexed.
fn read_entry_if_changed(path: &Path, known: Option<&FileEntry>) -> ReadOutcome {
    match known {
        Some(entry) if stat_matches(path, entry) => ReadOutcome::Valid(*entry),
        _ => read_entry(path),
    }
}

/// Directory listings, used to tell whether paths exist with exactly the case
/// they are spelled with. Case-insensitive filesystems find a file by any case,
/// so without checking, the old name of a file renamed from `rock.rbxm` to
/// `Rock.rbxm` would still seem to exist.
struct DirListings {
    names: HashMap<String, HashSet<String>>,
}

impl DirListings {
    /// Lists every directory that contains one of the given asset paths or
    /// one of their ancestors.
    fn new<'a>(root: &Path, paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut dirs = BTreeSet::from([""]);
        for path in paths {
            let mut current = path;
            while let Some((parent, _)) = current.rsplit_once('/') {
                if !dirs.insert(parent) {
                    break;
                }
                current = parent;
            }
        }

        let names = dirs
            .into_par_iter()
            .filter_map(|dir| {
                let entries = fs::read_dir(absolute_path(root, dir)).ok()?;
                let names = entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .collect();
                Some((dir.to_owned(), names))
            })
            .collect();

        Self { names }
    }

    /// Whether every component of an asset path exists exactly as spelled.
    fn exists_exactly(&self, path: &str) -> bool {
        if path.is_empty() {
            return true;
        }

        let mut start = 0;
        loop {
            let end = path[start..]
                .find('/')
                .map_or(path.len(), |offset| start + offset);
            let dir = if start == 0 { "" } else { &path[..start - 1] };

            if !self
                .names
                .get(dir)
                .is_some_and(|names| names.contains(&path[start..end]))
            {
                return false;
            }

            if end == path.len() {
                return true;
            }
            start = end + 1;
        }
    }
}

/// Creates the directory at the asset path `dir` and its ancestors, checking
/// that the ones that already exist have exactly that case. Case-insensitive
/// filesystems would put files meant for `Trees/` into an existing `trees/`,
/// whose name no longer matches the folder it stands for.
///
/// `checked` remembers directories that are known to be fine, so each one is
/// only checked once per batch.
fn create_dirs_exactly(root: &Path, dir: &str, checked: &mut HashSet<String>) -> io::Result<()> {
    if dir.is_empty() || checked.contains(dir) {
        return Ok(());
    }

    let mut end = 0;
    while end < dir.len() {
        end = dir[end..]
            .find('/')
            .map_or(dir.len(), |offset| end + offset);
        let current = &dir[..end];
        end += 1;

        if checked.contains(current) {
            continue;
        }

        let absolute = absolute_path(root, current);
        match fs::create_dir(&absolute) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let name = absolute.file_name().unwrap();
                let exact = fs::read_dir(absolute.parent().unwrap())?
                    .flatten()
                    .any(|entry| entry.file_name() == name);

                if !exact {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{current} exists with different capitalization"),
                    ));
                }
            }
            Err(err) => return Err(err),
        }

        checked.insert(current.to_owned());
    }

    Ok(())
}

/// Writes a file without ever leaving a partially written file behind, since
/// a truncated asset would be loaded into Studio as a broken model.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;

    // Temporary files start with a dot so that the store never mistakes them
    // for assets.
    let file_name = path.file_name().unwrap().to_string_lossy();
    let temp_path = parent.join(format!(".{file_name}.rojo-tmp"));
    fs::write(&temp_path, contents)?;

    if let Err(err) = fs::rename(&temp_path, path) {
        // Renaming over a file can fail on Windows if another program has it
        // open. Writing in place is less safe, but better than not syncing.
        log::debug!(
            "Could not atomically replace {}: {err}. Writing in place instead.",
            path.display()
        );
        let _ = fs::remove_file(&temp_path);
        fs::write(path, contents)?;
    }

    Ok(())
}

/// Removes empty directories from `start` upwards, stopping at `root`.
fn prune_empty_dirs(root: &Path, start: &Path) {
    let mut current = start;
    while current != root && current.starts_with(root) {
        if fs::remove_dir(current).is_err() {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
}

/// Converts an absolute path inside `root` into an asset path. Returns `None`
/// for paths outside of the root, and for hidden or temporary files.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let suffix = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();

    for component in suffix.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_str()?;
                if name.starts_with('.') {
                    return None;
                }
                parts.push(name);
            }
            _ => return None,
        }
    }

    Some(parts.join("/"))
}

/// Converts an asset path into an absolute path. The empty path is the asset
/// directory itself.
fn absolute_path(root: &Path, path: &str) -> PathBuf {
    let mut absolute = root.to_path_buf();
    for component in path.split('/').filter(|component| !component.is_empty()) {
        absolute.push(component);
    }
    absolute
}

/// Collects the asset files below `dir`, whose asset path is `prefix`.
fn collect_unit_files(dir: &Path, prefix: &str, output: &mut Vec<(String, PathBuf)>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                log::warn!("Could not read asset directory {}: {err}", dir.display());
            }
            return;
        }
    };

    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            log::warn!(
                "Ignoring {} because its name is not valid UTF-8",
                entry.path().display()
            );
            continue;
        };

        if name.starts_with('.') {
            continue;
        }

        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            if names::folder_name_from_dir(&name).is_some() {
                collect_unit_files(&entry.path(), &path, output);
            } else {
                log::warn!(
                    "Ignoring directory {} because its name was not created by Rojo",
                    entry.path().display()
                );
            }
        } else if file_type.is_file() && names::is_unit_file_name(&name) {
            output.push((path, entry.path()));
        }
    }
}

/// Reads every asset file below the directory at the asset path `prefix`.
/// Returns the files that could be read, and the paths of files that exist but
/// can't be read right now. Files that look unchanged since they were added
/// to `known` aren't read again.
fn scan_dir(
    root: &Path,
    prefix: &str,
    known: &BTreeMap<String, FileEntry>,
) -> (BTreeMap<String, FileEntry>, BTreeSet<String>) {
    let mut files = Vec::new();
    collect_unit_files(&absolute_path(root, prefix), prefix, &mut files);

    let outcomes: Vec<(String, ReadOutcome)> = files
        .into_par_iter()
        .map(|(path, absolute)| {
            let outcome = read_entry_if_changed(&absolute, known.get(&path));
            (path, outcome)
        })
        .collect();

    let mut valid = BTreeMap::new();
    let mut unreadable = BTreeSet::new();

    for (path, outcome) in outcomes {
        match outcome {
            ReadOutcome::Valid(entry) => {
                valid.insert(path, entry);
            }
            ReadOutcome::Unreadable => {
                unreadable.insert(path);
            }
            ReadOutcome::Missing => {}
        }
    }

    (valid, unreadable)
}

/// Brings the index entries at or below `path` up to date with the files on
/// disk. Returns the paths whose contents changed, and whether they were
/// removed.
fn sync_index(
    files: &mut BTreeMap<String, FileEntry>,
    path: &str,
    found: BTreeMap<String, FileEntry>,
    unreadable: &BTreeSet<String>,
) -> Vec<(String, bool)> {
    let mut changes = Vec::new();

    let stale: Vec<String> = keys_at_or_under(files, path)
        .filter(|existing| !found.contains_key(*existing) && !unreadable.contains(*existing))
        .cloned()
        .collect();

    for stale_path in stale {
        files.remove(&stale_path);
        changes.push((stale_path, true));
    }

    for (found_path, entry) in found {
        let previous = files.insert(found_path.clone(), entry);
        if previous.map(|previous| previous.hash) != Some(entry.hash) {
            changes.push((found_path, false));
        }
    }

    changes
}

/// Finds every instance declared with `$assets` and the directory it uses.
pub fn find_containers(tree: &RojoTree) -> Vec<(Ref, PathBuf)> {
    tree.descendants(tree.get_root_id())
        .filter_map(|instance| {
            let dir = instance.metadata().asset_dir.as_ref()?;
            Some((instance.id(), dir.clone()))
        })
        .collect()
}

fn container_key(project_folder: &Path, root: &Path) -> String {
    let relative = pathdiff::diff_paths(root, project_folder).unwrap_or_else(|| root.to_owned());
    let key = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");

    if key.is_empty() {
        ".".to_owned()
    } else {
        key
    }
}

impl Inner {
    fn container_index(&self, id: Ref) -> anyhow::Result<usize> {
        self.containers
            .iter()
            .position(|container| container.info.id == id)
            .with_context(|| format!("{id} is not an asset container"))
    }

    /// Scans a container's directory if it hasn't been scanned yet, creating
    /// it and starting to watch it for changes.
    fn ensure_scanned(&mut self, index: usize) -> anyhow::Result<()> {
        if self.containers[index].files.is_some() {
            return Ok(());
        }

        let key = self.containers[index].info.key.clone();
        let root = self.containers[index].info.root.clone();
        fs::create_dir_all(&root)
            .with_context(|| format!("Could not create asset directory {}", root.display()))?;

        let canonical_root = match &self.vfs {
            Some(vfs) => vfs.canonicalize(&root)?,
            None => dunce::canonicalize(&root)?,
        };

        // Reading the directory through the VFS makes it watch the directory
        // recursively, which is how we learn about changes made by git.
        if let Some(vfs) = &self.vfs {
            vfs.read_dir(&canonical_root)?;
        }

        // Files that didn't change since the last run aren't read again.
        let cached = self.cached_index.remove(&key).unwrap_or_default();

        let started = std::time::Instant::now();
        let (files, unreadable) = scan_dir(&canonical_root, "", &cached);
        for path in unreadable {
            log::warn!(
                "Ignoring asset file {path} in {key} because it is not a valid Roblox model"
            );
        }
        log::info!(
            "Indexed {} asset files in {key} in {:.0?}",
            files.len(),
            started.elapsed()
        );

        let container = &mut self.containers[index];
        container.canonical_root = Some(canonical_root);
        container.files = Some(files);

        self.index_changed = true;
        self.save_index_cache();
        Ok(())
    }

    /// Saves the index of every scanned asset directory, so that the next run
    /// doesn't have to read files that didn't change.
    fn save_index_cache(&mut self) {
        if !self.index_changed {
            return;
        }

        let containers = self.containers.iter().filter_map(|container| {
            let files = container.files.as_ref()?;
            Some((container.info.key.as_str(), files))
        });

        match cache::save(&cache::path_for_project(&self.project_folder), containers) {
            Ok(()) => self.index_changed = false,
            Err(err) => log::warn!("Could not save the asset index cache: {err}"),
        }
    }

    fn scanned(&mut self, id: Ref) -> anyhow::Result<usize> {
        let index = self.container_index(id)?;
        self.ensure_scanned(index)?;
        Ok(index)
    }
}

impl AssetStore {
    pub fn new(project_folder: &Path, vfs: Option<Arc<Vfs>>) -> Self {
        let state = BaseState::load(BaseState::path_for_project(project_folder));
        let cached_index = cache::load(&cache::path_for_project(project_folder));

        Self {
            inner: Mutex::new(Inner {
                project_folder: project_folder.to_owned(),
                containers: Vec::new(),
                state,
                staging: HashMap::new(),
                vfs,
                cached_index,
                index_changed: false,
            }),
            messages: MessageQueue::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub fn messages(&self) -> &MessageQueue<AssetMessage> {
        &self.messages
    }

    pub fn containers(&self) -> Vec<ContainerInfo> {
        self.lock()
            .containers
            .iter()
            .map(|container| container.info.clone())
            .collect()
    }

    /// Updates the set of asset directories from the instance tree. Returns
    /// whether anything changed.
    pub fn refresh_containers(&self, tree: &RojoTree) -> bool {
        let mut inner = self.lock();
        let project_folder = inner.project_folder.clone();

        let found = find_containers(tree);
        let mut new_containers = Vec::with_capacity(found.len());
        let mut changed = found.len() != inner.containers.len();

        for (id, root) in found {
            let key = container_key(&project_folder, &root);

            let existing = inner
                .containers
                .iter()
                .position(|container| container.info.root == root);

            match existing {
                Some(index) => {
                    let mut container = inner.containers.swap_remove(index);
                    if container.info.id != id {
                        container.info.id = id;
                        changed = true;
                    }
                    new_containers.push(container);
                }
                None => {
                    changed = true;
                    new_containers.push(Container {
                        info: ContainerInfo { id, key, root },
                        canonical_root: None,
                        files: None,
                    });
                }
            }
        }

        // Two instances syncing assets from the same directory would fight
        // over it, so only the first one gets it.
        let mut seen = BTreeSet::new();
        new_containers.retain(|container| {
            let unique = seen.insert(container.info.root.clone());
            if !unique {
                log::error!(
                    "The asset directory {} is used by more than one instance. Only the first one will be synced.",
                    container.info.key
                );
            }
            unique
        });

        inner.containers = new_containers;
        drop(inner);

        if changed {
            self.messages
                .push_messages(&[AssetMessage::ContainersChanged]);
        }

        changed
    }

    /// The name of the attribute that the plugin stores the session token in.
    /// It's specific to this machine's copy of the project, so people syncing
    /// the same place from different machines don't overwrite each other's.
    pub fn session_attribute(&self) -> String {
        let inner = self.lock();
        format!("__RojoAssetSync_{}", &inner.state.machine()[..12])
    }

    /// Records that a session finished syncing a place, and stored its token
    /// in the place.
    pub fn start_session(&self, place: &str, token: &str) -> anyhow::Result<()> {
        let mut inner = self.lock();
        inner.state.set_session(place, token);
        inner.state.save_if_dirty()?;
        Ok(())
    }

    /// Scans every asset directory that hasn't been scanned yet.
    pub fn scan_all(&self) -> anyhow::Result<()> {
        let mut inner = self.lock();
        for index in 0..inner.containers.len() {
            inner.ensure_scanned(index)?;
        }
        Ok(())
    }

    /// Updates the index for changes on the filesystem. Returns, for every
    /// path, whether it is inside an asset directory, in which case it should
    /// not be processed any further.
    ///
    /// Paths are read in parallel. Files that were just written are often
    /// still being scanned by antivirus software, which makes opening them
    /// slow, so reading the files of a large git checkout one at a time would
    /// take far longer than the checkout itself.
    pub fn handle_fs_changes(&self, paths: &[PathBuf]) -> Vec<bool> {
        let mut inner = self.lock();
        let mut owned = vec![false; paths.len()];
        let mut by_container: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();

        for (path_index, path) in paths.iter().enumerate() {
            let Some(index) = inner.containers.iter().position(|container| {
                container
                    .canonical_root
                    .as_ref()
                    .is_some_and(|root| path.starts_with(root))
            }) else {
                continue;
            };

            owned[path_index] = true;

            let container = &inner.containers[index];
            if container.files.is_none() {
                continue;
            }

            if let Some(relative) = relative_path(container.canonical_root.as_ref().unwrap(), path)
            {
                by_container.entry(index).or_default().insert(relative);
            }
        }

        let mut all_changes = Vec::new();
        if !by_container.is_empty() {
            inner.index_changed = true;
        }

        for (index, relative_paths) in by_container {
            let container = &mut inner.containers[index];
            let root = container.canonical_root.clone().unwrap();
            let files = container.files.as_mut().unwrap();

            let listings = DirListings::new(&root, relative_paths.iter().map(String::as_str));
            let known: &BTreeMap<String, FileEntry> = files;
            let probes: Vec<(String, Option<Probe>)> = relative_paths
                .into_par_iter()
                .map(|path| {
                    let probe = probe_path(&root, &path, known, &listings);
                    (path, probe)
                })
                .collect();

            let mut changes = Vec::new();
            for (path, probe) in probes {
                // We can't tell what happened to the path, so leave the index
                // as it is. A later event or rescan will sort it out.
                if let Some(probe) = probe {
                    changes.extend(sync_index(files, &path, probe.found, &probe.unreadable));
                }
            }

            all_changes.push((container.info.id, changes));
        }

        drop(inner);

        for (id, changes) in all_changes {
            self.push_changes(id, changes, None);
        }

        owned
    }

    /// Updates the index for a single change on the filesystem. See
    /// [`AssetStore::handle_fs_changes`].
    #[cfg(test)]
    pub fn handle_fs_change(&self, path: &Path) -> bool {
        self.handle_fs_changes(&[path.to_path_buf()])[0]
    }

    /// Re-checks every asset file of every scanned directory. Files whose size
    /// and modification time didn't change are not read. This catches changes
    /// that the filesystem watcher missed, which can happen when a huge number
    /// of files change at once.
    ///
    /// The index cache is saved here too, so it's written at most this often.
    pub fn rescan_all(&self) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let mut all_changes = Vec::new();

        for container in &mut inner.containers {
            let (Some(root), Some(files)) = (&container.canonical_root, &mut container.files)
            else {
                continue;
            };

            let mut found = Vec::new();
            collect_unit_files(root, "", &mut found);

            let found_paths: BTreeSet<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
            let mut changes: Vec<(String, Option<FileEntry>)> = files
                .keys()
                .filter(|path| !found_paths.contains(path.as_str()))
                .map(|path| (path.clone(), None))
                .collect();

            let known: &BTreeMap<String, FileEntry> = files;
            let updated: Vec<(String, Option<FileEntry>)> = found
                .par_iter()
                .filter_map(|(path, absolute)| {
                    let existing = known.get(path);
                    if existing.is_some_and(|entry| stat_matches(absolute, entry)) {
                        return None;
                    }

                    match read_entry(absolute) {
                        // Even if the contents didn't change, remember the new
                        // timestamp so the file isn't read again next time.
                        ReadOutcome::Valid(entry) if existing != Some(&entry) => {
                            Some((path.clone(), Some(entry)))
                        }
                        ReadOutcome::Missing if existing.is_some() => Some((path.clone(), None)),
                        _ => None,
                    }
                })
                .collect();

            changes.extend(updated);
            if !changes.is_empty() {
                inner.index_changed = true;
            }

            let mut changed_paths = Vec::new();
            for (path, entry) in changes {
                let previous = match entry {
                    Some(entry) => files.insert(path.clone(), entry),
                    None => files.remove(&path),
                };

                if previous.map(|entry| entry.hash) != entry.map(|entry| entry.hash) {
                    changed_paths.push((path, entry.is_none()));
                }
            }

            if !changed_paths.is_empty() {
                log::debug!(
                    "Rescan of {} found {} changed asset files",
                    container.info.key,
                    changed_paths.len()
                );
                all_changes.push((container.info.id, changed_paths));
            }
        }

        inner.save_index_cache();
        drop(guard);

        for (id, changes) in all_changes {
            self.push_changes(id, changes, None);
        }
    }

    fn push_changes(&self, container: Ref, changes: Vec<(String, bool)>, origin: Option<&str>) {
        if changes.is_empty() {
            return;
        }

        let messages: Vec<AssetMessage> = changes
            .into_iter()
            .map(|(path, removed)| {
                AssetMessage::Change(AssetChange {
                    container,
                    path,
                    removed,
                    origin: origin.map(str::to_owned),
                })
            })
            .collect();

        self.messages.push_messages(&messages);
    }

    /// Computes how to bring Studio and the filesystem in sync, given the hash
    /// of every asset Studio currently has.
    ///
    /// `skipped` lists assets that Studio has but couldn't serialize. Nothing
    /// is decided for them, since it's unknown what they contain.
    ///
    /// `studio_session` is the session token stored in the place, which tells
    /// whether the place was saved after the last session that synced it.
    ///
    /// Assets found to be in sync have their base updated right away. Nothing
    /// else is changed.
    pub fn plan(
        &self,
        client: &str,
        place: &str,
        studio_session: Option<&str>,
        container: Ref,
        studio: &[(String, Hash)],
        skipped: &[String],
    ) -> anyhow::Result<PlanResult> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let index = inner.scanned(container)?;

        // Uploads from clients that never applied them, for example because
        // the user aborted, would otherwise stay in memory forever.
        inner
            .staging
            .retain(|(staged_client, staged_container), _| {
                staged_client == client || *staged_container != container
            });

        let key = inner.containers[index].info.key.clone();
        let root = inner.containers[index].canonical_root.clone();
        let files = inner.containers[index].files.as_ref().unwrap();

        let studio_map: HashMap<&str, Hash> = studio
            .iter()
            .map(|(path, hash)| (path.as_str(), *hash))
            .collect();
        let skipped: BTreeSet<&str> = skipped.iter().map(String::as_str).collect();
        let staged = inner.staging.get(&(client.to_owned(), container));

        // Studio's uploaded version of an asset, if it's still the version
        // Studio has.
        let uploaded = |path: &str| {
            staged
                .and_then(|staged| staged.get(path))
                .filter(|staged| Some(staged.raw_hash) == studio_map.get(path).copied())
        };

        // Uploads that differ from their files are compared more closely,
        // since Studio may only have rounded some floats differently.
        let differing: Vec<(&str, &StagedAsset, &FileEntry)> = studio_map
            .keys()
            .filter_map(|&path| {
                let staged = uploaded(path)?;
                let entry = files.get(path)?;
                (entry.hash != staged.canonical_hash).then_some((path, staged, entry))
            })
            .collect();
        let noise_only: HashSet<&str> = differing
            .into_par_iter()
            .filter(|(path, staged, entry)| {
                root.as_deref()
                    .is_some_and(|root| file_only_differs_by_noise(root, path, entry, &staged.raw))
            })
            .map(|(path, ..)| path)
            .collect();

        let bases = inner
            .state
            .container(place, &key)
            .cloned()
            .unwrap_or_default();

        // A place that never had a session recorded can't have lost one.
        let place_saved = inner
            .state
            .session(place)
            .is_none_or(|last| studio_session == Some(last));

        let mut paths: BTreeSet<&str> = studio_map.keys().copied().collect();
        paths.extend(files.keys().map(String::as_str));
        paths.extend(bases.keys().map(String::as_str));

        let mut result = PlanResult {
            first_sync: bases.is_empty(),
            ..Default::default()
        };
        let mut agreed = Vec::new();

        for path in paths {
            if skipped.contains(path) {
                continue;
            }

            let sides = AssetSides {
                studio: studio_map.get(path).copied(),
                studio_matches_file: uploaded(path).map(|staged| {
                    files
                        .get(path)
                        .is_some_and(|entry| entry.hash == staged.canonical_hash)
                        || noise_only.contains(path)
                }),
                fs: files.get(path).map(|entry| entry.hash),
                base: bases.get(path),
                place_saved,
            };

            match decide(sides) {
                Decision::NeedsUpload => result.need_upload.push(path.to_owned()),
                Decision::Action(PlanAction::InSync, _) => {
                    agreed.push((path.to_owned(), sides.fs.zip(sides.studio)));
                }
                Decision::Action(action, reason) => result.actions.push(PlanEntry {
                    path: path.to_owned(),
                    action,
                    reason,
                }),
            }
        }

        for (path, hashes) in agreed {
            match hashes {
                Some((fs, studio)) => inner.state.record(place, &key, &path, fs, studio),
                None => inner.state.remove(place, &key, &path),
            }
        }

        if let Err(err) = inner.state.save_if_dirty() {
            log::warn!("Could not save asset sync state: {err}");
        }

        Ok(result)
    }

    /// Uploads Studio's version of some assets so they can be compared with
    /// the filesystem or written to it later.
    pub fn stage(
        &self,
        client: &str,
        container: Ref,
        entries: Vec<(String, Vec<u8>)>,
    ) -> anyhow::Result<()> {
        for (path, _) in &entries {
            if !names::is_valid_unit_path(path) {
                bail!("Invalid asset path {path:?}");
            }
        }

        // Canonicalizing is the expensive part, so do it without holding the
        // lock.
        let staged: Vec<(String, StagedAsset)> = entries
            .into_par_iter()
            .map(|(path, raw)| {
                let canonical = rbxm::canonicalize(&raw)
                    .with_context(|| format!("Studio sent an invalid model for {path}"))?;
                let staged = StagedAsset {
                    raw_hash: hash_bytes(&raw),
                    canonical_hash: hash_bytes(&canonical),
                    raw,
                };
                Ok((path, staged))
            })
            .collect::<anyhow::Result<_>>()?;

        let mut inner = self.lock();
        inner.container_index(container)?;
        inner
            .staging
            .entry((client.to_owned(), container))
            .or_default()
            .extend(staged);

        Ok(())
    }

    /// Carries out the parts of a plan that change the filesystem: writing
    /// staged Studio versions and deleting files.
    pub fn apply(
        &self,
        client: &str,
        place: &str,
        container: Ref,
        to_fs: &[String],
        delete_fs: &[String],
    ) -> anyhow::Result<()> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let index = inner.scanned(container)?;

        let mut staged = inner
            .staging
            .remove(&(client.to_owned(), container))
            .unwrap_or_default();

        let mut writes = Vec::with_capacity(to_fs.len());
        for path in to_fs {
            let Some(asset) = staged.remove(path) else {
                bail!("{path} was not uploaded before being applied");
            };
            writes.push((path.clone(), asset.raw));
        }

        let ops: Vec<WriteOp> = delete_fs
            .iter()
            .map(|path| WriteOp::Remove { path: path.clone() })
            .chain(
                writes
                    .into_iter()
                    .map(|(path, data)| WriteOp::Write { path, data }),
            )
            .collect();

        let (changes, failures) = execute_ops(inner, index, place, ops);
        drop(guard);

        self.push_changes(container, changes, Some(client));

        if let Some((path, err)) = failures.into_iter().next() {
            bail!("Could not sync asset {path}: {err}");
        }

        Ok(())
    }

    /// Records that a client applied files to Studio, or deleted assets from
    /// Studio because their files were deleted.
    pub fn ack(
        &self,
        place: &str,
        container: Ref,
        applied: &[AckEntry],
        removed: &[String],
    ) -> anyhow::Result<()> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let index = inner.scanned(container)?;
        let key = inner.containers[index].info.key.clone();

        for entry in applied {
            inner
                .state
                .record(place, &key, &entry.path, entry.fs_hash, entry.studio_hash);
        }

        for path in removed {
            inner.state.remove(place, &key, path);
        }

        if let Err(err) = inner.state.save_if_dirty() {
            log::warn!("Could not save asset sync state: {err}");
        }

        Ok(())
    }

    /// Lists what a place synced before and what's on disk, so that the plugin
    /// can give assets the same file names they had before.
    pub fn known(&self, place: &str, container: Ref) -> anyhow::Result<KnownAssets> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let index = inner.scanned(container)?;
        let container = &inner.containers[index];

        let synced = inner
            .state
            .container(place, &container.info.key)
            .map(|bases| {
                bases
                    .iter()
                    .map(|(path, base)| SyncedAsset {
                        path: path.clone(),
                        studio: base.studio,
                        older: base.older.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(KnownAssets {
            synced,
            files: container.files.as_ref().unwrap().keys().cloned().collect(),
            groups: inner.state.groups(place, &container.info.key).to_vec(),
        })
    }

    /// Remembers the groups of assets that are in sync now that a place
    /// started syncing, replacing the ones it remembered before.
    pub fn set_groups(
        &self,
        place: &str,
        container: Ref,
        groups: Vec<Group>,
    ) -> anyhow::Result<()> {
        let mut inner = self.lock();
        let index = inner.container_index(container)?;
        let key = inner.containers[index].info.key.clone();

        inner.state.set_groups(place, &key, groups);
        inner.state.save_if_dirty()?;
        Ok(())
    }

    /// Applies changes made in Studio during live sync.
    ///
    /// Returns the paths of operations that failed along with the reason, so
    /// the client can retry them.
    pub fn write(
        &self,
        client: &str,
        place: &str,
        container: Ref,
        ops: Vec<WriteOp>,
    ) -> anyhow::Result<Failures> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let index = inner.scanned(container)?;

        let (changes, failures) = execute_ops(inner, index, place, ops);
        drop(guard);

        self.push_changes(container, changes, Some(client));

        Ok(failures)
    }

    /// Reads asset files, compressed for sending to Studio.
    pub fn read(&self, container: Ref, paths: &[String]) -> anyhow::Result<Vec<ReadEntry>> {
        let root = {
            let mut inner = self.lock();
            let index = inner.scanned(container)?;
            inner.containers[index].canonical_root.clone().unwrap()
        };

        paths
            .par_iter()
            .filter_map(|path| {
                if !names::is_valid_unit_path(path) {
                    return Some(Err(anyhow::anyhow!("Invalid asset path {path:?}")));
                }

                let contents = match fs::read(absolute_path(&root, path)) {
                    Ok(contents) => contents,
                    // The file was deleted since the client asked for it. The
                    // client will hear about that separately.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
                    Err(err) => return Some(Err(err.into())),
                };

                let result = (|| -> anyhow::Result<ReadEntry> {
                    let canonical = rbxm::canonicalize(&contents)?;
                    let hash = hash_bytes(&canonical);
                    let data = rbxm::recompress(&canonical, ChunkCompression::Lz4)?;
                    Ok(ReadEntry {
                        path: path.clone(),
                        data,
                        hash,
                    })
                })();

                Some(result.with_context(|| format!("Could not read asset {path}")))
            })
            .collect()
    }

    /// Returns the path and contents of every asset file in a container, for
    /// building models and places.
    pub fn load_all(&self, container: Ref) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let root = {
            let inner = self.lock();
            let index = inner.container_index(container)?;
            inner.containers[index].info.root.clone()
        };

        let mut files = Vec::new();
        collect_unit_files(&root, "", &mut files);

        // Instances are created in this order, which decides the dedup suffixes
        // the plugin gives siblings with the same name.
        files.sort_by(|(a, _), (b, _)| names::compare_paths(a, b));

        files
            .into_par_iter()
            .map(|(path, absolute)| {
                let contents = fs::read(&absolute)
                    .with_context(|| format!("Could not read asset {}", absolute.display()))?;
                Ok((path, contents))
            })
            .collect()
    }

    /// Returns the hash of every indexed asset file in a container.
    #[cfg(test)]
    pub fn file_hashes(&self, container: Ref) -> anyhow::Result<BTreeMap<String, Hash>> {
        let mut inner = self.lock();
        let index = inner.scanned(container)?;
        Ok(inner.containers[index]
            .files
            .as_ref()
            .unwrap()
            .iter()
            .map(|(path, entry)| (path.clone(), entry.hash))
            .collect())
    }
}

/// What's on disk at or below an asset path.
struct Probe {
    found: BTreeMap<String, FileEntry>,
    unreadable: BTreeSet<String>,
}

/// Reads what's on disk at or below `path` without changing the index, so that
/// many paths can be probed in parallel. Returns `None` if it's impossible to
/// tell what happened to the path right now.
///
/// Files that look unchanged since they were indexed aren't read again, which
/// keeps moving thousands of files cheap.
fn probe_path(
    root: &Path,
    path: &str,
    known: &BTreeMap<String, FileEntry>,
    listings: &DirListings,
) -> Option<Probe> {
    let absolute = absolute_path(root, path);

    let nothing = || Probe {
        found: BTreeMap::new(),
        unreadable: BTreeSet::new(),
    };

    if !listings.exists_exactly(path) {
        return Some(nothing());
    }

    match fs::metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() => {
            let representable = path.is_empty()
                || path
                    .split('/')
                    .all(|component| names::folder_name_from_dir(component).is_some());

            if representable {
                let (found, unreadable) = scan_dir(root, path, known);
                Some(Probe { found, unreadable })
            } else {
                Some(nothing())
            }
        }
        Ok(metadata) if metadata.is_file() && names::is_valid_unit_path(path) => {
            let outcome = match known.get(path) {
                Some(entry) if metadata_matches(&metadata, entry) => ReadOutcome::Valid(*entry),
                _ => read_entry(&absolute),
            };

            Some(match outcome {
                ReadOutcome::Valid(entry) => Probe {
                    found: [(path.to_owned(), entry)].into(),
                    unreadable: BTreeSet::new(),
                },
                ReadOutcome::Unreadable => Probe {
                    found: BTreeMap::new(),
                    unreadable: [path.to_owned()].into(),
                },
                ReadOutcome::Missing => nothing(),
            })
        }
        Ok(_) => Some(nothing()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Some(nothing()),
        Err(_) => None,
    }
}

/// Paths whose contents changed, and whether they were removed.
type Changes = Vec<(String, bool)>;

/// Paths of operations that failed, and why.
type Failures = Vec<(String, String)>;

/// An asset file that was written, and the hash of what Studio sent for it.
struct WrittenFile {
    path: String,
    entry: FileEntry,
    raw_hash: Hash,
}

/// Removes the index entry of the file `path`, or of everything under the
/// directory `path`, keyed by what follows `path` in their paths.
fn take_index_entries(
    files: &mut BTreeMap<String, FileEntry>,
    path: &str,
) -> Vec<(String, FileEntry)> {
    let taken: Vec<String> = keys_at_or_under(files, path).cloned().collect();

    taken
        .into_iter()
        .map(|existing| {
            let entry = files.remove(&existing).unwrap();
            (existing[path.len()..].to_owned(), entry)
        })
        .collect()
}

/// Performs file operations on a container, keeping its index and the base
/// state of the given place up to date.
///
/// Removals happen first and writes last, so that a batch can replace a file
/// with a directory or the other way around.
fn execute_ops(
    inner: &mut Inner,
    index: usize,
    place: &str,
    ops: Vec<WriteOp>,
) -> (Changes, Failures) {
    inner.index_changed = true;

    let key = inner.containers[index].info.key.clone();
    let root = inner.containers[index].canonical_root.clone().unwrap();
    let files = inner.containers[index].files.as_mut().unwrap();
    let state = &mut inner.state;

    let mut changes = Vec::new();
    let mut failures = Vec::new();

    let mut removes = Vec::new();
    let mut moves = Vec::new();
    let mut writes = Vec::new();

    for op in ops {
        match op {
            WriteOp::Remove { path } => {
                if names::is_valid_unit_path(&path) {
                    removes.push(path);
                } else {
                    failures.push((path, "invalid path".to_owned()));
                }
            }
            WriteOp::Move { from, to } => {
                let valid = |path: &str| {
                    names::is_valid_unit_path(path) || names::is_valid_directory_path(path)
                };

                if valid(&from) && valid(&to) && from != to && !is_under(&to, &from) {
                    moves.push((from, to));
                } else {
                    failures.push((from, format!("invalid move to {to}")));
                }
            }
            WriteOp::Write { path, data } => {
                if names::is_valid_unit_path(&path) {
                    writes.push((path, data));
                } else {
                    failures.push((path, "invalid path".to_owned()));
                }
            }
        }
    }

    let listings = DirListings::new(
        &root,
        removes
            .iter()
            .chain(moves.iter().map(|(from, _)| from))
            .map(String::as_str),
    );

    for path in removes {
        let absolute = absolute_path(&root, &path);

        // On case-insensitive filesystems, removing a file whose name only
        // differs in case would remove another asset's file.
        let result = if listings.exists_exactly(&path) {
            fs::remove_file(&absolute)
        } else {
            Ok(())
        };

        match result {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                failures.push((path, err.to_string()));
                continue;
            }
        }

        prune_empty_dirs(&root, absolute.parent().unwrap());
        state.remove(place, &key, &path);
        if files.remove(&path).is_some() {
            changes.push((path, true));
        }
    }

    // Moves happen in two phases, so that a batch can swap the names of two
    // entries: everything is moved to a temporary name first, and then to its
    // destination. The index and bases are moved the same way, since moving
    // them one at a time would clobber swapped entries.
    //
    // Moving everything out first also removes directories that end up empty
    // before anything is moved in. That's what lets a directory's name change
    // case on case-insensitive filesystems, which would otherwise move files
    // meant for `Trees/` into the existing `trees/`.
    let mut in_flight = Vec::new();
    for (number, (from, to)) in moves.into_iter().enumerate() {
        let source = absolute_path(&root, &from);

        let is_dir = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata.is_dir(),
            Err(err) => {
                failures.push((from, err.to_string()));
                continue;
            }
        };

        // Asset files have to stay asset files, and directories directories.
        let valid = if is_dir {
            names::is_valid_directory_path(&from) && names::is_valid_directory_path(&to)
        } else {
            names::is_valid_unit_path(&from) && names::is_valid_unit_path(&to)
        };

        if !valid || !listings.exists_exactly(&from) {
            failures.push((from, format!("can't move to {to}")));
            continue;
        }

        let temp = root.join(format!(".rojo-move-{}-{number}", std::process::id()));
        if let Err(err) = fs::rename(&source, &temp) {
            failures.push((from, err.to_string()));
            continue;
        }
        prune_empty_dirs(&root, source.parent().unwrap());

        let index_entries = take_index_entries(files, &from);
        let base_entries = state.take_prefix(place, &key, &from);
        in_flight.push((from, to, source, temp, index_entries, base_entries));
    }

    // Directories that are known to exist with the right case.
    let mut checked_dirs = HashSet::new();

    // A directory that's moved has to be in place before anything is moved
    // into it.
    in_flight.sort_by_key(|(_, to, ..)| to.matches('/').count());

    for (from, to, source, temp, index_entries, base_entries) in in_flight {
        let destination = absolute_path(&root, &to);
        let parent = to.rsplit_once('/').map_or("", |(parent, _)| parent);

        let result = if destination.exists() {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{to} already exists"),
            ))
        } else {
            create_dirs_exactly(&root, parent, &mut checked_dirs)
                .and_then(|()| fs::rename(&temp, &destination))
        };

        // Put the entries back where they came from if the move failed, so the
        // index keeps matching the filesystem.
        let target = match result {
            Ok(()) => &to,
            Err(err) => {
                let _ = fs::create_dir_all(source.parent().unwrap())
                    .and_then(|()| fs::rename(&temp, &source));
                failures.push((from.clone(), err.to_string()));
                &from
            }
        };

        state.insert_with_prefix(place, &key, target, base_entries);

        for (suffix, entry) in index_entries {
            let new_path = format!("{target}{suffix}");
            if *target != from {
                changes.push((format!("{from}{suffix}"), true));
                changes.push((new_path.clone(), false));
            }
            files.insert(new_path, entry);
        }
    }

    // Directories are created before writing in parallel, so that each one
    // only has its case checked once.
    let mut writable = Vec::with_capacity(writes.len());
    for (path, data) in writes {
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        match create_dirs_exactly(&root, parent, &mut checked_dirs) {
            Ok(()) => writable.push((path, data)),
            Err(err) => failures.push((path, err.to_string())),
        }
    }

    let known: &BTreeMap<String, FileEntry> = files;
    let written: Vec<Result<WrittenFile, (String, String)>> = writable
        .into_par_iter()
        .map(|(path, raw)| {
            let result = (|| -> anyhow::Result<(FileEntry, Hash)> {
                let canonical = rbxm::canonicalize(&raw)?;
                let hash = hash_bytes(&canonical);
                let absolute = absolute_path(&root, &path);

                // Rewriting a file with identical contents would only bump its
                // modification time and churn the filesystem watcher.
                let existing = known.get(&path);
                let unchanged = existing.is_some_and(|entry| entry.hash == hash)
                    && fs::metadata(&absolute).is_ok();

                if !unchanged {
                    // Studio may only have rounded some floats differently,
                    // which isn't worth a change to the file.
                    let noise_only = existing.filter(|entry| {
                        file_only_differs_by_noise(&root, &path, entry, &canonical)
                    });
                    if let Some(existing) = noise_only {
                        return Ok((*existing, hash_bytes(&raw)));
                    }

                    write_atomic(&absolute, &canonical)?;
                }

                let metadata = fs::metadata(&absolute)?;
                let entry = FileEntry {
                    hash,
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                };

                Ok((entry, hash_bytes(&raw)))
            })();

            match result {
                Ok((entry, raw_hash)) => Ok(WrittenFile {
                    path,
                    entry,
                    raw_hash,
                }),
                Err(err) => Err((path, format!("{err:#}"))),
            }
        })
        .collect();

    for result in written {
        match result {
            Ok(WrittenFile {
                path,
                entry,
                raw_hash,
            }) => {
                state.record(place, &key, &path, entry.hash, raw_hash);

                let previous = files.insert(path.clone(), entry);
                if previous.map(|previous| previous.hash) != Some(entry.hash) {
                    changes.push((path, false));
                }
            }
            Err(failure) => failures.push(failure),
        }
    }

    if let Err(err) = state.save_if_dirty() {
        log::warn!("Could not save asset sync state: {err}");
    }

    (changes, failures)
}

#[cfg(test)]
mod test {
    use super::*;

    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    use crate::snapshot::{InstanceMetadata, InstanceSnapshot};

    /// Serializes a model the way Studio does, with LZ4 compressed chunks.
    fn studio_model(name: &str, transparency: f32) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let part = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Part")
                .with_name(name)
                .with_property("Transparency", transparency),
        );

        let mut output = Vec::new();
        rbx_binary::to_writer(&mut output, &dom, &[part]).unwrap();
        output
    }

    fn assert_in_sync(plan: &PlanResult) {
        assert_eq!(plan.need_upload, Vec::<String>::new());
        assert_eq!(plan.actions, Vec::new());
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        project: PathBuf,
        assets: PathBuf,
        tree: RojoTree,
        store: AssetStore,
        container: Ref,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let project = dunce::canonicalize(dir.path()).unwrap();
        let assets = project.join("assets");

        let mut tree = RojoTree::new(InstanceSnapshot::new().class_name("DataModel"));
        let container = tree.insert_instance(
            tree.get_root_id(),
            InstanceSnapshot::new()
                .class_name("Folder")
                .name("Assets")
                .metadata(InstanceMetadata::new().asset_dir(Some(assets.clone()))),
        );

        let store = AssetStore::new(&project, None);
        assert!(store.refresh_containers(&tree));
        store.scan_all().unwrap();

        Fixture {
            _dir: dir,
            project,
            assets,
            tree,
            store,
            container,
        }
    }

    fn write_op(path: &str, data: Vec<u8>) -> WriteOp {
        WriteOp::Write {
            path: path.to_owned(),
            data,
        }
    }

    #[test]
    fn writes_store_canonical_models() {
        let fixture = fixture();
        let model = studio_model("Rock", 0.5);

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Maps/Rock.rbxm", model.clone())],
            )
            .unwrap();
        assert_eq!(failures, Vec::new());

        let on_disk = fs::read(fixture.assets.join("Maps").join("Rock.rbxm")).unwrap();
        assert_eq!(on_disk, rbxm::canonicalize(&model).unwrap());
        assert!(rbxm::is_canonical(&on_disk).unwrap());

        let hashes = fixture.store.file_hashes(fixture.container).unwrap();
        assert_eq!(hashes.get("Maps/Rock.rbxm"), Some(&hash_bytes(&on_disk)));
    }

    /// The model of [`studio_model`], with its transparency rounded the other
    /// way, the way Studio sometimes rounds floats when loading and saving.
    fn rounded_model(name: &str, transparency: f32) -> Vec<u8> {
        studio_model(name, f32::from_bits(transparency.to_bits() + 1))
    }

    #[test]
    fn writes_that_only_round_floats_keep_the_file() {
        let fixture = fixture();
        let original = studio_model("Rock", 0.5);
        let rounded = rounded_model("Rock", 0.5);
        assert_ne!(
            rbxm::canonicalize(&original).unwrap(),
            rbxm::canonicalize(&rounded).unwrap()
        );

        let path = fixture.assets.join("Rock.rbxm");
        let write = |model: &Vec<u8>| {
            let failures = fixture
                .store
                .write(
                    "client",
                    "1",
                    fixture.container,
                    vec![write_op("Rock.rbxm", model.clone())],
                )
                .unwrap();
            assert_eq!(failures, Vec::new());
        };

        write(&original);
        let before = fs::read(&path).unwrap();
        write(&rounded);
        assert_eq!(fs::read(&path).unwrap(), before);

        // The rounded version is what the place synced last, so the place is
        // in sync without uploading anything.
        let plan = fixture
            .store
            .plan(
                "client",
                "1",
                None,
                fixture.container,
                &[("Rock.rbxm".to_owned(), hash_bytes(&rounded))],
                &[],
            )
            .unwrap();
        assert_in_sync(&plan);

        // Real changes are still written.
        let changed = studio_model("Rock", 0.75);
        write(&changed);
        assert_eq!(
            fs::read(&path).unwrap(),
            rbxm::canonicalize(&changed).unwrap()
        );
    }

    #[test]
    fn plans_treat_rounded_floats_as_in_sync() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", studio_model("Rock", 0.5))],
            )
            .unwrap();

        // Places that never synced have nothing to tell which side is right,
        // so Studio's contents are compared with the file's.
        let plan_for = |place: &str, model: Vec<u8>| {
            let studio = [("Rock.rbxm".to_owned(), hash_bytes(&model))];
            let plan = fixture
                .store
                .plan("client", place, None, fixture.container, &studio, &[])
                .unwrap();
            assert_eq!(plan.need_upload, vec!["Rock.rbxm".to_owned()]);

            fixture
                .store
                .stage(
                    "client",
                    fixture.container,
                    vec![("Rock.rbxm".to_owned(), model)],
                )
                .unwrap();
            fixture
                .store
                .plan("client", place, None, fixture.container, &studio, &[])
                .unwrap()
        };

        assert_in_sync(&plan_for("2", rounded_model("Rock", 0.5)));
        assert_eq!(
            plan_for("3", studio_model("Rock", 0.75)).actions,
            vec![PlanEntry {
                path: "Rock.rbxm".to_owned(),
                action: PlanAction::Conflict,
                reason: Some(PlanReason::BothChanged),
            }]
        );
    }

    #[test]
    fn writes_record_bases_for_the_writing_place() {
        let fixture = fixture();
        let model = studio_model("Rock", 0.5);

        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", model.clone())],
            )
            .unwrap();

        // The same place is in sync without uploading anything.
        let plan = fixture
            .store
            .plan(
                "client",
                "1",
                None,
                fixture.container,
                &[("Rock.rbxm".to_owned(), hash_bytes(&model))],
                &[],
            )
            .unwrap();
        assert_in_sync(&plan);

        // Another place has never synced, so it needs to compare contents.
        let plan = fixture
            .store
            .plan(
                "client",
                "2",
                None,
                fixture.container,
                &[("Rock.rbxm".to_owned(), hash_bytes(&model))],
                &[],
            )
            .unwrap();
        assert_eq!(plan.need_upload, vec!["Rock.rbxm".to_owned()]);
    }

    #[test]
    fn identical_writes_do_not_touch_the_file() {
        let fixture = fixture();
        let model = studio_model("Rock", 0.5);
        let path = fixture.assets.join("Rock.rbxm");

        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", model.clone())],
            )
            .unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", model)],
            )
            .unwrap();

        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn removes_prune_empty_directories() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("A/B/Rock.rbxm", studio_model("Rock", 0.0))],
            )
            .unwrap();
        assert!(fixture.assets.join("A").join("B").exists());

        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![WriteOp::Remove {
                    path: "A/B/Rock.rbxm".to_owned(),
                }],
            )
            .unwrap();

        assert!(!fixture.assets.join("A").exists());
        assert!(fixture.assets.exists());
        assert!(fixture
            .store
            .file_hashes(fixture.container)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn moves_can_swap_names() {
        let fixture = fixture();
        let first = studio_model("Part", 0.1);
        let second = studio_model("Part", 0.2);

        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    write_op("Dir/Part.rbxm", first.clone()),
                    write_op("Dir/Part~2.rbxm", second.clone()),
                ],
            )
            .unwrap();

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    WriteOp::Move {
                        from: "Dir/Part.rbxm".to_owned(),
                        to: "Dir/Part~2.rbxm".to_owned(),
                    },
                    WriteOp::Move {
                        from: "Dir/Part~2.rbxm".to_owned(),
                        to: "Dir/Part.rbxm".to_owned(),
                    },
                ],
            )
            .unwrap();
        assert_eq!(failures, Vec::new());

        let dir = fixture.assets.join("Dir");
        assert_eq!(
            fs::read(dir.join("Part.rbxm")).unwrap(),
            rbxm::canonicalize(&second).unwrap()
        );
        assert_eq!(
            fs::read(dir.join("Part~2.rbxm")).unwrap(),
            rbxm::canonicalize(&first).unwrap()
        );

        // The moved assets keep their bases, so they're still in sync.
        let plan = fixture
            .store
            .plan(
                "client",
                "1",
                None,
                fixture.container,
                &[
                    ("Dir/Part.rbxm".to_owned(), hash_bytes(&second)),
                    ("Dir/Part~2.rbxm".to_owned(), hash_bytes(&first)),
                ],
                &[],
            )
            .unwrap();
        assert_in_sync(&plan);
    }

    #[test]
    fn directory_moves_rekey_everything_inside() {
        let fixture = fixture();
        let model = studio_model("Rock", 0.3);
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Old/Sub/Rock.rbxm", model.clone())],
            )
            .unwrap();

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![WriteOp::Move {
                    from: "Old".to_owned(),
                    to: "New/Nested".to_owned(),
                }],
            )
            .unwrap();
        assert_eq!(failures, Vec::new());

        assert!(fixture
            .assets
            .join("New")
            .join("Nested")
            .join("Sub")
            .join("Rock.rbxm")
            .exists());
        assert!(!fixture.assets.join("Old").exists());

        let hashes = fixture.store.file_hashes(fixture.container).unwrap();
        assert!(hashes.contains_key("New/Nested/Sub/Rock.rbxm"));
        assert!(!hashes.contains_key("Old/Sub/Rock.rbxm"));
    }

    #[test]
    fn rejects_unsafe_paths() {
        let fixture = fixture();

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    write_op("../escape.rbxm", studio_model("Evil", 0.0)),
                    write_op("not-a-model.txt", studio_model("Evil", 0.0)),
                ],
            )
            .unwrap();

        assert_eq!(failures.len(), 2);
        assert!(!fixture.project.join("escape.rbxm").exists());
    }

    #[test]
    fn rejects_invalid_models() {
        let fixture = fixture();

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Broken.rbxm", b"definitely not a model".to_vec())],
            )
            .unwrap();

        assert_eq!(failures.len(), 1);
        assert!(!fixture.assets.join("Broken.rbxm").exists());
    }

    #[test]
    fn initial_sync_exports_studio_assets() {
        let fixture = fixture();
        let model = studio_model("Tree", 0.0);
        let studio = [("Trees/Tree.rbxm".to_owned(), hash_bytes(&model))];

        let plan = fixture
            .store
            .plan("client", "1", None, fixture.container, &studio, &[])
            .unwrap();
        assert_eq!(plan.need_upload, vec!["Trees/Tree.rbxm".to_owned()]);

        fixture
            .store
            .stage(
                "client",
                fixture.container,
                vec![("Trees/Tree.rbxm".to_owned(), model.clone())],
            )
            .unwrap();

        let plan = fixture
            .store
            .plan("client", "1", None, fixture.container, &studio, &[])
            .unwrap();
        assert_eq!(plan.need_upload, Vec::<String>::new());
        assert_eq!(
            plan.actions,
            vec![PlanEntry {
                path: "Trees/Tree.rbxm".to_owned(),
                action: PlanAction::ToFs,
                reason: None,
            }]
        );

        fixture
            .store
            .apply(
                "client",
                "1",
                fixture.container,
                &["Trees/Tree.rbxm".to_owned()],
                &[],
            )
            .unwrap();

        assert!(fixture.assets.join("Trees").join("Tree.rbxm").exists());

        let plan = fixture
            .store
            .plan("client", "1", None, fixture.container, &studio, &[])
            .unwrap();
        assert_in_sync(&plan);
    }

    #[test]
    fn file_changes_are_planned_for_studio_and_acked() {
        let fixture = fixture();
        let original = studio_model("Tree", 0.0);
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Tree.rbxm", original.clone())],
            )
            .unwrap();

        // Simulate `git checkout` replacing the file.
        let changed = rbxm::canonicalize(&studio_model("Tree", 1.0)).unwrap();
        fs::write(fixture.assets.join("Tree.rbxm"), &changed).unwrap();
        assert!(fixture
            .store
            .handle_fs_change(&fixture.assets.join("Tree.rbxm")));

        let studio = [("Tree.rbxm".to_owned(), hash_bytes(&original))];
        let plan = fixture
            .store
            .plan("client", "1", None, fixture.container, &studio, &[])
            .unwrap();
        assert_eq!(
            plan.actions,
            vec![PlanEntry {
                path: "Tree.rbxm".to_owned(),
                action: PlanAction::ToStudio,
                reason: None,
            }]
        );

        let read = fixture
            .store
            .read(fixture.container, &["Tree.rbxm".to_owned()])
            .unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(rbxm::canonicalize(&read[0].data).unwrap(), changed);

        // After Studio applied the file, its new serialization is the base.
        let reserialized = studio_model("Tree", 1.0);
        fixture
            .store
            .ack(
                "1",
                fixture.container,
                &[AckEntry {
                    path: "Tree.rbxm".to_owned(),
                    fs_hash: read[0].hash,
                    studio_hash: hash_bytes(&reserialized),
                }],
                &[],
            )
            .unwrap();

        let plan = fixture
            .store
            .plan(
                "client",
                "1",
                None,
                fixture.container,
                &[("Tree.rbxm".to_owned(), hash_bytes(&reserialized))],
                &[],
            )
            .unwrap();
        assert_in_sync(&plan);
    }

    #[test]
    fn fs_changes_produce_messages() {
        let fixture = fixture();
        let cursor = fixture.store.messages().cursor();

        let path = fixture.assets.join("Dir").join("Rock.rbxm");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            rbxm::canonicalize(&studio_model("Rock", 0.0)).unwrap(),
        )
        .unwrap();

        // A directory event picks up everything inside of it.
        assert!(fixture.store.handle_fs_change(&fixture.assets.join("Dir")));

        fs::remove_file(&path).unwrap();
        assert!(fixture.store.handle_fs_change(&path));

        // Paths outside of asset directories are left alone.
        assert!(!fixture
            .store
            .handle_fs_change(&fixture.project.join("default.project.json")));

        let receiver = fixture.store.messages().subscribe(cursor);
        let (_, messages) = futures::executor::block_on(receiver).unwrap();
        assert_eq!(
            messages,
            vec![
                AssetMessage::Change(AssetChange {
                    container: fixture.container,
                    path: "Dir/Rock.rbxm".to_owned(),
                    removed: false,
                    origin: None,
                }),
                AssetMessage::Change(AssetChange {
                    container: fixture.container,
                    path: "Dir/Rock.rbxm".to_owned(),
                    removed: true,
                    origin: None,
                }),
            ]
        );
    }

    #[test]
    fn batches_of_fs_changes_are_handled_together() {
        let fixture = fixture();
        let cursor = fixture.store.messages().cursor();

        let dir = fixture.assets.join("Dir");
        fs::create_dir_all(&dir).unwrap();
        for name in ["A", "B", "C"] {
            fs::write(
                dir.join(format!("{name}.rbxm")),
                rbxm::canonicalize(&studio_model(name, 0.0)).unwrap(),
            )
            .unwrap();
        }

        // Watchers report the same file several times, and directories along
        // with the files inside of them.
        let owned = fixture.store.handle_fs_changes(&[
            dir.clone(),
            dir.join("A.rbxm"),
            dir.join("A.rbxm"),
            dir.join("B.rbxm"),
            fixture.project.join("default.project.json"),
        ]);
        assert_eq!(owned, [true, true, true, true, false]);

        let receiver = fixture.store.messages().subscribe(cursor);
        let (_, messages) = futures::executor::block_on(receiver).unwrap();
        let mut paths: Vec<String> = messages
            .into_iter()
            .map(|message| match message {
                AssetMessage::Change(change) => change.path,
                other => panic!("unexpected message {other:?}"),
            })
            .collect();
        paths.sort();

        // Every file is reported exactly once.
        assert_eq!(paths, ["Dir/A.rbxm", "Dir/B.rbxm", "Dir/C.rbxm"]);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn files_renamed_to_a_different_case_are_indexed_once() {
        let fixture = fixture();
        let model = studio_model("Rock", 0.0);
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("rock.rbxm", model.clone())],
            )
            .unwrap();

        // The asset was renamed from `rock` to `Rock`.
        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    WriteOp::Remove {
                        path: "rock.rbxm".to_owned(),
                    },
                    write_op("Rock.rbxm", model),
                ],
            )
            .unwrap();
        assert_eq!(failures, Vec::new());

        // The watcher reports both names, and case-insensitive filesystems
        // find the file by either of them.
        fixture.store.handle_fs_changes(&[
            fixture.assets.join("rock.rbxm"),
            fixture.assets.join("Rock.rbxm"),
        ]);

        let hashes = fixture.store.file_hashes(fixture.container).unwrap();
        assert_eq!(hashes.keys().collect::<Vec<_>>(), ["Rock.rbxm"]);
        assert_eq!(names_in(&fixture.assets), ["Rock.rbxm"]);
    }

    #[test]
    fn directories_can_be_moved_to_a_different_case() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    write_op("trees/A.rbxm", studio_model("A", 0.0)),
                    write_op("trees/Sub/B.rbxm", studio_model("B", 0.0)),
                ],
            )
            .unwrap();

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    WriteOp::Move {
                        from: "trees/A.rbxm".to_owned(),
                        to: "Trees/A.rbxm".to_owned(),
                    },
                    WriteOp::Move {
                        from: "trees/Sub/B.rbxm".to_owned(),
                        to: "Trees/Sub/B.rbxm".to_owned(),
                    },
                ],
            )
            .unwrap();
        assert_eq!(failures, Vec::new());
        assert_eq!(names_in(&fixture.assets), ["Trees"]);

        fixture.store.handle_fs_changes(&[
            fixture.assets.join("trees"),
            fixture.assets.join("trees").join("A.rbxm"),
            fixture.assets.join("Trees"),
            fixture.assets.join("Trees").join("A.rbxm"),
        ]);

        let hashes = fixture.store.file_hashes(fixture.container).unwrap();
        assert_eq!(
            hashes.keys().collect::<Vec<_>>(),
            ["Trees/A.rbxm", "Trees/Sub/B.rbxm"]
        );
    }

    #[test]
    fn directories_are_moved_before_anything_is_moved_into_them() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    write_op("A/x.rbxm", studio_model("X", 0.0)),
                    write_op("C/y.rbxm", studio_model("Y", 0.0)),
                ],
            )
            .unwrap();

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    WriteOp::Move {
                        from: "C/y.rbxm".to_owned(),
                        to: "B/y.rbxm".to_owned(),
                    },
                    WriteOp::Move {
                        from: "A".to_owned(),
                        to: "B".to_owned(),
                    },
                ],
            )
            .unwrap();

        assert_eq!(failures, Vec::new());
        assert_eq!(names_in(&fixture.assets), ["B"]);
        assert_eq!(names_in(&fixture.assets.join("B")), ["x.rbxm", "y.rbxm"]);

        let hashes = fixture.store.file_hashes(fixture.container).unwrap();
        assert_eq!(hashes.keys().collect::<Vec<_>>(), ["B/x.rbxm", "B/y.rbxm"]);
    }

    #[test]
    fn writes_never_go_into_a_directory_with_different_capitalization() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("trees/A.rbxm", studio_model("A", 0.0))],
            )
            .unwrap();

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Trees/B.rbxm", studio_model("B", 0.0))],
            )
            .unwrap();

        let hashes = fixture.store.file_hashes(fixture.container).unwrap();
        if fixture.assets.join("TREES").exists() {
            // The filesystem is case-insensitive, so the file would have ended
            // up in `trees`.
            assert_eq!(failures.len(), 1);
            assert!(!hashes.contains_key("Trees/B.rbxm"));
            assert_eq!(names_in(&fixture.assets.join("trees")), ["A.rbxm"]);
        } else {
            assert_eq!(failures, Vec::new());
            assert!(hashes.contains_key("Trees/B.rbxm"));
        }
    }

    #[test]
    fn directories_named_like_asset_files_can_be_moved() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Old.rbxm/Part.rbxm", studio_model("Part", 0.0))],
            )
            .unwrap();

        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![WriteOp::Move {
                    from: "Old.rbxm".to_owned(),
                    to: "New".to_owned(),
                }],
            )
            .unwrap();
        assert_eq!(failures, Vec::new());
        assert!(fixture.assets.join("New").join("Part.rbxm").exists());

        // Asset files have to keep being asset files.
        let failures = fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![WriteOp::Move {
                    from: "New/Part.rbxm".to_owned(),
                    to: "New/Part".to_owned(),
                }],
            )
            .unwrap();
        assert_eq!(failures.len(), 1);
        assert!(fixture.assets.join("New").join("Part.rbxm").exists());
    }

    #[test]
    fn known_assets_include_deleted_files() {
        let fixture = fixture();
        let rock = studio_model("Rock", 0.0);
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    write_op("Rock.rbxm", rock.clone()),
                    write_op("Tree.rbxm", studio_model("Tree", 0.0)),
                ],
            )
            .unwrap();

        fs::remove_file(fixture.assets.join("Rock.rbxm")).unwrap();
        fixture
            .store
            .handle_fs_change(&fixture.assets.join("Rock.rbxm"));

        let known = fixture.store.known("1", fixture.container).unwrap();
        let synced: Vec<&str> = known
            .synced
            .iter()
            .map(|asset| asset.path.as_str())
            .collect();
        assert_eq!(synced, ["Rock.rbxm", "Tree.rbxm"]);
        assert_eq!(known.synced[0].studio, hash_bytes(&rock));
        assert_eq!(known.files, ["Tree.rbxm"]);

        // Other places haven't synced anything.
        let known = fixture.store.known("2", fixture.container).unwrap();
        assert!(known.synced.is_empty());
    }

    #[test]
    fn unchanged_files_are_not_read_again_on_the_next_run() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", studio_model("Rock", 0.0))],
            )
            .unwrap();
        fixture.store.rescan_all();
        let indexed = fixture.store.file_hashes(fixture.container).unwrap()["Rock.rbxm"];

        // Change the file without changing its size or modification time,
        // which only a run that trusts the cached index wouldn't notice.
        let path = fixture.assets.join("Rock.rbxm");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let changed = rbxm::canonicalize(&studio_model("Rock", 0.5)).unwrap();
        assert_eq!(changed.len() as u64, fs::metadata(&path).unwrap().len());
        fs::write(&path, &changed).unwrap();
        let set_modified = |time| {
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(time)
                .unwrap()
        };
        set_modified(modified);

        let next_run = AssetStore::new(&fixture.project, None);
        next_run.refresh_containers(&fixture.tree);
        next_run.scan_all().unwrap();
        assert_eq!(
            next_run.file_hashes(fixture.container).unwrap()["Rock.rbxm"],
            indexed
        );

        // Files that look changed are read again.
        set_modified(modified + std::time::Duration::from_secs(1));
        let next_run = AssetStore::new(&fixture.project, None);
        next_run.refresh_containers(&fixture.tree);
        next_run.scan_all().unwrap();
        assert_eq!(
            next_run.file_hashes(fixture.container).unwrap()["Rock.rbxm"],
            hash_bytes(&changed)
        );
    }

    #[test]
    fn groups_are_remembered_per_place() {
        let fixture = fixture();
        let group = Group {
            hash: [1; 32],
            units: vec![[2; 32], [3; 32]],
        };

        fixture
            .store
            .set_groups("1", fixture.container, vec![group.clone()])
            .unwrap();

        let known = fixture.store.known("1", fixture.container).unwrap();
        assert_eq!(known.groups, vec![group]);
        let known = fixture.store.known("2", fixture.container).unwrap();
        assert!(known.groups.is_empty());
    }

    #[test]
    fn rescan_catches_missed_changes() {
        let fixture = fixture();
        let path = fixture.assets.join("Rock.rbxm");
        fs::write(
            &path,
            rbxm::canonicalize(&studio_model("Rock", 0.0)).unwrap(),
        )
        .unwrap();

        fixture.store.rescan_all();
        assert!(fixture
            .store
            .file_hashes(fixture.container)
            .unwrap()
            .contains_key("Rock.rbxm"));

        fs::remove_file(&path).unwrap();
        fixture.store.rescan_all();
        assert!(fixture
            .store
            .file_hashes(fixture.container)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn assets_missing_from_studio_are_only_deleted_when_the_user_says_so() {
        let fixture = fixture();
        let model = studio_model("Rock", 0.0);
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", model.clone())],
            )
            .unwrap();

        // Studio no longer has the asset, and the file is unchanged. Studio
        // either deleted it or lost it, so the user has to decide.
        let plan = fixture
            .store
            .plan("client", "1", None, fixture.container, &[], &[])
            .unwrap();
        assert_eq!(
            plan.actions,
            vec![PlanEntry {
                path: "Rock.rbxm".to_owned(),
                action: PlanAction::Conflict,
                reason: Some(PlanReason::MissingInStudio),
            }]
        );
        assert!(fixture.assets.join("Rock.rbxm").exists());

        // The user decided Studio is right.

        fixture
            .store
            .apply(
                "client",
                "1",
                fixture.container,
                &[],
                &["Rock.rbxm".to_owned()],
            )
            .unwrap();
        assert!(!fixture.assets.join("Rock.rbxm").exists());
    }

    #[test]
    fn skipped_assets_are_never_deleted() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", studio_model("Rock", 0.0))],
            )
            .unwrap();

        // Studio has the asset but couldn't serialize it. It must not look like
        // Studio deleted it.
        let plan = fixture
            .store
            .plan(
                "client",
                "1",
                None,
                fixture.container,
                &[],
                &["Rock.rbxm".to_owned()],
            )
            .unwrap();

        assert_in_sync(&plan);
        assert!(fixture.assets.join("Rock.rbxm").exists());
    }

    #[test]
    fn first_syncs_are_reported() {
        let fixture = fixture();
        let model = studio_model("Rock", 0.0);

        let plan = fixture
            .store
            .plan("client", "1", None, fixture.container, &[], &[])
            .unwrap();
        assert!(plan.first_sync);

        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", model.clone())],
            )
            .unwrap();

        let studio = [("Rock.rbxm".to_owned(), hash_bytes(&model))];
        let plan = fixture
            .store
            .plan("client", "1", None, fixture.container, &studio, &[])
            .unwrap();
        assert!(!plan.first_sync);

        let plan = fixture
            .store
            .plan("client", "2", None, fixture.container, &studio, &[])
            .unwrap();
        assert!(plan.first_sync);
    }

    #[test]
    fn outdated_studio_versions_are_replaced_by_files() {
        let fixture = fixture();
        let first = studio_model("Rock", 0.1);
        let second = studio_model("Rock", 0.2);

        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", first.clone())],
            )
            .unwrap();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", second)],
            )
            .unwrap();

        // Studio has the first version again, for example because the place
        // was saved before the second one was synced. The file has the newer
        // one, which must not be overwritten.
        let plan = fixture
            .store
            .plan(
                "client",
                "1",
                None,
                fixture.container,
                &[("Rock.rbxm".to_owned(), hash_bytes(&first))],
                &[],
            )
            .unwrap();
        assert_eq!(
            plan.actions,
            vec![PlanEntry {
                path: "Rock.rbxm".to_owned(),
                action: PlanAction::ToStudio,
                reason: Some(PlanReason::OutdatedInStudio),
            }]
        );
    }

    #[test]
    fn unsaved_places_never_overwrite_files_with_new_versions() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![write_op("Rock.rbxm", studio_model("Rock", 0.1))],
            )
            .unwrap();
        fixture.store.start_session("1", "last").unwrap();

        // Studio has a version that was never synced, but the place doesn't
        // have the last session's token, so it might be older than the file.
        let unknown = studio_model("Rock", 0.9);
        let studio = [("Rock.rbxm".to_owned(), hash_bytes(&unknown))];
        let plan = fixture
            .store
            .plan(
                "client",
                "1",
                Some("older"),
                fixture.container,
                &studio,
                &[],
            )
            .unwrap();
        assert_eq!(plan.need_upload, vec!["Rock.rbxm".to_owned()]);

        fixture
            .store
            .stage(
                "client",
                fixture.container,
                vec![("Rock.rbxm".to_owned(), unknown)],
            )
            .unwrap();
        let plan = fixture
            .store
            .plan(
                "client",
                "1",
                Some("older"),
                fixture.container,
                &studio,
                &[],
            )
            .unwrap();
        assert_eq!(
            plan.actions,
            vec![PlanEntry {
                path: "Rock.rbxm".to_owned(),
                action: PlanAction::Conflict,
                reason: Some(PlanReason::UnsavedPlace),
            }]
        );
    }

    #[test]
    fn load_all_returns_every_asset() {
        let fixture = fixture();
        fixture
            .store
            .write(
                "client",
                "1",
                fixture.container,
                vec![
                    write_op("B.rbxm", studio_model("B", 0.0)),
                    write_op("Dir/A.rbxm", studio_model("A", 0.0)),
                ],
            )
            .unwrap();

        let loaded = fixture.store.load_all(fixture.container).unwrap();
        let paths: Vec<&str> = loaded.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["B.rbxm", "Dir/A.rbxm"]);
    }
}
