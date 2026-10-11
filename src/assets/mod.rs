//! Two-way syncing of assets between Roblox Studio and the filesystem.
//!
//! Scripts are synced instance by instance through the snapshot system, which
//! works well for them but doesn't scale to hundreds of thousands of parts and
//! can't represent many of their properties, like a MeshPart's mesh. Assets
//! are synced in bulk instead: an instance declared with `$assets` in a project
//! file has its children mirrored to a directory, where plain folders become
//! directories and every other instance becomes one `.rbxm` file holding it
//! and its descendants.
//!
//! The Studio plugin serializes and deserializes assets natively using
//! `SerializationService`, so the server only stores and compares bytes. It
//! never decodes assets during sync, which keeps sync lossless even for
//! properties rbx-dom doesn't know about yet. The only values it looks at are
//! floats, to tell rounding noise apart from real changes (see `compare`).

mod build;
mod cache;
mod compare;
mod names;
mod plan;
mod rbxm;
mod state;
mod store;

pub use build::dom_with_assets;
pub use plan::{PlanAction, PlanReason};
pub use state::{Group, Hash};
pub use store::{AckEntry, AssetMessage, AssetStore, WriteOp};
