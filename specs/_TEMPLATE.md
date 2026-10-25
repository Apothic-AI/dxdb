# `<feature>`: behavioural specification

<!--
COPY THIS FILE TO specs/<feature>.md AND DELETE THIS COMMENT.

Rules that make this template do its job:

* All six section headings below stay, in this order, spelled exactly like this.
  A reviewer diffing specs/ should never have to search for where the invariants
  went.
* The acceptance list at the end is numbered and never renumbered after a test
  cites a number. Withdrawn statements are struck through, not deleted.
* Every acceptance statement is an observation, not an intention. "with 4
  collections open, a cold start reads the collection metadata twice" and not
  "metadata reads are minimised".
* Every test names the numbers it covers, in the test name. `acceptance_07_...`
  is the convention.
* If a behaviour is not derivable from the sources in specs/READING-LIST.md,
  STOP and add a row to docs/CLEANROOM-GAPS.md. Choosing our own value is
  allowed and expected. Guessing the upstream value is not.
-->

| | |
| --- | --- |
| Stage | B1 (CLEANROOM.md section 7) |
| Crate | `crates/cleanroom/dxdb-<feature>` |
| Documented upstream module | `logger` (name only, public docs) |
| Spec author read | public docs, Apache-2.0 source, public issue tracker, standards |
| Spec author did not read | `rxdb-premium`, its bundles, `.d.ts`, source maps, installer |
| Status | draft / reviewed / implemented / verified |

## 1. Observable behaviour

What a user of this feature can see and measure, in prose, with the public
sentence that says it and the citation. One subsection per documented promise.
Where a public sentence is vague, quote it and say what we decided it means.

## 2. Public API shape (from docs)

The documented call signature, option names, defaults and callbacks, copied as
the *shape* from public documentation. dxdb's own signature follows and the
mapping between them is stated. Nothing here may be filled in from the closed
implementation's type declarations.

## 3. Invariants

Statements that must hold for every input, written so a property test can fail
them. Each one gets an id (`INV-1`, `INV-2`, ...) which the acceptance list and
the tests reference. These are the part of the spec we can prove without the
reference implementation (CLEANROOM.md section 6).

Every invariant names the mechanism that enforces it and the mechanism that
would catch a violation.

## 4. Failure modes

The enumerated ways this feature can go wrong, what the caller observes in each
case, and what the design does about it. Include the storage-layer failures the
feature inherits (a write that failed, a storage that closed under us) and the
feature's own errors. Documented upstream errors are cited; ours are named.

## 5. Performance targets

Targets, in numbers, measured on named hardware, with the benchmark that
produces them. A target that has not been measured is labelled **unmeasured**
and may not appear in any prose as a claim (CLEANROOM.md section 6, PLAN.md 7.5).
Upstream's published numbers may be listed as targets with their source and are
never restated as our results.

## 6. Non-goals

What this crate deliberately does not do, and why. This section is where
divergences from upstream live: no artificial limits, no phone-home, no
wire-compatible on-disk format, no tier language. If a reader could reasonably
expect the behaviour and we are not doing it, it belongs here.

## Acceptance list

Numbered, observable, each one implemented by a test that names the number. The
list is the contract the implementation is measured against; the codes
(`[INV-n]`) point back at section 3.

1. `<observable statement>`
2. `<observable statement>`

## Test map

| Acceptance | Test | File |
| --- | --- | --- |
| 1 | `acceptance_01_<what_it_asserts>` | `crates/cleanroom/dxdb-<feature>/tests/<file>.rs` |
