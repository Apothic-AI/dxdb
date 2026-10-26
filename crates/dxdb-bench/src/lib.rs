//! dxdb storage benchmarks.
//!
//! # Why a separate crate
//!
//! PLAN.md 7.5 and `docs/BENCHMARKS.md` require that storages be measured over
//! **the same operation mix**, because a number measured on a different operation set
//! cannot be compared to anything -- not to another dxdb storage, and not to
//! upstream. That means one shared definition of the scenario, and the only two ways
//! to get one are (a) a storage crate depending on another storage crate just to
//! reuse a helper, or (b) this crate, which depends on both.
//!
//! (b) is the right direction: benchmark-only dependencies stay in the benchmark
//! graph, so `dxdb-sqlite` and `dxdb-filesystem` ship to users without a benchmark
//! dependency between them.
//!
//! # Running
//!
//! ```bash
//! cargo run --release --package dxdb-bench --bin bench-filesystem -- --out out/fs.json
//! cargo run --release --package dxdb-bench --bin bench-sqlite     -- --out out/sqlite.json
//! ```
//!
//! `xtask/bench-storages.sh` runs both, records the box's idle state, and writes the
//! metadata that goes into `docs/BENCHMARKS.md`.
//!
//! # Read the per-storage module docs before quoting a number
//!
//! Each binary states what it does and does not measure. Two caveats carry into every
//! number here:
//!
//! * `query_sorted` and `count` are **scan-and-filter**, not index range scans,
//!   because the query planner that would emit index strings lives in `dxdb-query`,
//!   which is not published. Upstream's trial SQLite storage does exactly the same,
//!   so the number is real -- it is just the number for a scan.
//! * There is no `RxStorageInstance` layer in the path: `categorizeBulkWriteRows`,
//!   per-row conflict detection and change-event emission are above this and are not
//!   measured. A storage benchmark that skipped them will be faster than the real
//!   storage will be.

#![forbid(unsafe_code)]

pub mod scenario;

pub use scenario::{
    bulk_document, document, document_ids, document_shaped, document_with_id, now_ms, report_json,
    serial_document, time_ms, BenchOp, BenchReport, OpTimings, Scenario,
};

// ---------------------------------------------------------------------------
// WASM size-budget fixtures (PLAN.md 5.3)
// ---------------------------------------------------------------------------
//
// The four budget rows are "dxdb-js core only", "+ memory", "+ OPFS", "+ SQLite". OPFS
// is not built yet, so only the three that exist can be measured -- and saying that is
// better than leaving a row "pending", which reads as "nobody got to it".
//
// Each fixture re-exports a minimal entry point so the linker cannot drop the storage:
// a `cdylib` whose storage code is unreferenced is optimised out, and the resulting
// artifact would measure `dxdb-js` alone while claiming to include a storage. That is
// the single easiest way to produce a flattering, wrong budget number.

/// Entry point that forces `dxdb-js`'s surface into the artifact.
///
/// The real JS bundle will export far more; this exists so the measurement has a root
/// that reaches the bindings and nothing is dead-code-eliminated away.
#[cfg(feature = "wasm-size-fixtures")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn dxdb_size_fixture_probe(input: &str) -> String {
    // Reach through dxdb-js so its bindings are linked, then through whichever storage
    // feature is on, so its code cannot be dropped.
    let mut touched = input.len();

    #[cfg(feature = "memory-bundle")]
    {
        // `MemoryStorage` itself is a unit struct with no inherent `name()`; the name
        // comes from the `RxStorage` impl, so bring the trait into scope.
        use dxdb_storage::RxStorage as _;
        let storage = dxdb_memory::MemoryStorage::new();
        touched += storage.name().len();
    }

    #[cfg(feature = "opfs-bundle")]
    {
        // Reach through the storage's own generics and its wasm-bindgen bindings so
        // neither web-sys nor the file layout can be eliminated. Deliberately not a
        // constructed instance: OPFS needs a browser and a worker, and the size artifact
        // must not require either. `WebFiles` is the concrete `FileStore` the browser
        // path uses, so naming it is what pulls the web-sys surface in.
        touched += std::mem::size_of::<dxdb_opfs::OpfsStorage<dxdb_opfs::web::WebFiles>>();
        touched += dxdb_opfs::web::path_report().len();
    }

    #[cfg(feature = "sqlite-bundle")]
    {
        // A driver handle rather than a database: constructing a `Connection` needs a
        // host filesystem, which the size artifact must not require.
        let driver = dxdb_sqlite::RusqliteDriver::new();
        touched += dxdb_sqlite::Vfs::selected().describe().len();
        let _ = driver;
    }

    format!("probe:{touched}")
}

/// A stable symbol so a reader can confirm the artifact contains this fixture.
///
/// No `#[no_mangle]`: the lint rejects it (a duplicate symbol across libraries has
/// undefined behaviour), and the wasm-bindgen export above is already the linker root
/// that keeps the storage code from being eliminated.
#[cfg(feature = "wasm-size-fixtures")]
pub fn dxdb_size_fixture_marker() -> u32 {
    1
}
