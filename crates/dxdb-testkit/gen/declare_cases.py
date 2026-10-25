#!/usr/bin/env python3
"""Generate the testkit's case declaration tables from upstream RxDB.

This is the tool that makes the declaration honest. It reads the two upstream
suites, extracts every `it(...)` (implementations) and every `testCorrectQueries`
group and query (correctness), and emits Rust tables that name the upstream file
and line each case came from.

Two properties matter more than convenience:

*   **Provenance is derived, never typed.** A line number in the emitted table
    comes from locating the construct in the source at generation time, so it
    cannot drift from the file it claims to describe. `--check` re-runs the
    generator and fails if the committed tables differ.
*   **Nothing is silently dropped.** Every construct the extractor recognises is
    emitted. When a construct is recognised but cannot be expressed as an
    executable case (for example, it needs `dxdb-query`'s planner, which is
    another lane's crate), it is still emitted, with a `PortStatus` that says so
    and a reason string. An unported case is a row in the table, not a gap.

Usage:

    python3 crates/dxdb-testkit/gen/declare_cases.py            # rewrite tables
    python3 crates/dxdb-testkit/gen/declare_cases.py --check     # verify no drift

The upstream checkout is `$RXDB_UPSTREAM`, defaulting to `~/Code/rxdb`, matching
the Makefile's convention so CI can point at a shallow clone.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

# ---------------------------------------------------------------------------
# Upstream locations
# ---------------------------------------------------------------------------

IMPL_REL = "test/unit/rx-storage-implementations.test.ts"
QC_REL = "test/unit/rx-storage-query-correctness.test.ts"


def upstream_root() -> Path:
    return Path(os.environ.get("RXDB_UPSTREAM", "~/.rxdb-unset")).expanduser() if os.environ.get(
        "RXDB_UPSTREAM"
    ) else Path("~/Code/rxdb").expanduser()


# ---------------------------------------------------------------------------
# JavaScript literal parsing (shared with the fixture extractor)
# ---------------------------------------------------------------------------

sys.path.insert(0, str(Path(__file__).resolve().parent))
from jslit import parse, Unsupported  # noqa: E402


def strip_line(line: str) -> str:
    s = re.sub(r"'(\\.|[^'\\])*'", "''", line)
    s = re.sub(r'"(\\.|[^"\\])*"', '""', s)
    s = re.sub(r"`(\\.|[^`\\])*`", "``", s)
    s = re.sub(r"//.*", "", s)
    return s


def brace_depth_map(lines: list[str]) -> list[int]:
    nets = []
    for line in lines:
        s = strip_line(line)
        nets.append(
            s.count("{") - s.count("}") + s.count("[") - s.count("]") + s.count("(") - s.count(")")
        )
    depth = 0
    out = []
    for n in nets:
        out.append(depth)
        depth += n
    return out, nets


def find_balanced(text: str, start: int, opener: str, closer: str) -> int:
    depth = 0
    instr = None
    i = start
    esc = False
    while i < len(text):
        c = text[i]
        if instr:
            if esc:
                esc = False
            elif c == "\\":
                esc = True
            elif c == instr:
                instr = None
        elif c in "'\"":
            instr = c
        elif c == "`":
            j = text.find("`", i + 1)
            i = len(text) if j < 0 else j
        elif c == "/" and i + 1 < len(text) and text[i + 1] == "/":
            j = text.find("\n", i)
            i = len(text) if j < 0 else j
            continue
        elif c == "/" and i + 1 < len(text) and text[i + 1] == "*":
            j = text.find("*/", i)
            i = len(text) if j < 0 else j + 1
            continue
        elif c == opener:
            depth += 1
        elif c == closer:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise Unsupported("unbalanced %r" % (opener,))


# ---------------------------------------------------------------------------
# The declaration model
# ---------------------------------------------------------------------------

#: The `kind` vocabulary from the lane brief, plus one addition.
#:
#: `Static` is the addition, and it is not a fudge: upstream's
#: `.prepareQuery()`, `.getSortComparator()` and `.getQueryMatcher()` sections are
#: pure functions over a schema and a query, with no storage instance involved.
#: They are part of the conformance suite the storage crates are measured by, so
#: dropping them would understate the contract, but calling them `Query` would
#: wrongly imply a storage implements them. `Static` says what they are: the
#: in-memory halves of the query contract that `dxdb-query` provides (lane B).
KINDS = [
    "Insert",
    "BulkWrite",
    "FindByIds",
    "Query",
    "Count",
    "ChangedSince",
    "Cleanup",
    "Conflict",
    "Revision",
    "Attachment",
    "ChangeStream",
    "Static",
]

#: Why a case is not executable in this crate today.
#:
#: Ordered from "nothing to do" to "blocked on another lane". A case gets exactly
#: one status, the most specific blocker, so the summary counts are disjoint.
class PortStatus:
    EXERCISED = "Exercised"
    TEXT_ONLY = "TextOnly"
    NEEDS_QUERY_CRATE = "NeedsQueryCrate"
    NEEDS_MULTI_INSTANCE = "NeedsMultiInstance"
    NEEDS_ENCRYPTION_WRAPPER = "NeedsEncryptionWrapper"
    NEEDS_KEY_COMPRESSION_WRAPPER = "NeedsKeyCompressionWrapper"
    NEEDS_LOGIN_FEATURE = "NeedsLoginFeature"


# ---------------------------------------------------------------------------
# Implementations suite: one case per `it(...)`
# ---------------------------------------------------------------------------

IT_RE = re.compile(r"^([ \t]*)(describe|it)\(\s*'((?:\\.|[^'\\])*)'")
IT_RE_BACKTICK = re.compile(r"^([ \t]*)(describe|it)\(\s*`([^`]*)`")

#: Hand-assigned metadata for the 65 `it` cases, keyed by upstream line.
#:
#: `kind` and `status` are judgements, so they are written out rather than
#: inferred: a regex cannot tell "writes a tombstone then updates it" (a
#: `Revision` case) from "writes a document" (a `BulkWrite` case), and guessing
#: would make the table wrong in a way that looks authoritative. Every entry's
#: line number is *not* written here — it is matched in by the generator, so a
#: wrong line cannot survive generation.
#:
#: `exercised` is honest: `True` only where this crate runs the case against a
#: storage through the traits and would fail if the storage regressed.
IMPL_META: dict[int, dict] = {
    # -- creation -----------------------------------------------------------
    176: dict(kind="BulkWrite", status=PortStatus.EXERCISED,
              note="open+remove; asserts collectionName/databaseName are not mutated by creation"),
    195: dict(kind="BulkWrite", status=PortStatus.EXERCISED,
              note="20 instances on one database name, all removed"),
    218: dict(kind="BulkWrite", status=PortStatus.NEEDS_KEY_COMPRESSION_WRAPPER,
              note="requires create_storage_instance to reject keyCompression: true; "
                   "the rejection is the wrapper's job, the storage only has the flag"),
    244: dict(kind="BulkWrite", status=PortStatus.NEEDS_ENCRYPTION_WRAPPER,
              note="requires create_storage_instance to reject a schema with `encrypted` fields"),
    # -- .bulkWrite() ------------------------------------------------------
    # `Kind::Insert` rather than `BulkWrite`: this is the single-row degenerate case,
    # and it is the *only* place upstream exercises "insert one document".
    #
    # RxDB has no `insert()` method on a storage at all — every write is `bulkWrite`,
    # even for one row — which is why the brief's `Insert` kind has no upstream
    # `describe` block of its own. Classifying this case as `Insert` gives the kind a
    # real, exercised home instead of inventing one or leaving it empty.
    270: dict(kind="Insert", status=PortStatus.EXERCISED,
              note="single-row insert: write one doc, read it back through findDocumentsById"),
    309: dict(kind="Conflict", status=PortStatus.EXERCISED,
              note="second write of the same primary without previous -> 409"),
    372: dict(kind="Conflict", status=PortStatus.EXERCISED,
              note="two concurrent inserts of one primary: exactly one succeeds"),
    420: dict(kind="FindByIds", status=PortStatus.EXERCISED,
              note="with_deleted=false hides the tombstone"),
    483: dict(kind="Conflict", status=PortStatus.EXERCISED,
              note="cannot overwrite a deleted document without passing previous"),
    600: dict(kind="Revision", status=PortStatus.EXERCISED,
              note="a tombstone can be updated to a new revision and keeps its payload"),
    661: dict(kind="BulkWrite", status=PortStatus.EXERCISED,
              note="removing a property from a stored document must remove it, not null it"),
    728: dict(kind="BulkWrite", status=PortStatus.NEEDS_KEY_COMPRESSION_WRAPPER,
              note="stores a doc against compressionState.compressedSchema"),
    773: dict(kind="Revision", status=PortStatus.EXERCISED,
              note="_meta-only write; asserts parseRevision of the resulting _rev"),
    848: dict(kind="BulkWrite", status=PortStatus.EXERCISED,
              note="a second instance created after a write can write the same doc"),
    900: dict(kind="Revision", status=PortStatus.EXERCISED,
              note="revision height may jump by more than 1 in one write"),
    951: dict(kind="BulkWrite", status=PortStatus.EXERCISED,
              note="3 collections x 3 docs on one database name"),
    996: dict(kind="BulkWrite", status=PortStatus.EXERCISED,
              note="umlauts round-trip; a UTF-8/UTF-16 bug fails here"),
    1058: dict(kind="Conflict", status=PortStatus.EXERCISED,
               note="insert over a deleted doc without previous must throw"),
    1130: dict(kind="BulkWrite", status=PortStatus.EXERCISED,
               note="#9145: 5 parallel bulk_write calls x 50 docs, multi_instance false"),
    # -- .prepareQuery() ---------------------------------------------------
    1180: dict(kind="Static", status=PortStatus.NEEDS_QUERY_CRATE,
               note="prepareQuery must not crash on a schema with a nested index"),
    1197: dict(kind="Static", status=PortStatus.NEEDS_QUERY_CRATE,
               note="prepareQuery must not mutate its input schema or query"),
    # -- .getSortComparator() ---------------------------------------------
    1216: dict(kind="Static", status=PortStatus.NEEDS_QUERY_CRATE,
               note="sort comparator order matches the index order"),
    1275: dict(kind="Static", status=PortStatus.NEEDS_QUERY_CRATE,
               note="comparator stays correct for docs that do not match the selector"),
    1324: dict(kind="Static", status=PortStatus.NEEDS_QUERY_CRATE,
               note="comparator on a compound index with mixed directions"),
    # -- .getQueryMatcher() ------------------------------------------------
    1383: dict(kind="Static", status=PortStatus.NEEDS_QUERY_CRATE,
               note="matcher agrees with the expected document set"),
    1424: dict(kind="Static", status=PortStatus.NEEDS_QUERY_CRATE,
               note="matcher must also match deleted documents"),
    1457: dict(kind="Static", status=PortStatus.NEEDS_QUERY_CRATE,
               note="matcher over a nested object field"),
    # -- .query() ----------------------------------------------------------
    1512: dict(kind="Query", status=PortStatus.EXERCISED,
               note="query with an empty selector returns every live doc"),
    1578: dict(kind="Query", status=PortStatus.EXERCISED,
               note="query returns documents in sort order"),
    1626: dict(kind="Query", status=PortStatus.EXERCISED,
               note="query finds deleted documents when the selector asks for them"),
    1693: dict(kind="Query", status=PortStatus.EXERCISED,
               note=".query() order and .getSortComparator() order agree; "
                    "the comparator half needs dxdb-query, the query half does not"),
    1847: dict(kind="Query", status=PortStatus.EXERCISED,
               note="skip+limit on a non-indexed sort field"),
    2021: dict(kind="Query", status=PortStatus.EXERCISED,
               note="selector over a nested object"),
    2077: dict(kind="Query", status=PortStatus.EXERCISED,
               note="isFastMode()-scaled bulk load then query"),
    # -- .count() ----------------------------------------------------------
    2123: dict(kind="Count", status=PortStatus.EXERCISED,
               note="count equals the number of live documents"),
    # -- .findDocumentsById() ---------------------------------------------
    2184: dict(kind="FindByIds", status=PortStatus.EXERCISED,
               note="findByIds returns the documents that exist"),
    2221: dict(kind="FindByIds", status=PortStatus.EXERCISED,
               note="primary key spelled `_id`"),
    2292: dict(kind="FindByIds", status=PortStatus.EXERCISED,
               note="with_deleted=true returns tombstones"),
    2346: dict(kind="FindByIds", status=PortStatus.EXERCISED,
               note="with_deleted=true also returns live documents"),
    2430: dict(kind="FindByIds", status=PortStatus.EXERCISED,
               note="findByIds over many ids"),
    # -- .getChangedDocumentsSince() --------------------------------------
    2462: dict(kind="ChangedSince", status=PortStatus.EXERCISED,
               note="checkpoint advances to the newest changed document"),
    2546: dict(kind="ChangedSince", status=PortStatus.EXERCISED,
               note="limit is honoured and the total over a walk is exact"),
    2642: dict(kind="ChangedSince", status=PortStatus.EXERCISED,
               note="iterating checkpoints covers every change exactly once"),
    # -- .changeStream() ---------------------------------------------------
    2696: dict(kind="ChangeStream", status=PortStatus.EXERCISED,
               note="exactly one event per write"),
    2791: dict(kind="ChangeStream", status=PortStatus.EXERCISED,
               note="all events emitted, operation types and revisions correct"),
    2881: dict(kind="ChangeStream", status=PortStatus.EXERCISED,
               note="a bulk that failed entirely emits no empty EventBulk"),
    # -- attachments -------------------------------------------------------
    2938: dict(kind="Attachment", status=PortStatus.EXERCISED,
               note="store and read back attachment bytes by digest"),
    2995: dict(kind="Attachment", status=PortStatus.EXERCISED,
               note="attachment stubs appear on query/findByIds/changedSince/changeStream"),
    3109: dict(kind="Attachment", status=PortStatus.EXERCISED,
               note="one attachment added per write accumulates"),
    3193: dict(kind="Attachment", status=PortStatus.EXERCISED,
               note="a deleted document loses its attachments"),
    3267: dict(kind="Attachment", status=PortStatus.EXERCISED,
               note="several attachment reads in parallel"),
    # -- .cleanup ----------------------------------------------------------
    3369: dict(kind="Cleanup", status=PortStatus.EXERCISED,
               note="cleanup purges a tombstone past minimum_deleted_time"),
    3470: dict(kind="Cleanup", status=PortStatus.EXERCISED,
               note="cleanup purges several tombstones"),
    3550: dict(kind="Cleanup", status=PortStatus.EXERCISED,
               note="cleanup returns false while work remains and true when done"),
    # -- .close() / .remove() ---------------------------------------------
    3613: dict(kind="Cleanup", status=PortStatus.EXERCISED,
               note="close() twice does not error"),
    3633: dict(kind="Cleanup", status=PortStatus.EXERCISED,
               note="remove() deletes all data"),
    3676: dict(kind="Cleanup", status=PortStatus.EXERCISED,
               note="remove() after close() throws"),
    3693: dict(kind="Cleanup", status=PortStatus.EXERCISED,
               note="close() after remove() does not throw"),
    # -- multiInstance -----------------------------------------------------
    3757: dict(kind="BulkWrite", status=PortStatus.NEEDS_MULTI_INSTANCE,
               note="two instances on one collection see each other's writes"),
    3789: dict(kind="BulkWrite", status=PortStatus.NEEDS_MULTI_INSTANCE,
               note="bidirectional write/read across two instances"),
    3862: dict(kind="Query", status=PortStatus.NEEDS_MULTI_INSTANCE,
               note="a query finishes after the leading instance closes"),
    3890: dict(kind="Query", status=PortStatus.NEEDS_MULTI_INSTANCE,
               note="schema versions do not share state"),
    3971: dict(kind="Query", status=PortStatus.NEEDS_MULTI_INSTANCE,
               note="database names do not share state"),
    4026: dict(kind="ChangeStream", status=PortStatus.NEEDS_MULTI_INSTANCE,
               note="events propagate from one instance to the other"),
    # -- migration ---------------------------------------------------------
    4046: dict(kind="Query", status=PortStatus.EXERCISED,
               note="two pseudo-schema versions do not interfere"),
}


def impl_parents(lines: list[str]) -> dict[int, str]:
    """Map each `it`'s line to its enclosing `describe` title."""
    items = []
    for idx, line in enumerate(lines):
        m = IT_RE.match(line) or IT_RE_BACKTICK.match(line)
        if not m:
            continue
        items.append(dict(kw=m.group(2), title=m.group(3), line=idx + 1, indent=len(m.group(1))))
    depth_at, nets = brace_depth_map(lines)
    for item in items:
        base = depth_at[item["line"] - 1]
        dep = base
        j = item["line"] - 1
        while j < len(lines):
            dep += nets[j]
            if dep <= base and j > item["line"] - 1:
                break
            j += 1
        item["end"] = j + 1
    parents = {}
    for item in items:
        if item["kw"] != "it":
            continue
        best = None
        for other in items:
            if other is item or other["kw"] != "describe":
                continue
            if (
                other["line"] < item["line"]
                and other["end"] >= item["end"]
                and other["indent"] < item["indent"]
            ):
                if best is None or other["indent"] > best["indent"]:
                    best = other
        parents[item["line"]] = best["title"] if best else ""
    return parents


def rust_str(s: str) -> str:
    """A Rust string literal that survives quotes, backslashes and newlines."""
    out = s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\t", "\\t")
    return f'"{out}"'


@dataclass
class Case:
    id: str
    upstream_file: str
    upstream_line: int
    title: str
    kind: str
    status: str
    note: str
    section: str = ""
    # correctness-specific
    group: str = ""
    queries: int = 0


def build_impl_cases(root: Path) -> list[Case]:
    path = root / IMPL_REL
    lines = path.read_text().split("\n")
    parents = impl_parents(lines)
    cases = []
    counters = {k: 0 for k in KINDS}
    for idx, line in enumerate(lines):
        m = IT_RE.match(line) or IT_RE_BACKTICK.match(line)
        if not m or m.group(2) != "it":
            continue
        lineno = idx + 1
        meta = IMPL_META.get(lineno)
        if meta is None:
            raise SystemExit(
                f"{IMPL_REL}:{lineno}: `it` case has no declaration metadata. "
                f"Upstream moved; update IMPL_META in {Path(__file__).name}."
            )
        kind = meta["kind"]
        counters[kind] += 1
        cases.append(
            Case(
                id=f"impl-{kind.lower()}-{counters[kind]:02d}",
                upstream_file=IMPL_REL,
                upstream_line=lineno,
                title=m.group(3),
                kind=kind,
                status=meta["status"],
                note=meta["note"],
                section=parents.get(lineno, ""),
            )
        )
    return cases


# ---------------------------------------------------------------------------
# Query-correctness suite: one case per testCorrectQueries group
# ---------------------------------------------------------------------------

QC_CALL_RE = re.compile(r"testCorrectQueries\s*(<[\s\S]*?>)?\s*\(\s*\{")


def qc_groups(src: str) -> list[tuple[int, str]]:
    """Every testCorrectQueries *call* (not its declaration) as (line, body)."""
    out = []
    pos = 0
    while True:
        m = QC_CALL_RE.search(src, pos)
        if not m:
            return out
        if "input:" in src[m.start() : m.end() + 40]:
            pos = m.end()
            continue
        brace = src.index("{", m.end() - 1)
        end = find_balanced(src, brace, "{", "}")
        out.append((src[:brace].count("\n") + 1, src[brace + 1 : end]))
        pos = end


def qc_field(body: str, name: str) -> str | None:
    m = re.search(r"^\s*%s\s*:\s*" % re.escape(name), body, re.M)
    if not m:
        return None
    rest = body[m.end() :]
    for op, cl in (("[", "]"), ("{", "}"), ("(", ")")):
        if rest.startswith(op):
            return rest[: find_balanced(rest, 0, op, cl) + 1]
    if rest.startswith("withIndexes"):
        return rest[: find_balanced(rest, rest.index("("), "(", ")") + 1]
    j = rest.find(",\n")
    return rest[:j] if j > 0 else rest.split("\n")[0]


def strip_ts_casts(text: str) -> str:
    """Remove TypeScript-only casts so the value is valid JavaScript data.

    Upstream casts a few selectors with `as any` where the typed MangoQuery would
    reject a mixed-type `$in` (line 906). The cast is a type-system escape hatch,
    not data, so dropping it is what makes the value the same value JavaScript
    evaluates.
    """
    text = re.sub(r"\s+as\s+const\b", "", text)
    text = re.sub(r"\s+as\s+any\b", "", text)
    text = re.sub(r"\s+as\s+[A-Za-z_$][\w$.<>\[\], |]*", "", text)
    text = re.sub(r"\bsatisfies\s+[A-Za-z_$][\w$.<>\[\], |]*", "", text)
    return text


def build_qc_cases(root: Path) -> list[Case]:
    src = (root / QC_REL).read_text()
    cases = []
    for n, (block_line, body) in enumerate(qc_groups(src), start=1):
        title_m = re.search(r"testTitle\s*:\s*'((?:\\.|[^'\\])*)'", body)
        if not title_m:
            raise SystemExit(f"{QC_REL}:{block_line}: group has no testTitle")
        title = title_m.group(1)
        # The line that declares the title, which is the stable anchor a reader
        # jumps to. `block_line` is the call's own line.
        title_line = src[: src.index(title_m.group(0))].count("\n") + 1
        queries_src = qc_field(body, "queries") or "[]"
        try:
            queries = parse(strip_ts_casts(queries_src))
        except Unsupported as err:  # pragma: no cover - fails loudly by design
            raise SystemExit(f"{QC_REL}:{block_line}: cannot parse queries: {err}") from err
        queries = [q for q in queries if q is not None]
        not_run = "notRunIfTrue" in body

        # The correctness suite's cases run against `RxCollection`, i.e. above the
        # storage: each one compares the in-memory statics (getQueryMatcher +
        # getSortComparator from dxdb-query), the planner's
        # selectorSatisfiedByIndex, and the storage's own query() output. The
        # storage half needs dxdb-query to build a PreparedQuery, so until lane B
        # lands, the group is declared and its data is committed but its
        # assertions do not run. Saying so per group is the point of the column.
        cases.append(
            Case(
                id=f"qc-{n:02d}",
                upstream_file=QC_REL,
                upstream_line=title_line,
                title=title,
                kind="Query",
                status=PortStatus.NEEDS_QUERY_CRATE,
                note=(
                    "declared; data and expected ids committed; assertions need "
                    "dxdb-query's prepare_query to build a PreparedQuery"
                    + ("; upstream skips this group for dexie storages" if not_run else "")
                ),
                section="testCorrectQueries",
                queries=len(queries),
            )
        )
    return cases


# ---------------------------------------------------------------------------
# Emit
# ---------------------------------------------------------------------------

HEADER = """\
//! The case declaration table. **Generated — do not edit.**
//!
//! Regenerate with `python3 crates/dxdb-testkit/gen/declare_cases.py`; verify
//! with `--check`. The generator reads the upstream suites and emits one row per
//! upstream case, deriving every line number from the source so it cannot drift.
//!
//! This table is the storage contract, not documentation of it. Lane D implements
//! against it; `docs/GATES.md` measures storages by whether the rows marked
//! [`PortStatus::Exercised`](crate::decl::PortStatus::Exercised) pass. A row that
//! is declared but not exercised is a debt with a named reason, which is the only
//! form of "not done" that is useful to anyone reading it.
"""


def emit(cases: list[Case]) -> str:
    impl = [c for c in cases if c.upstream_file == IMPL_REL]
    qc = [c for c in cases if c.upstream_file == QC_REL]
    out = [HEADER]
    out.append("use crate::decl::{Case, PortStatus};")
    out.append("")
    out.append("/// `test/unit/rx-storage-implementations.test.ts` (4,115 lines).")
    out.append("///")
    out.append("/// One row per upstream `it(...)`. `section` is the enclosing `describe`,")
    out.append(f"/// which is how upstream groups them: {len(impl)} cases.")
    out.append("pub const IMPLEMENTATIONS_CASES: &[Case] = &[")
    for c in impl:
        out.append("    Case {")
        out.append(f"        id: {rust_str(c.id)},")
        out.append(f"        upstream_file: {rust_str(c.upstream_file)},")
        out.append(f"        upstream_line: {c.upstream_line},")
        out.append(f"        title: {rust_str(c.title)},")
        out.append(f"        kind: {rust_str(c.kind)},")
        out.append(f"        section: {rust_str(c.section)},")
        out.append(f"        status: PortStatus::{c.status},")
        out.append(f"        note: {rust_str(c.note)},")
        out.append("        queries: 0,")
        out.append("    },")
    out.append("];")
    out.append("")
    out.append("/// `test/unit/rx-storage-query-correctness.test.ts` (2,663 lines).")
    out.append("///")
    out.append("/// One row per `testCorrectQueries` group, i.e. per upstream `it(...)`.")
    out.append(f"/// The groups hold 89 queries between them; the per-group count is")
    out.append("/// `queries` below and the queries themselves live in")
    out.append("/// `crate::fixtures::correctness`.")
    out.append("pub const QUERY_CORRECTNESS_CASES: &[Case] = &[")
    for c in qc:
        out.append("    Case {")
        out.append(f"        id: {rust_str(c.id)},")
        out.append(f"        upstream_file: {rust_str(c.upstream_file)},")
        out.append(f"        upstream_line: {c.upstream_line},")
        out.append(f"        title: {rust_str(c.title)},")
        out.append(f"        kind: {rust_str(c.kind)},")
        out.append(f"        section: {rust_str(c.section)},")
        out.append(f"        status: PortStatus::{c.status},")
        out.append(f"        note: {rust_str(c.note)},")
        out.append(f"        queries: {c.queries},")
        out.append("    },")
    out.append("];")
    out.append("")
    out.append("/// Every declared case, both suites.")
    out.append("pub fn all_cases() -> impl Iterator<Item = &'static Case> {")
    out.append("    IMPLEMENTATIONS_CASES.iter().chain(QUERY_CORRECTNESS_CASES.iter())")
    out.append("}")
    out.append("")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="fail if the committed table is stale")
    args = ap.parse_args()

    root = upstream_root()
    if not (root / IMPL_REL).exists():
        print(
            f"error: upstream checkout not found at {root}\n"
            f"       set RXDB_UPSTREAM or clone github.com/pubkey/rxdb at 3b763ee17",
            file=sys.stderr,
        )
        return 2

    cases = build_impl_cases(root) + build_qc_cases(root)
    text = emit(cases)
    target = Path(__file__).resolve().parent.parent / "src" / "decl" / "generated.rs"

    if args.check:
        if not target.exists():
            print(f"error: {target} does not exist; run without --check", file=sys.stderr)
            return 1
        if target.read_text() != text:
            print(
                f"error: {target} is stale.\n"
                f"       Run `python3 crates/dxdb-testkit/gen/declare_cases.py` "
                f"and review the diff.",
                file=sys.stderr,
            )
            return 1
        print(f"ok: {target.name} matches upstream ({len(cases)} cases)")
        return 0

    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)
    impl = sum(1 for c in cases if c.upstream_file == IMPL_REL)
    qc = sum(1 for c in cases if c.upstream_file == QC_REL)
    print(f"wrote {target} ({impl} implementations, {qc} correctness, {len(cases)} total)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
