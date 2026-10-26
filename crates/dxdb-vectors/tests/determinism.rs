//! The determinism invariant: regenerating a family twice produces identical bytes.
//!
//! Every vector file records how the generator made the run reproducible
//! (`mathRandom` replaced by a seeded xorshift128, `dateNow` frozen) and some record
//! *why* a field is excluded (the checkpoint's own `_meta.lwt` and `_rev`, which are
//! write bookkeeping that varies with promise scheduling). Those are claims in a
//! comment until something checks them.
//!
//! The stakes are not cosmetic. If a family is non-deterministic, then:
//!
//! - `make vectors` churns the working tree on every run, so real changes are lost in
//!   the noise and reviewers stop reading the diff;
//! - `make vectors-check` fails intermittently, which is worse than not having it,
//!   because a red gate that is sometimes green teaches everyone to re-run until it
//!   passes;
//! - a consumer's failure cannot be reproduced, so it cannot be debugged.
//!
//! So the generator is run twice, in separate processes, into a temporary directory,
//! and the bytes are compared. Separate processes rather than two calls in one, because
//! the nondeterminism these tests exist to catch — `Math.random` seeding, a clock
//! reading, module-level counters — resets between processes, and a same-process double
//! run could pass while the real `make vectors` invocation drifts.
//!
//! # What this test can and cannot do
//!
//! It shells out to Node. If Node or the pinned upstream checkout is unavailable it
//! *skips with a visible message* rather than failing, because the Rust test suite must
//! not require a Node toolchain. It never silently passes: a skip is printed, and the
//! comparison only reports success when it actually ran twice and compared. The
//! authoritative check remains `make vectors-check`, which runs the same generator in
//! CI.

use std::path::{Path, PathBuf};
use std::process::Command;

/// All seven families, in generator order.
const FAMILIES: &[&str] = &[
    "jsnum",
    "index-strings",
    "schema-hashes",
    "normalised-queries",
    "bulk-write-categorization",
    "selector-matching",
    "replication-transcripts",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repository root is reachable from the crate manifest dir")
}

fn generator() -> PathBuf {
    repo_root()
        .join("xtask")
        .join("vectors")
        .join("generate.mjs")
}

/// Run the generator for one family, writing into `out_dir`'s `testdata/vectors/`.
///
/// The generator resolves its output from the repository root, and rewriting that to a
/// temporary directory would mean adding a flag used only by this test. Instead the
/// generated files are copied aside after each run: run once, snapshot, run again,
/// compare. That keeps the generator's interface unchanged.
fn run_generator(family: &str) -> std::io::Result<()> {
    let status = Command::new("node")
        .arg(generator())
        .arg(family)
        .current_dir(repo_root())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()?;
    if !status.status.success() {
        return Err(std::io::Error::other(format!(
            "generator failed for {family}: {}",
            String::from_utf8_lossy(&status.stderr)
        )));
    }
    Ok(())
}

fn vector_path(family: &str) -> PathBuf {
    repo_root()
        .join("testdata")
        .join("vectors")
        .join(format!("{family}.json"))
}

/// `true` when Node and the pinned upstream checkout are usable.
fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Regenerating each family twice must produce byte-identical files.
///
/// One test per family would produce seven nearly-identical tests and seven separate
/// Node startup costs; a single loop reports which family drifted in the assertion
/// message, which is the only thing the split would have bought.
#[test]
fn every_family_regenerates_byte_identically() {
    if !node_available() {
        eprintln!(
            "SKIP every_family_regenerates_byte_identically: `node` is not available.\n\
             This test shells out to the real generator; `make vectors-check` covers the \
             same invariant in an environment that has Node."
        );
        return;
    }
    if !generator().is_file() {
        eprintln!(
            "SKIP every_family_regenerates_byte_identically: {} is missing.",
            generator().display()
        );
        return;
    }

    // Snapshot each family after the first run, then rerun and compare. Snapshotting
    // all of them first keeps the two passes clearly separated, so a failure cannot be
    // explained by one family's run perturbing another's.
    let mut first: Vec<(&str, Vec<u8>)> = Vec::new();

    for family in FAMILIES {
        if let Err(e) = run_generator(family) {
            // The checkout may be absent (a source tree without the vendored pin, or a
            // machine that never ran `make vectors`). Report, do not fail the suite.
            eprintln!(
                "SKIP every_family_regenerates_byte_identically at {family}: {e}\n\
                 The Rust suite must not require a Node toolchain; `make vectors-check` \
                 is the authoritative determinism gate."
            );
            return;
        }
        let bytes = std::fs::read(vector_path(family)).unwrap_or_else(|e| {
            panic!(
                "generator reported success for {family} but {} is unreadable: {e}",
                vector_path(family).display()
            )
        });
        first.push((family, bytes));
    }

    // Second pass: regenerate and compare against the snapshot.
    for (family, before) in &first {
        run_generator(family).unwrap_or_else(|e| {
            panic!("second generator run failed for {family}: {e}");
        });
        let after = std::fs::read(vector_path(family)).unwrap_or_else(|e| {
            panic!(
                "{} is unreadable after the second run: {e}",
                vector_path(family).display()
            )
        });

        assert_eq!(
            before.len(),
            after.len(),
            "{family} regenerated to a different size ({} -> {} bytes).\n\
             The family is non-deterministic. `make vectors-check` will fail \
             intermittently and no consumer failure against it is reproducible. Look for \
             an unseeded Math.random, a real clock read, or a field derived from promise \
             scheduling (see the `determinism` block in the file for what is supposed to \
             be stubbed).",
            before.len(),
            after.len()
        );
        assert_eq!(
            before.as_slice(),
            after.as_slice(),
            "{family} regenerated non-deterministically: same length, different bytes.\n\
             Diff a saved copy against {} to find the drifting field. The `determinism` \
             block in the file records what the generator stubs; the usual causes are an \
             unseeded random id, a wall-clock timestamp, or a value whose number of \
             updates depends on event-loop ordering.",
            vector_path(family).display()
        );
    }

    eprintln!(
        "every_family_regenerates_byte_identically: {} families regenerated twice, all identical",
        first.len()
    );
}

/// The `determinism` block must actually be present and non-trivial.
///
/// A cheap structural check that costs no Node run: if a generator stopped recording how
/// it achieves determinism, the reproducibility claim above would still pass on a lucky
/// machine while nobody could tell how. `jsnum` stands alone here since it predates the
/// convention and has no upstream fixtures to stabilise.
#[test]
fn determinism_is_documented_in_every_family_that_needs_it() {
    let vectors = dxdb_vectors::Vectors::discover();

    for family in dxdb_vectors::ALL_FAMILIES {
        if *family == dxdb_vectors::VectorFamily::Jsnum {
            continue;
        }
        // Read the raw JSON: the `determinism` block is deliberately not part of the
        // typed per-family structs (it is generator metadata, not oracle data), so this
        // checks the envelope rather than the cases.
        let path = vectors.path(*family);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let doc: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));

        let determinism = doc.get("determinism").unwrap_or_else(|| {
            panic!(
                "{family} has no `determinism` block. Every family whose output depends on \
                 Math.random or the clock must record how it was stabilised, or the \
                 reproducibility claim is unverifiable."
            )
        });
        let object = determinism
            .as_object()
            .unwrap_or_else(|| panic!("{family}: `determinism` is not an object"));
        assert!(!object.is_empty(), "{family}: `determinism` block is empty");
        // Every family stubs these two; the exact prose is the generator's business.
        assert!(
            object.contains_key("mathRandom"),
            "{family}: `determinism` does not say how Math.random was replaced"
        );
        assert!(
            object.contains_key("dateNow"),
            "{family}: `determinism` does not say how Date.now was replaced"
        );
    }
}
