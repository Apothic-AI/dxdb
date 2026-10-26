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
