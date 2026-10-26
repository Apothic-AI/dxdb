//! Runs the **ported testkit suite** against `dxdb-opfs`.
//!
//! `dxdb_testkit::suite` is the port of upstream's `rx-storage-implementations.test.ts`,
//! dispatched on upstream line. The storage under test here is the real `OpfsStorage`
//! over an in-memory `FileStore`, and the browser run in `demo/` repeats it over real
//! OPFS through whichever access path the engine offers.
//!
//! The backend is in-memory here for one reason: a host test has no OPFS. That makes the
//! *storage* logic the thing under test, which is the right split — the file backend has
//! its own tests ([`file_store`](file_store.rs)) and the browser run covers the pairing.
//!
//! Every case is bounded by the watchdog: a storage test that hangs is an infrastructure
//! liability, not a slow test.

mod support;

use std::rc::Rc;

use support::watchdog::within;

use dxdb_opfs::{MemoryFiles, OpfsStorage};
use dxdb_testkit::decl::exercised_cases;

fn storage() -> OpfsStorage<MemoryFiles> {
    OpfsStorage::new(Rc::new(MemoryFiles::new()))
}

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
    impl_bulkwrite_03 => 1130,
    impl_bulkwrite_04 => 1512,
    impl_bulkwrite_06 => 1578,
    impl_bulkwrite_10 => 1626,
    impl_bulkwrite_11 => 1693,
    impl_bulkwrite_12 => 1847,
    impl_findbyids_02 => 2021,
    impl_findbyids_03 => 2077,
    impl_findbyids_04 => 2123,
    impl_findbyids_05 => 2184,
    impl_findbyids_06 => 2221,
    impl_query_01 => 2292,
    impl_query_02 => 2346,
    impl_query_03 => 2430,
    impl_changedsince_01 => 2462,
    impl_changedsince_02 => 2546,
    impl_changedsince_03 => 2642,
    impl_cleanup_01 => 2696,
    impl_cleanup_02 => 2791,
    impl_cleanup_03 => 2881,
    impl_cleanup_04 => 2938,
    impl_cleanup_05 => 2995,
    impl_cleanup_06 => 3109,
    impl_cleanup_07 => 3193,
    impl_attachment_01 => 3267,
    impl_attachment_02 => 3369,
    impl_attachment_03 => 3470,
    impl_attachment_04 => 3550,
    impl_attachment_05 => 3613,
    impl_changestream_01 => 3633,
    impl_changestream_02 => 3676,
    impl_changestream_03 => 3693,
    impl_count_01 => 4046,
}

/// Every exercised case in the testkit that belongs to the implementations suite must
/// have a `#[test]` above.
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
        "these declared-exercised cases have no #[test] here: {missing:?}"
    );
}
