//! What Studio and the filesystem last agreed on for every asset.
//!
//! When the plugin connects, an asset can differ between Studio and the
//! filesystem because either side changed while they were disconnected. The
//! base state tells which side changed, so most differences can be resolved
//! without asking: if only one side changed, that side wins.
//!
//! Studio can also go back in time, for example when it's closed without saving
//! the place after its assets were synced. Its assets then differ from the base
//! without having been edited. Two things tell those cases apart:
//!
//! - Every connection gets a session token that the plugin stores in the
//!   place. If the place doesn't have the token of the last session, it wasn't
//!   saved after that session, and anything it has that differs is outdated.
//! - Every asset remembers what Studio serialized it to before, so an asset
//!   that went back to an earlier version is recognized as outdated too.
//!
//! Studio also changes how it serializes an asset without anyone editing it,
//! for example by packing its textures again. So every asset remembers a
//! hash of Studio's version that leaves such changes out as well, which tells
//! that Studio still has what it last synced (see `compare::stable_hash`).
//!
//! Bases are tracked per place, since the same project can be served into
//! several places whose assets differ.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
};

use data_encoding::HEXLOWER;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

use super::names::keys_at_or_under;

pub type Hash = [u8; 32];

/// The start of a hash, which is plenty to recognize earlier versions of the
/// same asset, and halves the size of the state.
pub type ShortHash = [u8; 16];

const STATE_VERSION: u32 = 1;

/// How many earlier Studio versions of every asset are remembered.
const MAX_OLDER_VERSIONS: usize = 32;

pub fn short_hash(hash: &Hash) -> ShortHash {
    hash[..16].try_into().unwrap()
}

/// The base of a single asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseEntry {
    /// Hash of the asset file's contents.
    pub fs: Hash,

    /// Hash of the bytes Studio serialized the asset to. This can differ from
    /// `fs` even when nothing changed, for example when Studio versions add new
    /// properties, so the two sides are tracked separately.
    pub studio: Hash,

    /// What Studio serialized the asset to in earlier syncs, oldest first.
    pub older: Vec<ShortHash>,

    /// A hash of Studio's version that stays the same when Studio only
    /// changes how it serializes the asset, if it's known. Studio's version
    /// is only uploaded when it's needed, so it isn't always.
    pub stable: Option<ShortHash>,

    /// Whether the file is known to lack properties that Studio's version
    /// has, which a file from an older version of Studio does. Such a file is
    /// brought up to date the next time its place connects.
    pub behind: bool,
}

impl BaseEntry {
    pub fn new(fs: Hash, studio: Hash) -> Self {
        Self {
            fs,
            studio,
            older: Vec::new(),
            stable: None,
            behind: false,
        }
    }

    /// Whether Studio's version of the asset is one that was synced before
    /// the latest one.
    pub fn is_older_studio_version(&self, studio: &Hash) -> bool {
        *studio != self.studio && self.older.contains(&short_hash(studio))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BaseEntryRepr {
    Current(String, String),
    WithOlder(String, String, Vec<String>),
    WithStable(String, String, Vec<String>, String),
    WithBehind(String, String, Vec<String>, Option<String>, bool),
}

impl Serialize for BaseEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let fs = HEXLOWER.encode(&self.fs);
        let studio = HEXLOWER.encode(&self.studio);

        let older = || -> Vec<String> {
            self.older
                .iter()
                .map(|hash| HEXLOWER.encode(hash))
                .collect()
        };

        let stable = self.stable.as_ref().map(|stable| HEXLOWER.encode(stable));

        match stable {
            _ if self.behind => (fs, studio, older(), stable, true).serialize(serializer),
            Some(stable) => (fs, studio, older(), stable).serialize(serializer),
            None if self.older.is_empty() => (fs, studio).serialize(serializer),
            None => (fs, studio, older()).serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for BaseEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (fs, studio, older, stable, behind) = match BaseEntryRepr::deserialize(deserializer)? {
            BaseEntryRepr::Current(fs, studio) => (fs, studio, Vec::new(), None, false),
            BaseEntryRepr::WithOlder(fs, studio, older) => (fs, studio, older, None, false),
            BaseEntryRepr::WithStable(fs, studio, older, stable) => {
                (fs, studio, older, Some(stable), false)
            }
            BaseEntryRepr::WithBehind(fs, studio, older, stable, behind) => {
                (fs, studio, older, stable, behind)
            }
        };

        Ok(BaseEntry {
            fs: decode_hash(&fs).map_err(D::Error::custom)?,
            studio: decode_hash(&studio).map_err(D::Error::custom)?,
            older: older
                .iter()
                .map(|hash| decode_hash(hash))
                .collect::<Result<_, _>>()
                .map_err(D::Error::custom)?,
            stable: stable
                .map(|hash| decode_hash(&hash))
                .transpose()
                .map_err(D::Error::custom)?,
            behind,
        })
    }
}

fn decode_hash<const N: usize>(encoded: &str) -> Result<[u8; N], String> {
    let bytes = HEXLOWER
        .decode(encoded.as_bytes())
        .map_err(|err| err.to_string())?;

    bytes
        .try_into()
        .map_err(|_| format!("hash {encoded} has the wrong length"))
}

/// Bases of every asset in one asset directory, by path.
pub type ContainerBase = BTreeMap<String, BaseEntry>;

/// Assets that Studio serialized together, and what each of them serialized
/// to on its own at the time.
///
/// Serializing many assets at once is much faster than serializing each one
/// on its own, so the plugin serializes groups of assets when it connects. A
/// group with the same hash as before has the same contents as before, so
/// what its assets serialized to on their own back then is still right, and
/// only the assets of groups that changed have to be serialized on their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub hash: Hash,
    pub units: Vec<Hash>,
}

impl Serialize for Group {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let units: Vec<String> = self
            .units
            .iter()
            .map(|hash| HEXLOWER.encode(hash))
            .collect();
        (HEXLOWER.encode(&self.hash), units).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Group {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (hash, units) = <(String, Vec<String>)>::deserialize(deserializer)?;

        Ok(Group {
            hash: decode_hash(&hash).map_err(D::Error::custom)?,
            units: units
                .iter()
                .map(|hash| decode_hash(hash))
                .collect::<Result<_, _>>()
                .map_err(D::Error::custom)?,
        })
    }
}

/// Creates the directory that Rojo keeps files specific to this machine in.
/// It ignores itself in git, so users don't have to update their .gitignore.
pub fn create_private_dir(directory: &Path) -> io::Result<()> {
    fs_err::create_dir_all(directory)?;

    let gitignore = directory.join(".gitignore");
    if !gitignore.exists() {
        fs_err::write(&gitignore, "*\n")?;
    }

    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    version: u32,

    /// Identifies this copy of the project. Places store one session token
    /// per machine, so that people syncing the same place from their own
    /// machines, like in Team Create, don't overwrite each other's tokens.
    #[serde(default)]
    machine: String,

    /// Place ID -> the token of the last session that synced it.
    #[serde(default)]
    sessions: BTreeMap<String, String>,

    /// Place ID -> asset directory -> asset path -> base.
    places: BTreeMap<String, BTreeMap<String, ContainerBase>>,

    /// Place ID -> asset directory -> groups of assets that were in sync when
    /// the place last started syncing.
    #[serde(default)]
    groups: BTreeMap<String, BTreeMap<String, Vec<Group>>>,

    /// Place ID -> the asset directories whose assets were all looked at.
    /// See [`BaseState::was_inspected`].
    #[serde(default)]
    inspected: BTreeMap<String, BTreeSet<String>>,
}

/// Bases of all assets of a project, persisted to disk.
#[derive(Debug)]
pub struct BaseState {
    path: PathBuf,
    file: StateFile,
    dirty: bool,
}

impl BaseState {
    /// Where the state of the project in the given folder is stored.
    pub fn path_for_project(project_folder: &Path) -> PathBuf {
        project_folder.join(".rojo").join("asset-sync.json")
    }

    /// Loads the state at the given path, starting fresh if it doesn't exist
    /// or can't be read. Losing the state only means the user gets asked more
    /// questions on connect, so it's never fatal.
    pub fn load(path: PathBuf) -> Self {
        let mut file = match fs_err::read(&path) {
            Ok(contents) => match serde_json::from_slice::<StateFile>(&contents) {
                Ok(file) if file.version == STATE_VERSION => file,
                Ok(file) => {
                    log::warn!(
                        "Ignoring asset sync state with unknown version {} at {}",
                        file.version,
                        path.display()
                    );
                    StateFile::default()
                }
                Err(err) => {
                    log::warn!(
                        "Ignoring malformed asset sync state at {}: {err}",
                        path.display()
                    );
                    StateFile::default()
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => StateFile::default(),
            Err(err) => {
                log::warn!("Could not read asset sync state: {err}");
                StateFile::default()
            }
        };

        // Part of the machine ID ends up in the name of an attribute, which
        // only allows a few characters.
        let mut dirty = false;
        if file.machine.len() < 12 || !file.machine.bytes().all(|b| b.is_ascii_alphanumeric()) {
            file.machine = uuid::Uuid::new_v4().simple().to_string();
            dirty = true;
        }

        Self { path, file, dirty }
    }

    /// Identifies this copy of the project.
    pub fn machine(&self) -> &str {
        &self.file.machine
    }

    /// The token of the last session that synced the given place.
    pub fn session(&self, place: &str) -> Option<&str> {
        self.file.sessions.get(place).map(String::as_str)
    }

    pub fn set_session(&mut self, place: &str, token: &str) {
        let previous = self
            .file
            .sessions
            .insert(place.to_owned(), token.to_owned());
        if previous.as_deref() != Some(token) {
            self.dirty = true;
        }
    }

    pub fn container(&self, place: &str, container: &str) -> Option<&ContainerBase> {
        self.file.places.get(place)?.get(container)
    }

    /// The groups of assets that were in sync when the place last started
    /// syncing. See [`Group`].
    pub fn groups(&self, place: &str, container: &str) -> &[Group] {
        self.file
            .groups
            .get(place)
            .and_then(|containers| containers.get(container))
            .map_or(&[], Vec::as_slice)
    }

    pub fn set_groups(&mut self, place: &str, container: &str, groups: Vec<Group>) {
        self.file
            .groups
            .entry(place.to_owned())
            .or_default()
            .insert(container.to_owned(), groups);
        self.dirty = true;
    }

    /// Whether Studio's version of every asset of a place was looked at.
    ///
    /// Studio's version of an asset is only uploaded when it's needed to
    /// decide something, which it isn't for an asset neither side changed.
    /// Only looking at it gives its base a stable hash though, and tells
    /// whether its file lacks properties Studio saves. Bases recorded before
    /// Rojo kept track of those know neither. So every asset is asked for
    /// once, and this remembers that it was, since some assets can't be given
    /// a stable hash however often they're uploaded.
    pub fn was_inspected(&self, place: &str, container: &str) -> bool {
        self.file
            .inspected
            .get(place)
            .is_some_and(|containers| containers.contains(container))
    }

    pub fn set_inspected(&mut self, place: &str, container: &str) {
        let added = self
            .file
            .inspected
            .entry(place.to_owned())
            .or_default()
            .insert(container.to_owned());
        if added {
            self.dirty = true;
        }
    }

    /// Marks a place that's about to sync a container for the first time as
    /// inspected. It has no assets to look at yet, and everything it syncs
    /// from here on is looked at as it is.
    pub fn skip_inspecting_new_place(&mut self, place: &str, container: &str) {
        if self
            .container(place, container)
            .is_none_or(BTreeMap::is_empty)
        {
            self.set_inspected(place, container);
        }
    }

    /// Records whether the file of an asset lacks properties that Studio's
    /// version has. See [`BaseEntry::behind`].
    pub fn set_file_behind(&mut self, place: &str, container: &str, path: &str, behind: bool) {
        let entry = self
            .file
            .places
            .get_mut(place)
            .and_then(|containers| containers.get_mut(container))
            .and_then(|bases| bases.get_mut(path));

        if let Some(entry) = entry {
            if entry.behind != behind {
                entry.behind = behind;
                self.dirty = true;
            }
        }
    }

    #[cfg(test)]
    pub fn get(&self, place: &str, container: &str, path: &str) -> Option<&BaseEntry> {
        self.container(place, container)?.get(path)
    }

    /// Records that Studio and the filesystem agree on an asset. Studio's
    /// previous version of it, if any, is remembered as an older version.
    ///
    /// `stable` is the stable hash of Studio's version, if it's known. What
    /// was known about a version Studio still has is kept.
    pub fn record(
        &mut self,
        place: &str,
        container: &str,
        path: &str,
        fs: Hash,
        studio: Hash,
        stable: Option<ShortHash>,
    ) {
        let bases = self
            .file
            .places
            .entry(place.to_owned())
            .or_default()
            .entry(container.to_owned())
            .or_default();

        match bases.get_mut(path) {
            Some(entry) if entry.fs == fs && entry.studio == studio => {
                if stable.is_some() && entry.stable != stable {
                    entry.stable = stable;
                    self.dirty = true;
                }
            }
            Some(entry) => {
                if entry.studio != studio {
                    let previous = short_hash(&entry.studio);
                    let current = short_hash(&studio);
                    entry
                        .older
                        .retain(|older| *older != previous && *older != current);
                    entry.older.push(previous);
                    if entry.older.len() > MAX_OLDER_VERSIONS {
                        entry.older.remove(0);
                    }

                    // What was known about the previous version says nothing
                    // about this one.
                    entry.stable = stable;
                } else if stable.is_some() {
                    entry.stable = stable;
                }
                if entry.fs != fs {
                    // Nothing is known about another file.
                    entry.behind = false;
                }
                entry.fs = fs;
                entry.studio = studio;
                self.dirty = true;
            }
            None => {
                bases.insert(
                    path.to_owned(),
                    BaseEntry {
                        stable,
                        ..BaseEntry::new(fs, studio)
                    },
                );
                self.dirty = true;
            }
        }
    }

    pub fn remove(&mut self, place: &str, container: &str, path: &str) {
        let removed = self
            .file
            .places
            .get_mut(place)
            .and_then(|containers| containers.get_mut(container))
            .and_then(|bases| bases.remove(path));

        if removed.is_some() {
            self.dirty = true;
        }
    }

    /// Removes and returns the base of the file `path`, or of everything under
    /// the directory `path`, keyed by what follows `path` in their paths.
    pub fn take_prefix(
        &mut self,
        place: &str,
        container: &str,
        path: &str,
    ) -> Vec<(String, BaseEntry)> {
        let Some(bases) = self
            .file
            .places
            .get_mut(place)
            .and_then(|containers| containers.get_mut(container))
        else {
            return Vec::new();
        };

        let taken: Vec<String> = keys_at_or_under(bases, path).cloned().collect();

        if !taken.is_empty() {
            self.dirty = true;
        }

        taken
            .into_iter()
            .map(|existing| {
                let entry = bases.remove(&existing).unwrap();
                (existing[path.len()..].to_owned(), entry)
            })
            .collect()
    }

    /// Inserts bases returned by [`BaseState::take_prefix`] under a new path.
    pub fn insert_with_prefix(
        &mut self,
        place: &str,
        container: &str,
        path: &str,
        entries: Vec<(String, BaseEntry)>,
    ) {
        if entries.is_empty() {
            return;
        }

        let bases = self
            .file
            .places
            .entry(place.to_owned())
            .or_default()
            .entry(container.to_owned())
            .or_default();

        for (suffix, entry) in entries {
            bases.insert(format!("{path}{suffix}"), entry);
        }
        self.dirty = true;
    }

    /// Re-keys the base of the file `from`, or of everything under the
    /// directory `from`, to `to`. Used when a place moves assets, so that they
    /// keep their bases.
    ///
    /// Only the given place's bases move. To every other place, the move looks
    /// like the files were deleted and added somewhere else, which is exactly
    /// what has to be applied to them when they connect next.
    #[cfg(test)]
    pub fn move_prefix(&mut self, place: &str, container: &str, from: &str, to: &str) {
        let entries = self.take_prefix(place, container, from);
        self.insert_with_prefix(place, container, to, entries);
    }

    /// Writes the state to disk if it changed since the last save.
    pub fn save_if_dirty(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        create_private_dir(self.path.parent().unwrap())?;

        self.file.version = STATE_VERSION;
        let contents = serde_json::to_vec(&self.file).map_err(io::Error::other)?;
        let temp_path = self.path.with_extension("json.tmp");
        fs_err::write(&temp_path, contents)?;
        fs_err::rename(&temp_path, &self.path)?;

        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn hash(byte: u8) -> Hash {
        [byte; 32]
    }

    fn state() -> (tempfile::TempDir, BaseState) {
        let dir = tempfile::tempdir().unwrap();
        let state = BaseState::load(BaseState::path_for_project(dir.path()));
        (dir, state)
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = BaseState::path_for_project(dir.path());

        let mut state = BaseState::load(path.clone());
        state.record("123", "assets", "Tree.rbxm", hash(1), hash(2), None);
        state.record("123", "assets", "Tree.rbxm", hash(1), hash(3), None);
        state.record("123", "assets", "Folder/Rock.rbxm", hash(3), hash(3), None);
        state.record("0", "assets", "Tree.rbxm", hash(4), hash(5), Some([7; 16]));
        state.set_session("123", "token");
        state.set_inspected("123", "assets");
        state.record("7", "assets", "Old.rbxm", hash(1), hash(2), None);
        state.set_file_behind("7", "assets", "Old.rbxm", true);
        state.record("7", "assets", "Older.rbxm", hash(1), hash(2), Some([8; 16]));
        state.set_file_behind("7", "assets", "Older.rbxm", true);
        let machine = state.machine().to_owned();
        state.save_if_dirty().unwrap();

        assert!(dir.path().join(".rojo").join(".gitignore").exists());

        let loaded = BaseState::load(path);
        let tree = loaded.get("123", "assets", "Tree.rbxm").unwrap();
        assert_eq!(tree.studio, hash(3));
        assert_eq!(tree.older, vec![short_hash(&hash(2))]);
        assert_eq!(
            loaded.get("123", "assets", "Folder/Rock.rbxm"),
            Some(&BaseEntry::new(hash(3), hash(3)))
        );
        assert_eq!(
            loaded.get("0", "assets", "Tree.rbxm"),
            Some(&BaseEntry {
                stable: Some([7; 16]),
                ..BaseEntry::new(hash(4), hash(5))
            })
        );
        assert_eq!(loaded.get("999", "assets", "Tree.rbxm"), None);
        assert_eq!(loaded.session("123"), Some("token"));
        assert_eq!(loaded.session("0"), None);
        assert!(loaded.was_inspected("123", "assets"));
        assert!(!loaded.was_inspected("123", "other"));
        assert!(!loaded.was_inspected("0", "assets"));
        assert_eq!(
            loaded.get("7", "assets", "Old.rbxm"),
            Some(&BaseEntry {
                behind: true,
                ..BaseEntry::new(hash(1), hash(2))
            })
        );
        assert_eq!(
            loaded.get("7", "assets", "Older.rbxm"),
            Some(&BaseEntry {
                behind: true,
                stable: Some([8; 16]),
                ..BaseEntry::new(hash(1), hash(2))
            })
        );
        assert_eq!(loaded.machine(), machine);
    }

    #[test]
    fn reads_entries_without_older_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = BaseState::path_for_project(dir.path());
        fs_err::create_dir_all(path.parent().unwrap()).unwrap();

        let fs = HEXLOWER.encode(&hash(1));
        let studio = HEXLOWER.encode(&hash(2));
        fs_err::write(
            &path,
            format!(
                r#"{{"version":1,"places":{{"1":{{"assets":{{"Tree.rbxm":["{fs}","{studio}"]}}}}}}}}"#
            ),
        )
        .unwrap();

        let state = BaseState::load(path);
        assert_eq!(
            state.get("1", "assets", "Tree.rbxm"),
            Some(&BaseEntry::new(hash(1), hash(2)))
        );
        assert!(!state.machine().is_empty());
    }

    #[test]
    fn remembers_older_studio_versions() {
        let (_dir, mut state) = state();

        for version in 1..=40 {
            state.record("1", "assets", "Tree.rbxm", hash(0), hash(version), None);
        }
        // Recording the same version again doesn't add it twice.
        state.record("1", "assets", "Tree.rbxm", hash(0), hash(40), None);

        let entry = state.get("1", "assets", "Tree.rbxm").unwrap().clone();
        assert_eq!(entry.studio, hash(40));
        assert_eq!(entry.older.len(), MAX_OLDER_VERSIONS);
        assert!(entry.is_older_studio_version(&hash(39)));
        assert!(entry.is_older_studio_version(&hash(8)));
        assert!(!entry.is_older_studio_version(&hash(7)));
        assert!(!entry.is_older_studio_version(&hash(40)));
        assert!(!entry.is_older_studio_version(&hash(41)));

        // Going back to an older version moves it to the end.
        state.record("1", "assets", "Tree.rbxm", hash(0), hash(20), None);
        let entry = state.get("1", "assets", "Tree.rbxm").unwrap();
        assert_eq!(entry.older.last(), Some(&short_hash(&hash(40))));
        assert!(!entry.older.contains(&short_hash(&hash(20))));
    }

    #[test]
    fn stable_hashes_belong_to_one_studio_version() {
        let (_dir, mut state) = state();
        let stable = |state: &BaseState| state.get("1", "assets", "Tree.rbxm").unwrap().stable;

        state.record("1", "assets", "Tree.rbxm", hash(0), hash(1), Some([1; 16]));
        assert_eq!(stable(&state), Some([1; 16]));

        // Not knowing it this time doesn't forget it, as long as Studio's
        // version is still the same one. The file may have changed.
        state.record("1", "assets", "Tree.rbxm", hash(0), hash(1), None);
        state.record("1", "assets", "Tree.rbxm", hash(9), hash(1), None);
        assert_eq!(stable(&state), Some([1; 16]));

        // It can become known later.
        state.record("1", "assets", "Tree.rbxm", hash(9), hash(1), Some([2; 16]));
        assert_eq!(stable(&state), Some([2; 16]));

        // A new version of Studio's has its own, or none.
        state.record("1", "assets", "Tree.rbxm", hash(9), hash(2), None);
        assert_eq!(stable(&state), None);
        state.record("1", "assets", "Tree.rbxm", hash(9), hash(3), Some([3; 16]));
        assert_eq!(stable(&state), Some([3; 16]));
    }

    #[test]
    fn files_are_only_behind_until_they_change() {
        let (_dir, mut state) = state();
        let behind = |state: &BaseState| state.get("1", "assets", "Tree.rbxm").unwrap().behind;

        state.record("1", "assets", "Tree.rbxm", hash(0), hash(1), None);
        assert!(!behind(&state));
        state.set_file_behind("1", "assets", "Tree.rbxm", true);
        assert!(behind(&state));

        // Studio serializing the asset differently says nothing about the file.
        state.record("1", "assets", "Tree.rbxm", hash(0), hash(2), None);
        assert!(behind(&state));

        // Another file isn't known to be behind.
        state.record("1", "assets", "Tree.rbxm", hash(9), hash(2), None);
        assert!(!behind(&state));

        // Nothing is recorded for assets that were never synced.
        state.set_file_behind("1", "assets", "Rock.rbxm", true);
        assert_eq!(state.get("1", "assets", "Rock.rbxm"), None);
    }

    #[test]
    fn groups_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = BaseState::path_for_project(dir.path());

        let groups = vec![Group {
            hash: hash(9),
            units: vec![hash(1), hash(2)],
        }];
        let mut state = BaseState::load(path.clone());
        state.set_groups("1", "assets", groups.clone());
        state.save_if_dirty().unwrap();

        let loaded = BaseState::load(path);
        assert_eq!(loaded.groups("1", "assets"), groups.as_slice());
        assert!(loaded.groups("2", "assets").is_empty());
    }

    #[test]
    fn invalid_machine_ids_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = BaseState::path_for_project(dir.path());
        fs_err::create_dir_all(path.parent().unwrap()).unwrap();
        fs_err::write(&path, r#"{"version":1,"machine":"not ok!","places":{}}"#).unwrap();

        let state = BaseState::load(path);
        assert_ne!(state.machine(), "not ok!");
        assert!(state.machine().len() >= 12);
    }

    #[test]
    fn malformed_state_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = BaseState::path_for_project(dir.path());
        fs_err::create_dir_all(path.parent().unwrap()).unwrap();
        fs_err::write(&path, "{ not json").unwrap();

        let state = BaseState::load(path);
        assert_eq!(state.get("0", "assets", "Tree.rbxm"), None);
    }

    #[test]
    fn move_prefix_rekeys_descendants_only() {
        let (_dir, mut state) = state();
        state.record("1", "assets", "Maps/A.rbxm", hash(1), hash(1), None);
        state.record("1", "assets", "Maps/Sub/B.rbxm", hash(2), hash(2), None);
        state.record("1", "assets", "Maps2/C.rbxm", hash(3), hash(3), None);
        state.record("2", "assets", "Maps/A.rbxm", hash(4), hash(4), None);

        state.move_prefix("1", "assets", "Maps", "Levels");

        assert_eq!(
            state.get("1", "assets", "Levels/A.rbxm"),
            Some(&BaseEntry::new(hash(1), hash(1)))
        );
        assert_eq!(
            state.get("1", "assets", "Levels/Sub/B.rbxm"),
            Some(&BaseEntry::new(hash(2), hash(2)))
        );
        assert_eq!(
            state.get("1", "assets", "Maps2/C.rbxm"),
            Some(&BaseEntry::new(hash(3), hash(3)))
        );
        assert_eq!(state.get("1", "assets", "Maps/A.rbxm"), None);

        // Other places keep their bases where they were.
        assert_eq!(
            state.get("2", "assets", "Maps/A.rbxm"),
            Some(&BaseEntry::new(hash(4), hash(4)))
        );
        assert_eq!(state.get("2", "assets", "Levels/A.rbxm"), None);
    }
}
