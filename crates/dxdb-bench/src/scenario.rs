//! The benchmark scenario: upstream's operation mix, defined once.
//!
//! # Why this exists as a separate module
//!
//! PLAN.md 7.5 and `docs/BENCHMARKS.md` require that storages be measured over
//! **the same operation mix**, because a number measured on a different operation
//! set cannot be compared to anything -- not to another dxdb storage, and not to
//! upstream. So the data generator, the operation list and the result shape live
//! here and both storage benches are thin wrappers over it.
//!
//! # Fidelity to upstream's mix
//!
//! Modelled on `test/performance.test.ts` -> `runPerformanceTests()` in
//! `src/plugins/test-utils/performance.ts`, which is the harness that file calls.
//! Its defaults, which these mirror:
//!
//! | Parameter | Upstream default | Here |
//! | --- | --- | --- |
//! | `docsAmount` | 3000 | [`Scenario::docs`] |
//! | `insertBatches` | 6 (so 500 docs per batch) | [`Scenario::batches`] |
//! | `serialDocsAmount` | 50 | [`Scenario::serial_docs`] |
//! | `collectionsAmount` | 4 | not modelled: only collection 0 is measured |
//! | `parallelQueryAmount` | 4 | [`Scenario::parallel_queries`] |
//!
//! Upstream's `waitBetweenTests` and its `requestIdlePromise()` interleaving exist
//! to stop a browser's event loop from starving; there is no event loop here, so
//! they are deliberately not reproduced.
//!
//! # The document shape is upstream's, and it matters more than the code
//!
//! `averageSchemaData()` (`schema-objects.ts:432`) builds: an ~11-character `id`,
//! a 9-12 character `var1`, a numeric `var2` in 0..50000, a nested `deep` object
//! with two ~7-10 character strings plus `deepNr`, and a 5-element `list` of small
//! objects. That comes out around 300 bytes of JSON. Measured on 30-byte documents
//! a storage comparison says almost nothing, because per-call overhead dominates
//! and every implementation looks alike; so the generator here reproduces the
//! shape rather than using a minimal `{"id":...}`.
//!
//! # What each operation corresponds to on the storage trait
//!
//! Upstream measures through `RxCollection`; dxdb's storage layer is below that.
//! The mapping is stated so a reader can tell what is and is not covered:
//!
//! | Upstream timing key | Here | Notes |
//! | --- | --- | --- |
//! | `insert-documents-<n>` | `bulk_insert` | batched, `insert_batches` batches of `docs/batches` |
//! | `serial-inserts-<n>` | `insert_serial` | one document per call |
//! | `find-by-ids-<n>` | `find_by_id_bulk` | all ids in one call |
//! | `serial-find-by-id-<n>` | `find_by_id_serial` | one id per call |
//! | `find-by-query` | `query_sorted` | full scan, sorted by `(var2, var1)` |
//! | `find-by-query-parallel-<n>` | `query_parallel` | `n` selector queries |
//! | `4x-count` | `count` | `n` count queries |
//! | -- | `changed_since` | dxdb-only; upstream's `getChangedDocumentsSince` is not in its perf test |
//! | `property-access` | not measured | a JS document-object concern, not a storage one |
//! | `time-to-first-insert` | not measured | would measure language startup, not storage |

use std::fmt::Write as _;

/// One document in upstream's `averageSchemaData()` shape, with `var1`/`var2`
/// generated from the seed.
pub fn document(seed: u64) -> String {
    let mut probe = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    probe = probe
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let var2 = (probe >> 33) % 50_000;
    let var1 = format!("{}", seed % 2);
    document_shaped(seed, &var1, var2).1
}

/// A document in upstream's shape with explicit `var1`/`var2` values.
///
/// The rest of the fields come from the seed, so two storages are measured on
/// byte-identical input.
pub fn document_shaped(seed: u64, var1: &str, var2: u64) -> (String, String) {
    // Small LCG: deterministic, no dependency, and good enough for picking string
    // characters. Not used for anything security-relevant.
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut next = |modulo: u64| -> u64 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % modulo.max(1)
    };

    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    fn text(len: usize, next: &mut impl FnMut(u64) -> u64) -> String {
        (0..len)
            .map(|_| ALPHABET[next(ALPHABET.len() as u64) as usize] as char)
            .collect()
    }

    let id = format!("{}{}", text(8, &mut next), seed % 1000);
    let deep1 = text(8, &mut next);
    let deep2 = text(8, &mut next);
    let deep_nr = next(11);

    let mut list = String::new();
    for item in 0..5 {
        if item > 0 {
            list.push(',');
        }
        let d1 = text(4, &mut next);
        let d2 = text(6, &mut next);
        let _ = write!(list, r#"{{"deep1":"{d1}","deep2":"{d2}"}}"#);
    }

    // Field order matches upstream's object literal, which also keeps `id` first for
    // the storage key.
    let body = format!(
        r#"{{"id":"{id}","var1":"{var1}","var2":{var2},"deep":{{"deep1":"{deep1}","deep2":"{deep2}","deeper":{{"deepNr":{deep_nr}}}}},"list":[{list}]}}"#
    );
    (id, body)
}

/// A document with upstream's bulk-insert field overrides.
///
/// Upstream's bulk insert does **not** use random `var1`/`var2`: it passes
/// `averageSchemaData({ var1: (idx % 2) + '', var2: idx % parallelQueryAmount })`
/// (`performance.ts:235-241`). That is load-bearing for the mix rather than an
/// optimisation. The later `find-by-query-parallel` and `4x-count` operations select
/// `var2 == 0..parallelQueryAmount-1` and assert they matched something; with
/// uniformly random values in 0..50000 those selectors match almost nothing, and the
/// measurements would be of an empty result set.
///
/// First version of this generator used the random form for every document and the
/// parallel-query case panicked with "matched nothing". Keeping the two shapes
/// distinct here is what makes the selector operations measure real work.
pub fn bulk_document(seed: u64, parallel_queries: usize) -> (String, String) {
    let var1 = (seed % 2).to_string();
    let var2 = seed % parallel_queries.max(1) as u64;
    document_shaped(seed, &var1, var2)
}

/// A document with upstream's serial-insert override: `var2: 1000`
/// (`performance.ts:265`).
pub fn serial_document(seed: u64) -> (String, String) {
    let var1 = format!("{}", seed % 2);
    document_shaped(seed, &var1, 1000)
}

/// A document and its primary value, together.
///
/// The id comes **from the document**, never from the storage layer: in RxDB the
/// primary key is a field of the document (`primaryPath`), so a write keyed by
/// anything else is not the same operation. Getting this wrong the first time is why
/// this function exists -- the bench inserted `b0-d0000` keys while `find_by_id`
/// looked up the ids inside the documents, and the mismatch showed up as a failed
/// assertion rather than as a wrong-but-plausible number.
pub fn document_with_id(seed: u64) -> (String, String) {
    let body = document(seed);
    let start = body.find("\"id\":\"").expect("generator emits id") + 6;
    let rest = &body[start..];
    let end = rest.find('"').expect("terminated");
    (rest[..end].to_string(), body)
}

/// The ids the scenario writes, in seed order.
pub fn document_ids(scenario: &Scenario) -> Vec<String> {
    (0..scenario.docs)
        .map(|i| document_with_id(i as u64).0)
        .collect()
}

/// One operation's timings.
#[derive(Clone, Debug, Default)]
pub struct OpTimings {
    /// Operation name, matching the `docs/BENCHMARKS.md` column or an upstream
    /// timing key where one exists.
    pub op: &'static str,
    /// One entry per run, in milliseconds. The full set is kept rather than only a
    /// mean so the JSON artifact carries enough for someone to compute a different
    /// statistic (p50, p99) without re-running.
    pub samples_ms: Vec<f64>,
}

impl OpTimings {
    /// A new accumulator for one operation.
    pub fn new(op: &'static str) -> Self {
        Self {
            op,
            samples_ms: Vec::new(),
        }
    }

    /// Records one measurement.
    pub fn push(&mut self, ms: f64) {
        self.samples_ms.push(ms);
    }

    /// Mean of the samples.
    pub fn mean(&self) -> f64 {
        if self.samples_ms.is_empty() {
            return 0.0;
        }
        self.samples_ms.iter().sum::<f64>() / self.samples_ms.len() as f64
    }

    /// Median.
    pub fn median(&self) -> f64 {
        if self.samples_ms.is_empty() {
            return 0.0;
        }
        let mut sorted = self.samples_ms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        }
    }

    /// Minimum, which is the sample least contaminated by whatever else the box was
    /// doing.
    pub fn min(&self) -> f64 {
        self.samples_ms
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
            .min(f64::MAX)
    }

    /// Maximum.
    pub fn max(&self) -> f64 {
        self.samples_ms
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// Operations per second implied by the mean, for an op that does `count`
    /// operations per sample.
    pub fn per_second(&self, count: usize) -> f64 {
        let mean = self.mean();
        if mean <= 0.0 || count == 0 {
            return 0.0;
        }
        (count as f64) * 1000.0 / mean
    }
}

/// The scenario parameters. Defaults are upstream's.
#[derive(Clone, Copy, Debug)]
pub struct Scenario {
    /// Documents inserted in batches.
    pub docs: usize,
    /// Number of batches `docs` is split into (upstream: 6).
    pub batches: usize,
    /// Documents inserted one at a time.
    pub serial_docs: usize,
    /// Parallel selector queries per count/query sample.
    pub parallel_queries: usize,
    /// Runs per operation. Criterion-style repetition is overkill here because each
    /// sample already does a whole batch; this is the number of independent
    /// datasets, and its median is what gets reported.
    pub runs: usize,
}

impl Default for Scenario {
    fn default() -> Self {
        Scenario {
            docs: 3000,
            batches: 6,
            serial_docs: 50,
            parallel_queries: 4,
            runs: 5,
        }
    }
}

impl Scenario {
    /// Documents per insert batch.
    pub fn docs_per_batch(&self) -> usize {
        (self.docs / self.batches.max(1)).max(1)
    }

    /// The sorted query upstream runs: selector `{}`, sort by `(var2, var1)`.
    ///
    /// Expressed as a string for the summary table; a storage bench that can push
    /// this down should use it, and one that cannot should say so in its own module
    /// docs rather than quietly scanning.
    pub const fn sorted_query_description(&self) -> &'static str {
        "selector {} , sort [(var2,asc),(var1,asc)]"
    }
}

/// Wall-clock start, in milliseconds from the Unix epoch.
///
/// `SystemTime` rather than `Instant` because the JSON artifact records an absolute
/// timestamp that a CI diff can compare across runs; `Instant` is only meaningful
/// within one process.
pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Runs `f` and returns the elapsed milliseconds.
pub fn time_ms<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let start = std::time::Instant::now();
    let out = f();
    (out, start.elapsed().as_secs_f64() * 1000.0)
}

/// Renders a `BenchReport` as the JSON artifact CI diffs.
///
/// Hand-rolled JSON so the escaping and field order are visible here rather than
/// depending on a derive; the only free-form strings are the storage name and the
/// environment description, and both are escaped.
pub fn report_json(report: &BenchReport) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    let _ = writeln!(out, "  \"storage\": {},", json_str(&report.storage));
    let _ = writeln!(out, "  \"schema_version\": 1,");
    out.push_str("  \"environment\": {\n");
    let _ = writeln!(out, "    \"box\": {},", json_str(&report.box_name));
    let _ = writeln!(out, "    \"toolchain\": {},", json_str(&report.toolchain));
    let _ = writeln!(out, "    \"idle_at_start\": {},", report.idle_at_start);
    let _ = writeln!(
        out,
        "    \"idle_evidence\": {},",
        json_str(&report.idle_evidence)
    );
    let _ = writeln!(out, "    \"date\": {},", json_str(&report.date));
    let _ = writeln!(out, "    \"command\": {},", json_str(&report.command));
    let _ = writeln!(out, "    \"duration_ms\": {}", report.duration_ms);
    out.push_str("  },\n");
    out.push_str("  \"scenario\": {\n");
    let _ = writeln!(out, "    \"docs\": {},", report.scenario.docs);
    let _ = writeln!(out, "    \"batches\": {},", report.scenario.batches);
    let _ = writeln!(out, "    \"serial_docs\": {},", report.scenario.serial_docs);
    let _ = writeln!(
        out,
        "    \"parallel_queries\": {},",
        report.scenario.parallel_queries
    );
    let _ = writeln!(out, "    \"runs\": {},", report.scenario.runs);
    let _ = writeln!(out, "    \"document_bytes\": {}", report.document_bytes);
    out.push_str("  },\n");
    out.push_str("  \"operations\": [\n");
    for (index, op) in report.operations.iter().enumerate() {
        let timings = &op.timings;
        let comma = if index + 1 == report.operations.len() {
            ""
        } else {
            ","
        };
        let samples: Vec<String> = timings
            .samples_ms
            .iter()
            .map(|v| format!("{v:.4}"))
            .collect();
        let _ = writeln!(
            out,
            "    {{\"op\": {}, \"per_sample\": {}, \"mean_ms\": {:.4}, \"median_ms\": {:.4}, \"min_ms\": {:.4}, \"max_ms\": {:.4}, \"samples_ms\": [{}]}}{}",
            json_str(timings.op),
            op.per_sample_ops,
            timings.mean(),
            timings.median(),
            timings.min(),
            timings.max(),
            samples.join(", "),
            comma
        );
    }
    out.push_str("  ]\n}\n");
    out
}

/// One operation in a report: its timings and how many logical operations each
/// sample covered.
#[derive(Clone, Debug)]
pub struct BenchOp {
    /// The timings.
    pub timings: OpTimings,
    /// Logical operations per sample, so `ops/s` is derived rather than guessed.
    pub per_sample_ops: usize,
}

/// A whole run, ready to serialise.
#[derive(Clone, Debug)]
pub struct BenchReport {
    /// Storage under test, e.g. `filesystem-redb`.
    pub storage: &'static str,
    /// Box description.
    pub box_name: &'static str,
    /// Toolchain string.
    pub toolchain: String,
    /// Whether the box was idle when the run started.
    pub idle_at_start: bool,
    /// The evidence for that claim, so a reader can judge it.
    pub idle_evidence: String,
    /// Run date, `YYYY-MM-DD`.
    pub date: String,
    /// The command that produced this file.
    pub command: String,
    /// Total wall time.
    pub duration_ms: u128,
    /// Scenario parameters.
    pub scenario: Scenario,
    /// Size of one generated document, in bytes.
    pub document_bytes: usize,
    /// Per-operation results.
    pub operations: Vec<BenchOp>,
}

fn json_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_matches_upstreams_shape_and_size() {
        // Size is the property that makes the comparison meaningful: upstream's
        // averageSchemaData() is a few hundred bytes, not a minimal record.
        let body = document(42);
        let bytes = body.len();
        assert!(
            (200..500).contains(&bytes),
            "document is {bytes} bytes, expected upstream's few-hundred-byte shape: {body}"
        );
        assert!(body.starts_with(r#"{"id":""#));
        for field in [
            "\"var1\":",
            "\"var2\":",
            "\"deep\":",
            "\"deeper\":",
            "\"list\":[",
        ] {
            assert!(body.contains(field), "missing {field} in {body}");
        }
        // The list is 5 elements, as upstream's `new Array(5)`.
        assert_eq!(
            body.matches("\"deep1\":").count(),
            6,
            "5 list items + deep.deep1"
        );
    }

    #[test]
    fn documents_are_deterministic_and_distinct() {
        assert_eq!(
            document(7),
            document(7),
            "same seed must give the same bytes"
        );
        assert_ne!(document(7), document(8));
        // And the two storages are therefore measured on identical input.
        let a: Vec<String> = (0..10).map(document).collect();
        let b: Vec<String> = (0..10).map(document).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn the_id_comes_from_the_document_body() {
        // The invariant whose violation produced a failed find-by-id assertion: the
        // key a storage is given must be the document's own primary value.
        let (id, body) = document_with_id(11);
        assert!(
            body.contains(&format!(r#""id":"{id}""#)),
            "id {id} not in {body}"
        );
        let scenario = Scenario {
            docs: 5,
            ..Scenario::default()
        };
        let ids = document_ids(&scenario);
        for (i, expected) in ids.iter().enumerate() {
            assert_eq!(&document_with_id(i as u64).0, expected);
        }
    }

    #[test]
    fn ids_are_unique() {
        let scenario = Scenario {
            docs: 500,
            ..Scenario::default()
        };
        let ids = document_ids(&scenario);
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "duplicate ids would make find-by-id ambiguous"
        );
        assert_eq!(ids.len(), 500);
    }

    #[test]
    fn ids_extracted_from_the_body_match_the_body() {
        let ids = document_ids(&Scenario {
            docs: 3,
            ..Scenario::default()
        });
        for (i, id) in ids.iter().enumerate() {
            let body = document(i as u64);
            assert!(
                body.contains(&format!(r#""id":"{id}""#)),
                "id {id} not in body {body}"
            );
        }
    }

    #[test]
    fn statistics_are_computed_from_the_samples() {
        let mut timings = OpTimings::new("op");
        for v in [10.0, 20.0, 30.0, 40.0] {
            timings.push(v);
        }
        assert_eq!(timings.mean(), 25.0);
        assert_eq!(timings.median(), 25.0);
        assert_eq!(timings.min(), 10.0);
        assert_eq!(timings.max(), 40.0);
        // 4 ops at 25 ms mean = 160 ops/s.
        assert!((timings.per_second(4) - 160.0).abs() < 1e-9);

        let mut odd = OpTimings::new("op");
        for v in [10.0, 20.0, 30.0] {
            odd.push(v);
        }
        assert_eq!(odd.median(), 20.0, "odd count takes the middle sample");
    }

    #[test]
    fn empty_timings_do_not_panic_or_divide_by_zero() {
        let timings = OpTimings::new("op");
        assert_eq!(timings.mean(), 0.0);
        assert_eq!(timings.median(), 0.0);
        assert_eq!(timings.per_second(10), 0.0);
    }

    #[test]
    fn batches_divide_evenly_like_upstreams_3000_over_6() {
        let scenario = Scenario::default();
        assert_eq!(scenario.docs_per_batch(), 500);
        assert_eq!(scenario.docs_per_batch() * scenario.batches, scenario.docs);
    }

    #[test]
    fn the_report_is_valid_json_with_every_operation() {
        let report = BenchReport {
            storage: "test",
            box_name: "test box",
            toolchain: "1.95.0".into(),
            idle_at_start: true,
            idle_evidence: "IDLE".into(),
            date: "2026-10-26".into(),
            command: "cargo bench".into(),
            duration_ms: 1234,
            scenario: Scenario::default(),
            document_bytes: 300,
            operations: vec![BenchOp {
                timings: {
                    let mut t = OpTimings::new("bulk_insert");
                    t.push(1.5);
                    t.push(2.5);
                    t
                },
                per_sample_ops: 10,
            }],
        };
        let json = report_json(&report);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["storage"], "test");
        assert_eq!(parsed["operations"][0]["op"], "bulk_insert");
        assert_eq!(parsed["operations"][0]["median_ms"], 2.0);
        assert_eq!(parsed["operations"][0]["samples_ms"][0], 1.5);
        assert_eq!(parsed["environment"]["idle_at_start"], true);
        assert_eq!(parsed["scenario"]["docs"], 3000);
    }

    #[test]
    fn json_escapes_free_form_strings() {
        let report = BenchReport {
            storage: "a\"b\\c",
            box_name: "x\ny",
            toolchain: "t".into(),
            idle_at_start: false,
            idle_evidence: "".into(),
            date: "d".into(),
            command: "c".into(),
            duration_ms: 0,
            scenario: Scenario::default(),
            document_bytes: 0,
            operations: vec![],
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&report_json(&report)).expect("valid JSON");
        assert_eq!(parsed["storage"], "a\"b\\c");
        assert_eq!(parsed["environment"]["box"], "x\ny");
    }
}
