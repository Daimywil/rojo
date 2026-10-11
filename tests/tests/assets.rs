//! End-to-end tests for syncing assets declared with `$assets`.

use std::{fs, path::Path, process::Command};

use rbx_dom_weak::{types::Ref, InstanceBuilder, WeakDom};
use tempfile::tempdir;

use librojo::{
    web_api::{
        AssetContents, AssetHash, AssetWriteOp, AssetsApplyRequest, AssetsKnownRequest,
        AssetsKnownResponse, AssetsOkResponse, AssetsPlanRequest, AssetsPlanResponse,
        AssetsReadRequest, AssetsReadResponse, AssetsSessionRequest, AssetsStageRequest,
        AssetsWriteRequest, AssetsWriteResponse,
    },
    AssetPlanAction, AssetPlanReason, SessionId,
};

use crate::rojo_test::{
    io_util::{get_working_dir_path, ROJO_PATH},
    serve_util::{run_serve_test, TestServeSession},
};

/// Serializes a part the way Studio does, with compressed chunks.
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

/// Serializes a part the way asset files are stored, with uncompressed chunks.
fn stored_model(name: &str, transparency: f32) -> Vec<u8> {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let part = dom.insert(
        dom.root_ref(),
        InstanceBuilder::new("Part")
            .with_name(name)
            .with_property("Transparency", transparency),
    );

    let mut output = Vec::new();
    rbx_binary::Serializer::new()
        .compression_type(rbx_binary::CompressionType::None)
        .serialize(&mut output, &dom, &[part])
        .unwrap();
    output
}

fn model_name(contents: &[u8]) -> String {
    let dom = rbx_binary::from_reader(contents).unwrap();
    dom.get_by_ref(dom.root().children()[0])
        .unwrap()
        .name
        .clone()
}

fn hash(bytes: &[u8]) -> Vec<u8> {
    blake3::hash(bytes).as_bytes().to_vec()
}

struct Assets<'a> {
    session: &'a TestServeSession,
    session_id: SessionId,
    container: Ref,
}

impl Assets<'_> {
    fn write(&self, ops: Vec<AssetWriteOp>) -> AssetsWriteResponse {
        self.session.post_api_assets(
            "write",
            &AssetsWriteRequest {
                session_id: self.session_id,
                client_id: "studio".to_owned(),
                place_id: "1".to_owned(),
                container: self.container,
                ops,
            },
        )
    }

    fn plan(&self, place_id: &str, assets: Vec<AssetHash>) -> AssetsPlanResponse {
        self.plan_with_session(place_id, None, assets)
    }

    fn plan_with_session(
        &self,
        place_id: &str,
        session: Option<&str>,
        assets: Vec<AssetHash>,
    ) -> AssetsPlanResponse {
        self.session.post_api_assets(
            "plan",
            &AssetsPlanRequest {
                session_id: self.session_id,
                client_id: "studio".to_owned(),
                place_id: place_id.to_owned(),
                session: session.map(str::to_owned),
                container: self.container,
                assets,
                skipped: Vec::new(),
            },
        )
    }

    fn start_session(&self, place_id: &str, token: &str) {
        let _: AssetsOkResponse = self.session.post_api_assets(
            "session",
            &AssetsSessionRequest {
                session_id: self.session_id,
                place_id: place_id.to_owned(),
                session: token.to_owned(),
            },
        );
    }

    fn read(&self, paths: &[&str]) -> AssetsReadResponse {
        self.session.post_api_assets(
            "read",
            &AssetsReadRequest {
                session_id: self.session_id,
                container: self.container,
                paths: paths.iter().map(|path| path.to_string()).collect(),
            },
        )
    }
}

fn write_op(path: &str, data: Vec<u8>) -> AssetWriteOp {
    AssetWriteOp {
        kind: "write".to_owned(),
        path: path.to_owned(),
        data: Some(data),
        to: None,
    }
}

fn with_assets(callback: impl FnOnce(&TestServeSession, Assets)) {
    run_serve_test("assets", |session, _redactions| {
        let info = session.get_api_rojo().unwrap();
        let assets = session.get_api_assets().unwrap();

        assert_eq!(assets.containers.len(), 1);
        assert_eq!(assets.containers[0].path, "assets");

        // The assets are synced into the instance that declared them.
        let read = session.get_api_read(info.root_instance_id).unwrap();
        let storage = read
            .instances
            .values()
            .find(|instance| instance.name == "ServerStorage")
            .unwrap();
        assert_eq!(assets.containers[0].id, storage.id);

        let assets = Assets {
            session: &session,
            session_id: info.session_id,
            container: storage.id,
        };
        callback(&session, assets);
    });
}

#[test]
fn studio_writes_are_stored_losslessly() {
    with_assets(|session, assets| {
        let model = studio_model("Rock", 0.5);
        let response = assets.write(vec![write_op("Maps/Rock.rbxm", model.clone())]);
        assert!(response.failures.is_empty());

        let file = session.path().join("assets").join("Maps").join("Rock.rbxm");
        let on_disk = fs::read(&file).unwrap();

        // The file is exactly what was sent, except that its chunks are stored
        // uncompressed so git can delta-compress them.
        assert_eq!(on_disk, stored_model("Rock", 0.5));
        assert_eq!(model_name(&on_disk), "Rock");

        // Studio's version is what was last synced, so nothing needs to happen.
        let plan = assets.plan(
            "1",
            vec![AssetHash {
                path: "Maps/Rock.rbxm".to_owned(),
                hash: hash(&model),
            }],
        );
        assert!(plan.need_upload.is_empty());
        assert!(plan.actions.is_empty());
        assert!(!plan.first_sync);
    });
}

#[test]
fn file_changes_are_streamed_to_clients() {
    with_assets(|session, assets| {
        let model = studio_model("Rock", 0.5);
        assets.write(vec![write_op("Rock.rbxm", model.clone())]);

        let cursor = session.get_api_assets().unwrap().message_cursor;

        // Change the file like `git checkout` would.
        let file = session.path().join("assets").join("Rock.rbxm");
        fs::write(&file, stored_model("Pebble", 1.0)).unwrap();

        let packet = session.get_api_assets_socket_change(cursor).unwrap();
        assert_eq!(packet.changes.len(), 1);
        assert_eq!(packet.changes[0].path, "Rock.rbxm");
        assert_eq!(packet.changes[0].container, assets.container);
        assert!(!packet.changes[0].removed);
        assert_eq!(packet.changes[0].origin, None);

        // Studio hasn't changed the asset since, so the file wins.
        let plan = assets.plan(
            "1",
            vec![AssetHash {
                path: "Rock.rbxm".to_owned(),
                hash: hash(&model),
            }],
        );
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].action, AssetPlanAction::ToStudio);

        let read = assets.read(&["Rock.rbxm"]);
        assert_eq!(read.files.len(), 1);
        assert_eq!(model_name(&read.files[0].data), "Pebble");
        assert_eq!(read.files[0].hash, hash(&fs::read(&file).unwrap()));
    });
}

#[test]
fn deleted_files_are_streamed_to_clients() {
    with_assets(|session, assets| {
        assets.write(vec![write_op("Dir/Rock.rbxm", studio_model("Rock", 0.5))]);
        let cursor = session.get_api_assets().unwrap().message_cursor;

        fs::remove_dir_all(session.path().join("assets").join("Dir")).unwrap();

        let packet = session.get_api_assets_socket_change(cursor).unwrap();
        assert_eq!(packet.changes[0].path, "Dir/Rock.rbxm");
        assert!(packet.changes[0].removed);
    });
}

#[test]
fn first_sync_exports_studio_assets() {
    with_assets(|session, assets| {
        let tree = studio_model("Tree", 0.0);
        let studio = vec![AssetHash {
            path: "Trees/Tree.rbxm".to_owned(),
            hash: hash(&tree),
        }];

        let plan = assets.plan("1", studio.clone());
        assert!(plan.first_sync);
        assert_eq!(plan.need_upload, vec!["Trees/Tree.rbxm".to_owned()]);

        let _: AssetsOkResponse = session.post_api_assets(
            "stage",
            &AssetsStageRequest {
                session_id: assets.session_id,
                client_id: "studio".to_owned(),
                container: assets.container,
                assets: vec![AssetContents {
                    path: "Trees/Tree.rbxm".to_owned(),
                    data: tree.clone(),
                }],
            },
        );

        let plan = assets.plan("1", studio.clone());
        assert!(plan.need_upload.is_empty());
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].action, AssetPlanAction::ToFs);

        let _: AssetsOkResponse = session.post_api_assets(
            "apply",
            &AssetsApplyRequest {
                session_id: assets.session_id,
                client_id: "studio".to_owned(),
                place_id: "1".to_owned(),
                container: assets.container,
                to_fs: vec!["Trees/Tree.rbxm".to_owned()],
                delete_fs: Vec::new(),
            },
        );

        let file = session
            .path()
            .join("assets")
            .join("Trees")
            .join("Tree.rbxm");
        assert_eq!(model_name(&fs::read(file).unwrap()), "Tree");

        let plan = assets.plan("1", studio);
        assert!(plan.actions.is_empty());
        assert!(!plan.first_sync);
    });
}

#[test]
fn known_assets_list_what_a_place_synced() {
    with_assets(|session, assets| {
        let tree = studio_model("Tree", 0.0);
        assets.write(vec![write_op("Trees/Tree~2.rbxm", tree.clone())]);

        let known = |place_id: &str| -> AssetsKnownResponse {
            session.post_api_assets(
                "known",
                &AssetsKnownRequest {
                    session_id: assets.session_id,
                    place_id: place_id.to_owned(),
                    container: assets.container,
                },
            )
        };

        let response = known("1");
        assert_eq!(response.files, vec!["Trees/Tree~2.rbxm".to_owned()]);
        assert_eq!(response.synced.len(), 1);
        assert_eq!(response.synced[0].path, "Trees/Tree~2.rbxm");
        assert_eq!(response.synced[0].hashes.len(), 1);
        assert_eq!(&response.synced[0].hashes[0][..], &hash(&tree)[..16]);

        // Other places see the file, but haven't synced it.
        let response = known("2");
        assert_eq!(response.files.len(), 1);
        assert!(response.synced.is_empty());
    });
}

#[test]
fn places_that_were_not_saved_get_their_assets_back() {
    with_assets(|session, assets| {
        assert!(session
            .get_api_assets()
            .unwrap()
            .session_attribute
            .starts_with("__RojoAssetSync_"));

        // A session syncs an asset that Studio just created, and stores its
        // token in the place.
        let tree = studio_model("Tree", 0.0);
        assets.write(vec![write_op("Tree.rbxm", tree)]);
        assets.start_session("1", "session-a");

        // Studio is closed without saving, so the place doesn't have the token
        // or the asset when it connects again. The file is restored.
        let plan = assets.plan_with_session("1", Some("session-before"), Vec::new());
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].path, "Tree.rbxm");
        assert_eq!(plan.actions[0].action, AssetPlanAction::ToStudio);
        assert_eq!(
            plan.actions[0].reason,
            Some(AssetPlanReason::OutdatedInStudio)
        );

        // If the place was saved, the asset was deleted on purpose or lost,
        // which only the user can tell.
        let plan = assets.plan_with_session("1", Some("session-a"), Vec::new());
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].action, AssetPlanAction::Conflict);
        assert_eq!(
            plan.actions[0].reason,
            Some(AssetPlanReason::MissingInStudio)
        );

        // Either way, the file is still there.
        assert!(session.path().join("assets").join("Tree.rbxm").exists());
    });
}

#[test]
fn unsafe_paths_are_rejected() {
    with_assets(|session, assets| {
        let response = assets.write(vec![
            write_op("../escape.rbxm", studio_model("Evil", 0.0)),
            write_op("CON.rbxm", studio_model("Evil", 0.0)),
            write_op("Broken.rbxm", b"not a model".to_vec()),
        ]);

        assert_eq!(response.failures.len(), 3);
        assert!(!session.path().join("escape.rbxm").exists());
        assert!(!session.path().join("assets").join("Broken.rbxm").exists());
    });
}

#[test]
fn build_includes_assets() {
    let _ = env_logger::try_init();

    let dir = tempdir().unwrap();
    let project = dir.path().join("project");
    let assets = project.join("assets");
    fs::create_dir_all(assets.join("Maps - Bounty").join("Namek")).unwrap();

    fs::write(
        project.join("default.project.json"),
        r#"{
            "name": "assets",
            "tree": {
                "$className": "DataModel",
                "ServerStorage": {
                    "$className": "ServerStorage",
                    "$assets": "assets"
                }
            }
        }"#,
    )
    .unwrap();
    fs::write(
        assets.join("Maps - Bounty").join("Namek").join("Rock.rbxm"),
        stored_model("Rock", 0.5),
    )
    .unwrap();
    fs::write(assets.join("Tree.rbxm"), studio_model("Tree", 0.0)).unwrap();

    let output_path = dir.path().join("place.rbxl");
    let output = Command::new(ROJO_PATH)
        .args([
            "build",
            project.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "error")
        .current_dir(get_working_dir_path())
        .output()
        .expect("Couldn't start Rojo");

    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "Rojo did not exit successfully");

    let place = rbx_binary::from_reader(fs::read(output_path).unwrap().as_slice()).unwrap();
    let find_child = |parent: Ref, name: &str| -> Ref {
        *place
            .get_by_ref(parent)
            .unwrap()
            .children()
            .iter()
            .find(|child| place.get_by_ref(**child).unwrap().name == name)
            .unwrap_or_else(|| panic!("{name} was not built"))
    };

    let storage = find_child(place.root_ref(), "ServerStorage");
    let maps = find_child(storage, "Maps - Bounty");
    let namek = find_child(maps, "Namek");
    let rock = find_child(namek, "Rock");
    find_child(storage, "Tree");

    assert_eq!(place.get_by_ref(maps).unwrap().class, "Folder");
    assert_eq!(place.get_by_ref(rock).unwrap().class, "Part");

    // The asset directory isn't touched by building.
    assert!(!project.join(".rojo").exists());
    assert!(Path::new(&assets).join("Tree.rbxm").exists());
}
