//! Machine-readable events for programs that run `rojo serve` and need to
//! follow what it does, like an app that runs several servers at once.
//!
//! With `rojo serve --events json`, every event is one line on stdout: the
//! marker [`EVENT_PREFIX`] followed by a JSON object. The marker keeps event
//! lines apart from everything else Rojo prints. Without the flag nothing is
//! printed, so everything here can be called unconditionally.
//!
//! The emitter is a global rather than something handed to whoever reports
//! events, because they're reported from places that have nothing else in
//! common, down to the panic hook.

use std::{
    collections::HashSet,
    io::{self, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::session_id::SessionId;

/// What every event line starts with.
pub const EVENT_PREFIX: &str = "@rojo-event ";

/// Something that happened to a `rojo serve` server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "event",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ServeEvent {
    /// The initial tree is built and the server accepts connections.
    Listening {
        port: u16,
        address: IpAddr,
        project_name: String,
        /// The project's `servePlaceIds`, or null if it serves any place.
        serve_place_ids: Option<Vec<u64>>,
        session_id: SessionId,
        server_version: String,
        protocol_version: u64,
    },

    /// A Studio plugin started syncing. What the plugin didn't say about
    /// itself, like plugins from before it said anything, is left out.
    ClientConnected {
        client_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        place_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        game_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        studio_session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        plugin_version: Option<String>,
    },

    ClientDisconnected {
        client_id: String,
        reason: DisconnectReason,
    },

    /// A file or directory could not be turned into instances. The tree
    /// keeps what it had for it.
    SyncError { path: String, message: String },

    /// A path reported with `sync_error` loaded again, or was removed.
    SyncErrorCleared { path: String },

    /// The server is stopping. It's the last event, but a server that is
    /// killed doesn't get to send it.
    Shutdown {
        reason: ShutdownReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    /// The plugin closed the connection, as it does when disconnecting.
    ClosedByClient,
    /// The connection ended without being closed, as it does when Studio
    /// closes or crashes.
    ConnectionLost,
    ServerShutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    /// Something asked the server to stop.
    Requested,
    /// The server could not start or keep running.
    Error,
    Panic,
}

struct Emitter {
    /// Paths that were last reported as failing to load, to tell when they
    /// load again.
    failed_paths: Mutex<HashSet<PathBuf>>,
}

static EMITTER: OnceLock<Emitter> = OnceLock::new();

/// Starts printing events. Until this is called, they're dropped.
pub fn enable() {
    let _ = EMITTER.set(Emitter {
        failed_paths: Mutex::new(HashSet::new()),
    });
}

pub fn is_enabled() -> bool {
    EMITTER.get().is_some()
}

/// Milliseconds since the Unix epoch, which is how every time is reported
/// to programs.
pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

pub fn emit(event: &ServeEvent) {
    if !is_enabled() {
        return;
    }

    let line = format_line(event, unix_millis());

    // Written and flushed under one lock, so events from different threads
    // never share a line and readers see each one right away, also through a
    // pipe.
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Reports that `path` could not be loaded.
pub fn sync_error(path: &Path, message: String) {
    let Some(emitter) = EMITTER.get() else {
        return;
    };

    emitter.failed_paths.lock().unwrap().insert(path.to_owned());
    emit(&ServeEvent::SyncError {
        path: display_path(path),
        message,
    });
}

/// Reports that `path` loaded or is gone, which only matters if it was
/// reported as failing before.
pub fn sync_ok(path: &Path) {
    let Some(emitter) = EMITTER.get() else {
        return;
    };

    if emitter.failed_paths.lock().unwrap().remove(path) {
        emit(&ServeEvent::SyncErrorCleared {
            path: display_path(path),
        });
    }
}

fn display_path(path: &Path) -> String {
    // Canonical paths on Windows can start with `\\?\`, which programs
    // showing them to people have no use for.
    dunce::simplified(path).display().to_string()
}

fn format_line(event: &ServeEvent, time: u64) -> String {
    #[derive(Serialize)]
    struct Line<'a> {
        #[serde(flatten)]
        event: &'a ServeEvent,
        time: u64,
    }

    // JSON escapes line breaks, so an event is always exactly one line.
    let json = serde_json::to_string(&Line { event, time })
        .unwrap_or_else(|err| format!(r#"{{"event":"error","message":"{err}"}}"#));

    format!("{EVENT_PREFIX}{json}")
}

#[cfg(test)]
mod test {
    use super::*;

    use std::net::Ipv4Addr;

    fn json(event: &ServeEvent) -> serde_json::Value {
        let line = format_line(event, 1234);
        let json = line
            .strip_prefix(EVENT_PREFIX)
            .expect("event lines start with the marker");
        assert!(!json.contains('\n'), "an event must be a single line");
        serde_json::from_str(json).expect("event lines hold JSON")
    }

    #[test]
    fn lines_start_with_the_marker_and_the_event_name() {
        let line = format_line(
            &ServeEvent::ClientDisconnected {
                client_id: "abc".to_owned(),
                reason: DisconnectReason::ConnectionLost,
            },
            1234,
        );

        assert_eq!(
            line,
            r#"@rojo-event {"event":"client_disconnected","clientId":"abc","reason":"connection_lost","time":1234}"#
        );
    }

    #[test]
    fn listening_describes_the_server() {
        let session_id = SessionId::new();
        let event = json(&ServeEvent::Listening {
            port: 34873,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            project_name: "game".to_owned(),
            serve_place_ids: Some(vec![1, 2]),
            session_id,
            server_version: "7.7.1".to_owned(),
            protocol_version: 5,
        });

        assert_eq!(
            event,
            serde_json::json!({
                "event": "listening",
                "port": 34873,
                "address": "127.0.0.1",
                "projectName": "game",
                "servePlaceIds": [1, 2],
                "sessionId": session_id.to_string(),
                "serverVersion": "7.7.1",
                "protocolVersion": 5,
                "time": 1234,
            })
        );
    }

    #[test]
    fn listening_says_when_any_place_is_served() {
        let event = json(&ServeEvent::Listening {
            port: 34873,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            project_name: "game".to_owned(),
            serve_place_ids: None,
            session_id: SessionId::new(),
            server_version: "7.7.1".to_owned(),
            protocol_version: 5,
        });

        assert_eq!(event["servePlaceIds"], serde_json::Value::Null);
    }

    #[test]
    fn client_connected_leaves_out_what_the_plugin_did_not_send() {
        let event = json(&ServeEvent::ClientConnected {
            client_id: "abc".to_owned(),
            place_id: Some(123),
            game_id: None,
            studio_session_id: Some("studio".to_owned()),
            plugin_version: None,
        });

        assert_eq!(
            event,
            serde_json::json!({
                "event": "client_connected",
                "clientId": "abc",
                "placeId": 123,
                "studioSessionId": "studio",
                "time": 1234,
            })
        );
    }

    #[test]
    fn messages_with_line_breaks_stay_on_one_line() {
        let event = json(&ServeEvent::SyncError {
            path: "src/a.model.json".to_owned(),
            message: "first line\nsecond line".to_owned(),
        });

        assert_eq!(event["event"], "sync_error");
        assert_eq!(event["message"], "first line\nsecond line");
    }

    #[test]
    fn shutdown_says_why() {
        assert_eq!(
            json(&ServeEvent::Shutdown {
                reason: ShutdownReason::Requested,
                message: None,
            }),
            serde_json::json!({ "event": "shutdown", "reason": "requested", "time": 1234 })
        );

        assert_eq!(
            json(&ServeEvent::Shutdown {
                reason: ShutdownReason::Error,
                message: Some("port in use".to_owned()),
            }),
            serde_json::json!({
                "event": "shutdown",
                "reason": "error",
                "message": "port in use",
                "time": 1234,
            })
        );
    }
}
