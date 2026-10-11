//! Decides how to reconcile an asset that may have changed in Studio, on the
//! filesystem, or both, since the two last agreed on it.

use serde::{Deserialize, Serialize};

use super::state::{BaseEntry, Hash};

/// What should happen to a single asset to bring Studio and the filesystem back
/// in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanAction {
    /// Both sides already agree.
    InSync,
    /// Write Studio's version to the filesystem.
    ToFs,
    /// Apply the file's version to Studio.
    ToStudio,
    /// Studio deleted the asset, so delete its file.
    DeleteFs,
    /// The file was deleted, so delete the asset from Studio.
    DeleteStudio,
    /// It's unclear which side is right. The user has to pick.
    Conflict,
}

/// Why an action was picked, where it isn't obvious.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanReason {
    /// Both Studio and the file changed.
    BothChanged,
    /// The file exists but Studio doesn't have the asset anymore. Either it was
    /// deleted in Studio, or Studio lost it.
    MissingInStudio,
    /// Studio changed the asset, but its file was deleted.
    DeletedOnDisk,
    /// Studio has a version that was never synced, but the place wasn't saved
    /// after it was last synced, so this version may be older than the file.
    UnsavedPlace,
    /// Studio has an outdated version of the asset, because the place wasn't
    /// saved after the asset was last synced.
    OutdatedInStudio,
    /// Studio has an asset that isn't on disk and was never synced, but the
    /// place wasn't saved after it was last synced. The asset may have been
    /// deleted while syncing the changes the place lost.
    NewInUnsavedPlace,
}

/// What is known about one asset while planning.
#[derive(Debug, Clone, Copy)]
pub struct AssetSides<'a> {
    /// Hash of the bytes Studio serialized the asset to, if Studio has it.
    pub studio: Option<Hash>,

    /// Hash of the canonical form of Studio's bytes, if they were uploaded.
    pub studio_canonical: Option<Hash>,

    /// Hash of the asset file, if it exists.
    pub fs: Option<Hash>,

    /// What both sides agreed on last, if they ever did.
    pub base: Option<&'a BaseEntry>,

    /// Whether the place was saved after the last session that synced it. If
    /// it wasn't, Studio is missing whatever was synced in that session, and
    /// that's only on disk now.
    pub place_saved: bool,
}

/// The result of planning a single asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Action(PlanAction, Option<PlanReason>),
    /// The decision depends on Studio's actual contents, which have to be
    /// uploaded first.
    NeedsUpload,
}

/// Decides what to do with an asset. This is a three-way merge where the base
/// is the state both sides last agreed on:
///
/// - If only the file changed, or Studio went back to an older version, the
///   file wins.
/// - If only Studio changed, Studio wins, unless the place wasn't saved after
///   the last sync. Studio's version might be older than the file then.
/// - If both sides changed, their contents are compared. Identical contents
///   are in sync, anything else is a conflict.
/// - Assets missing from Studio are restored if the place wasn't saved, and
///   are a conflict otherwise, since Studio may have lost them.
/// - Without a base, both sides are compared directly, and an asset that only
///   exists on one side is copied to the other. If the place wasn't saved, an
///   asset only Studio has may have been deleted since, so that's a conflict.
///
/// Whenever it isn't clear which side is right, the user gets to decide, so
/// that nothing is overwritten or deleted by mistake.
pub fn decide(sides: AssetSides) -> Decision {
    use Decision::*;
    use PlanAction::*;
    use PlanReason::*;

    let compare = |fs: Hash, reason: PlanReason| match sides.studio_canonical {
        Some(studio) if studio == fs => Action(InSync, None),
        Some(_) => Action(Conflict, Some(reason)),
        None => NeedsUpload,
    };

    let needs_studio_contents =
        |action: PlanAction, reason: Option<PlanReason>| match sides.studio_canonical {
            Some(_) => Action(action, reason),
            None => NeedsUpload,
        };

    match (sides.studio, sides.fs, sides.base) {
        (Some(studio), Some(fs), Some(base)) => {
            let studio_changed = studio != base.studio;
            let fs_changed = fs != base.fs;

            if studio_changed && base.is_older_studio_version(&studio) {
                // Studio's contents can still match the file, for example if
                // the asset was changed and then changed back.
                return match sides.studio_canonical {
                    Some(canonical) if canonical == fs => Action(InSync, None),
                    _ => Action(ToStudio, Some(OutdatedInStudio)),
                };
            }

            match (studio_changed, fs_changed) {
                (false, false) => Action(InSync, None),
                (false, true) => Action(ToStudio, None),
                (true, false) if sides.place_saved => needs_studio_contents(ToFs, None),
                (true, false) => compare(fs, UnsavedPlace),
                (true, true) => compare(fs, BothChanged),
            }
        }
        (Some(_), Some(fs), None) => compare(fs, BothChanged),

        (Some(studio), None, Some(base)) => {
            if studio == base.studio || base.is_older_studio_version(&studio) {
                Action(DeleteStudio, None)
            } else {
                needs_studio_contents(Conflict, Some(DeletedOnDisk))
            }
        }
        (Some(_), None, None) if sides.place_saved => needs_studio_contents(ToFs, None),
        (Some(_), None, None) => needs_studio_contents(Conflict, Some(NewInUnsavedPlace)),

        (None, Some(_), Some(_)) => {
            if sides.place_saved {
                Action(Conflict, Some(MissingInStudio))
            } else {
                Action(ToStudio, Some(OutdatedInStudio))
            }
        }
        (None, Some(_), None) => Action(ToStudio, None),

        (None, None, _) => Action(InSync, None),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use super::super::state::short_hash;

    const A: Hash = [1; 32];
    const B: Hash = [2; 32];
    const C: Hash = [3; 32];
    const D: Hash = [4; 32];

    fn base(fs: Hash, studio: Hash) -> BaseEntry {
        BaseEntry::new(fs, studio)
    }

    fn sides(
        studio: Option<Hash>,
        studio_canonical: Option<Hash>,
        fs: Option<Hash>,
        base: Option<&BaseEntry>,
    ) -> AssetSides<'_> {
        AssetSides {
            studio,
            studio_canonical,
            fs,
            base,
            place_saved: true,
        }
    }

    fn unsaved(sides: AssetSides<'_>) -> AssetSides<'_> {
        AssetSides {
            place_saved: false,
            ..sides
        }
    }

    fn action(action: PlanAction) -> Decision {
        Decision::Action(action, None)
    }

    fn because(action: PlanAction, reason: PlanReason) -> Decision {
        Decision::Action(action, Some(reason))
    }

    #[test]
    fn unchanged_assets_are_in_sync_without_uploading() {
        let base = base(B, A);
        assert_eq!(
            decide(sides(Some(A), None, Some(B), Some(&base))),
            action(PlanAction::InSync)
        );
        assert_eq!(
            decide(unsaved(sides(Some(A), None, Some(B), Some(&base)))),
            action(PlanAction::InSync)
        );
    }

    #[test]
    fn studio_changes_win_when_files_did_not_change() {
        let base = base(B, A);
        assert_eq!(
            decide(sides(Some(C), None, Some(B), Some(&base))),
            Decision::NeedsUpload
        );
        assert_eq!(
            decide(sides(Some(C), Some(C), Some(B), Some(&base))),
            action(PlanAction::ToFs)
        );
    }

    #[test]
    fn file_changes_win_when_studio_did_not_change() {
        let base = base(B, A);
        assert_eq!(
            decide(sides(Some(A), None, Some(C), Some(&base))),
            action(PlanAction::ToStudio)
        );
    }

    #[test]
    fn changes_on_both_sides_are_compared() {
        let base_ab = base(B, A);
        assert_eq!(
            decide(sides(Some(C), None, Some(C), Some(&base_ab))),
            Decision::NeedsUpload
        );
        assert_eq!(
            decide(sides(Some(C), Some(C), Some(C), Some(&base_ab))),
            action(PlanAction::InSync)
        );

        let base_bb = base(B, B);
        assert_eq!(
            decide(sides(Some(C), Some(A), Some(C), Some(&base_bb))),
            because(PlanAction::Conflict, PlanReason::BothChanged)
        );
    }

    #[test]
    fn without_a_base_both_sides_are_compared() {
        assert_eq!(
            decide(sides(Some(A), Some(B), Some(B), None)),
            action(PlanAction::InSync)
        );
        assert_eq!(
            decide(sides(Some(A), Some(A), Some(B), None)),
            because(PlanAction::Conflict, PlanReason::BothChanged)
        );
        assert_eq!(
            decide(sides(Some(A), None, Some(B), None)),
            Decision::NeedsUpload
        );
    }

    #[test]
    fn new_assets_are_copied_to_the_other_side() {
        assert_eq!(
            decide(sides(Some(A), Some(A), None, None)),
            action(PlanAction::ToFs)
        );
        assert_eq!(
            decide(sides(None, None, Some(A), None)),
            action(PlanAction::ToStudio)
        );
    }

    #[test]
    fn files_deleted_on_disk_are_deleted_from_studio() {
        let base = base(B, A);
        assert_eq!(
            decide(sides(Some(A), None, None, Some(&base))),
            action(PlanAction::DeleteStudio)
        );
    }

    #[test]
    fn deleting_a_changed_asset_is_a_conflict() {
        let base = base(B, A);
        assert_eq!(
            decide(sides(Some(C), None, None, Some(&base))),
            Decision::NeedsUpload
        );
        assert_eq!(
            decide(sides(Some(C), Some(C), None, Some(&base))),
            because(PlanAction::Conflict, PlanReason::DeletedOnDisk)
        );
    }

    #[test]
    fn assets_missing_from_studio_are_never_deleted_on_their_own() {
        let base = base(B, A);

        // The place was saved, so Studio may have deleted the asset on purpose,
        // or lost it. Only the user knows.
        assert_eq!(
            decide(sides(None, None, Some(B), Some(&base))),
            because(PlanAction::Conflict, PlanReason::MissingInStudio)
        );
        assert_eq!(
            decide(sides(None, None, Some(C), Some(&base))),
            because(PlanAction::Conflict, PlanReason::MissingInStudio)
        );

        // The place wasn't saved after the asset was synced, so Studio lost it.
        assert_eq!(
            decide(unsaved(sides(None, None, Some(B), Some(&base)))),
            because(PlanAction::ToStudio, PlanReason::OutdatedInStudio)
        );
    }

    #[test]
    fn older_studio_versions_are_replaced_by_the_file() {
        // Studio synced A, then D. The file has D's contents (B).
        let mut base = base(B, D);
        base.older.push(short_hash(&A));

        // Studio went back to A, for example because the place wasn't saved
        // after D was synced.
        assert_eq!(
            decide(sides(Some(A), None, Some(B), Some(&base))),
            because(PlanAction::ToStudio, PlanReason::OutdatedInStudio)
        );
        assert_eq!(
            decide(unsaved(sides(Some(A), None, Some(B), Some(&base)))),
            because(PlanAction::ToStudio, PlanReason::OutdatedInStudio)
        );

        // The file changed too, which is newer than an older version either way.
        assert_eq!(
            decide(sides(Some(A), None, Some(C), Some(&base))),
            because(PlanAction::ToStudio, PlanReason::OutdatedInStudio)
        );

        // An outdated version whose file was deleted can be deleted too.
        assert_eq!(
            decide(sides(Some(A), None, None, Some(&base))),
            action(PlanAction::DeleteStudio)
        );
    }

    #[test]
    fn new_assets_in_unsaved_places_are_a_conflict() {
        assert_eq!(
            decide(unsaved(sides(Some(A), None, None, None))),
            Decision::NeedsUpload
        );
        assert_eq!(
            decide(unsaved(sides(Some(A), Some(A), None, None))),
            because(PlanAction::Conflict, PlanReason::NewInUnsavedPlace)
        );
    }

    #[test]
    fn unknown_studio_versions_in_unsaved_places_are_a_conflict() {
        let base = base(B, A);

        assert_eq!(
            decide(unsaved(sides(Some(C), None, Some(B), Some(&base)))),
            Decision::NeedsUpload
        );
        assert_eq!(
            decide(unsaved(sides(Some(C), Some(C), Some(B), Some(&base)))),
            because(PlanAction::Conflict, PlanReason::UnsavedPlace)
        );
        // Unless Studio's contents match the file anyway.
        assert_eq!(
            decide(unsaved(sides(Some(C), Some(B), Some(B), Some(&base)))),
            action(PlanAction::InSync)
        );
    }
}
