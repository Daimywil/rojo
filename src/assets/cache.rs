//! Remembers what every asset file contains between runs, so that starting
//! Rojo only has to look at the size and modification time of asset files
//! instead of reading all of them.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};

use super::state::{create_private_dir, Hash};

const CACHE_VERSION: u32 = 1;

/// What's known about an asset file: the hash of its contents, and the size
/// and modification time it had when it was hashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEntry {
    pub hash: Hash,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// An entry as it's stored: the hash in hex, the size, and the modification
/// time in nanoseconds since the Unix epoch.
#[derive(Serialize, Deserialize)]
struct StoredEntry(String, u64, Option<u64>);

#[derive(Default, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    /// Asset directory -> asset path -> entry
    containers: BTreeMap<String, BTreeMap<String, StoredEntry>>,
}

/// Where the cache of the project in the given folder is stored.
pub fn path_for_project(project_folder: &Path) -> PathBuf {
    project_folder.join(".rojo").join("asset-index.json")
}

fn to_nanos(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_nanos().try_into().ok())
}

/// Loads the cached index of every asset directory. Anything that can't be
/// read is left out, since the files are simply read again then.
pub fn load(path: &Path) -> BTreeMap<String, BTreeMap<String, FileEntry>> {
    let file = match fs_err::read(path) {
        Ok(contents) => match serde_json::from_slice::<CacheFile>(&contents) {
            Ok(file) if file.version == CACHE_VERSION => file,
            Ok(_) => return BTreeMap::new(),
            Err(err) => {
                log::debug!("Ignoring malformed asset index cache: {err}");
                return BTreeMap::new();
            }
        },
        Err(_) => return BTreeMap::new(),
    };

    file.containers
        .into_iter()
        .map(|(key, entries)| {
            let entries = entries
                .into_iter()
                .filter_map(|(path, StoredEntry(hash, len, modified))| {
                    let hash = HEXLOWER.decode(hash.as_bytes()).ok()?.try_into().ok()?;
                    let modified = modified.map(|nanos| UNIX_EPOCH + Duration::from_nanos(nanos));
                    Some((
                        path,
                        FileEntry {
                            hash,
                            len,
                            modified,
                        },
                    ))
                })
                .collect();
            (key, entries)
        })
        .collect()
}

/// Saves the index of every asset directory.
pub fn save<'a>(
    path: &Path,
    containers: impl IntoIterator<Item = (&'a str, &'a BTreeMap<String, FileEntry>)>,
) -> io::Result<()> {
    let file = CacheFile {
        version: CACHE_VERSION,
        containers: containers
            .into_iter()
            .map(|(key, entries)| {
                let entries = entries
                    .iter()
                    .map(|(path, entry)| {
                        let stored = StoredEntry(
                            HEXLOWER.encode(&entry.hash),
                            entry.len,
                            entry.modified.and_then(to_nanos),
                        );
                        (path.clone(), stored)
                    })
                    .collect();
                (key.to_owned(), entries)
            })
            .collect(),
    };

    create_private_dir(path.parent().unwrap())?;
    let contents = serde_json::to_vec(&file).map_err(io::Error::other)?;
    let temp_path = path.with_extension("json.tmp");
    fs_err::write(&temp_path, contents)?;
    fs_err::rename(&temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for_project(dir.path());

        let entry = FileEntry {
            hash: [7; 32],
            len: 1234,
            modified: Some(UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_789)),
        };
        let entries: BTreeMap<String, FileEntry> = [
            ("Tree.rbxm".to_owned(), entry),
            (
                "Maps/Rock.rbxm".to_owned(),
                FileEntry {
                    modified: None,
                    ..entry
                },
            ),
        ]
        .into();

        save(&path, [("assets", &entries)]).unwrap();
        assert!(dir.path().join(".rojo").join(".gitignore").exists());

        let loaded = load(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["assets"], entries);
    }

    #[test]
    fn missing_or_malformed_caches_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for_project(dir.path());
        assert!(load(&path).is_empty());

        fs_err::create_dir_all(path.parent().unwrap()).unwrap();
        fs_err::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_empty());
    }
}
