//! The SQLite storage benchmark: upstream's operation mix over `dxdb-sqlite`.
//!
//! Run: `cargo run --release --package dxdb-sqlite --bin bench-sqlite -- --out <file.json>`
//!
//! The scenario (document shape, operation list, statistics) comes from
//! [`dxdb_filesystem::bench`], which both storage benches share. That is the
//! requirement in PLAN.md 7.5: a number measured on a different operation mix cannot
//! be compared to anything, including to the other dxdb storages.
//!
//! # Why `bulk_write` goes through the driver trait, not a storage impl
//!
//! `dxdb-sqlite` has the `SqliteDriver` trait and a native driver, but not yet a
//! full `RxStorageInstance` implementation. So this measures the storage *engine*:
//! the DDL from the crate docs, the prepared statements from `StatementId`, and the
//! transaction discipline (`BEGIN IMMEDIATE` + `busy_timeout`). What it does not
//! measure is the layer above -- `categorizeBulkWriteRows` and per-row conflict
//! detection -- because that lives in `dxdb-storage` and belongs to a trait-level
//! bench, not a driver-level one. Stated here so the number is not over-read.
//!
//! # Index pushdown, and the honest reading of `query_sorted`
//!
//! The DDL has an index-string table and the `(deleted, lwt, id)` index, but the
//! *planner* that would produce index strings for a query lives in `dxdb-query`,
//! which is not published. So `query_sorted` here is `SELECT data ... ORDER BY`
//! over the JSON column -- a sort inside SQLite rather than an index range scan.
//! That is closer to upstream's trial storage than to a finished implementation, and
//! `docs/BENCHMARKS.md` records it as such.
//!
//! `find_by_id_bulk` **is** a real index lookup: `id` is the primary key, so it is a
//! B-tree seek per document and no scan is involved.

use std::path::PathBuf;

use dxdb_bench::scenario::{self as bench, BenchOp, BenchReport, OpTimings, Scenario};
use dxdb_sqlite::driver::{Param, StatementId, TableNames};
use dxdb_sqlite::rusqlite_driver::RusqliteDriver;
use dxdb_sqlite::traits::SqliteDriver;

fn main() -> std::process::ExitCode {
    let mut out_path: Option<PathBuf> = None;
    let mut runs = Scenario::default().runs;
    let mut docs = Scenario::default().docs;
    // WAL + NORMAL is what the driver configures by default, and it does not fsync
    // on every commit. `FULL` does. Measuring both is how the durability-policy
    // difference against redb gets established instead of asserted.
    let mut synchronous: Option<String> = None;

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
            "--synchronous" => match args.next() {
                Some(v) => synchronous = Some(v),
                None => {
                    eprintln!("error: --synchronous needs a value (NORMAL, FULL, OFF)");
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
                    "usage: bench-sqlite --out <file.json> [--runs N] [--docs N]\n\
                     Measures dxdb-sqlite over upstream's test/performance.test.ts mix."
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
    let work_dir = std::env::temp_dir().join(format!("dxdb-bench-sqlite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work_dir);
    if let Err(e) = std::fs::create_dir_all(&work_dir) {
        eprintln!("error: cannot create {}: {e}", work_dir.display());
        return std::process::ExitCode::from(1);
    }

    let document_bytes = bench::document(0).len();
    eprintln!(
        "bench-sqlite: {} docs ({} batches of {}), {} serial, {} parallel queries, {} runs, {} B/doc",
        scenario.docs,
        scenario.batches,
        scenario.docs_per_batch(),
        scenario.serial_docs,
        scenario.parallel_queries,
        scenario.runs,
        document_bytes
    );

    let mut bulk_insert = OpTimings::new("bulk_insert");
    let mut insert_serial = OpTimings::new("insert_serial");
    let mut find_by_id_bulk = OpTimings::new("find_by_id_bulk");
    let mut find_by_id_serial = OpTimings::new("find_by_id_serial");
    let mut query_sorted = OpTimings::new("query_sorted");
    let mut query_parallel = OpTimings::new("query_parallel");
    let mut count = OpTimings::new("count");
    let mut changed_since = OpTimings::new("changed_since");

    let tables = TableNames::new("bench", 0);

    for run in 0..scenario.runs {
        let db_path = work_dir.join(format!("run{run}.sqlite"));
        let mut driver = RusqliteDriver::new();
        let db = match driver.open(&db_path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("error: open failed: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        if let Some(mode) = &synchronous {
            driver
                .set_pragma(&db, "synchronous", mode)
                .expect("set synchronous");
        }
        create_schema(&mut driver, &db, &tables);

        let ids = bench::document_ids(&scenario);
        // (id, body) pairs: the key is the document's own primary value.
        let bodies: Vec<(String, String)> = (0..scenario.docs)
            .map(|i| bench::bulk_document(i as u64, scenario.parallel_queries))
            .collect();

        // ---- bulk_insert: scenario.batches transactions of docs/batches rows ----
        let per_batch = scenario.docs_per_batch();
        let ((), ms) = bench::time_ms(|| {
            for (batch, chunk) in bodies.chunks(per_batch).enumerate() {
                insert_batch(&mut driver, &db, &tables, batch, chunk);
            }
        });
        bulk_insert.push(ms);
        eprintln!("  run {run}: bulk_insert {ms:.1} ms");

        // ---- insert_serial: one row per call, one transaction each ----
        let ((), ms) = bench::time_ms(|| {
            let stmt = driver
                .prepare(&db, StatementId::InsertDocument, &tables)
                .expect("prepare insert");
            for i in 0..scenario.serial_docs {
                let (id, body) = bench::serial_document(1_000_000 + i as u64);
                driver
                    .execute(
                        &db,
                        &stmt,
                        &[
                            Param::Text(&id),
                            Param::Text("1-a"),
                            Param::Int(0),
                            Param::Int((i + 1) as i64),
                            Param::Text(&body),
                        ],
                        "insert_serial",
                    )
                    .expect("serial insert");
            }
        });
        insert_serial.push(ms);
        eprintln!("  run {run}: insert_serial {ms:.1} ms");

        // ---- find_by_id_bulk: every id, one prepared statement ----
        let ((), ms) = bench::time_ms(|| {
            let stmt = driver
                .prepare(&db, StatementId::SelectDocumentById, &tables)
                .expect("prepare select");
            let mut found = 0usize;
            for id in &ids {
                driver
                    .query(&db, &stmt, &[Param::Text(id)], "find_by_id", &mut |_row| {
                        found += 1;
                        Ok(())
                    })
                    .expect("find by id");
            }
            assert_eq!(found, scenario.docs, "find-by-id returned wrong count");
        });
        find_by_id_bulk.push(ms);
        eprintln!("  run {run}: find_by_id_bulk {ms:.1} ms");

        // ---- find_by_id_serial: one id per statement execution ----
        let ((), ms) = bench::time_ms(|| {
            for id in ids.iter().take(scenario.serial_docs) {
                let stmt = driver
                    .prepare(&db, StatementId::SelectDocumentById, &tables)
                    .expect("prepare select");
                driver
                    .query(&db, &stmt, &[Param::Text(id)], "find_by_id", &mut |_row| {
                        Ok(())
                    })
                    .expect("find by id");
            }
        });
        find_by_id_serial.push(ms);
        eprintln!("  run {run}: find_by_id_serial {ms:.1} ms");

        // ---- query_sorted: SELECT ... ORDER BY over the JSON column ----
        // Not an index range scan: the planner that would emit index strings is in
        // dxdb-query, which is not published. See the module docs.
        let (rows, ms) = bench::time_ms(|| {
            let stmt = driver
                .prepare(&db, StatementId::SelectAllDocuments, &tables)
                .expect("prepare scan");
            let mut collected: Vec<Vec<u8>> = Vec::new();
            driver
                .query(&db, &stmt, &[], "query_sorted", &mut |row| {
                    collected.push(row.get_text(4)?.as_bytes().to_vec());
                    Ok(())
                })
                .expect("scan");
            // Sort in Rust, matching what the filesystem bench does, so the two
            // numbers measure the same work rather than one measuring SQLite's sort
            // and the other measuring a Rust sort.
            collected.sort_by(|a, b| {
                let (av2, av1) = sort_keys(a);
                let (bv2, bv1) = sort_keys(b);
                av2.cmp(&bv2).then_with(|| av1.cmp(&bv1))
            });
            collected
        });
        assert_eq!(
            rows.len(),
            scenario.docs + scenario.serial_docs,
            "query returned wrong count"
        );
        query_sorted.push(ms);
        eprintln!("  run {run}: query_sorted {ms:.1} ms ({} rows)", rows.len());

        // ---- query_parallel: n selector queries, var2 == i ----
        let ((), ms) = bench::time_ms(|| {
            let mut total = 0usize;
            for target in 0..scenario.parallel_queries as u64 {
                let stmt = driver
                    .prepare(&db, StatementId::SelectAllDocuments, &tables)
                    .expect("prepare scan");
                driver
                    .query(&db, &stmt, &[], "query_parallel", &mut |row| {
                        let body = row.get_text(4)?.as_bytes().to_vec();
                        if sort_keys(&body).0 == target {
                            total += 1;
                        }
                        Ok(())
                    })
                    .expect("selector scan");
            }
            assert!(total > 0, "parallel query matched nothing");
        });
        query_parallel.push(ms);
        eprintln!("  run {run}: query_parallel {ms:.1} ms");

        // ---- count: n selector counts ----
        let ((), ms) = bench::time_ms(|| {
            for target in 0..scenario.parallel_queries as u64 {
                let stmt = driver
                    .prepare(&db, StatementId::SelectAllDocuments, &tables)
                    .expect("prepare scan");
                let mut matched = 0u64;
                driver
                    .query(&db, &stmt, &[], "count", &mut |row| {
                        if sort_keys(row.get_text(4)?.as_bytes()).0 == target {
                            matched += 1;
                        }
                        Ok(())
                    })
                    .expect("count scan");
                assert!(matched <= (scenario.docs + scenario.serial_docs) as u64);
            }
        });
        count.push(ms);
        eprintln!("  run {run}: count {ms:.1} ms");

        // ---- changed_since: the indexed path, via SelectIdsChangedSince ----
        let ((), ms) = bench::time_ms(|| {
            let stmt = driver
                .prepare(&db, StatementId::SelectIdsChangedSince, &tables)
                .expect("prepare changed_since");
            let mut seen = 0usize;
            driver
                .query(
                    &db,
                    &stmt,
                    &[Param::Int(0), Param::Int(100)],
                    "changed_since",
                    &mut |_row| {
                        seen += 1;
                        Ok(())
                    },
                )
                .expect("changed_since");
            assert_eq!(seen, 100, "changed_since should return the limit");
        });
        changed_since.push(ms);
        eprintln!("  run {run}: changed_since {ms:.1} ms");

        driver.close(db).expect("close");
        let _ = std::fs::remove_file(&db_path);
    }
    let _ = std::fs::remove_dir_all(&work_dir);

    let operations = vec![
        BenchOp {
            timings: bulk_insert,
            per_sample_ops: scenario.docs,
        },
        BenchOp {
            timings: insert_serial,
            per_sample_ops: scenario.serial_docs,
        },
        BenchOp {
            timings: find_by_id_bulk,
            per_sample_ops: scenario.docs,
        },
        BenchOp {
            timings: find_by_id_serial,
            per_sample_ops: scenario.serial_docs,
        },
        BenchOp {
            timings: query_sorted,
            per_sample_ops: 1,
        },
        BenchOp {
            timings: query_parallel,
            per_sample_ops: scenario.parallel_queries,
        },
        BenchOp {
            timings: count,
            per_sample_ops: scenario.parallel_queries,
        },
        BenchOp {
            timings: changed_since,
            per_sample_ops: 1,
        },
    ];

    let report = BenchReport {
        storage: if synchronous.is_some() {
            "sqlite-rusqlite-synchronous-override"
        } else {
            "sqlite-rusqlite"
        },
        box_name: "fly open-surrealdb-ds-builder 87477e0cd01748 (8 cores, shared)",
        toolchain: option_env!("RUSTC_VERSION").unwrap_or("1.95.0").to_string(),
        idle_at_start: std::env::var("DXDB_BENCH_IDLE").as_deref() == Ok("yes"),
        idle_evidence: std::env::var("DXDB_BENCH_IDLE_EVIDENCE")
            .unwrap_or_else(|_| "not recorded".to_string()),
        date: std::env::var("DXDB_BENCH_DATE").unwrap_or_else(|_| "unknown".to_string()),
        command: std::env::var("DXDB_BENCH_COMMAND").unwrap_or_else(|_| {
            "cargo run --release --package dxdb-sqlite --bin bench-sqlite -- --out <file>"
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

    eprintln!("\nsqlite-rusqlite, median of {} runs:", scenario.runs);
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

/// The DDL from the crate docs: `STRICT`, an index-string table, and the
/// `(deleted, lwt, id)` index that `cleanup`/`changed_since` ride on.
fn create_schema(driver: &mut RusqliteDriver, db: &rusqlite::Connection, tables: &TableNames) {
    db.execute_batch(&format!(
        "CREATE TABLE {docs} (
             id TEXT NOT NULL PRIMARY KEY,
             revision TEXT NOT NULL,
             deleted INTEGER NOT NULL CHECK (deleted IN (0,1)),
             lwt INTEGER NOT NULL,
             data TEXT NOT NULL
         ) STRICT;
         CREATE INDEX {docs}_cleanup ON {docs} (deleted, lwt, id);
         CREATE INDEX {docs}_lwt ON {docs} (lwt, id);
         CREATE TABLE {idx} (
             index_string TEXT NOT NULL,
             id TEXT NOT NULL,
             PRIMARY KEY (index_string, id)
         ) STRICT, WITHOUT ROWID;
         CREATE TABLE {att} (
             id TEXT NOT NULL,
             attachment_id TEXT NOT NULL,
             data BLOB NOT NULL,
             PRIMARY KEY (id, attachment_id)
         ) STRICT, WITHOUT ROWID;",
        docs = tables.docs,
        idx = tables.index,
        att = tables.attachments
    ))
    .expect("schema");

    // One prepared statement, reused for the whole run, so the measurement is not
    // dominated by SQL parsing.
    driver
        .prepare(db, StatementId::InsertDocument, tables)
        .expect("warm insert statement");
}

/// Inserts one batch inside a single `BEGIN IMMEDIATE` transaction.
///
/// One commit per batch, matching upstream's `bulkInsert` and the filesystem
/// benchmark's `put_documents`. Without the transaction this would be 500 commits
/// and the number would measure fsync rather than the storage.
fn insert_batch(
    driver: &mut RusqliteDriver,
    db: &rusqlite::Connection,
    tables: &TableNames,
    batch: usize,
    docs: &[(String, String)],
) {
    let stmt = driver
        .prepare(db, StatementId::InsertDocument, tables)
        .expect("prepare insert");
    driver.begin(db).expect("begin");
    for (i, (id, body)) in docs.iter().enumerate() {
        let lwt = (batch * docs.len() + i) as i64 + 1;
        driver
            .execute(
                db,
                &stmt,
                &[
                    Param::Text(id),
                    Param::Text("1-a"),
                    Param::Int(0),
                    Param::Int(lwt),
                    // `Param::Text`, not `Param::Blob`: the `data` column is TEXT and
                    // STRICT refuses a BLOB in it (error 3091). The document is
                    // ordered JSON, which is text, so Text is also the correct
                    // binding rather than a workaround.
                    Param::Text(body),
                ],
                "bulk_insert",
            )
            .expect("bulk insert row");
    }
    driver.commit(db).expect("commit");
}

/// Extracts `(var2, var1)` for the sort without a JSON parse, so the two storage
/// benches measure the same work.
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
