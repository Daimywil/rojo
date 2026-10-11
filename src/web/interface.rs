//! Defines all the structs needed to interact with the Rojo Serve API. This is
//! useful for tests to be able to use the same data structures as the
//! implementation.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use rbx_dom_weak::{
    types::{Ref, Variant, VariantType},
    Ustr, UstrMap,
};
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::{
    session_id::SessionId,
    snapshot::{
        AppliedPatchSet, InstanceMetadata as RojoInstanceMetadata, InstanceWithMeta, RojoTree,
    },
};

/// Server version to report over the API, not exposed outside this crate.
pub(crate) const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Current protocol version, which is required to match.
pub const PROTOCOL_VERSION: u64 = 5;

/// Message returned by Rojo API when a change has occurred.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeMessage<'a> {
    pub removed: Vec<Ref>,
    pub added: HashMap<Ref, Instance<'a>>,
    pub updated: Vec<InstanceUpdate>,
}

impl<'a> SubscribeMessage<'a> {
    pub(crate) fn from_patch_update(tree: &'a RojoTree, patch: AppliedPatchSet) -> Self {
        let removed = patch.removed;

        let mut added = HashMap::new();
        for id in patch.added {
            let instance = tree.get_instance(id).unwrap();
            added.insert(id, Instance::from_rojo_instance(instance));

            for instance in tree.descendants(id) {
                added.insert(instance.id(), Instance::from_rojo_instance(instance));
            }
        }

        let updated = patch
            .updated
            .into_iter()
            .map(|update| {
                let changed_metadata = update
                    .changed_metadata
                    .as_ref()
                    .map(InstanceMetadata::from_rojo_metadata);

                let changed_properties = update
                    .changed_properties
                    .into_iter()
                    .filter(|(_key, value)| property_filter(value.as_ref()))
                    .collect();

                InstanceUpdate {
                    id: update.id,
                    changed_name: update.changed_name,
                    changed_class_name: update.changed_class_name,
                    changed_properties,
                    changed_metadata,
                }
            })
            .collect();

        Self {
            removed,
            added,
            updated,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceUpdate {
    pub id: Ref,
    pub changed_name: Option<String>,
    pub changed_class_name: Option<Ustr>,

    // TODO: Transform from UstrMap<String, Option<_>> to something else, since
    // null will get lost when decoding from JSON in some languages.
    #[serde(default)]
    pub changed_properties: UstrMap<Option<Variant>>,
    pub changed_metadata: Option<InstanceMetadata>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceMetadata {
    pub ignore_unknown_instances: bool,
}

impl InstanceMetadata {
    pub(crate) fn from_rojo_metadata(meta: &RojoInstanceMetadata) -> Self {
        Self {
            ignore_unknown_instances: meta.ignore_unknown_instances,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Instance<'a> {
    pub id: Ref,
    pub parent: Ref,
    pub name: Cow<'a, str>,
    pub class_name: Ustr,
    pub properties: UstrMap<Cow<'a, Variant>>,
    pub children: Cow<'a, [Ref]>,
    pub metadata: Option<InstanceMetadata>,
}

impl Instance<'_> {
    pub(crate) fn from_rojo_instance(source: InstanceWithMeta<'_>) -> Instance<'_> {
        let properties = source
            .properties()
            .iter()
            .filter(|(_key, value)| property_filter(Some(value)))
            .map(|(key, value)| (*key, Cow::Borrowed(value)))
            .collect();

        Instance {
            id: source.id(),
            parent: source.parent(),
            name: Cow::Borrowed(source.name()),
            class_name: source.class_name(),
            properties,
            children: Cow::Borrowed(source.children()),
            metadata: Some(InstanceMetadata::from_rojo_metadata(source.metadata())),
        }
    }
}

fn property_filter(value: Option<&Variant>) -> bool {
    let ty = value.map(|value| value.ty());

    // Lua can't do anything with SharedString values. They also can't be
    // serialized directly by Serde!
    ty != Some(VariantType::SharedString)
}

/// Response body from /api/rojo
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfoResponse {
    pub session_id: SessionId,
    pub server_version: String,
    pub protocol_version: u64,
    pub project_name: String,
    pub expected_place_ids: Option<HashSet<u64>>,
    pub unexpected_place_ids: Option<HashSet<u64>>,
    pub game_id: Option<u64>,
    pub place_id: Option<u64>,
    pub root_instance_id: Ref,
}

/// Response body from /api/read/{id}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResponse<'a> {
    pub session_id: SessionId,
    pub message_cursor: u32,
    pub instances: HashMap<Ref, Instance<'a>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRequest {
    pub session_id: SessionId,
    pub removed: Vec<Ref>,

    #[serde(default)]
    pub added: HashMap<Ref, ()>,
    pub updated: Vec<InstanceUpdate>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResponse {
    pub session_id: SessionId,
}

/// Packet type enum for different websocket message types
#[derive(Debug, Serialize, Deserialize, Display, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum SocketPacketType {
    Messages,
    // TODO: Can we cleanly use the socket for all communication?
    // Serialize,
    // RefPatch,
}

/// Body content for messages packet type
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagesPacket<'a> {
    pub message_cursor: u32,
    pub messages: Vec<SubscribeMessage<'a>>,
}

/// Body content for different packet types
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SocketPacketBody<'a> {
    Messages(MessagesPacket<'a>),
    // TODO: Can we cleanly use the socket for all communication?
    // Serialize(SerializePacket),
    // RefPatch(RefPatchPacket<'a>),
}

/// Message content from /api/socket
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketPacket<'a> {
    pub session_id: SessionId,
    pub packet_type: SocketPacketType,
    pub body: SocketPacketBody<'a>,
}

/// Response body from /api/open/{id}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResponse {
    pub session_id: SessionId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializeRequest {
    pub session_id: SessionId,
    pub ids: Vec<Ref>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializeResponse {
    pub session_id: SessionId,
    #[serde(with = "serde_bytes")]
    pub model_contents: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefPatchRequest {
    pub session_id: SessionId,
    pub ids: HashSet<Ref>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefPatchResponse<'a> {
    pub session_id: SessionId,
    pub patch: SubscribeMessage<'a>,
}

/// An asset directory that is synced into an instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetContainer {
    /// The instance that the assets are children of.
    pub id: Ref,

    /// The asset directory, relative to the project.
    pub path: String,
}

/// Response body from GET /api/assets
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsResponse {
    pub session_id: SessionId,

    /// Cursor to pass to /api/assets/socket to hear about every change made
    /// after this response.
    pub message_cursor: u32,

    pub containers: Vec<AssetContainer>,

    /// The name of the attribute of ServerStorage that stores the token of the
    /// last session that synced the place's assets.
    pub session_attribute: String,
}

/// The hash of an asset, as computed by Studio.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetHash {
    pub path: String,
    #[serde(with = "serde_bytes")]
    pub hash: Vec<u8>,
}

/// Request body for POST /api/assets/plan
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsPlanRequest {
    pub session_id: SessionId,
    /// Identifies the client across requests. Uploads are kept per client.
    pub client_id: String,
    /// The place being synced. Changes are tracked per place.
    pub place_id: String,
    /// The session token stored in the place, if it has one. It tells whether
    /// the place was saved after the last session that synced it.
    #[serde(default)]
    pub session: Option<String>,
    pub container: Ref,
    /// The hash of every asset Studio has in the container.
    pub assets: Vec<AssetHash>,
    /// Assets Studio has but couldn't serialize. Nothing is decided for them,
    /// so in particular, their files are never deleted.
    #[serde(default)]
    pub skipped: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetPlanEntry {
    pub path: String,
    pub action: crate::assets::PlanAction,
    /// Why the action was picked, for conflicts and other actions that aren't
    /// obvious.
    pub reason: Option<crate::assets::PlanReason>,
}

/// Response body from POST /api/assets/plan
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsPlanResponse {
    pub session_id: SessionId,
    /// Assets that have to be uploaded with /api/assets/stage before the plan
    /// can be final. If this is empty, the plan is final.
    pub need_upload: Vec<String>,
    /// Every asset that isn't in sync, and what to do about it.
    pub actions: Vec<AssetPlanEntry>,
    /// Whether this place never synced this container before. Without a
    /// previous sync to compare against, both sides are simply merged, which
    /// clients may want to confirm first.
    pub first_sync: bool,
}

/// An asset serialized by Studio.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetContents {
    pub path: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Request body for POST /api/assets/stage
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsStageRequest {
    pub session_id: SessionId,
    pub client_id: String,
    pub container: Ref,
    pub assets: Vec<AssetContents>,
}

/// Request body for POST /api/assets/apply
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsApplyRequest {
    pub session_id: SessionId,
    pub client_id: String,
    pub place_id: String,
    pub container: Ref,
    /// Staged assets to write to the filesystem.
    #[serde(default)]
    pub to_fs: Vec<String>,
    /// Asset files to delete.
    #[serde(default)]
    pub delete_fs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetAck {
    pub path: String,
    /// The hash of the file contents that were applied, from /api/assets/read.
    #[serde(with = "serde_bytes")]
    pub fs_hash: Vec<u8>,
    /// The hash of what Studio serialized the asset to after applying it.
    #[serde(with = "serde_bytes")]
    pub studio_hash: Vec<u8>,
}

/// Request body for POST /api/assets/ack
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsAckRequest {
    pub session_id: SessionId,
    pub place_id: String,
    pub container: Ref,
    /// Files that were applied to Studio.
    #[serde(default)]
    pub applied: Vec<AssetAck>,
    /// Assets that were deleted from Studio because their files were deleted.
    #[serde(default)]
    pub removed: Vec<String>,
}

/// A change made in Studio during live sync. `kind` is one of `write`,
/// `remove`, or `move`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetWriteOp {
    pub kind: String,
    pub path: String,
    /// The serialized asset, for `write`.
    #[serde(default, with = "serde_bytes")]
    pub data: Option<Vec<u8>>,
    /// Where to move the file or directory at `path` to, for `move`.
    #[serde(default)]
    pub to: Option<String>,
}

/// Request body for POST /api/assets/write
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsWriteRequest {
    pub session_id: SessionId,
    pub client_id: String,
    pub place_id: String,
    pub container: Ref,
    pub ops: Vec<AssetWriteOp>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetWriteFailure {
    pub path: String,
    pub error: String,
}

/// Response body from POST /api/assets/write
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsWriteResponse {
    pub session_id: SessionId,
    /// Operations that could not be carried out. The client should try them
    /// again later.
    pub failures: Vec<AssetWriteFailure>,
}

/// Request body for POST /api/assets/session, sent once a client finished
/// syncing a place and stored the session token in it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsSessionRequest {
    pub session_id: SessionId,
    pub place_id: String,
    pub session: String,
}

/// Request body for POST /api/assets/read
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsReadRequest {
    pub session_id: SessionId,
    pub container: Ref,
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetFile {
    pub path: String,
    /// The asset file's contents, ready to be deserialized by Studio.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    /// The hash of the file's contents, to acknowledge applying it with.
    #[serde(with = "serde_bytes")]
    pub hash: Vec<u8>,
}

/// Response body from POST /api/assets/read
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsReadResponse {
    pub session_id: SessionId,
    /// The requested files. Files that no longer exist are left out.
    pub files: Vec<AssetFile>,
}

/// Request body for POST /api/assets/known
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsKnownRequest {
    pub session_id: SessionId,
    pub place_id: String,
    pub container: Ref,
}

/// An asset that a place synced before.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetSynced {
    pub path: String,
    /// The first 16 bytes of the hash of everything Studio serialized the
    /// asset to when it was synced, latest first.
    pub hashes: Vec<serde_bytes::ByteBuf>,
}

/// Response body from POST /api/assets/known
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsKnownResponse {
    pub session_id: SessionId,
    /// Every asset the place synced before, even if its file was deleted.
    pub synced: Vec<AssetSynced>,
    /// Every asset file on disk.
    pub files: Vec<String>,
    /// Groups of assets that were in sync when the place last started
    /// syncing.
    pub groups: Vec<AssetGroup>,
}

/// Assets that Studio serialized together.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetGroup {
    /// The hash of the assets serialized together.
    pub hash: serde_bytes::ByteBuf,
    /// The hash of each asset serialized on its own, in order.
    pub units: Vec<serde_bytes::ByteBuf>,
}

/// Request body for POST /api/assets/groups
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsGroupsRequest {
    pub session_id: SessionId,
    pub place_id: String,
    pub container: Ref,
    pub groups: Vec<AssetGroup>,
}

/// Response body from POST requests that don't return anything else.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsOkResponse {
    pub session_id: SessionId,
}

/// A change to an asset file on the filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetFileChange {
    pub container: Ref,
    pub path: String,
    pub removed: bool,
    /// The client that made this change, if it came from a client.
    pub origin: Option<String>,
}

/// Message sent over /api/assets/socket
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetsSocketPacket {
    pub session_id: SessionId,
    pub message_cursor: u32,
    pub changes: Vec<AssetFileChange>,
    /// The set of asset directories changed. Clients should start their asset
    /// sync over.
    pub containers_changed: bool,
}

/// General response type returned from all Rojo routes
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    kind: ErrorResponseKind,
    details: String,
}

impl ErrorResponse {
    pub fn not_found<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::NotFound,
            details: details.into(),
        }
    }

    pub fn bad_request<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::BadRequest,
            details: details.into(),
        }
    }

    pub fn forbidden<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::Forbidden,
            details: details.into(),
        }
    }

    pub fn internal_error<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::InternalError,
            details: details.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ErrorResponseKind {
    NotFound,
    BadRequest,
    Forbidden,
    InternalError,
}
