use crossbeam_channel::{select, Receiver, RecvError, Sender};
use jod_thread::JoinHandle;
use memofs::{IoResultExt, Vfs, VfsEvent};
use rbx_dom_weak::types::{Ref, Variant};
use std::path::{Path, PathBuf};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    assets::AssetStore,
    message_queue::MessageQueue,
    snapshot::{
        apply_patch_set, compute_patch_set, AppliedPatchSet, InstigatingSource, PatchSet, RojoTree,
    },
    snapshot_middleware::{snapshot_from_vfs, snapshot_project_node},
};

/// How often asset directories are checked for changes the file watcher
/// missed.
const ASSET_RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Processes file change events, updates the DOM, and sends those updates
/// through a channel for other stuff to consume.
///
/// Owns the connection between Rojo's VFS and its DOM by holding onto another
/// thread that processes messages.
///
/// Consumers of ChangeProcessor, like ServeSession, are intended to communicate
/// with this object via channels.
///
/// ChangeProcessor expects to be the only writer to the RojoTree and Vfs
/// objects passed to it.
pub struct ChangeProcessor {
    /// Controls the runtime of the processor thread. When signaled, the job
    /// thread will finish its current work and terminate.
    ///
    /// This channel should be signaled before dropping ChangeProcessor or we'll
    /// hang forever waiting for the message processing loop to terminate.
    shutdown_sender: Sender<()>,

    /// A handle to the message processing thread. When dropped, we'll block
    /// until it's done.
    ///
    /// Allowed to be unused because dropping this value has side effects.
    #[allow(unused)]
    job_thread: JoinHandle<Result<(), RecvError>>,
}

impl ChangeProcessor {
    /// Spin up the ChangeProcessor, connecting it to the given tree, VFS, and
    /// outbound message queue.
    pub fn start(
        tree: Arc<Mutex<RojoTree>>,
        vfs: Arc<Vfs>,
        message_queue: Arc<MessageQueue<AppliedPatchSet>>,
        asset_store: Arc<AssetStore>,
        tree_mutation_receiver: Receiver<PatchSet>,
    ) -> Self {
        let (shutdown_sender, shutdown_receiver) = crossbeam_channel::bounded(1);
        let vfs_receiver = vfs.event_receiver();

        // File watchers can drop events when huge numbers of files change at
        // once, like during a git checkout. Rescanning asset directories every
        // so often makes sure Studio still hears about every change.
        let asset_rescan_ticker = crossbeam_channel::tick(ASSET_RESCAN_INTERVAL);

        let task = JobThreadContext {
            tree,
            vfs,
            message_queue,
            asset_store,
        };

        let job_thread = jod_thread::Builder::new()
            .name("ChangeProcessor thread".to_owned())
            .spawn(move || {
                log::trace!("ChangeProcessor thread started");

                loop {
                    select! {
                        recv(vfs_receiver) -> event => {
                            // Bursts of changes, like a git checkout, are handled
                            // together so asset files can be read in parallel.
                            let mut events = vec![event?];
                            events.extend(vfs_receiver.try_iter());
                            task.handle_vfs_events(events);
                        },
                        recv(tree_mutation_receiver) -> patch_set => {
                            task.handle_tree_event(patch_set?);
                        },
                        recv(asset_rescan_ticker) -> _ => {
                            task.asset_store.rescan_all();
                        },
                        recv(shutdown_receiver) -> _ => {
                            log::trace!("ChangeProcessor shutdown signal received...");
                            return Ok(());
                        },
                    }
                }
            })
            .expect("Could not start ChangeProcessor thread");

        Self {
            shutdown_sender,
            job_thread,
        }
    }
}

impl Drop for ChangeProcessor {
    fn drop(&mut self) {
        // Signal the job thread to start spinning down. Without this we'll hang
        // forever waiting for the thread to finish its infinite loop.
        let _ = self.shutdown_sender.send(());

        // After this function ends, the job thread will be joined. It might
        // block for a small period of time while it processes its last work.
    }
}

/// Contains all of the state needed to synchronize the DOM and VFS.
struct JobThreadContext {
    /// A handle to the DOM we're managing.
    tree: Arc<Mutex<RojoTree>>,

    /// A handle to the VFS we're managing.
    vfs: Arc<Vfs>,

    /// Whenever changes are applied to the DOM, we should push those changes
    /// into this message queue to inform any connected clients.
    message_queue: Arc<MessageQueue<AppliedPatchSet>>,

    /// Handles changes inside of asset directories, which aren't part of the
    /// DOM.
    asset_store: Arc<AssetStore>,
}

/// Canonicalizes a path from a filesystem event even if it no longer exists,
/// by canonicalizing its closest existing ancestor. Files in asset directories
/// are written through temporary files, which are usually gone by the time
/// their events arrive.
fn canonicalize_event_path(vfs: &Vfs, path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();

    loop {
        if let Ok(canonical) = vfs.canonicalize(existing) {
            let mut result = canonical;
            result.extend(missing.iter().rev());
            return Some(result);
        }

        missing.push(existing.file_name()?);
        existing = existing.parent()?;
    }
}

impl JobThreadContext {
    /// Computes and applies patches to the DOM for a given file path.
    ///
    /// This function finds the nearest ancestor to the given path that has associated instances
    /// in the tree.
    /// It then computes and applies changes for each affected instance ID and
    /// returns a vector of applied patch sets.
    fn apply_patches(&self, path: PathBuf) -> Vec<AppliedPatchSet> {
        let mut tree = self.tree.lock().unwrap();
        let mut applied_patches = Vec::new();

        // Find the nearest ancestor to this path that has
        // associated instances in the tree. This helps make sure
        // that we handle additions correctly, especially if we
        // receive events for descendants of a large tree being
        // created all at once.
        let mut current_path = path.as_path();
        let affected_ids = loop {
            let ids = tree.get_ids_at_path(current_path);

            log::trace!("Path {} affects IDs {:?}", current_path.display(), ids);

            if !ids.is_empty() {
                break ids.to_vec();
            }

            log::trace!("Trying parent path...");
            match current_path.parent() {
                Some(parent) => current_path = parent,
                None => break Vec::new(),
            }
        };

        for id in affected_ids {
            if let Some(patch) = compute_and_apply_changes(&mut tree, &self.vfs, id) {
                if !patch.is_empty() {
                    applied_patches.push(patch);
                }
            }
        }

        applied_patches
    }

    fn handle_vfs_events(&self, events: Vec<VfsEvent>) {
        // Update the VFS immediately with the events.
        for event in &events {
            log::trace!("Vfs event: {:?}", event);
            self.vfs
                .commit_event(event)
                .expect("Error applying VFS change");
        }

        let paths: Vec<PathBuf> = events
            .iter()
            .map(|event| match event {
                VfsEvent::Create(path) | VfsEvent::Write(path) | VfsEvent::Remove(path) => {
                    path.clone()
                }
                _ => PathBuf::new(),
            })
            .collect();

        // The watcher reports asset files relative to the asset directory's
        // canonical path, so they're usually matched without canonicalizing
        // them. That saves opening every changed file, which is slow while
        // antivirus software is still scanning it.
        let mut owned = self.asset_store.handle_fs_changes(&paths);

        let unmatched: Vec<usize> = (0..paths.len())
            .filter(|&index| !owned[index] && !paths[index].as_os_str().is_empty())
            .collect();
        if !unmatched.is_empty() {
            let canonical: Vec<PathBuf> = unmatched
                .iter()
                .map(|&index| canonicalize_event_path(&self.vfs, &paths[index]).unwrap_or_default())
                .collect();
            let owned_canonical = self.asset_store.handle_fs_changes(&canonical);
            for (position, &index) in unmatched.iter().enumerate() {
                owned[index] = owned_canonical[position];
            }
        }

        for (event, owned) in events.into_iter().zip(owned) {
            if !owned {
                self.handle_vfs_event(event);
            }
        }
    }

    fn handle_vfs_event(&self, event: VfsEvent) {
        // For a given VFS event, we might have many changes to different parts
        // of the tree. Calculate and apply all of these changes.
        let applied_patches = match event {
            VfsEvent::Create(path) | VfsEvent::Write(path) => {
                self.apply_patches(self.vfs.canonicalize(&path).unwrap())
            }
            VfsEvent::Remove(path) => {
                // MemoFS does not track parent removals yet, so we can canonicalize
                // the parent path safely and then append the removed path's file name.
                let parent = path.parent().unwrap();
                let file_name = path.file_name().unwrap();
                let parent_normalized = self.vfs.canonicalize(parent).unwrap();
                self.apply_patches(parent_normalized.join(file_name))
            }
            _ => {
                log::warn!("Unhandled VFS event: {:?}", event);
                Vec::new()
            }
        };

        if !applied_patches.is_empty() {
            // The project file may have gained or lost `$assets` declarations.
            let tree = self.tree.lock().unwrap();
            self.asset_store.refresh_containers(&tree);
        }

        // Notify anyone listening to the message queue about the changes we
        // just made.
        self.message_queue.push_messages(&applied_patches);
    }

    fn handle_tree_event(&self, patch_set: PatchSet) {
        log::trace!("Applying PatchSet from client: {:#?}", patch_set);

        let applied_patch = {
            let mut tree = self.tree.lock().unwrap();

            for &id in &patch_set.removed_instances {
                if let Some(instance) = tree.get_instance(id) {
                    if let Some(instigating_source) = &instance.metadata().instigating_source {
                        match instigating_source {
                            InstigatingSource::Path(path) => {
                                if let Err(err) = fs::remove_file(path) {
                                    log::error!(
                                        "Failed to remove file {}: {}",
                                        path.display(),
                                        err
                                    );
                                }
                            }
                            InstigatingSource::ProjectNode { .. } => {
                                log::warn!(
                                    "Cannot remove instance {:?}, it's from a project file",
                                    id
                                );
                            }
                        }
                    } else {
                        // TODO
                        log::warn!(
                            "Cannot remove instance {:?}, it is not an instigating source.",
                            id
                        );
                    }
                } else {
                    log::warn!("Cannot remove instance {:?}, it does not exist.", id);
                }
            }

            for update in &patch_set.updated_instances {
                let id = update.id;

                if let Some(instance) = tree.get_instance(id) {
                    if update.changed_name.is_some() {
                        log::warn!("Cannot rename instances yet.");
                    }

                    if update.changed_class_name.is_some() {
                        log::warn!("Cannot change ClassName yet.");
                    }

                    if update.changed_metadata.is_some() {
                        log::warn!("Cannot change metadata yet.");
                    }

                    for (key, changed_value) in &update.changed_properties {
                        if key == "Source" {
                            if let Some(instigating_source) =
                                &instance.metadata().instigating_source
                            {
                                match instigating_source {
                                    InstigatingSource::Path(path) => {
                                        if let Some(Variant::String(value)) = changed_value {
                                            if let Err(err) = fs::write(path, value) {
                                                log::error!(
                                                    "Failed to write file {}: {}",
                                                    path.display(),
                                                    err
                                                );
                                            }
                                        } else {
                                            log::warn!("Cannot change Source to non-string value.");
                                        }
                                    }
                                    InstigatingSource::ProjectNode { .. } => {
                                        log::warn!(
                                            "Cannot remove instance {:?}, it's from a project file",
                                            id
                                        );
                                    }
                                }
                            } else {
                                log::warn!(
                                    "Cannot update instance {:?}, it is not an instigating source.",
                                    id
                                );
                            }
                        } else {
                            log::warn!("Cannot change properties besides BaseScript.Source.");
                        }
                    }
                } else {
                    log::warn!("Cannot update instance {:?}, it does not exist.", id);
                }
            }

            apply_patch_set(&mut tree, patch_set)
        };

        if !applied_patch.is_empty() {
            self.message_queue.push_messages(&[applied_patch]);
        }
    }
}

fn compute_and_apply_changes(tree: &mut RojoTree, vfs: &Vfs, id: Ref) -> Option<AppliedPatchSet> {
    let metadata = tree
        .get_metadata(id)
        .expect("metadata missing for instance present in tree");

    let instigating_source = match &metadata.instigating_source {
        Some(path) => path,
        None => {
            log::error!(
                "Instance {:?} did not have an instigating source, but was considered for an update.",
                id
            );
            log::error!("This is a bug. Please file an issue!");
            return None;
        }
    };

    // How we process a file change event depends on what created this
    // file/folder in the first place.
    let applied_patch_set = match instigating_source {
        InstigatingSource::Path(path) => match vfs.metadata(path).with_not_found() {
            Ok(Some(_)) => {
                // Our instance was previously created from a path and that
                // path still exists. We can generate a snapshot starting at
                // that path and use it as the source for our patch.

                let snapshot = match snapshot_from_vfs(&metadata.context, vfs, path) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        log::error!("Snapshot error: {:?}", err);
                        return None;
                    }
                };

                let patch_set = compute_patch_set(snapshot, tree, id);
                apply_patch_set(tree, patch_set)
            }
            Ok(None) => {
                // Our instance was previously created from a path, but that
                // path no longer exists.
                //
                // We associate deleting the instigating file for an
                // instance with deleting that instance.

                let mut patch_set = PatchSet::new();
                patch_set.removed_instances.push(id);

                apply_patch_set(tree, patch_set)
            }
            Err(err) => {
                log::error!("Error processing filesystem change: {:?}", err);
                return None;
            }
        },

        InstigatingSource::ProjectNode {
            path,
            name,
            node,
            parent_class,
        } => {
            // This instance is the direct subject of a project node. Since
            // there might be information associated with our instance from
            // the project file, we snapshot the entire project node again.

            let snapshot_result = snapshot_project_node(
                &metadata.context,
                path,
                name,
                node,
                vfs,
                parent_class.as_ref().map(|name| name.as_str()),
            );

            let snapshot = match snapshot_result {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    log::error!("{:?}", err);
                    return None;
                }
            };

            let patch_set = compute_patch_set(snapshot, tree, id);
            apply_patch_set(tree, patch_set)
        }
    };

    Some(applied_patch_set)
}
