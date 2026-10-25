//! The coverage tally, as data rather than as prose in a report.
//!
//! A lane reports "N of M exercised, here are the reasons". This module computes
//! that number from the declaration table so the report cannot round up, and it
//! computes the *line* tally too, which is the number that is easiest to inflate and
//! hardest to check.
//!
//! # The two tallies, and why the second one is the honest one
//!
//! **Case coverage** counts declared cases against exercised ones. It is the number
//! a reader can audit by opening the table.
//!
//! **Line coverage** counts upstream lines against lines this crate actually
//! accounts for. It is deliberately computed by *subtraction* from the upstream line
//! count, using ranges measured from the upstream files, and every subtracted range
//! is named. That means a line this crate does not account for is a line the report
//! says it does not account for, rather than a line quietly inside a total.
//!
//! The query-correctness suite makes the distinction concrete: it is 2,663 lines of
//! which 2,279 are group *data*. All 2,279 are accounted for — the data is
//! extracted, committed and asserted against, and a wrong expected-id list fails a
//! case. But only the 265-line harness and the 89 queries' assertions are code this
//! crate wrote. Reporting "2,663 lines ported" without that split would be true in a
//! way that misleads, so both numbers are reported.

use crate::decl::{
    all_cases, Coverage, Kind, PortStatus, IMPLEMENTATIONS_CASES, QUERY_CORRECTNESS_CASES,
    UPSTREAM_IMPLEMENTATIONS_LINES, UPSTREAM_QUERY_CORRECTNESS_LINES,
};

/// One upstream line range, with where it went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRange {
    /// First line, 1-based, inclusive.
    pub from: u32,
    /// Last line, 1-based, inclusive.
    pub to: u32,
    /// What this range is, and what happened to it.
    pub what: &'static str,
    /// How this crate accounts for it.
    pub accounting: Accounting,
}

/// How a range of upstream lines is accounted for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accounting {
    /// Ported to executable Rust that runs today.
    PortedAndExercised,
    /// The lines are *data* that this crate extracted, committed and asserts
    /// against, but they are not code this crate wrote.
    ExtractedData,
    /// Ported, but the assertion is blocked on another lane's crate.
    PortedPendingDependency,
    /// Not ported; declared in the table with a reason.
    DeclaredNotPorted,
    /// Boilerplate that has no Rust equivalent: imports, the mocha wrapper, closing
    /// braces. Named so the subtraction is explicable rather than a fudge factor.
    Scaffolding,
}

/// A line tally for one suite.
#[derive(Clone, Debug)]
pub struct LineTally {
    /// Upstream file.
    pub file: &'static str,
    /// Upstream line count at the pin.
    pub upstream_lines: usize,
    /// The ranges this crate accounts for, in order, non-overlapping.
    pub ranges: &'static [LineRange],
}

impl LineTally {
    /// Lines per accounting category.
    pub fn by_accounting(&self) -> Vec<(Accounting, u32)> {
        let mut out: Vec<(Accounting, u32)> = Vec::new();
        for accounting in [
            Accounting::PortedAndExercised,
            Accounting::ExtractedData,
            Accounting::PortedPendingDependency,
            Accounting::DeclaredNotPorted,
            Accounting::Scaffolding,
        ] {
            let total: u32 = self
                .ranges
                .iter()
                .filter(|range| range.accounting == accounting)
                .map(|range| range.to - range.from + 1)
                .sum();
            out.push((accounting, total));
        }
        out
    }

    /// Total lines accounted for.
    pub fn accounted(&self) -> u32 {
        self.by_accounting().iter().map(|(_, n)| *n).sum()
    }

    /// Total lines of *code* this crate wrote that runs: ported and exercised, plus
    /// ported and pending a dependency. Data is excluded, because calling extracted
    /// data "ported code" is the specific inflation this tally exists to avoid.
    pub fn code_lines(&self) -> u32 {
        self.by_accounting()
            .iter()
            .filter(|(accounting, _)| {
                matches!(
                    accounting,
                    Accounting::PortedAndExercised | Accounting::PortedPendingDependency
                )
            })
            .map(|(_, n)| *n)
            .sum()
    }

    /// True when every upstream line is inside exactly one declared range. A false
    /// here means the tally has a gap or an overlap and its numbers do not add up,
    /// which is a bug in this module rather than in a storage.
    pub fn is_partition(&self) -> bool {
        let mut sorted: Vec<LineRange> = self.ranges.to_vec();
        sorted.sort_by_key(|range| range.from);
        let mut expected = 1u32;
        for range in &sorted {
            if range.from != expected {
                return false;
            }
            if range.to < range.from {
                return false;
            }
            expected = range.to + 1;
        }
        expected == self.upstream_lines as u32 + 1
    }

    /// The accountings of any line this tally does not cover, as `(from, to)` runs.
    /// Empty when [`LineTally::is_partition`].
    pub fn gaps(&self) -> Vec<(u32, u32)> {
        let mut sorted: Vec<LineRange> = self.ranges.to_vec();
        sorted.sort_by_key(|range| range.from);
        let mut gaps = Vec::new();
        let mut expected = 1u32;
        for range in sorted {
            if range.from > expected {
                gaps.push((expected, range.from - 1));
            }
            if range.to >= expected {
                expected = range.to + 1;
            }
        }
        if expected <= self.upstream_lines as u32 {
            gaps.push((expected, self.upstream_lines as u32));
        }
        gaps
    }
}

/// The implementations suite's line tally.
///
/// The ranges come from the upstream file's structure, measured at the pin:
///
/// * 1-63: imports and `declare type` aliases.
/// * 65-112: the four document helpers (`getWriteData`, `getTestDataSchema`,
///   `getNestedDocSchema`, `testContext`), ported to
///   [`crate::fixtures::write`] and [`crate::schemas`].
/// * 173: the outer `describe`. The mocha wrapper has no Rust equivalent.
/// * 174-175, 269, 1179, 1215, 1382, 1511, 2122, 2183, 2461, 2695, 2927, 3368,
///   3605, 3632, 3708, 3756, 4045: `describe` lines.
/// * Each `it(` body: see [`IMPLEMENTATIONS_BODIES`].
pub const IMPLEMENTATIONS_TALLY: LineTally = LineTally {
    file: crate::decl::UPSTREAM_IMPLEMENTATIONS,
    upstream_lines: UPSTREAM_IMPLEMENTATIONS_LINES,
    ranges: IMPLEMENTATIONS_RANGES,
};

/// The implementations suite's ranges.
const IMPLEMENTATIONS_RANGES: &[LineRange] = &[
    LineRange {
        from: 1,
        to: 63,
        what: "imports and `declare type` aliases",
        accounting: Accounting::Scaffolding,
    },
    LineRange {
        from: 64,
        to: 112,
        what: "getWriteData / getTestDataSchema / getNestedDocSchema / testContext",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 113,
        to: 172,
        what: "addRxPlugin, the type declarations and the outer describe's first statement",
        accounting: Accounting::Scaffolding,
    },
    LineRange {
        from: 173,
        to: 175,
        what: "outer describe + `describe('RxStorageInstance')` + `describe('creation')`",
        accounting: Accounting::Scaffolding,
    },
    LineRange {
        from: 176,
        to: 193,
        what: "`open and close`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 194,
        to: 217,
        what: "`open many instances on the same database name`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 218,
        to: 243,
        what: "`must throw if keyCompression is set but no key-compression plugin is used`",
        accounting: Accounting::DeclaredNotPorted,
    },
    LineRange {
        from: 244,
        to: 268,
        what: "`must throw if encryption is defined in schema but no encryption plugin is used`",
        accounting: Accounting::DeclaredNotPorted,
    },
    LineRange {
        from: 269,
        to: 421,
        what: "`.bulkWrite()`: write, conflict, concurrent-insert",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 422,
        to: 1178,
        what: "`.bulkWrite()`: deleted docs, meta-only writes, umlauts, #9145",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 1179,
        to: 1214,
        what: "`.prepareQuery()`: must not crash / must not mutate (needs dxdb-query)",
        accounting: Accounting::PortedPendingDependency,
    },
    LineRange {
        from: 1215,
        to: 1381,
        what: "`.getSortComparator()` (needs dxdb-query)",
        accounting: Accounting::PortedPendingDependency,
    },
    LineRange {
        from: 1382,
        to: 1510,
        what: "`.getQueryMatcher()` (needs dxdb-query)",
        accounting: Accounting::PortedPendingDependency,
    },
    LineRange {
        from: 1511,
        to: 2121,
        what: "`.query()`: ordering, deleted docs, skip+limit, nested, bulk load",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 2122,
        to: 2182,
        what: "`.count()`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 2183,
        to: 2460,
        what: "`.findDocumentsById()`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 2461,
        to: 2694,
        what: "`.getChangedDocumentsSince()`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 2695,
        to: 2926,
        what: "`.changeStream()`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 2927,
        to: 3367,
        what: "`attachments`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 3368,
        to: 3604,
        what: "`.cleanup`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 3605,
        to: 3631,
        what: "`.close()`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 3632,
        to: 3707,
        what: "`.remove()`",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 3708,
        to: 3756,
        what: "`multiInstance` helpers (`getMultiInstanceRxStorageInstance`)",
        accounting: Accounting::DeclaredNotPorted,
    },
    LineRange {
        from: 3757,
        to: 4044,
        what: "`multiInstance` cases (need a multi-instance forwarding layer)",
        accounting: Accounting::DeclaredNotPorted,
    },
    LineRange {
        from: 4045,
        to: 4115,
        what: "`migration`",
        accounting: Accounting::PortedAndExercised,
    },
];

/// The query-correctness suite's line tally.
///
/// The split that matters: 2,279 lines are group data, all of which this crate
/// extracted, committed and asserts against; 313 lines are the `it` harness that runs
/// them; 70 lines are imports. Only the harness is code this crate wrote, and only
/// part of it can run without `dxdb-query`, which is why the harness is split.
pub const QUERY_CORRECTNESS_TALLY: LineTally = LineTally {
    file: crate::decl::UPSTREAM_QUERY_CORRECTNESS,
    upstream_lines: UPSTREAM_QUERY_CORRECTNESS_LINES,
    ranges: QUERY_CORRECTNESS_RANGES,
};

/// The query-correctness suite's ranges.
const QUERY_CORRECTNESS_RANGES: &[LineRange] = &[
    LineRange {
        from: 1,
        to: 40,
        what: "imports",
        accounting: Accounting::Scaffolding,
    },
    LineRange {
        from: 41,
        to: 45,
        what: "TEST_CONTEXT and the four RESULT_WINDOW_BUFFER_VARIANTS",
        accounting: Accounting::ExtractedData,
    },
    LineRange {
        from: 46,
        to: 70,
        what: "the variant table's type",
        accounting: Accounting::Scaffolding,
    },
    LineRange {
        from: 71,
        to: 107,
        what: "outer describe, TestCorrectQueriesInput type, withIndexes",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 108,
        to: 119,
        what: "testCorrectQueries: the notRunIfTrue gate",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 120,
        to: 135,
        what: "build one storage instance and insert the group's data",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 136,
        to: 152,
        what: "assert the write produced no errors",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 153,
        to: 185,
        what: "create the four RxDatabase variants (event-reduce buffer sizes)",
        accounting: Accounting::DeclaredNotPorted,
    },
    LineRange {
        from: 186,
        to: 203,
        what: "getStaticsResultIds: the in-memory expected result",
        accounting: Accounting::PortedPendingDependency,
    },
    LineRange {
        from: 204,
        to: 246,
        what: "the per-query loop: normalise, add `_deleted`, prepare",
        accounting: Accounting::PortedPendingDependency,
    },
    LineRange {
        from: 247,
        to: 260,
        what: "assert selectorSatisfiedByIndex against the query plan",
        accounting: Accounting::PortedPendingDependency,
    },
    LineRange {
        from: 261,
        to: 296,
        what: "assert storageInstance.query() against expectedResultDocIds",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 297,
        to: 320,
        what: "assert RxCollection.find() and findByIds() (needs dxdb-core)",
        accounting: Accounting::DeclaredNotPorted,
    },
    LineRange {
        from: 321,
        to: 340,
        what: "assert .count() when the query has no skip/limit",
        accounting: Accounting::PortedAndExercised,
    },
    LineRange {
        from: 341,
        to: 381,
        what: "the live-query remove/re-insert cycle (needs dxdb-core event-reduce)",
        accounting: Accounting::DeclaredNotPorted,
    },
    LineRange {
        from: 382,
        to: 2663,
        what: "25 group calls: schemas, data and 89 queries",
        accounting: Accounting::ExtractedData,
    },
];

/// Everything a report needs, computed.
#[derive(Clone, Debug)]
pub struct Report {
    /// Case coverage over both suites.
    pub overall: Coverage,
    /// Case coverage of the implementations suite.
    pub implementations: Coverage,
    /// Case coverage of the query-correctness suite.
    pub query_correctness: Coverage,
    /// Per-kind declared and exercised totals.
    pub by_kind: Vec<(Kind, usize, usize)>,
    /// Line tally of the implementations suite.
    pub implementations_lines: LineTally,
    /// Line tally of the query-correctness suite.
    pub query_correctness_lines: LineTally,
    /// Queries declared inside the query-correctness groups.
    pub queries_declared: usize,
}

impl Report {
    /// Computes the report from the declaration table.
    pub fn compute() -> Report {
        let by_kind = crate::decl::totals_by_kind();
        Report {
            overall: Coverage::both(),
            implementations: Coverage::implementations(),
            query_correctness: Coverage::query_correctness(),
            by_kind,
            implementations_lines: IMPLEMENTATIONS_TALLY,
            query_correctness_lines: QUERY_CORRECTNESS_TALLY,
            queries_declared: QUERY_CORRECTNESS_CASES
                .iter()
                .map(|case| case.queries as usize)
                .sum(),
        }
    }

    /// The cases that do not run, grouped by status.
    pub fn unexercised(&self) -> std::collections::BTreeMap<&'static str, Vec<&'static crate::decl::Case>> {
        crate::decl::unexercised_by_status()
    }

    /// A Markdown rendering, in the shape a lane reports in.
    pub fn to_markdown(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();

        out.push_str("### Declaration table by kind\n\n");
        out.push_str("| kind | declared | exercised | not exercised |\n");
        out.push_str("| --- | ---: | ---: | ---: |\n");
        for (kind, declared, exercised) in &self.by_kind {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                kind.as_str(),
                declared,
                exercised,
                declared - exercised
            );
        }
        let _ = writeln!(
            out,
            "| **total** | **{}** | **{}** | **{}** |",
            self.overall.total_declared,
            self.overall.total_exercised,
            self.overall.total_declared - self.overall.total_exercised
        );

        out.push_str("\n### Declared versus exercised, per suite\n\n");
        let _ = writeln!(
            out,
            "| suite | cases declared | exercised | % |\n| --- | ---: | ---: | ---: |"
        );
        let _ = writeln!(
            out,
            "| implementations | {} | {} | {:.1}% |",
            self.implementations.total_declared,
            self.implementations.total_exercised,
            self.implementations.exercised_percent()
        );
        let _ = writeln!(
            out,
            "| query correctness | {} | {} | {:.1}% |",
            self.query_correctness.total_declared,
            self.query_correctness.total_exercised,
            self.query_correctness.exercised_percent()
        );
        let _ = writeln!(
            out,
            "\nThe query-correctness count is *groups*, not queries: the {} groups hold {} queries between them, all of which are extracted and asserted against.",
            self.query_correctness.total_declared, self.queries_declared
        );

        out.push_str("\n### Why cases do not run\n\n");
        out.push_str("| status | count | meaning |\n| --- | ---: | --- |\n");
        for (status, cases) in self.unexercised() {
            let meaning = match status {
                "NeedsQueryCrate" => "assertion calls `dxdb-query` (lane B)",
                "NeedsMultiInstance" => "needs a multi-instance forwarding layer",
                "NeedsEncryptionWrapper" => "asserts the encryption wrapper's refusal",
                "NeedsKeyCompressionWrapper" => "asserts the key-compression wrapper's refusal",
                "TextOnly" => "declared only, reason recorded per case",
                _ => "",
            };
            let _ = writeln!(out, "| `{status}` | {} | {meaning} |", cases.len());
        }

        out.push_str("\n### Line tally\n\n");
        for tally in [&self.implementations_lines, &self.query_correctness_lines] {
            let _ = writeln!(
                out,
                "**{name}** — {upstream} upstream lines; {ported} lines are code this crate wrote, of which {exercised} runs and {pending} is pending a dependency. {data} lines are fixture data, extracted and asserted against.",
                name = if tally.file.contains("query-correctness") {
                    "query correctness"
                } else {
                    "implementations"
                },
                upstream = tally.upstream_lines,
                ported = tally.code_lines(),
                exercised = tally
                    .by_accounting()
                    .iter()
                    .find(|(a, _)| *a == Accounting::PortedAndExercised)
                    .map(|(_, n)| *n)
                    .unwrap_or(0),
                pending = tally
                    .by_accounting()
                    .iter()
                    .find(|(a, _)| *a == Accounting::PortedPendingDependency)
                    .map(|(_, n)| *n)
                    .unwrap_or(0),
                data = tally
                    .by_accounting()
                    .iter()
                    .find(|(a, _)| *a == Accounting::ExtractedData)
                    .map(|(_, n)| *n)
                    .unwrap_or(0),
            );
            if let Some((from, to)) = tally.gaps().first() {
                let _ = writeln!(out, "\n> **gap:** lines {from}-{to} are not accounted for.");
            }
            out.push('\n');
        }
        out
    }
}

/// Asserts the declaration table is internally consistent, returning what is wrong.
///
/// Called by a test rather than by the library, so a broken table fails `make test`
/// instead of producing a report nobody should trust.
pub fn validate() -> Vec<String> {
    let mut problems = Vec::new();

    for case in all_cases() {
        if Kind::parse(case.kind).is_none() {
            problems.push(format!("{}: unknown kind {:?}", case.id, case.kind));
        }
        if case.upstream_line == 0 {
            problems.push(format!("{}: no upstream line", case.id));
        }
        if !case.upstream_file.starts_with("test/unit/") {
            problems.push(format!(
                "{}: upstream file is not a test/unit path: {}",
                case.id, case.upstream_file
            ));
        }
        if case.note.trim().is_empty() {
            problems.push(format!("{}: no note explaining the case", case.id));
        }
    }

    // Ids must be unique; a duplicate would make a report ambiguous.
    let mut ids: Vec<&str> = all_cases().map(|case| case.id).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    if ids.len() != before {
        problems.push("duplicate case ids".to_string());
    }

    // Upstream lines must be unique within a file.
    for suite in [IMPLEMENTATIONS_CASES, QUERY_CORRECTNESS_CASES] {
        let mut lines: Vec<u32> = suite.iter().map(|case| case.upstream_line).collect();
        lines.sort_unstable();
        let before = lines.len();
        lines.dedup();
        if lines.len() != before {
            problems.push("two cases claim the same upstream line".to_string());
        }
    }

    // Line tallies must partition the upstream files, or their totals are fiction.
    for tally in [&IMPLEMENTATIONS_TALLY, &QUERY_CORRECTNESS_TALLY] {
        if !tally.is_partition() {
            let gaps = tally.gaps();
            problems.push(format!(
                "{}: line tally is not a partition (gaps: {:?})",
                tally.file, gaps
            ));
        }
        if tally.accounted() as usize != tally.upstream_lines {
            problems.push(format!(
                "{}: tally accounts for {} of {} lines",
                tally.file,
                tally.accounted(),
                tally.upstream_lines
            ));
        }
    }

    // The correctness groups' query count must match the declared total.
    let group_queries: usize = crate::fixtures::correctness::groups()
        .iter()
        .map(|group| group.queries.len())
        .sum();
    let declared_queries: usize = QUERY_CORRECTNESS_CASES.iter().map(|c| c.queries as usize).sum();
    if group_queries != declared_queries {
        problems.push(format!(
            "correctness data has {group_queries} queries but the table declares {declared_queries}"
        ));
    }
    if crate::fixtures::correctness::groups().len() != QUERY_CORRECTNESS_CASES.len() {
        problems.push(format!(
            "correctness data has {} groups but the table declares {}",
            crate::fixtures::correctness::groups().len(),
            QUERY_CORRECTNESS_CASES.len()
        ));
    }

    problems
}

/// The statuses a case can have, for a test that proves each one is reachable.
pub const ALL_STATUSES: [PortStatus; 7] = [
    PortStatus::Exercised,
    PortStatus::TextOnly,
    PortStatus::NeedsQueryCrate,
    PortStatus::NeedsMultiInstance,
    PortStatus::NeedsEncryptionWrapper,
    PortStatus::NeedsKeyCompressionWrapper,
    PortStatus::NeedsLoginFeature,
];
