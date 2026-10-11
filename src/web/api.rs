//! Defines Rojo's HTTP API, all under /api. These endpoints generally return
//! JSON.

use std::{collections::HashMap, fs, net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc};

use futures::{sink::SinkExt, stream::StreamExt};
use hyper::{body, Body, Method, Request, Response, StatusCode};
use hyper_tungstenite::{is_upgrade_request, tungstenite::Message, upgrade, HyperWebsocket};
use opener::OpenError;
use rbx_dom_weak::{
    types::{Ref, Variant},
    InstanceBuilder, UstrMap, WeakDom,
};

use serde::{de::DeserializeOwned, Serialize};

use crate::{
    assets::{AckEntry, AssetMessage, Group, Hash, WriteOp},
    serve_session::ServeSession,
    session_id::SessionId,
    snapshot::{InstanceWithMeta, PatchSet, PatchUpdate},
    web::{
        interface::{
            AssetContainer, AssetFile, AssetFileChange, AssetGroup, AssetPlanEntry, AssetSynced,
            AssetWriteFailure, AssetsAckRequest, AssetsApplyRequest, AssetsGroupsRequest,
            AssetsKnownRequest, AssetsKnownResponse, AssetsOkResponse, AssetsPlanRequest,
            AssetsPlanResponse, AssetsReadRequest, AssetsReadResponse, AssetsResponse,
            AssetsSessionRequest, AssetsSocketPacket, AssetsStageRequest, AssetsWriteRequest,
            AssetsWriteResponse, ErrorResponse, Instance, MessagesPacket, OpenResponse,
            ReadResponse, ServerInfoResponse, SocketPacket, SocketPacketBody, SocketPacketType,
            SubscribeMessage, WriteRequest, WriteResponse, PROTOCOL_VERSION, SERVER_VERSION,
        },
        origin::canonical,
        util::{deserialize_msgpack, msgpack, msgpack_ok, serialize_msgpack},
    },
    web_api::{
        InstanceUpdate, RefPatchRequest, RefPatchResponse, SerializeRequest, SerializeResponse,
    },
};

pub async fn call(
    serve_session: Arc<ServeSession>,
    remote_addr: SocketAddr,
    mut request: Request<Body>,
) -> Response<Body> {
    let service = ApiService::new(serve_session, remote_addr);

    match (request.method(), request.uri().path()) {
        (&Method::GET, "/api/rojo") => service.handle_api_rojo().await,
        (&Method::GET, path) if path.starts_with("/api/read/") => {
            service.handle_api_read(request).await
        }
        (&Method::GET, path) if path.starts_with("/api/socket/") => {
            if is_upgrade_request(&request) {
                service.handle_api_socket(&mut request).await
            } else {
                msgpack(
                    ErrorResponse::bad_request(
                        "/api/socket must be called as a websocket upgrade request",
                    ),
                    StatusCode::BAD_REQUEST,
                )
            }
        }
        (&Method::POST, "/api/serialize") => service.handle_api_serialize(request).await,
        (&Method::POST, "/api/ref-patch") => service.handle_api_ref_patch(request).await,

        (&Method::POST, path) if path.starts_with("/api/open/") => {
            service.handle_api_open(request).await
        }
        (&Method::POST, "/api/write") => service.handle_api_write(request).await,

        (&Method::GET, "/api/assets") => service.handle_api_assets().await,
        (&Method::GET, path) if path.starts_with("/api/assets/socket/") => {
            if is_upgrade_request(&request) {
                service.handle_api_assets_socket(&mut request).await
            } else {
                msgpack(
                    ErrorResponse::bad_request(
                        "/api/assets/socket must be called as a websocket upgrade request",
                    ),
                    StatusCode::BAD_REQUEST,
                )
            }
        }
        (&Method::POST, "/api/assets/plan") => {
            service
                .handle_assets_request(request, handle_assets_plan)
                .await
        }
        (&Method::POST, "/api/assets/stage") => {
            service
                .handle_assets_request(request, handle_assets_stage)
                .await
        }
        (&Method::POST, "/api/assets/apply") => {
            service
                .handle_assets_request(request, handle_assets_apply)
                .await
        }
        (&Method::POST, "/api/assets/ack") => {
            service
                .handle_assets_request(request, handle_assets_ack)
                .await
        }
        (&Method::POST, "/api/assets/write") => {
            service
                .handle_assets_request(request, handle_assets_write)
                .await
        }
        (&Method::POST, "/api/assets/session") => {
            service
                .handle_assets_request(request, handle_assets_session)
                .await
        }
        (&Method::POST, "/api/assets/read") => {
            service
                .handle_assets_request(request, handle_assets_read)
                .await
        }
        (&Method::POST, "/api/assets/known") => {
            service
                .handle_assets_request(request, handle_assets_known)
                .await
        }
        (&Method::POST, "/api/assets/groups") => {
            service
                .handle_assets_request(request, handle_assets_groups)
                .await
        }

        (_method, path) => msgpack(
            ErrorResponse::not_found(format!("Route not found: {}", path)),
            StatusCode::NOT_FOUND,
        ),
    }
}

pub struct ApiService {
    serve_session: Arc<ServeSession>,
    remote_addr: SocketAddr,
}

impl ApiService {
    pub fn new(serve_session: Arc<ServeSession>, remote_addr: SocketAddr) -> Self {
        ApiService {
            serve_session,
            remote_addr,
        }
    }

    /// Get a summary of information about the server
    async fn handle_api_rojo(&self) -> Response<Body> {
        let tree = self.serve_session.tree();
        let root_instance_id = tree.get_root_id();

        msgpack_ok(&ServerInfoResponse {
            server_version: SERVER_VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            session_id: self.serve_session.session_id(),
            project_name: self.serve_session.project_name().to_owned(),
            expected_place_ids: self.serve_session.serve_place_ids().cloned(),
            unexpected_place_ids: self.serve_session.blocked_place_ids().cloned(),
            place_id: self.serve_session.place_id(),
            game_id: self.serve_session.game_id(),
            root_instance_id,
        })
    }

    /// Handle WebSocket upgrade for real-time message streaming
    async fn handle_api_socket(&self, request: &mut Request<Body>) -> Response<Body> {
        let argument = &request.uri().path()["/api/socket/".len()..];
        let input_cursor: u32 = match argument.parse() {
            Ok(v) => v,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Malformed message cursor: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        // Upgrade the connection to WebSocket
        let (response, websocket) = match upgrade(request, None) {
            Ok(result) => result,
            Err(err) => {
                return msgpack(
                    ErrorResponse::internal_error(format!("WebSocket upgrade failed: {}", err)),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        };

        let serve_session = Arc::clone(&self.serve_session);

        // Spawn a task to handle the WebSocket connection
        tokio::spawn(async move {
            if let Err(e) =
                handle_websocket_subscription(serve_session, websocket, input_cursor).await
            {
                log::error!("Error in websocket subscription: {}", e);
            }
        });

        response
    }

    /// Lists the asset directories that are synced into instances.
    async fn handle_api_assets(&self) -> Response<Body> {
        let asset_store = self.serve_session.asset_store();

        // The cursor is read first so that no change made while the client
        // syncs can slip by unnoticed.
        let message_cursor = asset_store.messages().cursor();
        let containers = asset_store
            .containers()
            .into_iter()
            .map(|container| AssetContainer {
                id: container.id,
                path: container.key,
            })
            .collect();

        msgpack_ok(AssetsResponse {
            session_id: self.serve_session.session_id(),
            message_cursor,
            containers,
            session_attribute: asset_store.session_attribute(),
        })
    }

    /// Streams changes to asset files to the client.
    async fn handle_api_assets_socket(&self, request: &mut Request<Body>) -> Response<Body> {
        let argument = &request.uri().path()["/api/assets/socket/".len()..];
        let input_cursor: u32 = match argument.parse() {
            Ok(v) => v,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Malformed message cursor: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let (response, websocket) = match upgrade(request, None) {
            Ok(result) => result,
            Err(err) => {
                return msgpack(
                    ErrorResponse::internal_error(format!("WebSocket upgrade failed: {}", err)),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        };

        let serve_session = Arc::clone(&self.serve_session);

        tokio::spawn(async move {
            if let Err(e) = handle_assets_websocket(serve_session, websocket, input_cursor).await {
                log::error!("Error in assets websocket subscription: {}", e);
            }
        });

        response
    }

    /// Handles an asset sync request. The handler runs on a thread that is
    /// allowed to block, since asset requests can read, hash, and write
    /// thousands of files.
    async fn handle_assets_request<Req, Res>(
        &self,
        request: Request<Body>,
        handler: fn(&ServeSession, Req) -> anyhow::Result<Res>,
    ) -> Response<Body>
    where
        Req: AssetsRequest + DeserializeOwned + Send + 'static,
        Res: Serialize + Send + 'static,
    {
        let body = match body::to_bytes(request.into_body()).await {
            Ok(body) => body,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Could not read body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let request: Req = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };
        drop(body);

        if request.session_id() != self.serve_session.session_id() {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let serve_session = Arc::clone(&self.serve_session);
        let result = tokio::task::spawn_blocking(move || handler(&serve_session, request)).await;

        match result {
            Ok(Ok(response)) => msgpack_ok(response),
            Ok(Err(err)) => msgpack(
                ErrorResponse::internal_error(format!("{err:#}")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Err(err) => msgpack(
                ErrorResponse::internal_error(format!("Asset request failed: {err}")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        }
    }

    async fn handle_api_write(&self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let tree_mutation_sender = self.serve_session.tree_mutation_sender();

        let body = body::to_bytes(request.into_body()).await.unwrap();

        let request: WriteRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let updated_instances = request
            .updated
            .into_iter()
            .map(|update| PatchUpdate {
                id: update.id,
                changed_class_name: update.changed_class_name,
                changed_name: update.changed_name,
                changed_properties: update.changed_properties,
                changed_metadata: None,
            })
            .collect();

        tree_mutation_sender
            .send(PatchSet {
                removed_instances: Vec::new(),
                added_instances: Vec::new(),
                updated_instances,
            })
            .unwrap();

        msgpack_ok(WriteResponse { session_id })
    }

    async fn handle_api_read(&self, request: Request<Body>) -> Response<Body> {
        let argument = &request.uri().path()["/api/read/".len()..];
        let requested_ids: Result<Vec<Ref>, _> = argument.split(',').map(Ref::from_str).collect();

        let requested_ids = match requested_ids {
            Ok(ids) => ids,
            Err(_) => {
                return msgpack(
                    ErrorResponse::bad_request("Malformed ID list"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let message_queue = self.serve_session.message_queue();
        let message_cursor = message_queue.cursor();

        let tree = self.serve_session.tree();

        let mut instances = HashMap::new();

        for id in requested_ids {
            if let Some(instance) = tree.get_instance(id) {
                instances.insert(id, Instance::from_rojo_instance(instance));

                for descendant in tree.descendants(id) {
                    instances.insert(descendant.id(), Instance::from_rojo_instance(descendant));
                }
            }
        }

        msgpack_ok(ReadResponse {
            session_id: self.serve_session.session_id(),
            message_cursor,
            instances,
        })
    }

    /// Accepts a list of IDs and returns them serialized as a binary model.
    /// The model is sent in a schema that causes Roblox to deserialize it as
    /// a Luau `buffer`.
    ///
    /// The returned model is a folder that contains ObjectValues with names
    /// that correspond to the requested Instances. These values have their
    /// `Value` property set to point to the requested Instance.
    async fn handle_api_serialize(&self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let body = body::to_bytes(request.into_body()).await.unwrap();

        let request: SerializeRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let mut response_dom = WeakDom::new(InstanceBuilder::new("Folder"));

        let tree = self.serve_session.tree();
        for id in &request.ids {
            if let Some(instance) = tree.get_instance(*id) {
                let clone = response_dom.insert(
                    Ref::none(),
                    InstanceBuilder::new(instance.class_name())
                        .with_name(instance.name())
                        .with_properties(instance.properties().clone()),
                );
                let object_value = response_dom.insert(
                    response_dom.root_ref(),
                    InstanceBuilder::new("ObjectValue")
                        .with_name(id.to_string())
                        .with_property("Value", clone),
                );

                let mut child_ref = clone;
                if let Some(parent_class) = parent_requirements(&instance.class_name()) {
                    child_ref =
                        response_dom.insert(object_value, InstanceBuilder::new(parent_class));
                    response_dom.transfer_within(clone, child_ref);
                }

                response_dom.transfer_within(child_ref, object_value);
            } else {
                return msgpack(
                    ErrorResponse::bad_request(format!("provided id {id} is not in the tree")),
                    StatusCode::BAD_REQUEST,
                );
            }
        }
        drop(tree);

        let mut source = Vec::new();
        rbx_binary::to_writer(&mut source, &response_dom, &[response_dom.root_ref()]).unwrap();

        msgpack_ok(SerializeResponse {
            session_id: self.serve_session.session_id(),
            model_contents: source,
        })
    }

    /// Returns a list of all referent properties that point towards the
    /// provided IDs. Used because the plugin does not store a RojoTree,
    /// and referent properties need to be updated after the serialize
    /// endpoint is used.
    async fn handle_api_ref_patch(self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let body = body::to_bytes(request.into_body()).await.unwrap();

        let request: RefPatchRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let mut instance_updates: HashMap<Ref, InstanceUpdate> = HashMap::new();

        let tree = self.serve_session.tree();
        for instance in tree.descendants(tree.get_root_id()) {
            for (prop_name, prop_value) in instance.properties() {
                let Variant::Ref(prop_value) = prop_value else {
                    continue;
                };
                if let Some(target_id) = request.ids.get(prop_value) {
                    let instance_id = instance.id();
                    let update =
                        instance_updates
                            .entry(instance_id)
                            .or_insert_with(|| InstanceUpdate {
                                id: instance_id,
                                changed_class_name: None,
                                changed_name: None,
                                changed_metadata: None,
                                changed_properties: UstrMap::default(),
                            });
                    update
                        .changed_properties
                        .insert(*prop_name, Some(Variant::Ref(*target_id)));
                }
            }
        }

        msgpack_ok(RefPatchResponse {
            session_id: self.serve_session.session_id(),
            patch: SubscribeMessage {
                added: HashMap::new(),
                removed: Vec::new(),
                updated: instance_updates.into_values().collect(),
            },
        })
    }

    /// Open a script with the given ID in the user's default text editor.
    async fn handle_api_open(&self, request: Request<Body>) -> Response<Body> {
        // Opening a file launches a local program, so it must never be reachable
        // by a remote client even when the server is bound to an exposed address.
        //
        // `remote_addr` is the immediate peer, which is the best locality signal
        // we have: the legitimate caller is a sandboxed Roblox plugin whose only
        // credential is being able to reach the port, so there is no secret to
        // authenticate it with. A connection forwarded over loopback by an
        // SSH/Tailscale tunnel or a local reverse proxy therefore appears local
        // and is allowed. That is delegated trust rather than a bypass: by
        // standing up that tunnel or proxy the user has decided the remote end is
        // trusted, and reachability is bounded by that hop's own authentication
        // (e.g. SSH keys or Tailscale ACLs). This gate only stops direct,
        // unauthenticated peers.
        //
        // An IPv4 client reaching a dual-stack (`::`) bind appears as an
        // IPv4-mapped IPv6 peer (`::ffff:127.0.0.1`), so canonicalize to the bare
        // IPv4 form before the loopback test, matching `origin`'s handling.
        if !canonical(self.remote_addr.ip()).is_loopback() {
            return msgpack(
                ErrorResponse::forbidden("/api/open is only available to local clients"),
                StatusCode::FORBIDDEN,
            );
        }

        let argument = &request.uri().path()["/api/open/".len()..];
        let requested_id = match Ref::from_str(argument) {
            Ok(id) => id,
            Err(_) => {
                return msgpack(
                    ErrorResponse::bad_request("Invalid instance ID"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let tree = self.serve_session.tree();

        let instance = match tree.get_instance(requested_id) {
            Some(instance) => instance,
            None => {
                return msgpack(
                    ErrorResponse::bad_request("Instance not found"),
                    StatusCode::NOT_FOUND,
                );
            }
        };

        let script_path = match pick_script_path(instance) {
            Some(path) => path,
            None => {
                return msgpack(
                    ErrorResponse::bad_request(
                        "No appropriate file could be found to open this script",
                    ),
                    StatusCode::NOT_FOUND,
                );
            }
        };

        match opener::open(&script_path) {
            Ok(()) => {}
            Err(error) => match error {
                OpenError::Io(io_error) => {
                    return msgpack(
                        ErrorResponse::internal_error(format!(
                            "Attempting to open {} failed because of the following io error: {}",
                            script_path.display(),
                            io_error
                        )),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    )
                }
                OpenError::ExitStatus {
                    cmd,
                    status,
                    stderr,
                } => {
                    return msgpack(
                        ErrorResponse::internal_error(format!(
                            r#"The command '{}' to open '{}' failed with the error code '{}'.
                            Error logs:
                            {}"#,
                            cmd,
                            script_path.display(),
                            status,
                            stderr
                        )),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    )
                }
            },
        };

        msgpack_ok(OpenResponse {
            session_id: self.serve_session.session_id(),
        })
    }
}

/// If this instance is represented by a script, try to find the correct .lua or .luau
/// file to open to edit it.
fn pick_script_path(instance: InstanceWithMeta<'_>) -> Option<PathBuf> {
    match instance.class_name().as_str() {
        "Script" | "LocalScript" | "ModuleScript" => {}
        _ => return None,
    }

    // Pick the first listed relevant path that has an extension of .lua or .luau that
    // exists.
    instance
        .metadata()
        .relevant_paths
        .iter()
        .find(|path| {
            // We should only ever open Lua or Luau files to be safe.
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("lua") => {}
                Some("luau") => {}
                _ => return false,
            }

            fs::metadata(path)
                .map(|meta| meta.is_file())
                .unwrap_or(false)
        })
        .map(|path| path.to_owned())
}

/// Handle WebSocket connection for streaming subscription messages
async fn handle_websocket_subscription(
    serve_session: Arc<ServeSession>,
    websocket: HyperWebsocket,
    input_cursor: u32,
) -> anyhow::Result<()> {
    let mut websocket = websocket.await?;

    let session_id = serve_session.session_id();
    let tree_handle = serve_session.tree_handle();
    let message_queue = serve_session.message_queue();

    log::debug!(
        "WebSocket subscription established for session {}",
        session_id
    );

    // Now continuously listen for new messages using select to handle both incoming messages
    // and WebSocket control messages concurrently
    let mut cursor = input_cursor;
    loop {
        let receiver = message_queue.subscribe(cursor);

        tokio::select! {
            // Handle new messages from the message queue
            result = receiver => {
                match result {
                    Ok((new_cursor, messages)) => {
                        if !messages.is_empty() {
                            let msgpack_message = {
                                let tree = tree_handle.lock().unwrap();
                                let api_messages = messages
                                    .into_iter()
                                    .map(|patch| SubscribeMessage::from_patch_update(&tree, patch))
                                    .collect();

                                let response = SocketPacket {
                                    session_id,
                                    packet_type: SocketPacketType::Messages,
                                    body: SocketPacketBody::Messages(MessagesPacket {
                                        message_cursor: new_cursor,
                                        messages: api_messages,
                                    }),
                                };

                                serialize_msgpack(response)?
                            };

                            log::debug!("Sending batch of messages over WebSocket subscription");

                            if websocket.send(Message::Binary(msgpack_message)).await.is_err() {
                                // Client disconnected
                                log::debug!("WebSocket subscription closed by client");
                                break;
                            }
                            cursor = new_cursor;
                        }
                    }
                    Err(_) => {
                        // Message queue disconnected
                        log::debug!("Message queue disconnected; closing WebSocket subscription");
                        let _ = websocket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }

            // Handle incoming WebSocket messages (ping/pong/close)
            msg = websocket.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) => {
                        log::debug!("WebSocket subscription closed by client");
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        // tungstenite handles pong automatically
                        log::debug!("Received ping: {:?}", data);
                    }
                    Some(Ok(Message::Pong(data))) => {
                        log::debug!("Received pong: {:?}", data);
                    }
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                        // Ignore text/binary messages from client for subscription endpoint
                        // TODO: Use this for bidirectional sync or requesting fallbacks?
                        log::debug!("Ignoring message from client since we don't use it for anything yet: {:?}", msg);
                    }
                    Some(Ok(Message::Frame(_))) => {
                        // This should never happen according to tungstenite docs
                        unreachable!();
                    }
                    Some(Err(e)) => {
                        log::error!("WebSocket error: {}", e);
                        break;
                    }
                    None => {
                        // WebSocket stream ended
                        log::debug!("WebSocket stream ended");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// A request to one of the asset sync endpoints.
trait AssetsRequest {
    fn session_id(&self) -> SessionId;
}

macro_rules! impl_assets_request {
    ($($request:ty),*) => {
        $(impl AssetsRequest for $request {
            fn session_id(&self) -> SessionId {
                self.session_id
            }
        })*
    };
}

impl_assets_request!(
    AssetsPlanRequest,
    AssetsStageRequest,
    AssetsApplyRequest,
    AssetsAckRequest,
    AssetsWriteRequest,
    AssetsReadRequest,
    AssetsSessionRequest,
    AssetsKnownRequest,
    AssetsGroupsRequest
);

fn parse_hash(bytes: &[u8]) -> anyhow::Result<Hash> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Hashes must be 32 bytes long, got {}", bytes.len()))
}

fn handle_assets_plan(
    session: &ServeSession,
    request: AssetsPlanRequest,
) -> anyhow::Result<AssetsPlanResponse> {
    let assets = request
        .assets
        .into_iter()
        .map(|asset| Ok((asset.path, parse_hash(&asset.hash)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let plan = session.asset_store().plan(
        &request.client_id,
        &request.place_id,
        request.session.as_deref(),
        request.container,
        &assets,
        &request.skipped,
    )?;

    Ok(AssetsPlanResponse {
        session_id: session.session_id(),
        first_sync: plan.first_sync,
        need_upload: plan.need_upload,
        actions: plan
            .actions
            .into_iter()
            .map(|entry| AssetPlanEntry {
                path: entry.path,
                action: entry.action,
                reason: entry.reason,
            })
            .collect(),
    })
}

fn handle_assets_stage(
    session: &ServeSession,
    request: AssetsStageRequest,
) -> anyhow::Result<AssetsOkResponse> {
    let assets = request
        .assets
        .into_iter()
        .map(|asset| (asset.path, asset.data))
        .collect();

    session
        .asset_store()
        .stage(&request.client_id, request.container, assets)?;

    Ok(AssetsOkResponse {
        session_id: session.session_id(),
    })
}

fn handle_assets_apply(
    session: &ServeSession,
    request: AssetsApplyRequest,
) -> anyhow::Result<AssetsOkResponse> {
    session.asset_store().apply(
        &request.client_id,
        &request.place_id,
        request.container,
        &request.to_fs,
        &request.delete_fs,
    )?;

    Ok(AssetsOkResponse {
        session_id: session.session_id(),
    })
}

fn handle_assets_ack(
    session: &ServeSession,
    request: AssetsAckRequest,
) -> anyhow::Result<AssetsOkResponse> {
    let applied = request
        .applied
        .into_iter()
        .map(|ack| {
            Ok(AckEntry {
                path: ack.path,
                fs_hash: parse_hash(&ack.fs_hash)?,
                studio_hash: parse_hash(&ack.studio_hash)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    session.asset_store().ack(
        &request.place_id,
        request.container,
        &applied,
        &request.removed,
    )?;

    Ok(AssetsOkResponse {
        session_id: session.session_id(),
    })
}

fn handle_assets_write(
    session: &ServeSession,
    request: AssetsWriteRequest,
) -> anyhow::Result<AssetsWriteResponse> {
    let mut failures = Vec::new();
    let mut ops = Vec::with_capacity(request.ops.len());

    for op in request.ops {
        match (op.kind.as_str(), op.data, op.to) {
            ("write", Some(data), _) => ops.push(WriteOp::Write {
                path: op.path,
                data,
            }),
            ("remove", _, _) => ops.push(WriteOp::Remove { path: op.path }),
            ("move", _, Some(to)) => ops.push(WriteOp::Move { from: op.path, to }),
            (kind, _, _) => failures.push(AssetWriteFailure {
                error: format!("malformed {kind} operation"),
                path: op.path,
            }),
        }
    }

    let store_failures = session.asset_store().write(
        &request.client_id,
        &request.place_id,
        request.container,
        ops,
    )?;

    failures.extend(
        store_failures
            .into_iter()
            .map(|(path, error)| AssetWriteFailure { path, error }),
    );

    for failure in &failures {
        log::warn!("Could not sync asset {}: {}", failure.path, failure.error);
    }

    Ok(AssetsWriteResponse {
        session_id: session.session_id(),
        failures,
    })
}

fn handle_assets_session(
    session: &ServeSession,
    request: AssetsSessionRequest,
) -> anyhow::Result<AssetsOkResponse> {
    session
        .asset_store()
        .start_session(&request.place_id, &request.session)?;

    Ok(AssetsOkResponse {
        session_id: session.session_id(),
    })
}

fn handle_assets_read(
    session: &ServeSession,
    request: AssetsReadRequest,
) -> anyhow::Result<AssetsReadResponse> {
    let files = session
        .asset_store()
        .read(request.container, &request.paths)?
        .into_iter()
        .map(|entry| AssetFile {
            path: entry.path,
            data: entry.data,
            hash: entry.hash.to_vec(),
        })
        .collect();

    Ok(AssetsReadResponse {
        session_id: session.session_id(),
        files,
    })
}

fn handle_assets_known(
    session: &ServeSession,
    request: AssetsKnownRequest,
) -> anyhow::Result<AssetsKnownResponse> {
    let known = session
        .asset_store()
        .known(&request.place_id, request.container)?;

    let synced = known
        .synced
        .into_iter()
        .map(|asset| {
            let latest = std::iter::once(asset.studio[..16].to_vec());
            let older = asset.older.iter().rev().map(|hash| hash.to_vec());

            AssetSynced {
                path: asset.path,
                hashes: latest
                    .chain(older)
                    .map(serde_bytes::ByteBuf::from)
                    .collect(),
            }
        })
        .collect();

    let groups = known
        .groups
        .into_iter()
        .map(|group| AssetGroup {
            hash: serde_bytes::ByteBuf::from(group.hash.to_vec()),
            units: group
                .units
                .iter()
                .map(|hash| serde_bytes::ByteBuf::from(hash.to_vec()))
                .collect(),
        })
        .collect();

    Ok(AssetsKnownResponse {
        session_id: session.session_id(),
        synced,
        files: known.files,
        groups,
    })
}

fn handle_assets_groups(
    session: &ServeSession,
    request: AssetsGroupsRequest,
) -> anyhow::Result<AssetsOkResponse> {
    let groups = request
        .groups
        .into_iter()
        .map(|group| {
            Ok(Group {
                hash: parse_hash(&group.hash)?,
                units: group
                    .units
                    .iter()
                    .map(|hash| parse_hash(hash))
                    .collect::<anyhow::Result<_>>()?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    session
        .asset_store()
        .set_groups(&request.place_id, request.container, groups)?;

    Ok(AssetsOkResponse {
        session_id: session.session_id(),
    })
}

/// Streams asset file changes over a websocket, much like
/// `handle_websocket_subscription` does for instance changes.
async fn handle_assets_websocket(
    serve_session: Arc<ServeSession>,
    websocket: HyperWebsocket,
    input_cursor: u32,
) -> anyhow::Result<()> {
    let mut websocket = websocket.await?;
    let session_id = serve_session.session_id();
    let mut cursor = input_cursor;

    loop {
        let receiver = serve_session.asset_store().messages().subscribe(cursor);

        tokio::select! {
            result = receiver => {
                let Ok((new_cursor, messages)) = result else {
                    let _ = websocket.send(Message::Close(None)).await;
                    break;
                };

                let mut changes = Vec::new();
                let mut containers_changed = false;

                for message in messages {
                    match message {
                        AssetMessage::Change(change) => changes.push(AssetFileChange {
                            container: change.container,
                            path: change.path,
                            removed: change.removed,
                            origin: change.origin,
                        }),
                        AssetMessage::ContainersChanged => containers_changed = true,
                    }
                }

                let packet = serialize_msgpack(AssetsSocketPacket {
                    session_id,
                    message_cursor: new_cursor,
                    changes,
                    containers_changed,
                })?;

                if websocket.send(Message::Binary(packet)).await.is_err() {
                    break;
                }
                cursor = new_cursor;
            }

            message = websocket.next() => {
                match message {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(err)) => {
                        log::debug!("Assets websocket error: {}", err);
                        break;
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    Ok(())
}

/// Certain Instances MUST be a child of specific classes. This function
/// tracks that information for the Serialize endpoint.
///
/// If a parent requirement exists, it will be returned.
/// Otherwise returns `None`.
fn parent_requirements(class: &str) -> Option<&str> {
    Some(match class {
        "Attachment" | "Bone" => "Part",
        "Animator" => "Humanoid",
        "BaseWrap" | "WrapLayer" | "WrapTarget" | "WrapDeformer" => "MeshPart",
        _ => return None,
    })
}
