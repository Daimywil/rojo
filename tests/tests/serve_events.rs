//! Tests for what `rojo serve` tells the programs that run it: the events of
//! `--events json` and `/api/status`.

use std::fs;

use serde_json::json;

use crate::rojo_test::serve_util::{close_socket, TestServeSession};

/// What a plugin that says which Studio it is in adds to the URL it connects
/// to.
const PLUGIN_QUERY: &str =
    "placeId=123&gameId=9680415868&studioSessionId=3F2A44C1-0B7E&pluginVersion=7.7.1";

#[test]
fn listening_is_sent_once_the_server_accepts_connections() {
    let session = TestServeSession::new_with_events("serve_events");

    let listening = session.wait_for_event("listening");
    assert_eq!(listening["port"], session.port());
    assert_eq!(listening["address"], "127.0.0.1");
    assert_eq!(listening["projectName"], "serve_events");
    // In order, unlike in the project file.
    assert_eq!(listening["servePlaceIds"], json!([123, 456]));
    assert_eq!(listening["protocolVersion"], 5);

    // Nothing has to be waited for after the event: the server answers.
    let info = session
        .get_api_rojo()
        .expect("the server should answer once it sent listening");
    assert_eq!(listening["sessionId"], info.session_id.to_string());
    assert_eq!(listening["serverVersion"], info.server_version);
}

#[test]
fn clients_are_reported_as_they_come_and_go() {
    let session = TestServeSession::new_with_events("serve_events");
    session.wait_for_event("listening");

    let socket = session.connect_api_socket(PLUGIN_QUERY);
    let connected = session.wait_for_event("client_connected");
    let client_id = connected["clientId"]
        .as_str()
        .expect("clients should have an ID")
        .to_owned();
    assert_eq!(connected["placeId"], 123);
    assert_eq!(connected["gameId"], 9680415868u64);
    assert_eq!(connected["studioSessionId"], "3F2A44C1-0B7E");
    assert_eq!(connected["pluginVersion"], "7.7.1");

    close_socket(socket);
    let disconnected = session.wait_for_event("client_disconnected");
    assert_eq!(disconnected["clientId"], client_id.as_str());
    assert_eq!(disconnected["reason"], "closed_by_client");

    // A Studio that crashes doesn't close its connection first.
    let socket = session.connect_api_socket(PLUGIN_QUERY);
    let connected = session.wait_for_event("client_connected");
    assert_ne!(connected["clientId"], client_id.as_str());

    drop(socket);
    let disconnected = session.wait_for_event("client_disconnected");
    assert_eq!(disconnected["clientId"], connected["clientId"]);
    assert_eq!(disconnected["reason"], "connection_lost");
}

#[test]
fn plugins_that_say_nothing_about_themselves_still_connect() {
    let session = TestServeSession::new_with_events("serve_events");
    session.wait_for_event("listening");

    // The plugins published before this connect without a query.
    let socket = session.connect_api_socket("");
    let connected = session.wait_for_event("client_connected");

    let fields: Vec<&str> = connected
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(fields, ["clientId", "event", "time"]);

    let status = session.get_api_status();
    assert_eq!(status.clients.len(), 1);
    assert_eq!(status.clients[0].place_id, None);
    assert_eq!(status.clients[0].studio_session_id, None);

    // What can't be read is left out as well, rather than refusing the
    // plugin.
    let other = session.connect_api_socket("placeId=not-a-number&studioSessionId=abc");
    let connected = session.wait_for_event("client_connected");
    assert_eq!(connected.get("placeId"), None);
    assert_eq!(connected["studioSessionId"], "abc");

    close_socket(socket);
    close_socket(other);
}

#[test]
fn files_that_fail_to_load_are_reported() {
    let session = TestServeSession::new_with_events("serve_events");
    session.wait_for_event("listening");

    let model_path = session.path().join("src").join("thing.model.json");

    fs::write(&model_path, "{ this is not JSON").unwrap();
    let error = session.wait_for_event("sync_error");
    let path = error["path"].as_str().unwrap();
    assert!(
        path.ends_with("thing.model.json"),
        "the event should name the file that failed to load, got {path}"
    );
    assert!(
        !path.starts_with(r"\\?\"),
        "paths should be ones people can read, got {path}"
    );
    assert!(
        !error["message"].as_str().unwrap().is_empty(),
        "the event should say what is wrong with the file"
    );

    fs::write(&model_path, r#"{ "ClassName": "Model" }"#).unwrap();
    let cleared = session.wait_for_event("sync_error_cleared");
    assert_eq!(cleared["path"], error["path"]);
}

#[test]
fn shutdown_is_sent_when_the_server_is_asked_to_stop() {
    let mut session = TestServeSession::new_with_events("serve_events");
    session.wait_for_event("listening");

    let _socket = session.connect_api_socket(PLUGIN_QUERY);
    let connected = session.wait_for_event("client_connected");

    // Lines that don't ask for anything are ignored.
    session.write_stdin_line("");
    session.write_stdin_line("something else");
    session.write_stdin_line("shutdown");

    // The clients are gone before the server is.
    let disconnected = session.wait_for_event("client_disconnected");
    assert_eq!(disconnected["clientId"], connected["clientId"]);
    assert_eq!(disconnected["reason"], "server_shutdown");

    let shutdown = session.wait_for_event("shutdown");
    assert_eq!(shutdown["reason"], "requested");
    assert_eq!(shutdown.get("message"), None);

    assert!(session.wait_for_exit().success());
}

#[test]
fn shutdown_says_why_the_server_could_not_start() {
    let first = TestServeSession::new_with_events("serve_events");
    first.wait_for_event("listening");

    // The port is taken, so a second server can't listen on it.
    let port = first.port().to_string();
    let mut second = TestServeSession::new_with_events_on_port("serve_events", first.port());

    let shutdown = second.wait_for_event("shutdown");
    assert_eq!(shutdown["reason"], "error");
    assert!(
        shutdown["message"].as_str().unwrap().contains(&port),
        "the event should say what went wrong: {shutdown}"
    );
    assert!(!second.wait_for_exit().success());
}

#[test]
fn nothing_is_printed_as_an_event_without_the_flag() {
    let mut session = TestServeSession::new_with_captured_output("serve_events");
    session.wait_to_come_online();

    let socket = session.connect_api_socket(PLUGIN_QUERY);
    close_socket(socket);

    // Without events, stdin isn't read either.
    session.write_stdin_line("shutdown");
    std::thread::sleep(std::time::Duration::from_millis(300));
    session
        .get_api_rojo()
        .expect("the server should keep running");

    let lines = session.stdout_so_far();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Rojo server listening")),
        "the server should print what it always printed, got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("@rojo-event")),
        "the server should not print events, got {lines:?}"
    );
}

#[test]
fn status_describes_the_server_and_its_clients() {
    let mut session = TestServeSession::new("serve_events");
    let info = session.wait_to_come_online();

    let status = session.get_api_status();
    assert_eq!(status.port as usize, session.port());
    assert_eq!(status.address.to_string(), "127.0.0.1");
    assert_eq!(status.project_name, "serve_events");
    assert_eq!(status.serve_place_ids, Some(vec![123, 456]));
    assert_eq!(status.session_id, info.session_id);
    assert_eq!(status.server_version, info.server_version);
    assert_eq!(status.protocol_version, info.protocol_version);
    assert!(status.started_at > 0);
    assert_eq!(status.clients, Vec::new());

    let first = session.connect_api_socket(PLUGIN_QUERY);
    let second = session.connect_api_socket("placeId=456&studioSessionId=other");

    let status = wait_for_clients(&session, 2);
    let client = &status.clients[0];
    assert_eq!(client.place_id, Some(123));
    assert_eq!(client.game_id, Some(9680415868));
    assert_eq!(client.studio_session_id.as_deref(), Some("3F2A44C1-0B7E"));
    assert_eq!(client.plugin_version.as_deref(), Some("7.7.1"));
    assert!(client.connected_since >= status.started_at);

    let other = &status.clients[1];
    assert_eq!(other.place_id, Some(456));
    assert_eq!(other.game_id, None);
    assert_eq!(other.studio_session_id.as_deref(), Some("other"));
    assert_ne!(other.client_id, client.client_id);

    close_socket(first);
    let status = wait_for_clients(&session, 1);
    assert_eq!(
        status.clients[0].studio_session_id.as_deref(),
        Some("other")
    );

    drop(second);
    wait_for_clients(&session, 0);
}

/// The server learns that a socket opened or closed a moment after the
/// client does, so the status is asked for until it shows it.
fn wait_for_clients(session: &TestServeSession, count: usize) -> librojo::web_api::StatusResponse {
    for _ in 0..100 {
        let status = session.get_api_status();
        if status.clients.len() == count {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    panic!("the server never had {count} clients");
}
