//! Module: Decide what an ingest should do with one location, by content.
//! Correctness: Correct when identical bytes are stored once however many
//! places they appear, when only a location whose content CHANGED produces a
//! version link, and when a location with nothing behind it is reported rather
//! than skipped.
//! Last revised: 2026-08-25
//! Last changed: New.
//!
//! # Why content is the identity
//!
//! The same bytes legitimately live in several places. Measured on one research
//! tree: 3,411 markdown files, 905 distinct contents -- 73% of the files are a
//! copy of something else, and 56 of them are symlinks.
//!
//! Two causes, and only one is incidental. Worktree checkouts duplicate a repo
//! by accident. But `corpus/clojure/clojurebrainteasers.md` and
//! `corpus/functional-programming/clojurebrainteasers.md` are byte-identical
//! ON PURPOSE: one book, filed under two topics. Path-keyed storage forces a
//! choice between storing it twice and losing one of the classifications.
//!
//! So content is the identity and a path is a LOCATION pointing at it. A
//! symlink, a worktree copy and a deliberate cross-file are then the same
//! thing, and the ingest needs no symlink resolution, no worktree detection and
//! no canonicalisation heuristics to handle them.

use std::collections::{BTreeMap, BTreeSet};

/// A content hash. Opaque here: this module decides, it does not hash.
pub type ContentId = String;

/// Where content was found. A path, but this module never touches a filesystem.
pub type LocationId = String;

/// What an ingest should do about one location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Bytes nobody has stored yet. Ingest them, and point the location at them.
    IngestContent {
        content: ContentId,
        location: LocationId,
    },
    /// These bytes are already stored. Record the location and DO NOT re-ingest.
    ///
    /// The whole point of the module: the second, third and eighth copy of a
    /// file cost one link each.
    LinkExisting {
        content: ContentId,
        location: LocationId,
    },
    /// The location is where it was and holds what it held. Nothing to do.
    Unchanged {
        content: ContentId,
        location: LocationId,
    },
    /// This location's content changed. The old content is superseded BY the
    /// new one, as observed at this location.
    Version {
        location: LocationId,
        from: ContentId,
        to: ContentId,
        /// True when `to` is bytes nobody had stored before.
        ingest_needed: bool,
    },
    /// The location is gone from disk. Drop the link; the content may survive.
    Unlink {
        content: ContentId,
        location: LocationId,
    },
    /// A location with nothing behind it -- a broken symlink, an unreadable
    /// file. REPORTED rather than skipped: silence here is how a dangling
    /// skill link stayed invisible while everything downstream reported
    /// success.
    Missing {
        location: LocationId,
        reason: String,
    },
}

/// What the store already knows.
#[derive(Debug, Clone, Default)]
pub struct Known {
    /// Where each location currently points.
    pub locations: BTreeMap<LocationId, ContentId>,
    /// Content already stored, whether or not a location still points at it.
    pub contents: BTreeSet<ContentId>,
}

/// What was found on disk this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    /// The location holds content with this hash.
    Content(ContentId),
    /// The location exists but nothing is behind it.
    Missing(String),
}

/// Decide one location.
///
/// Pure, and deliberately so: the interesting rules are all decisions, and a
/// decision buried in an I/O loop cannot be asserted. See the module note in
/// the TDD skill about extracting the choice out of the loop.
pub fn decide(known: &Known, location: &str, observed: &Observed) -> Action {
    let previous = known.locations.get(location);
    match observed {
        Observed::Missing(reason) => Action::Missing {
            location: location.to_owned(),
            reason: reason.clone(),
        },
        Observed::Content(content) => match previous {
            // A location that held these exact bytes last time.
            Some(before) if before == content => Action::Unchanged {
                content: content.clone(),
                location: location.to_owned(),
            },
            // A location whose content CHANGED. This is the only thing that
            // makes a version. Same content arriving somewhere new is another
            // location, not a new version -- otherwise every worktree checkout
            // would manufacture history that never happened.
            Some(before) => Action::Version {
                location: location.to_owned(),
                from: before.clone(),
                to: content.clone(),
                ingest_needed: !known.contents.contains(content),
            },
            None if known.contents.contains(content) => Action::LinkExisting {
                content: content.clone(),
                location: location.to_owned(),
            },
            None => Action::IngestContent {
                content: content.clone(),
                location: location.to_owned(),
            },
        },
    }
}

/// Locations the store knows that this pass did not see: they are gone.
///
/// Separate from [`decide`] because it is a question about the SET, not about
/// any one location, and it can only be answered once the walk is complete.
pub fn vanished(known: &Known, seen: &BTreeSet<LocationId>) -> Vec<Action> {
    known
        .locations
        .iter()
        .filter(|(location, _)| !seen.contains(*location))
        .map(|(location, content)| Action::Unlink {
            content: content.clone(),
            location: location.clone(),
        })
        .collect()
}

/// Content that no location points at any more.
///
/// This is what "no longer exists on disk anywhere" means, and the current
/// path-keyed store cannot express the question at all.
pub fn orphaned(known: &Known, applied: &[Action]) -> Vec<ContentId> {
    let mut live: BTreeMap<LocationId, ContentId> = known.locations.clone();
    for action in applied {
        match action {
            Action::Unlink { location, .. } => {
                live.remove(location);
            }
            Action::IngestContent { content, location }
            | Action::LinkExisting { content, location } => {
                live.insert(location.clone(), content.clone());
            }
            Action::Version { location, to, .. } => {
                live.insert(location.clone(), to.clone());
            }
            Action::Unchanged { .. } | Action::Missing { .. } => {}
        }
    }
    let referenced: BTreeSet<&ContentId> = live.values().collect();
    known
        .contents
        .iter()
        .filter(|content| !referenced.contains(content))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(locations: &[(&str, &str)], contents: &[&str]) -> Known {
        Known {
            locations: locations
                .iter()
                .map(|(l, c)| ((*l).to_owned(), (*c).to_owned()))
                .collect(),
            contents: contents.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn found(hash: &str) -> Observed {
        Observed::Content(hash.to_owned())
    }

    #[test]
    fn unseen_content_at_a_new_location_is_ingested() {
        let action = decide(&known(&[], &[]), "corpus/rust/x.md", &found("h1"));
        assert_eq!(
            action,
            Action::IngestContent {
                content: "h1".to_owned(),
                location: "corpus/rust/x.md".to_owned(),
            }
        );
    }

    /// The reason this module exists. A book filed under two topics, a symlink,
    /// and a worktree copy are all this case: the bytes are already stored, so
    /// the new place costs one link and no re-ingest.
    #[test]
    fn known_content_at_a_new_location_is_linked_not_reingested() {
        let store = known(&[("corpus/clojure/b.md", "h1")], &["h1"]);
        let action = decide(&store, "corpus/functional-programming/b.md", &found("h1"));
        assert_eq!(
            action,
            Action::LinkExisting {
                content: "h1".to_owned(),
                location: "corpus/functional-programming/b.md".to_owned(),
            },
            "identical bytes must not be stored twice"
        );
    }

    #[test]
    fn a_location_holding_what_it_held_is_unchanged() {
        let store = known(&[("a.md", "h1")], &["h1"]);
        assert_eq!(
            decide(&store, "a.md", &found("h1")),
            Action::Unchanged {
                content: "h1".to_owned(),
                location: "a.md".to_owned()
            }
        );
    }

    /// A version is a statement about one LOCATION over time.
    #[test]
    fn a_location_whose_content_changed_makes_a_version() {
        let store = known(&[("a.md", "h1")], &["h1"]);
        assert_eq!(
            decide(&store, "a.md", &found("h2")),
            Action::Version {
                location: "a.md".to_owned(),
                from: "h1".to_owned(),
                to: "h2".to_owned(),
                ingest_needed: true,
            }
        );
    }

    /// The rule that keeps worktrees from inventing history: the same content
    /// showing up somewhere new is another location, never a new version.
    #[test]
    fn the_same_content_at_a_new_location_is_not_a_version() {
        let store = known(&[("skills/x.md", "h1")], &["h1"]);
        let action = decide(&store, ".worktrees/dedup/skills/x.md", &found("h1"));
        assert!(
            !matches!(action, Action::Version { .. }),
            "a copy is a location, not a version: {action:?}"
        );
        assert!(matches!(action, Action::LinkExisting { .. }));
    }

    /// Reverting a file lands back on content that is already stored. No third
    /// record, and the version edge points at the existing bytes.
    #[test]
    fn a_location_reverting_to_earlier_bytes_reuses_that_content() {
        let store = known(&[("a.md", "h2")], &["h1", "h2"]);
        assert_eq!(
            decide(&store, "a.md", &found("h1")),
            Action::Version {
                location: "a.md".to_owned(),
                from: "h2".to_owned(),
                to: "h1".to_owned(),
                ingest_needed: false,
            },
            "the earlier content is still stored; do not ingest it again"
        );
    }

    /// A dangling symlink. Reported, because silence here is exactly how a
    /// broken skill link stayed invisible while every downstream count agreed.
    #[test]
    fn a_location_with_nothing_behind_it_is_reported() {
        let observed = Observed::Missing("broken symlink".to_owned());
        assert_eq!(
            decide(
                &known(&[], &[]),
                "~/.claude/skills/ferrosa-design",
                &observed
            ),
            Action::Missing {
                location: "~/.claude/skills/ferrosa-design".to_owned(),
                reason: "broken symlink".to_owned(),
            }
        );
    }

    #[test]
    fn a_location_not_seen_this_pass_is_unlinked() {
        let store = known(&[("gone.md", "h1"), ("here.md", "h1")], &["h1"]);
        let seen: BTreeSet<LocationId> = ["here.md".to_owned()].into_iter().collect();
        assert_eq!(
            vanished(&store, &seen),
            vec![Action::Unlink {
                content: "h1".to_owned(),
                location: "gone.md".to_owned()
            }]
        );
    }

    /// Content survives while ANY location still points at it.
    #[test]
    fn content_survives_while_another_location_still_points_at_it() {
        let store = known(&[("a.md", "h1"), ("b.md", "h1")], &["h1"]);
        let applied = vec![Action::Unlink {
            content: "h1".to_owned(),
            location: "a.md".to_owned(),
        }];
        assert!(
            orphaned(&store, &applied).is_empty(),
            "b.md still points at it"
        );
    }

    /// Content with no locations left is what "gone from disk everywhere"
    /// means -- a question the path-keyed store cannot ask.
    #[test]
    fn content_with_no_locations_left_is_orphaned() {
        let store = known(&[("a.md", "h1")], &["h1"]);
        let applied = vec![Action::Unlink {
            content: "h1".to_owned(),
            location: "a.md".to_owned(),
        }];
        assert_eq!(orphaned(&store, &applied), vec!["h1".to_owned()]);
    }

    /// A version moves the location off the old content.
    #[test]
    fn superseded_content_with_no_other_location_is_orphaned() {
        let store = known(&[("a.md", "h1")], &["h1", "h2"]);
        let applied = vec![Action::Version {
            location: "a.md".to_owned(),
            from: "h1".to_owned(),
            to: "h2".to_owned(),
            ingest_needed: false,
        }];
        assert_eq!(orphaned(&store, &applied), vec!["h1".to_owned()]);
    }

    /// The measured shape of the real tree: 8 locations, 1 content, 1 ingest.
    #[test]
    fn eight_copies_of_one_file_cost_one_ingest_and_seven_links() {
        let mut store = Known::default();
        let mut ingests = 0;
        let mut links = 0;
        for i in 0..8 {
            let location = format!("copy-{i}/clojureapplied.md");
            match decide(&store, &location, &found("h1")) {
                Action::IngestContent { content, location } => {
                    ingests += 1;
                    store.contents.insert(content.clone());
                    store.locations.insert(location, content);
                }
                Action::LinkExisting { content, location } => {
                    links += 1;
                    store.locations.insert(location, content);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(ingests, 1, "the bytes are stored once");
        assert_eq!(links, 7);
        assert_eq!(store.locations.len(), 8, "every location is still recorded");
    }
}
