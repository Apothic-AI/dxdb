//! The document cache: one live handle per `(collection, id)`.
//!
//! Ported in behaviour from `src/doc-cache.ts`. Upstream's cache exists for two reasons,
//! and both are observable:
//!
//! 1. **Identity.** Calling `findOne(id)` twice must return handles that reflect the
//!    same document. If the cache minted a second handle for the same id, an
//!    incremental update applied to one would be invisible to the other.
//! 2. **Revision ordering.** A change event can arrive before or after the write that
//!    caused it, and a slow read can resolve after a fast write. The cache therefore
//!    never blindly overwrites: it keeps the state with the **higher revision height**,
//!    which is `findNewestOfDocumentStates` (`src/incremental-write.ts:237`).
//!
//! Upstream also evicts on a timer in dev mode to catch reference leaks. That is a
//! `WeakRef` sweep tied to a JS GC; there is no equivalent here, so this cache holds
//! states until [`DocumentCache::clear`]. The consequence is memory rather than
//! behaviour, and it is stated so nobody assumes an eviction policy exists.
//!
//! Unlike upstream, this module does **not** own `RxDocument` handles: a handle in Rust
//! is a struct that borrows or clones the cached state, and the layer that builds
//! handles is [`crate::document`]. This cache is the state store both share.

use std::collections::HashMap;

use dxdb_storage::{get_height_of_revision, DocumentData};

use crate::error::{CoreError, CoreResult};

/// The newest state known for each document of one collection.
#[derive(Default)]
pub struct DocumentCache {
    states: HashMap<String, DocumentData>,
}

impl DocumentCache {
    /// An empty cache.
    pub fn new() -> Self {
        DocumentCache::default()
    }

    /// The cached state for `id`, if a handle may see it.
    ///
    /// Returns `None` for a tombstone: a deleted document has no live handle, matching
    /// upstream's `getLatestDocumentDataIfExists` being used only after a
    /// `withDeleted` read.
    pub fn get(&self, id: &str) -> Option<&DocumentData> {
        self.states.get(id).filter(|state| !state.is_deleted())
    }

    /// The cached state for `id`, including a tombstone.
    pub fn get_with_deleted(&self, id: &str) -> Option<&DocumentData> {
        self.states.get(id)
    }

    /// Number of cached ids, tombstones included. Exposed for leak assertions.
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// True when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Drops every cached state.
    pub fn clear(&mut self) {
        self.states.clear();
    }

    /// Removes one id.
    pub fn remove(&mut self, id: &str) -> Option<DocumentData> {
        self.states.remove(id)
    }

    /// Inserts a state, **keeping the higher revision** if one is already cached.
    ///
    /// Returns the state now held, so a caller can tell whether its write won. This is
    /// the whole of the cache's correctness story: a change event for revision 2 that
    /// arrives after a read of revision 3 must not roll the cache back.
    ///
    /// The comparison is on the revision height, not on the `lwt`: two writes in
    /// different databases of the same replication can share an `lwt`, but a higher
    /// height is unambiguously newer.
    pub fn insert(&mut self, id: &str, state: DocumentData) -> CoreResult<&DocumentData> {
        let incoming_height = height_of(&state)?;
        match self.states.get(id) {
            Some(existing) => {
                let existing_height = height_of(existing)?;
                if incoming_height >= existing_height {
                    self.states.insert(id.to_string(), state);
                }
            }
            None => {
                self.states.insert(id.to_string(), state);
            }
        }
        Ok(self
            .states
            .get(id)
            .expect("just inserted or already present"))
    }

    /// Applies a batch, keeping the newest revision per id.
    ///
    /// Taking a `Vec` rather than one-at-a-time is what `mapDocumentsDataToCacheDocs`
    /// does, and it matters for the `findByIds` path where one id may appear twice
    /// because two indexes returned it.
    pub fn insert_many(&mut self, states: Vec<DocumentData>, primary_path: &str) -> CoreResult<()> {
        for state in states {
            let id = state
                .id(primary_path)
                .map_err(|error| CoreError::InvalidDocument(error.to_string()))?
                .to_string();
            self.insert(&id, state)?;
        }
        Ok(())
    }

    /// Every cached live state, in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &DocumentData)> {
        self.states.iter().filter(|(_, state)| !state.is_deleted())
    }
}

/// The revision height of a document, or an error when it has no usable `_rev`.
fn height_of(state: &DocumentData) -> CoreResult<u32> {
    let revision = state.rev().ok_or_else(|| {
        CoreError::InvalidDocument("a cached state must have a `_rev`".to_string())
    })?;
    dxdb_storage::try_get_height_of_revision(revision)
        .ok_or_else(|| CoreError::InvalidDocument(format!("malformatted revision: {revision:?}")))
}

/// Picks the newest of several states for the same document.
///
/// `findNewestOfDocumentStates` (`src/incremental-write.ts:237`). Used by the write
/// queue to decide what a modifier should see when several writes for one document are
/// in flight.
pub fn find_newest_of_document_states(states: &[DocumentData]) -> CoreResult<&DocumentData> {
    let mut newest = states
        .first()
        .ok_or_else(|| CoreError::ShouldNeverHappen("no states to compare".to_string()))?;
    let mut newest_height = height_of(newest)?;
    for state in &states[1..] {
        let height = height_of(state)?;
        if height > newest_height {
            newest = state;
            newest_height = height;
        }
    }
    Ok(newest)
}

/// The height of a revision string, exposed so callers do not re-derive it.
pub fn revision_height(state: &DocumentData) -> CoreResult<u32> {
    height_of(state)
}

const _: fn(&str) -> u32 = get_height_of_revision;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(id: &str, height: u32, deleted: bool) -> DocumentData {
        DocumentData::from_value(json!({
            "id": id,
            "_rev": format!("{height}-token"),
            "_deleted": deleted,
            "_meta": { "lwt": height as f64 },
            "_attachments": {}
        }))
        .expect("valid")
    }

    /// The rule the cache exists for: a lower revision must never replace a higher one,
    /// whatever order the events arrive in.
    #[test]
    fn an_older_revision_never_replaces_a_newer_one() {
        let mut cache = DocumentCache::new();
        cache.insert("a", state("a", 3, false)).expect("insert 3");
        cache.insert("a", state("a", 2, false)).expect("insert 2");
        assert_eq!(
            cache.get("a").expect("cached").rev(),
            Some("3-token"),
            "revision 2 must not roll the cache back from 3"
        );
    }

    #[test]
    fn a_newer_revision_replaces_an_older_one() {
        let mut cache = DocumentCache::new();
        cache.insert("a", state("a", 1, false)).expect("insert 1");
        cache.insert("a", state("a", 2, false)).expect("insert 2");
        assert_eq!(cache.get("a").expect("cached").rev(), Some("2-token"));
    }

    #[test]
    fn the_same_height_keeps_the_most_recently_seen_state() {
        // Two databases can produce the same height for one document; the cache keeps
        // whichever arrived last, because neither is provably newer.
        let mut cache = DocumentCache::new();
        cache.insert("a", state("a", 2, false)).expect("first");
        let mut second = state("a", 2, false);
        second.set("marker", json!("second"));
        cache.insert("a", second).expect("second");
        assert_eq!(
            cache.get("a").expect("cached").get("marker"),
            Some(&json!("second"))
        );
    }

    #[test]
    fn a_tombstone_is_visible_only_with_deleted() {
        let mut cache = DocumentCache::new();
        cache.insert("a", state("a", 2, true)).expect("insert");
        assert!(cache.get("a").is_none(), "no live handle for a tombstone");
        assert!(cache.get_with_deleted("a").is_some());
        assert_eq!(cache.len(), 1, "the tombstone is still cached");
        assert_eq!(cache.iter().count(), 0, "but not a live state");
    }

    #[test]
    fn newest_of_several_states_is_the_highest_height() {
        let states = vec![
            state("a", 1, false),
            state("a", 7, false),
            state("a", 4, false),
        ];
        assert_eq!(
            find_newest_of_document_states(&states)
                .expect("newest")
                .rev(),
            Some("7-token")
        );
    }

    #[test]
    fn a_state_without_a_revision_is_rejected_rather_than_cached() {
        let mut cache = DocumentCache::new();
        let orphan = DocumentData::from_value(json!({
            "id": "a", "_deleted": false, "_meta": { "lwt": 1.0 }, "_attachments": {}
        }))
        .expect("valid");
        assert!(cache.insert("a", orphan).is_err());
        assert!(cache.is_empty());
    }
}
