//! The memory storage benchmark: upstream's operation mix over `dxdb-memory`.
//!
//! Run: `cargo run --release --package dxdb-bench --bin bench-memory -- --out <file.json>`
//!
//! # Why this row matters more than the others
//!
//! `dxdb-memory` is the reference implementation: every other storage is measured
//! against its behaviour, and the conformance suite uses it as the second control after
//! the scaffold. A storage benchmark table whose reference row is "pending" has no
//! baseline to read the other rows against.
//!
//! # The shape differs from the engine benches, and that is the honest thing
//!
//! The filesystem and sqlite benches drive a *driver API* (`put_document`,
//! `StatementId::InsertDocument`). This one drives the real `RxStorageInstance` trait,
//! because that is what `dxdb-memory` is. That means these numbers include
//! `categorize_bulk_write_rows`, document cloning, event emission and the change feed --
//! work the two engine benches skip.
//!
//! So a memory number is **not** directly comparable to a filesystem number as "which
//! engine is faster". It is comparable as "what the full storage contract costs", and it
//! is the number a user of any storage actually experiences. Recorded in BENCHMARKS.md
//! as a separate axis rather than mixed into the same column, because mixing them would
//! be the exact mistake the file warns about.
//!
//! Operations are the same set, in the same order, over the same generated documents.

use std::path::PathBuf;

use dxdb_bench::scenario::{self as bench, BenchOp, BenchReport, OpTimings, Scenario};
use dxdb_storage::{
    insert_row, BulkWriteRow, DocumentData, InstanceCreationParams, RxStorage, RxStorageInstance,
    StorageSchema,
};

fn schema() -> StorageSchema {
    // Built the way the conformance harness builds one: a raw schema in upstream's
    // `averageSchema()` shape, run through `dxdb-schema`'s defaulting (which injects
    // `_rev`, `_deleted`, `_meta`, `_attachments` into properties and required, and
    // prefixes `_deleted` to each index), then read as a `StorageSchema`.
    //
    // The first version hand-wrote the `StorageSchema` struct with `_deleted`-prefixed
    // indexes but a schema `properties` block that did not declare `_deleted`. The
    // storage rejected it: `InvalidSchema("CI1: index field `_deleted` is not in the
    // schema")`. That is the schema layer working correctly -- an index on a field the
    // schema does not allow is a bug, and hand-writing the projection is how you get
    // one. Defaulting is the supported path.
    let raw = serde_json::json!({
        "title": "averageSchema_bench",
        "version": 0,
        "keyCompression": false,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "var1": {"type": "string"},
            "var2": {"type": "number", "minimum": 0, "maximum": 50000, "multipleOf": 1},
            "deep": {
                "type": "object",
                "properties": {
                    "deep1": {"type": "string"},
                    "deep2": {"type": "string"}
                }
            },
            "list": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "deep1": {"type": "string"},
                        "deep2": {"type": "string"}
                    }
                }
            }
        },
        "required": ["id", "var1", "var2"],
        "indexes": ["var1", "var2"]
    });
    let filled = dxdb_schema::fill_with_default_settings(&raw)
        .expect("bench schema is one dxdb-schema accepts");
    StorageSchema::from_value(&filled).expect("filled schema reads as a StorageSchema")
}

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
                    "usage: bench-memory --out <file.json> [--runs N] [--docs N]\n\
                     Measures dxdb-memory over upstream's test/performance.test.ts mix,\n\
                     through the real RxStorageInstance trait."
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
    let document_bytes = bench::document(0).len();

    eprintln!(
        "bench-memory: {} docs ({} batches of {}), {} serial, {} parallel queries, {} runs, {} B/doc",
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

    for run in 0..scenario.runs {
        // A fresh database name per run: memory instances share state per
        // (database, collection, version), so reusing a name would carry run N's rows
        // into run N+1 and flatter the scans.
        let database_name = format!("bench-memory-{}-{run}", std::process::id());
        let storage = dxdb_memory::MemoryStorage::new();
        let params = InstanceCreationParams {
            database_instance_token: format!("token-{run}"),
            database_name: database_name.clone(),
            collection_name: "bench".to_string(),
            schema: schema(),
            options: serde_json::Value::Null,
            multi_instance: false,
            dev_mode: false,
        };
        let instance = futures::executor::block_on(storage.create_storage_instance(params))
            .expect("create instance");

        let docs_data: Vec<DocumentData> = (0..scenario.docs)
            .map(|i| {
                let (_id, body) = bench::bulk_document(i as u64, scenario.parallel_queries);
                let value: serde_json::Value = serde_json::from_str(&body).expect("generated JSON");
                // `_meta.lwt` and `_rev` are what the contract needs; the generator is
                // about the payload shape, so they are added here.
                let mut map = value.as_object().expect("object").clone();
                map.insert("_rev".into(), serde_json::json!("1-a"));
                map.insert("_deleted".into(), serde_json::json!(false));
                map.insert("_meta".into(), serde_json::json!({"lwt": (i as f64) + 1.0}));
                DocumentData::new(map)
            })
            .collect();

        // ---- bulk_insert: batches of docs_per_batch, one bulk_write each ----
        let per_batch = scenario.docs_per_batch();
        let ((), ms) = bench::time_ms(|| {
            for chunk in docs_data.chunks(per_batch) {
                let rows: Vec<BulkWriteRow> = chunk.iter().cloned().map(insert_row).collect();
                futures::executor::block_on(instance.bulk_write(rows, "bench".to_string()))
                    .expect("bulk_write");
            }
        });
        bulk_insert.push(ms);
        eprintln!("  run {run}: bulk_insert {ms:.1} ms");

        // ---- insert_serial: one bulk_write per document ----
        let ((), ms) = bench::time_ms(|| {
            for i in 0..scenario.serial_docs {
                let (_id, body) = bench::serial_document(1_000_000 + i as u64);
                let value: serde_json::Value = serde_json::from_str(&body).expect("JSON");
                let mut map = value.as_object().expect("object").clone();
                map.insert("_rev".into(), serde_json::json!("1-a"));
                map.insert("_deleted".into(), serde_json::json!(false));
                map.insert(
                    "_meta".into(),
                    serde_json::json!({"lwt": 1_000_000.0 + i as f64}),
                );
                let doc = DocumentData::new(map);
                futures::executor::block_on(
                    instance.bulk_write(vec![insert_row(doc)], "serial".to_string()),
                )
                .expect("serial write");
            }
        });
        insert_serial.push(ms);
        eprintln!("  run {run}: insert_serial {ms:.1} ms");

        let ids: Vec<String> = docs_data
            .iter()
            .filter_map(|d| d.id("id").ok().map(str::to_string))
            .collect();

        // ---- find_by_id_bulk: every id in one call ----
        let ((), ms) = bench::time_ms(|| {
            let found =
                futures::executor::block_on(instance.find_documents_by_id(ids.clone(), false))
                    .expect("find_by_id");
            assert_eq!(found.len(), scenario.docs, "bulk find count");
        });
        find_by_id_bulk.push(ms);
        eprintln!("  run {run}: find_by_id_bulk {ms:.1} ms");

        // ---- find_by_id_serial: one call per id ----
        let ((), ms) = bench::time_ms(|| {
            for id in ids.iter().take(scenario.serial_docs) {
                let found = futures::executor::block_on(
                    instance.find_documents_by_id(vec![id.clone()], false),
                )
                .expect("find_by_id");
                assert_eq!(found.len(), 1);
            }
        });
        find_by_id_serial.push(ms);
        eprintln!("  run {run}: find_by_id_serial {ms:.1} ms");

        // ---- query_sorted: full scan, the same sort the other benches use ----
        let ((), ms) = bench::time_ms(|| {
            let prepared = bench::sorted_query();
            let rows = futures::executor::block_on(instance.query(prepared)).expect("query");
            let expected = scenario.docs + scenario.serial_docs;
            assert_eq!(rows.len(), expected, "query count");
        });
        query_sorted.push(ms);
        eprintln!("  run {run}: query_sorted {ms:.1} ms");

        // ---- query_parallel: n selector queries ----
        let ((), ms) = bench::time_ms(|| {
            for target in 0..scenario.parallel_queries as u64 {
                let prepared = bench::selector_query("var2", target);
                let rows = futures::executor::block_on(instance.query(prepared)).expect("query");
                assert!(!rows.is_empty(), "selector matched nothing");
            }
        });
        query_parallel.push(ms);
        eprintln!("  run {run}: query_parallel {ms:.1} ms");

        // ---- count: n selector counts ----
        let ((), ms) = bench::time_ms(|| {
            for target in 0..scenario.parallel_queries as u64 {
                let prepared = bench::selector_query("var2", target);
                let result = futures::executor::block_on(instance.count(prepared)).expect("count");
                assert!(result.count > 0);
            }
        });
        count.push(ms);
        eprintln!("  run {run}: count {ms:.1} ms");

        // ---- changed_since: limit 100 from the beginning ----
        let ((), ms) = bench::time_ms(|| {
            let changed =
                futures::executor::block_on(instance.get_changed_documents_since(100, None))
                    .expect("changed_since");
            assert_eq!(changed.documents.len(), 100, "changed_since limit");
        });
        changed_since.push(ms);
        eprintln!("  run {run}: changed_since {ms:.1} ms");

        let _ = futures::executor::block_on(instance.remove());
    }

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
        storage: "memory",
        box_name: "fly open-surrealdb-ds-builder 87477e0cd01748 (8 cores, AMD EPYC, shared)",
        toolchain: option_env!("RUSTC_VERSION").unwrap_or("1.95.0").to_string(),
        idle_at_start: std::env::var("DXDB_BENCH_IDLE").as_deref() == Ok("yes"),
        idle_evidence: std::env::var("DXDB_BENCH_IDLE_EVIDENCE")
            .unwrap_or_else(|_| "not recorded".to_string()),
        date: std::env::var("DXDB_BENCH_DATE").unwrap_or_else(|_| "unknown".to_string()),
        command: std::env::var("DXDB_BENCH_COMMAND").unwrap_or_else(|_| {
            "cargo run --release --package dxdb-bench --bin bench-memory -- --out <file>"
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

    eprintln!("\nmemory, median of {} runs:", scenario.runs);
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
