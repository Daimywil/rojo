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
//! Bases are tracked per place, since the same project can be served into
//! several places whose assets differ.

use std::{
    collections::BTreeMap,
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
}

impl BaseEntry {
    pub fn new(fs: Hash, studio: Hash) -> Self {
        Self {
            fs,
            studio,
            older: Vec::new(),
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
}

impl Serialize for BaseEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let fs = HEXLOWER.encode(&self.fs);
        let studio = HEXLOWER.encode(&self.studio);

        if self.older.is_empty() {
            (fs, studio).serialize(serializer)
        } else {
            let older: Vec<String> = self
                .older
                .iter()
                .map(|hash| HEXLOWER.encode(hash))
                .collect();
            (fs, studio, older).serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for BaseEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (fs, studio, older) = match BaseEntryRepr::deserialize(deserializer)? {
            BaseEntryRepr::Current(fs, studio) => (fs, studio, Vec::new()),
            BaseEntryRepr::WithOlder(fs, studio, older) => (fs, studio, older),
        };

        Ok(BaseEntry {
            fs: decode_hash(&fs).map_err(D::Error::custom)?,
            studio: decode_hash(&studio).map_err(D::Error::custom)?,
            older: older
                .iter()
                .map(|hash| decode_hash(hash))
                .collect::<Result<_, _>>()
                .map_err(D::Error::custom)?,
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

    #[cfg(test)]
    pub fn get(&self, place: &str, container: &str, path: &str) -> Option<&BaseEntry> {
        self.container(place, container)?.get(path)
    }

    /// Records that Studio and the filesystem agree on an asset. Studio's
    /// previous version of it, if any, is remembered as an older version.
    pub fn record(&mut self, place: &str, container: &str, path: &str, fs: Hash, studio: Hash) {
        let bases = self
            .file
            .places
            .entry(place.to_owned())
            .or_default()
            .entry(container.to_owned())
            .or_default();

        match bases.get_mut(path) {
            Some(entry) if entry.fs == fs && entry.studio == studio => {}
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
                }
                entry.fs = fs;
                entry.studio = studio;
                self.dirty = true;
            }
            None => {
                bases.insert(path.to_owned(), BaseEntry::new(fs, studio));
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
        state.record("123", "assets", "Tree.rbxm", hash(1), hash(2));
        state.record("123", "assets", "Tree.rbxm", hash(1), hash(3));
        state.record("123", "assets", "Folder/Rock.rbxm", hash(3), hash(3));
        state.record("0", "assets", "Tree.rbxm", hash(4), hash(5));
        state.set_session("123", "token");
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
            Some(&BaseEntry::new(hash(4), hash(5)))
        );
        assert_eq!(loaded.get("999", "assets", "Tree.rbxm"), None);
        assert_eq!(loaded.session("123"), Some("token"));
        assert_eq!(loaded.session("0"), None);
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
            state.record("1", "assets", "Tree.rbxm", hash(0), hash(version));
        }
        // Recording the same version again doesn't add it twice.
        state.record("1", "assets", "Tree.rbxm", hash(0), hash(40));

        let entry = state.get("1", "assets", "Tree.rbxm").unwrap().clone();
        assert_eq!(entry.studio, hash(40));
        assert_eq!(entry.older.len(), MAX_OLDER_VERSIONS);
        assert!(entry.is_older_studio_version(&hash(39)));
        assert!(entry.is_older_studio_version(&hash(8)));
        assert!(!entry.is_older_studio_version(&hash(7)));
        assert!(!entry.is_older_studio_version(&hash(40)));
        assert!(!entry.is_older_studio_version(&hash(41)));

        // Going back to an older version moves it to the end.
        state.record("1", "assets", "Tree.rbxm", hash(0), hash(20));
        let entry = state.get("1", "assets", "Tree.rbxm").unwrap();
        assert_eq!(entry.older.last(), Some(&short_hash(&hash(40))));
        assert!(!entry.older.contains(&short_hash(&hash(20))));
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
        state.record("1", "assets", "Maps/A.rbxm", hash(1), hash(1));
        state.record("1", "assets", "Maps/Sub/B.rbxm", hash(2), hash(2));
        state.record("1", "assets", "Maps2/C.rbxm", hash(3), hash(3));
        state.record("2", "assets", "Maps/A.rbxm", hash(4), hash(4));

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
