//! Keeps track of the Studio plugins that are syncing with the server, for
//! `/api/status` and the events of `rojo serve --events`.
//!
//! A client is one open `/api/socket` connection: the plugin opens it once it
//! is done connecting and keeps it open for as long as it syncs.

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::{
    serve_events::{self, DisconnectReason, ServeEvent},
    web::interface::StatusClient,
};

/// How long a string a plugin says about itself can be. They end up in
/// events and logs of other programs, so they're not taken on trust.
const MAX_DETAIL_LENGTH: usize = 128;

/// What a plugin says about itself in the query of the URL it connects to.
/// Plugins from before they said anything say nothing, so all of it is
/// optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientDetails {
    pub place_id: Option<u64>,
    pub game_id: Option<u64>,
    /// Made up by the plugin when Studio opens a place, to tell Studios with
    /// the same place open apart.
    pub studio_session_id: Option<String>,
    pub plugin_version: Option<String>,
}

impl ClientDetails {
    /// Reads the details from a query string like `placeId=1&gameId=2`.
    /// Anything missing or malformed is left out rather than refused, so
    /// that it can never keep a plugin from connecting.
    pub fn from_query(query: Option<&str>) -> Self {
        let mut details = Self::default();

        for pair in query.unwrap_or_default().split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let Some(value) = percent_decode(value) else {
                continue;
            };

            match key {
                "placeId" => details.place_id = value.parse().ok(),
                "gameId" => details.game_id = value.parse().ok(),
                "studioSessionId" => details.studio_session_id = text_detail(value),
                "pluginVersion" => details.plugin_version = text_detail(value),
                _ => {}
            }
        }

        details
    }
}

fn text_detail(value: String) -> Option<String> {
    let usable = !value.is_empty()
        && value.len() <= MAX_DETAIL_LENGTH
        && !value.chars().any(char::is_control);

    usable.then_some(value)
}

/// Decodes the `%XX` escapes of a query value.
fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut rest = value.as_bytes();

    while let Some((&byte, tail)) = rest.split_first() {
        match byte {
            b'%' => {
                let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
                rest = &tail[2..];
            }
            b'+' => {
                bytes.push(b' ');
                rest = tail;
            }
            _ => {
                bytes.push(byte);
                rest = tail;
            }
        }
    }

    String::from_utf8(bytes).ok()
}

struct Client {
    id: String,
    details: ClientDetails,
    connected_since: u64,
}

#[derive(Default)]
pub struct ConnectedClients {
    clients: Mutex<Vec<Client>>,
}

impl ConnectedClients {
    /// Adds a client until the returned [`ClientConnection`] is dropped.
    pub fn connect(self: &Arc<Self>, details: ClientDetails) -> ClientConnection {
        let id = Uuid::new_v4().to_string();

        self.clients.lock().unwrap().push(Client {
            id: id.clone(),
            details: details.clone(),
            connected_since: serve_events::unix_millis(),
        });

        serve_events::emit(&ServeEvent::ClientConnected {
            client_id: id.clone(),
            place_id: details.place_id,
            game_id: details.game_id,
            studio_session_id: details.studio_session_id,
            plugin_version: details.plugin_version,
        });

        ClientConnection {
            clients: Arc::clone(self),
            id,
            reason: DisconnectReason::ConnectionLost,
        }
    }

    /// The connected clients, the one that connected first first.
    pub fn list(&self) -> Vec<StatusClient> {
        self.clients
            .lock()
            .unwrap()
            .iter()
            .map(|client| StatusClient {
                client_id: client.id.clone(),
                place_id: client.details.place_id,
                game_id: client.details.game_id,
                studio_session_id: client.details.studio_session_id.clone(),
                plugin_version: client.details.plugin_version.clone(),
                connected_since: client.connected_since,
            })
            .collect()
    }

    /// Forgets every client, as when the server stops. Their connections
    /// don't report anything anymore once they end.
    pub fn disconnect_all(&self, reason: DisconnectReason) {
        let clients = std::mem::take(&mut *self.clients.lock().unwrap());

        for client in clients {
            serve_events::emit(&ServeEvent::ClientDisconnected {
                client_id: client.id,
                reason,
            });
        }
    }

    fn disconnect(&self, id: &str, reason: DisconnectReason) {
        let removed = {
            let mut clients = self.clients.lock().unwrap();
            let count = clients.len();
            clients.retain(|client| client.id != id);
            clients.len() != count
        };

        if removed {
            serve_events::emit(&ServeEvent::ClientDisconnected {
                client_id: id.to_owned(),
                reason,
            });
        }
    }
}

/// A client's place in [`ConnectedClients`], which it leaves when this is
/// dropped. That way a connection that ends in any way, including with an
/// error, is never left behind.
pub struct ClientConnection {
    clients: Arc<ConnectedClients>,
    id: String,
    reason: DisconnectReason,
}

impl ClientConnection {
    /// Sets why the connection ended, for when it's known. Until then it
    /// counts as lost.
    pub fn set_reason(&mut self, reason: DisconnectReason) {
        self.reason = reason;
    }
}

impl Drop for ClientConnection {
    fn drop(&mut self) {
        self.clients.disconnect(&self.id, self.reason);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn reads_what_the_plugin_says_about_itself() {
        let details = ClientDetails::from_query(Some(
            "placeId=122782083499089&gameId=9680415868&studioSessionId=3F2A-11&pluginVersion=7.7.1",
        ));

        assert_eq!(
            details,
            ClientDetails {
                place_id: Some(122782083499089),
                game_id: Some(9680415868),
                studio_session_id: Some("3F2A-11".to_owned()),
                plugin_version: Some("7.7.1".to_owned()),
            }
        );
    }

    #[test]
    fn plugins_that_say_nothing_still_connect() {
        assert_eq!(ClientDetails::from_query(None), ClientDetails::default());
        assert_eq!(
            ClientDetails::from_query(Some("")),
            ClientDetails::default()
        );
    }

    #[test]
    fn malformed_details_are_left_out() {
        let details = ClientDetails::from_query(Some(
            "placeId=abc&gameId=-1&studioSessionId=&pluginVersion=%zz&unknown=1&noValue",
        ));
        assert_eq!(details, ClientDetails::default());

        let long = "a".repeat(MAX_DETAIL_LENGTH + 1);
        let details = ClientDetails::from_query(Some(&format!(
            "studioSessionId={long}&pluginVersion=a%0Ab&placeId=5"
        )));
        assert_eq!(
            details,
            ClientDetails {
                place_id: Some(5),
                ..Default::default()
            }
        );
    }

    #[test]
    fn decodes_escaped_values() {
        let details = ClientDetails::from_query(Some(
            "studioSessionId=%7Babc%7D&pluginVersion=7.7.1%2Bfork+build",
        ));

        assert_eq!(details.studio_session_id.as_deref(), Some("{abc}"));
        assert_eq!(details.plugin_version.as_deref(), Some("7.7.1+fork build"));
    }

    #[test]
    fn clients_are_listed_until_their_connection_ends() {
        let clients = Arc::new(ConnectedClients::default());

        let first = clients.connect(ClientDetails {
            place_id: Some(1),
            ..Default::default()
        });
        let second = clients.connect(ClientDetails::default());

        let listed = clients.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].place_id, Some(1));
        assert_eq!(listed[1].place_id, None);
        assert_ne!(listed[0].client_id, listed[1].client_id);

        drop(first);
        let listed = clients.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].place_id, None);

        clients.disconnect_all(DisconnectReason::ServerShutdown);
        assert!(clients.list().is_empty());

        // Its connection ending afterwards has nothing left to remove.
        drop(second);
        assert!(clients.list().is_empty());
    }
}
