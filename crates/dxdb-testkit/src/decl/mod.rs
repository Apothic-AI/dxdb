//! The case declaration layer: what the conformance suite asserts, named.
//!
//! This module is the contract lane D (and every storage after it) implements
//! against. It exists because "we ported the 4,115-line suite" is a claim nobody
//! can check, while "90 declared cases, 63 exercised, 27 with these reasons" is a
//! claim a reader can audit in an afternoon.
//!
//! # Structure
//!
//! * [`Case`] — one row: id, upstream file, upstream line, title, kind, and
//!   whether it runs today.
//! * [`Kind`] — the twelve behaviours a storage is measured on. The lane brief
//!   names eleven; [`Kind::Static`] is the twelfth and is documented on its
//!   variant, because dropping the three upstream `describe` blocks that are pure
//!   query functions would understate the contract.
//! * [`PortStatus`] — why a case does or does not run. A case that does not run
//!   still has a row, and the row carries the reason.
//!
//! Both tables are generated from upstream by
//! `crates/dxdb-testkit/gen/declare_cases.py`, so their line numbers cannot drift
//! from the file they name. `make testkit-declaration-check` re-runs the generator
//! and fails on a diff.

use std::collections::BTreeMap;

mod generated;

pub use generated::{all_cases, IMPLEMENTATIONS_CASES, QUERY_CORRECTNESS_CASES};

/// The upstream file the implementations suite lives in.
pub const UPSTREAM_IMPLEMENTATIONS: &str = "test/unit/rx-storage-implementations.test.ts";
/// The upstream file the query-correctness suite lives in.
pub const UPSTREAM_QUERY_CORRECTNESS: &str = "test/unit/rx-storage-query-correctness.test.ts";
/// The upstream commit the port is pinned to (`NOTICE`).
pub const UPSTREAM_COMMIT: &str = "3b763ee17d62bc96cfae556ea0ccf92f9891c8c1";
/// The upstream release the pin corresponds to.
pub const UPSTREAM_VERSION: &str = "17.6.0";
/// Line count of the implementations suite at the pin, for the coverage tally.
pub const UPSTREAM_IMPLEMENTATIONS_LINES: usize = 4115;
/// Line count of the query-correctness suite at the pin, for the coverage tally.
pub const UPSTREAM_QUERY_CORRECTNESS_LINES: usize = 2663;

/// What a case asserts about a storage.
///
/// The eleven brief-named kinds plus [`Kind::Static`]. The mapping from an
/// upstream `describe` block to a kind is not one-to-one, and deliberately so:
/// upstream groups by *method name*, while a kind says what a storage must get
/// right. "should NOT be able to overwrite a deleted the document" lives under
/// `.bulkWrite()` upstream and is a [`Kind::Conflict`] here, because that is the
/// behaviour a storage author has to implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A brand-new document is stored and is readable at the revision written.
    Insert,
    /// `bulk_write` semantics: several rows, partial success, no cross-document
    /// transaction, per-row error list. Includes `close`/`remove` lifecycle,
    /// because those are `RxStorageInstance` methods with the same shape.
    BulkWrite,
    /// `find_documents_by_id`, in both `with_deleted` directions.
    FindByIds,
    /// `query(prepared)` returns the right documents in the right order.
    Query,
    /// `count()` agrees with `query().len()`, including with a limit.
    Count,
    /// `get_changed_documents_since` paginates the `(_meta.lwt, primary)` order
    /// without duplicating or skipping, and checkpoints resume exactly.
    ChangedSince,
    /// `cleanup` purges tombstones past the threshold and reports whether more
    /// work remains.
    Cleanup,
    /// A write whose `previous` does not match is rejected rather than applied.
    Conflict,
    /// Revision height and token behaviour: monotonic height, height jumps,
    /// `_meta`-only writes, tombstone revision updates.
    Revision,
    /// Attachment stubs and payloads: store, fetch by digest, deliver through
    /// every fetch method, drop on delete.
    Attachment,
    /// The change feed: one event per write, no empty bulks, cross-instance
    /// propagation.
    ChangeStream,
    /// A pure function of schema and query, with no storage instance involved:
    /// `prepareQuery`, `getSortComparator`, `getQueryMatcher`.
    ///
    /// **Not in the lane brief's list.** It is added because upstream's
    /// `.prepareQuery()`, `.getSortComparator()` and `.getQueryMatcher()` sections
    /// are inside the 4,115 lines and inside the contract a storage is measured
    /// by, and `dxdb-query` (lane B) provides them. Labelling them `Query` would
    /// imply a storage implements them, which is false. Their rows are
    /// [`PortStatus::NeedsQueryCrate`] rather than exercised, and this crate's
    /// job is to say so rather than to hide the gap by omission.
    Static,
}

impl Kind {
    /// Every kind, in the order the brief lists them.
    pub const ALL: [Kind; 12] = [
        Kind::Insert,
        Kind::BulkWrite,
        Kind::FindByIds,
        Kind::Query,
        Kind::Count,
        Kind::ChangedSince,
        Kind::Cleanup,
        Kind::Conflict,
        Kind::Revision,
        Kind::Attachment,
        Kind::ChangeStream,
        Kind::Static,
    ];

    /// The spelling used in the generated tables.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Insert => "Insert",
            Kind::BulkWrite => "BulkWrite",
            Kind::FindByIds => "FindByIds",
            Kind::Query => "Query",
            Kind::Count => "Count",
            Kind::ChangedSince => "ChangedSince",
            Kind::Cleanup => "Cleanup",
            Kind::Conflict => "Conflict",
            Kind::Revision => "Revision",
            Kind::Attachment => "Attachment",
            Kind::ChangeStream => "ChangeStream",
            Kind::Static => "Static",
        }
    }

    /// Parses the spelling used in the generated tables.
    pub fn parse(value: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == value)
    }
}

/// Whether a case runs against a storage today, and if not, why.
///
/// Ordered from "done" outward. A case has exactly one status — the most specific
/// blocker — so the counts in [`Coverage`] are disjoint and add up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortStatus {
    /// Runs in this crate against a `RxStorage` and would fail on a regression.
    Exercised,
    /// Declared only: the case body is not written. Used where the case needs
    /// something no crate in the workspace provides at all.
    TextOnly,
    /// The assertions call `dxdb-query` (`prepare_query`, a sort comparator, a
    /// query matcher), which is lane B's crate and is not in this workspace yet.
    /// The case's *data* is committed by this crate; only the assertion is
    /// blocked, and it unblocks by merging lane B, not by rewriting the case.
    NeedsQueryCrate,
    /// Needs a storage that shares state between two instances on one collection
    /// (`multi_instance: true`) and forwards change events between them. The
    /// upstream cases support this through `rx-storage-multiinstance.ts`; there is
    /// no such layer in this workspace yet, so the cases are declared and their
    /// mechanics are documented on the storage they will run against.
    NeedsMultiInstance,
    /// Needs the encryption wrapper: upstream asserts that a plain storage
    /// *rejects* a schema with `encrypted` fields only when the wrapper is absent,
    /// which is a property of the wrapper stack.
    NeedsEncryptionWrapper,
    /// Needs the key-compression wrapper (`jsonschema-key-compression`'s
    /// replacement) for the same reason.
    NeedsKeyCompressionWrapper,
    /// Needs a runtime capability the default build does not have.
    NeedsLoginFeature,
}

impl PortStatus {
    /// The spelling used in the generated tables.
    pub fn as_str(self) -> &'static str {
        match self {
            PortStatus::Exercised => "Exercised",
            PortStatus::TextOnly => "TextOnly",
            PortStatus::NeedsQueryCrate => "NeedsQueryCrate",
            PortStatus::NeedsMultiInstance => "NeedsMultiInstance",
            PortStatus::NeedsEncryptionWrapper => "NeedsEncryptionWrapper",
            PortStatus::NeedsKeyCompressionWrapper => "NeedsKeyCompressionWrapper",
            PortStatus::NeedsLoginFeature => "NeedsLoginFeature",
        }
    }

    /// True when the case runs today.
    pub fn is_exercised(self) -> bool {
        matches!(self, PortStatus::Exercised)
    }
}

/// One declared case.
///
/// `upstream_file` and `upstream_line` are always present and always derived from
/// the upstream source by the generator. There is no constructor that lets a case
/// exist without provenance: a test nobody can check against the original is worse
/// than no test, because it looks like coverage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Case {
    /// Stable identifier, e.g. `impl-conflict-01` or `qc-07`.
    pub id: &'static str,
    /// Upstream path relative to the RxDB checkout root.
    pub upstream_file: &'static str,
    /// 1-based line in `upstream_file` that declares the case (`it(` for the
    /// implementations suite; the `testTitle` for the correctness suite).
    pub upstream_line: u32,
    /// The upstream title, verbatim.
    pub title: &'static str,
    /// The behaviour asserted, as a [`Kind`] spelling.
    pub kind: &'static str,
    /// The enclosing upstream `describe`, for orientation.
    pub section: &'static str,
    /// Whether it runs, and why not.
    pub status: PortStatus,
    /// Why, in one sentence. Always present, including for exercised cases, so a
    /// reader can tell what the case actually checks without opening upstream.
    pub note: &'static str,
    /// Number of queries in a query-correctness group; `0` for an implementations
    /// case. Kept as data rather than as an estimate so the "89 queries" total is
    /// a sum over the table, not a number in a report.
    pub queries: u32,
}

impl Case {
    /// The kind as an enum. Panics on an unknown string, which can only happen if
    /// the generator and [`Kind`] disagree — a build-time bug, not a data
    /// condition.
    pub fn kind_enum(&self) -> Kind {
        Kind::parse(self.kind).unwrap_or_else(|| panic!("unknown kind {:?} in case {}", self.kind, self.id))
    }

    /// `test/unit/….test.ts:376`, the citation form the lane brief requires.
    pub fn citation(&self) -> String {
        format!("{}:{}", self.upstream_file, self.upstream_line)
    }
}

/// Counts by kind and by status, for an honest report.
///
/// `declared` and `exercised` are separate maps rather than one map of pairs, so
/// the report can print both columns per kind without a second pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Cases per kind, exercised or not.
    pub declared: BTreeMap<Kind, usize>,
    /// Cases per kind that run today.
    pub exercised: BTreeMap<Kind, usize>,
    /// Cases per status, across all kinds.
    pub by_status: BTreeMap<&'static str, usize>,
    /// Total declared.
    pub total_declared: usize,
    /// Total exercised.
    pub total_exercised: usize,
}

impl Coverage {
    /// Computes coverage over any iterator of cases.
    pub fn of<'a>(cases: impl Iterator<Item = &'a Case>) -> Coverage {
        let mut coverage = Coverage::default();
        for case in cases {
            let kind = case.kind_enum();
            coverage.total_declared += 1;
            *coverage.declared.entry(kind).or_default() += 1;
            if case.status.is_exercised() {
                coverage.total_exercised += 1;
                *coverage.exercised.entry(kind).or_default() += 1;
            }
            *coverage.by_status.entry(case.status.as_str()).or_default() += 1;
        }
        coverage
    }

    /// Coverage for the implementations suite.
    pub fn implementations() -> Coverage {
        Coverage::of(IMPLEMENTATIONS_CASES.iter())
    }

    /// Coverage for the query-correctness suite.
    pub fn query_correctness() -> Coverage {
        Coverage::of(QUERY_CORRECTNESS_CASES.iter())
    }

    /// Coverage over both suites.
    pub fn both() -> Coverage {
        Coverage::of(all_cases())
    }

    /// Exercised as a fraction of declared, in percent. `0.0` when nothing is
    /// declared, rather than a NaN that would print in a report.
    pub fn exercised_percent(&self) -> f64 {
        if self.total_declared == 0 {
            0.0
        } else {
            (self.total_exercised as f64 / self.total_declared as f64) * 100.0
        }
    }
}

/// The `kind` totals of the declaration table, as a fixed array.
///
/// Exists so a report can print a stable, ordered table without sorting a map, and
/// so the table's shape is reviewable in one place.
pub fn totals_by_kind() -> Vec<(Kind, usize, usize)> {
    let coverage = Coverage::both();
    Kind::ALL
        .into_iter()
        .map(|kind| {
            let declared = coverage.declared.get(&kind).copied().unwrap_or(0);
            let exercised = coverage.exercised.get(&kind).copied().unwrap_or(0);
            (kind, declared, exercised)
        })
        .collect()
}

/// Every case marked [`PortStatus::Exercised`], in declaration order.
///
/// The suite dispatches on `upstream_line` from this list rather than on a
/// hand-maintained array, so "exercised" cannot drift from the code that runs.
pub fn exercised_cases() -> impl Iterator<Item = &'static Case> {
    all_cases().filter(|case| case.status.is_exercised())
}

/// The cases that do not run, grouped by status, for the report's "what I could
/// not port" section.
pub fn unexercised_by_status() -> BTreeMap<&'static str, Vec<&'static Case>> {
    let mut out: BTreeMap<&'static str, Vec<&'static Case>> = BTreeMap::new();
    for case in all_cases() {
        if !case.status.is_exercised() {
            out.entry(case.status.as_str()).or_default().push(case);
        }
    }
    out
}
