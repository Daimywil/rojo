//! Keeps track of the Rojo servers running on this machine, so that the Studio
//! plugin can list them instead of having their ports typed in.
//!
//! Every `rojo serve` writes a file describing itself to a directory shared by
//! every Rojo on the machine, and removes it when it stops. Servers that are
//! killed can't remove theirs, so entries whose port nothing listens on
//! anymore are left out and removed whenever the list is read.
//!
//! To read the list, the plugin needs an address that always answers. Every
//! server answers on its own port, and one of them at a time also listens on
//! [`DISCOVERY_PORT`]: each one tries to, and the others try again every few
//! seconds, so another takes over when the one listening stops.

use std::{
    collections::HashMap,
    fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// The port that one of the running servers always listens on, to tell the
/// plugin about all of them. It's right below Rojo's default port, which
/// projects are least likely to pick for themselves.
pub const DISCOVERY_PORT: u16 = 34871;

/// How long a port may take to accept a connection before its server counts
/// as gone. Connecting to a port on this machine is instant either way, except
/// on Windows, which retries refused connections to loopback for a while.
const ALIVE_TIMEOUT: Duration = Duration::from_millis(300);

/// A running Rojo server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerEntry {
    pub port: u16,
    /// The address the server listens on.
    pub address: IpAddr,
    pub project_name: String,
    /// The project file the server serves.
    pub project_path: PathBuf,
    pub pid: u32,
    /// When the server started, in seconds since the Unix epoch.
    pub started_at: u64,
}

impl ServerEntry {
    pub fn new(port: u16, address: IpAddr, project_name: String, project_path: PathBuf) -> Self {
        Self {
            port,
            address,
            project_name,
            project_path,
            pid: process::id(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
        }
    }

    /// Whether something still listens on the server's port.
    fn is_alive(&self) -> bool {
        // A server listening on every address can be reached on loopback.
        let ip = if self.address.is_unspecified() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            self.address
        };

        TcpStream::connect_timeout(&SocketAddr::new(ip, self.port), ALIVE_TIMEOUT).is_ok()
    }
}

/// The directory every Rojo on this machine registers its servers in.
pub fn registry_dir() -> PathBuf {
    std::env::temp_dir().join("rojo-servers")
}

/// A server's entry in the registry, which is removed when this is dropped.
#[derive(Debug)]
pub struct Registration {
    path: PathBuf,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Adds a server to the registry until the returned [`Registration`] is
/// dropped.
pub fn register(entry: &ServerEntry) -> io::Result<Registration> {
    register_in(&registry_dir(), entry)
}

fn register_in(dir: &Path, entry: &ServerEntry) -> io::Result<Registration> {
    fs::create_dir_all(dir)?;

    let path = dir.join(format!("{}-{}.json", entry.pid, entry.port));
    let contents = serde_json::to_vec_pretty(entry).map_err(io::Error::other)?;

    // Written to another file first, so a reader never sees half an entry.
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, contents)?;
    fs::rename(&temp_path, &path)?;

    Ok(Registration { path })
}

/// Lists the running servers by port, removing the entries of servers that
/// stopped.
pub fn list() -> Vec<ServerEntry> {
    list_in(&registry_dir())
}

fn list_in(dir: &Path) -> Vec<ServerEntry> {
    let Ok(dir_entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let entries: Vec<(PathBuf, ServerEntry)> = dir_entries
        .flatten()
        .map(|dir_entry| dir_entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| {
            let entry = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            Some((path, entry))
        })
        .collect();

    // Checking whether servers are alive can take a moment each, so they're
    // all checked at once.
    let alive: Vec<bool> = std::thread::scope(|scope| {
        let checks: Vec<_> = entries
            .iter()
            .map(|(_, entry)| scope.spawn(|| entry.is_alive()))
            .collect();
        checks
            .into_iter()
            .map(|check| check.join().unwrap_or(false))
            .collect()
    });

    // A port can only belong to one server at a time, so of several entries
    // for the same port, the one that started last is the one listening.
    let mut by_port: HashMap<u16, ServerEntry> = HashMap::new();
    for ((path, entry), alive) in entries.into_iter().zip(alive) {
        if !alive {
            let _ = fs::remove_file(&path);
            continue;
        }

        match by_port.get(&entry.port) {
            Some(existing) if existing.started_at > entry.started_at => {}
            _ => {
                by_port.insert(entry.port, entry);
            }
        }
    }

    let mut servers: Vec<ServerEntry> = by_port.into_values().collect();
    servers.sort_by_key(|server| server.port);
    servers
}

#[cfg(test)]
mod test {
    use super::*;

    use std::net::TcpListener;

    fn entry(port: u16, name: &str, started_at: u64) -> ServerEntry {
        ServerEntry {
            port,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            project_name: name.to_owned(),
            project_path: PathBuf::from(format!("{name}/default.project.json")),
            pid: started_at as u32,
            started_at,
        }
    }

    /// A port something listens on, for as long as the listener is kept.
    fn listening_port() -> (TcpListener, u16) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn lists_registered_servers_until_they_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (_listener, port) = listening_port();

        let server = entry(port, "game", 1);
        let registration = register_in(dir.path(), &server).unwrap();
        assert_eq!(list_in(dir.path()), vec![server]);

        drop(registration);
        assert_eq!(list_in(dir.path()), Vec::new());
    }

    #[test]
    fn servers_that_stopped_are_left_out_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (listener, port) = listening_port();

        // The server was killed, so its entry stays behind.
        std::mem::forget(register_in(dir.path(), &entry(port, "game", 1)).unwrap());
        drop(listener);

        assert_eq!(list_in(dir.path()), Vec::new());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_newest_server_on_a_port_wins() {
        let dir = tempfile::tempdir().unwrap();
        let (_listener, port) = listening_port();

        let old = entry(port, "old", 1);
        let new = entry(port, "new", 2);
        std::mem::forget(register_in(dir.path(), &old).unwrap());
        let _registration = register_in(dir.path(), &new).unwrap();

        assert_eq!(list_in(dir.path()), vec![new]);
    }

    #[test]
    fn missing_registries_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(list_in(&dir.path().join("nothing")), Vec::new());
    }
}
