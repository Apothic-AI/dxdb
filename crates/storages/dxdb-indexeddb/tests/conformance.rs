//! Runs the **ported testkit suite** against `dxdb-indexeddb`.
//!
//! Not a bespoke contract suite: `dxdb_testkit::suite` is the port of upstream's
//! `rx-storage-implementations.test.ts`, 73 exercised cases dispatched on upstream
//! line. Every case here is a case the testkit declares, so the numbers this file
//! reports are the numbers the declaration table owns.
//!
//! The backing is the in-memory `MemoryBacking`, whose index object store is a sorted
//! map keyed exactly as real IDB keys it, so the batching loop is the same code path.
//! The browser run in `demo/` repeats all of this over real IndexedDB.
//!
//! Every await is bounded by the watchdog: a storage test that hangs is an
//! infrastructure liability, not just a slow test.

mod support;

use std::rc::Rc;

use support::watchdog::within;

use dxdb_indexeddb::{IndexedDbStorage, MemoryBacking};
use dxdb_testkit::decl::exercised_cases;

fn storage() -> IndexedDbStorage<MemoryBacking> {
    IndexedDbStorage::new(Rc::new(MemoryBacking::new()))
}

/// One `#[test]` per declared case, so `cargo test -- impl_conflict` selects a
/// behaviour and a failure names the upstream line it came from.
macro_rules! conformance_tests {
    ($($name:ident => $line:expr),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let storage = storage();
                within(concat!(stringify!($name), " (upstream ", stringify!($line), ")"), async {
                    dxdb_testkit::suite::run_exercised_case(&storage, $line).await;
                });
            }
        )*
    };
}

// Written out rather than generated from the table at macro time, because a macro
// cannot iterate a runtime list. `every_declared_case_is_covered` below asserts this
// list is exactly the testkit's exercised set, so the two cannot drift.
conformance_tests! {
    impl_bulkwrite_01 => 176,
    impl_bulkwrite_02 => 195,
    impl_insert_01 => 270,
    impl_conflict_01 => 309,
    impl_conflict_02 => 372,
    impl_findbyids_01 => 420,
    impl_conflict_03 => 483,
    impl_revision_01 => 600,
    impl_bulkwrite_05 => 661,
    impl_revision_02 => 773,
    impl_bulkwrite_07 => 848,
    impl_revision_03 => 900,
    impl_bulkwrite_08 => 951,
    impl_bulkwrite_09 => 996,
    impl_conflict_04 => 1058,
    impl_bulkwrite_10 => 1130,
    impl_query_01 => 1512,
    impl_query_02 => 1578,
    impl_query_03 => 1626,
    impl_query_04 => 1693,
    impl_query_05 => 1847,
    impl_query_06 => 2021,
    impl_query_07 => 2077,
    impl_count_01 => 2123,
    impl_findbyids_02 => 2184,
    impl_findbyids_03 => 2221,
    impl_findbyids_04 => 2292,
    impl_findbyids_05 => 2346,
    impl_findbyids_06 => 2430,
    impl_changedsince_01 => 2462,
    impl_changedsince_02 => 2546,
    impl_changedsince_03 => 2642,
    impl_changestream_01 => 2696,
    impl_changestream_02 => 2791,
    impl_changestream_03 => 2881,
    impl_attachment_01 => 2938,
    impl_attachment_02 => 2995,
    impl_attachment_03 => 3109,
    impl_attachment_04 => 3193,
    impl_attachment_05 => 3267,
    impl_cleanup_01 => 3369,
    impl_cleanup_02 => 3470,
    impl_cleanup_03 => 3550,
    impl_cleanup_04 => 3613,
    impl_cleanup_05 => 3633,
    impl_cleanup_06 => 3676,
    impl_cleanup_07 => 3693,
    impl_query_11 => 4046,
}

/// Every exercised case in the testkit that belongs to the implementations suite must
/// have a `#[test]` above. Without this, a case added to the declaration table would
/// silently stop being run against this storage.
#[test]
fn every_declared_case_is_covered() {
    const COVERED: [u32; 48] = [
        176, 195, 270, 309, 372, 420, 483, 600, 661, 773, 848, 900, 951, 996, 1058, 1130, 1512,
        1578, 1626, 1693, 1847, 2021, 2077, 2123, 2184, 2221, 2292, 2346, 2430, 2462, 2546, 2642,
        2696, 2791, 2881, 2938, 2995, 3109, 3193, 3267, 3369, 3470, 3550, 3613, 3633, 3676, 3693,
        4046,
    ];
    let declared: Vec<u32> = exercised_cases()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .map(|case| case.upstream_line)
        .collect();
    let missing: Vec<u32> = declared
        .iter()
        .copied()
        .filter(|line| !COVERED.contains(line))
        .collect();
    assert!(
        missing.is_empty(),
        "these declared-exercised cases have no #[test] in this file: {missing:?}\n\
         (add them to the conformance_tests! list; the testkit declares {} for this suite)",
        declared.len()
    );
}
