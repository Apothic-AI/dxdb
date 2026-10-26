//! The filesystem storage benchmark: upstream's operation mix over `dxdb-filesystem`.
//!
//! Run: `cargo run --release --package dxdb-filesystem --bin bench-filesystem -- --out <file.json>`
//!
//! # Design decisions that affect the numbers, stated up front
//!
//! **Batched `put_document` inside one redb transaction.** The public
//! `put_document` commits per call, which is the right API for a single write and
//! the wrong one for a 500-document batch because it makes 500 fsyncs. Upstream's
//! `bulkInsert` is one `bulk_write` call, so the bulk-insert case builds the same
//! keys and commits once, which is what a real `bulk_write` implementation would
//! do. The per-document path is measured separately as `insert_serial`, so both
//! numbers exist and neither is silently substituted for the other.
//!
//! **Query and count scan.** `dxdb-filesystem` has no query engine yet -- the
//! planner lives in `dxdb-query`, which is not published -- so these cases scan the
//! document prefix and filter/sort in Rust. That is exactly what upstream's
//! *trial* SQLite storage does (`sqlite-storage-instance.ts:322`), so the number is
//! real but it is a scan, not an index range scan. Recorded in BENCHMARKS.md as
//! such rather than presented as an indexed query.
//!
//! **`changed_since` is measured even though upstream's perf test omits it.**
//! `getChangedDocumentsSince` is part of the storage contract (PLAN.md 4.2), and
//! the `chg` table exists precisely to make it cheap. A number here is worth more
//! than a sentence claiming it is cheap.

use std::path::PathBuf;

use dxdb_bench::scenario::{self as bench, BenchOp, BenchReport, OpTimings, Scenario};
use dxdb_filesystem::layout;
use redb::ReadableTable;

fn main() -> std::process::ExitCode {
    let mut out_path: Option<PathBuf> = None;
    let mut runs = Scenario::default().runs;
    let mut docs = Scenario::default().docs;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => match args.next() {
                Some(v) => out_path = Some(PathBuf::from(v)),
                None => {
                    eprintln!("error: --out needs a path");
                    return std::process::ExitCode::from(2);
                }
            },
            "--runs" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => runs = v,
                None => {
                    eprintln!("error: --runs needs a number");
                    return std::process::ExitCode::from(2);
                }
            },
            "--docs" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => docs = v,
                None => {
                    eprintln!("error: --docs needs a number");
                    return std::process::ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                println!(
                    "usage: bench-filesystem --out <file.json> [--runs N] [--docs N]\n\
                     Measures dxdb-filesystem over upstream's test/performance.test.ts mix."
                );
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unexpected argument {other:?}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    let scenario = Scenario {
        runs,
        docs,
        ..Scenario::default()
    };

    let started = bench::now_ms();
    let work_dir = std::env::temp_dir().join(format!("dxdb-bench-fs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work_dir);
    if let Err(e) = std::fs::create_dir_all(&work_dir) {
        eprintln!("error: cannot create {}: {e}", work_dir.display());
        return std::process::ExitCode::from(1);
    }

    let document_bytes = bench::document(0).len();
    eprintln!(
        "bench-filesystem: {} docs ({} batches of {}), {} serial, {} parallel queries, {} runs, {} B/doc",
        scenario.docs,
        scenario.batches,
        scenario.docs_per_batch(),
        scenario.serial_docs,
        scenario.parallel_queries,
        scenario.runs,
        document_bytes
    );

    let mut operations: Vec<BenchOp> = Vec::new();
    let mut bulk_insert = OpTimings::new("bulk_insert");
    let mut insert_serial = OpTimings::new("insert_serial");
    let mut find_by_id_bulk = OpTimings::new("find_by_id_bulk");
    let mut find_by_id_serial = OpTimings::new("find_by_id_serial");
    let mut query_sorted = OpTimings::new("query_sorted");
    let mut query_parallel = OpTimings::new("query_parallel");
    let mut count = OpTimings::new("count");
    let mut changed_since_t = OpTimings::new("changed_since");

    for run in 0..scenario.runs {
        // A fresh directory per run, so one run cannot read another's rows (which
        // on a scan-based query would flatter the numbers).
        let run_dir = work_dir.join(format!("run{run}"));
        let _ = std::fs::remove_dir_all(&run_dir);
        let store = match dxdb_filesystem::FilesystemStorage::open(&run_dir) {
            Ok(store) => store,
            Err(e) => {
                eprintln!("error: open failed: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        let collection = "bench";
        let version = 0;

        let ids = bench::document_ids(&scenario);
        // (id, body) pairs: the key is the document's own primary value.
        let docs: Vec<(String, Vec<u8>)> = (0..scenario.docs)
            .map(|i| {
                let (id, body) = bench::bulk_document(i as u64, scenario.parallel_queries);
                (id, body.into_bytes())
            })
            .collect();

        // ---- bulk_insert: scenario.batches transactions of docs/batches rows ----
        let per_batch = scenario.docs_per_batch();
        let ((), ms) = bench::time_ms(|| {
            for (batch, chunk) in docs.chunks(per_batch).enumerate() {
                bulk_write_batch(&store, collection, version, batch, chunk);
            }
        });
        bulk_insert.push(ms);
        eprintln!("  run {run}: bulk_insert {ms:.1} ms");

        // ---- insert_serial: one document per call, through the committing API ----
        let ((), ms) = bench::time_ms(|| {
            for i in 0..scenario.serial_docs {
                let (id, body) = bench::serial_document(1_000_000 + i as u64);
                store
                    .put_document(
                        collection,
                        version,
                        &id,
                        (i + 1) as i64,
                        false,
                        body.as_bytes(),
                    )
                    .expect("serial insert");
            }
        });
        insert_serial.push(ms);
        eprintln!("  run {run}: insert_serial {ms:.1} ms");

        // ---- find_by_id_bulk: every id in one call ----
        let ((), ms) = bench::time_ms(|| {
            let mut found = 0usize;
            for id in &ids {
                if store
                    .get_document(collection, version, id)
                    .expect("get")
                    .is_some()
                {
                    found += 1;
                }
            }
            assert_eq!(found, scenario.docs, "bulk find returned wrong count");
        });
        find_by_id_bulk.push(ms);
        eprintln!("  run {run}: find_by_id_bulk {ms:.1} ms");

        // ---- find_by_id_serial: one id per call ----
        let ((), ms) = bench::time_ms(|| {
            for id in ids.iter().take(scenario.serial_docs) {
                let _ = store.get_document(collection, version, id).expect("get");
            }
        });
        find_by_id_serial.push(ms);
        eprintln!("  run {run}: find_by_id_serial {ms:.1} ms");

        // ---- query_sorted: full scan, sort by (var2, var1) ----
        // Scan-then-sort, because there is no planner here yet. Stated in the module
        // docs and in BENCHMARKS.md, not hidden.
        let (rows, ms) = bench::time_ms(|| {
            let mut rows = store.documents(collection, version).expect("scan");
            rows.sort_by(|a, b| {
                let (av2, av1) = sort_keys(&a.1);
                let (bv2, bv1) = sort_keys(&b.1);
                av2.cmp(&bv2).then_with(|| av1.cmp(&bv1))
            });
            rows
        });
        let expected = scenario.docs + scenario.serial_docs;
        assert_eq!(rows.len(), expected, "sorted query returned wrong count");
        query_sorted.push(ms);
        eprintln!("  run {run}: query_sorted {ms:.1} ms ({} rows)", rows.len());

        // ---- query_parallel: n selector queries, var2 == i ----
        let ((), ms) = bench::time_ms(|| {
            let mut total = 0usize;
            for target in 0..scenario.parallel_queries as u64 {
                // A selector scan: keep documents whose var2 equals the target.
                // This is the shape upstream's parallel query measures.
                let rows = store.documents(collection, version).expect("scan");
                total += rows
                    .iter()
                    .filter(|(_, value)| sort_keys(value).0 == target)
                    .count();
            }
            assert!(total > 0, "parallel query matched nothing");
        });
        query_parallel.push(ms);
        eprintln!("  run {run}: query_parallel {ms:.1} ms");

        // ---- count: n selector counts ----
        let ((), ms) = bench::time_ms(|| {
            for target in 0..scenario.parallel_queries as u64 {
                let rows = store.documents(collection, version).expect("scan");
                let matched = rows
                    .iter()
                    .filter(|(_, value)| sort_keys(value).0 == target)
                    .count();
                assert!(matched <= scenario.docs + scenario.serial_docs);
            }
        });
        count.push(ms);
        eprintln!("  run {run}: count {ms:.1} ms");

        // ---- changed_since: ascending lwt scan limited to 100 ----
        let ((), ms) = bench::time_ms(|| {
            let rows = changed_since(&store, collection, version, 0, 100);
            assert_eq!(rows, 100, "changed_since should return the limit");
        });
        changed_since_t.push(ms);
        eprintln!("  run {run}: changed_since {ms:.1} ms");

        // Drop before the next run deletes the directory out from under it.
        drop(store);
        let _ = std::fs::remove_dir_all(&run_dir);
    }
    let _ = std::fs::remove_dir_all(&work_dir);

    let document_ids_count = scenario.docs;
    let serial = scenario.serial_docs;
    let parallel = scenario.parallel_queries;
    operations.push(BenchOp {
        timings: bulk_insert,
        per_sample_ops: document_ids_count,
    });
    operations.push(BenchOp {
        timings: insert_serial,
        per_sample_ops: serial,
    });
    operations.push(BenchOp {
        timings: find_by_id_bulk,
        per_sample_ops: document_ids_count,
    });
    operations.push(BenchOp {
        timings: find_by_id_serial,
        per_sample_ops: serial,
    });
    operations.push(BenchOp {
        timings: query_sorted,
        per_sample_ops: 1,
    });
    operations.push(BenchOp {
        timings: query_parallel,
        per_sample_ops: parallel,
    });
    operations.push(BenchOp {
        timings: count,
        per_sample_ops: parallel,
    });
    operations.push(BenchOp {
        timings: changed_since_t,
        per_sample_ops: 1,
    });

    let report = BenchReport {
        storage: "filesystem-redb",
        box_name: "fly open-surrealdb-ds-builder 87477e0cd01748 (8 cores, shared)",
        toolchain: option_env!("RUSTC_VERSION").unwrap_or("1.95.0").to_string(),
        idle_at_start: std::env::var("DXDB_BENCH_IDLE").as_deref() == Ok("yes"),
        idle_evidence: std::env::var("DXDB_BENCH_IDLE_EVIDENCE")
            .unwrap_or_else(|_| "not recorded".to_string()),
        date: std::env::var("DXDB_BENCH_DATE").unwrap_or_else(|_| "unknown".to_string()),
        command: std::env::var("DXDB_BENCH_COMMAND").unwrap_or_else(|_| {
            "cargo run --release --package dxdb-filesystem --bin bench-filesystem -- --out <file>"
                .to_string()
        }),
        duration_ms: bench::now_ms().saturating_sub(started),
        scenario,
        document_bytes,
        operations,
    };

    let json = bench::report_json(&report);

    match out_path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, &json) {
                eprintln!("error: cannot write {}: {e}", path.display());
                return std::process::ExitCode::from(1);
            }
            println!("wrote {}", path.display());
        }
        None => print!("{json}"),
    }

    // A summary a human can read without parsing JSON.
    eprintln!("\nfilesystem-redb, median of {} runs:", scenario.runs);
    for op in &report.operations {
        eprintln!(
            "  {:<18} {:>10.2} ms   {:>12.0} ops/s",
            op.timings.op,
            op.timings.median(),
            op.timings.per_second(op.per_sample_ops)
        );
    }

    std::process::ExitCode::SUCCESS
}

/// Writes a batch of documents in one redb transaction.
///
/// Uses [`FilesystemStorage::put_documents`], which is the batched API, so this
/// measures one commit per batch rather than one per row. That is what upstream's
/// `bulkInsert` measures.
fn bulk_write_batch(
    store: &dxdb_filesystem::FilesystemStorage,
    collection: &str,
    version: u32,
    batch: usize,
    docs: &[(String, Vec<u8>)],
) {
    let rows: Vec<(String, i64, bool, Vec<u8>)> = docs
        .iter()
        .enumerate()
        .map(|(i, (id, body))| {
            (
                id.clone(),
                (batch * docs.len() + i) as i64 + 1,
                false,
                body.clone(),
            )
        })
        .collect();
    let written = store
        .put_documents(collection, version, &rows)
        .expect("bulk insert");
    assert_eq!(written, docs.len());
}

/// Extracts `(var2, var1)` for the sort, without a JSON parser in the hot loop.
///
/// The generator emits a fixed field order, so a substring search is sound for this
/// input. A real query engine would use `dxdb-json`; doing that here would measure
/// the parser rather than the storage.
fn sort_keys(body: &[u8]) -> (u64, String) {
    let text = std::str::from_utf8(body).unwrap_or("");
    let var2 = text
        .find("\"var2\":")
        .and_then(|pos| {
            let rest = &text[pos + 7..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            rest[..end].parse::<u64>().ok()
        })
        .unwrap_or(0);
    let var1 = text
        .find("\"var1\":\"")
        .map(|pos| {
            let rest = &text[pos + 8..];
            rest[..rest.find('"').unwrap_or(0)].to_string()
        })
        .unwrap_or_default();
    (var2, var1)
}

/// Counts documents with `lwt > after`, ascending, up to `limit`.
///
/// Uses the `chg` table, which is the point of having it: the key is
/// `BE(flip(lwt)) ++ collection \0 version \0 id`, so an ascending scan from the
/// cursor is already in the right order and the limit stops the iterator.
fn changed_since(
    store: &dxdb_filesystem::FilesystemStorage,
    collection: &str,
    version: u32,
    after_lwt: i64,
    limit: usize,
) -> usize {
    let tx = match store.read_transaction() {
        Ok(tx) => tx,
        Err(_) => return 0,
    };
    let table = match tx.open_table(dxdb_filesystem::CHG) {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let start = layout::encode_lwt(after_lwt + 1).to_vec();
    let prefix = layout::prefix(collection, version);
    let mut count = 0usize;
    if let Ok(range) = table.range(start.as_slice()..) {
        for entry in range {
            let Ok((key, _)) = entry else { break };
            let key = key.value();
            // Key layout: 8 bytes lwt, then the collection prefix.
            if key.len() < 8 + prefix.len() {
                continue;
            }
            if &key[8..8 + prefix.len()] != prefix.as_slice() {
                continue;
            }
            count += 1;
            if count >= limit {
                break;
            }
        }
    }
    count
}
