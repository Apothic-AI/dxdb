#!/usr/bin/env python3
"""Audit dxdb against PLAN.md section 10's definition of done.

Section 10 is a checklist, and the plan says it exists because "port complete" is
otherwise unfalsifiable. This script is the falsification attempt: every item maps
to evidence that is either present or absent, and nothing is inferred.

Run: python3 xtask/audit.py            # human report
     python3 xtask/audit.py --json     # machine report
     python3 xtask/audit.py --strict   # exit 1 if any item is unmet

The point is not to produce a green report. The point is that a red item names
concretely what is missing, so "we are nearly done" cannot be said while an item
is red.
"""

import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UPSTREAM_DEFAULT = os.path.expanduser("~/Code/rxdb")


def run(cmd, cwd=None):
    """Run a command, returning (exit status, stdout)."""
    try:
        outcome = subprocess.run(
            cmd, cwd=cwd or ROOT, capture_output=True, text=True, timeout=180
        )
        return outcome.returncode, outcome.stdout + outcome.stderr
    except (subprocess.TimeoutExpired, FileNotFoundError) as error:
        return 1, str(error)


def cargo_check_cmd():
    """The remote check command, routed the same way the Makefile routes it.

    The box has two reachable paths (WireGuard 6PN, and Fly's API proxy) and either
    can be down while the other works. Hard-coding one made this audit report a false
    negative -- "0/4 targets pass" -- while the targets were fine and only the route
    was broken.
    """
    shim = os.path.join(ROOT, "xtask", "remote-route.sh")
    try:
        route = subprocess.run(
            [shim], capture_output=True, text=True, timeout=60
        ).stdout.strip()
    except (subprocess.TimeoutExpired, FileNotFoundError):
        route = ""
    if "proxy" in route:
        port = route.split(":", 1)[1] if ":" in route else "2223"
        return [
            "cargo", "remote-3000", "-H", "fly-builder-proxy",
            "-p", port, "-d", "1.95.0", "check", "--workspace",
        ]
    return ["cargo", "remote-3000", "-r", "fly", "-d", "1.95.0", "check", "--workspace"]


def read(path):
    try:
        with open(os.path.join(ROOT, path), encoding="utf8") as handle:
            return handle.read()
    except OSError:
        return ""


def glob_files(pattern_root, suffix=".rs"):
    found = []
    for base in pattern_root:
        directory = os.path.join(ROOT, base)
        if not os.path.isdir(directory):
            continue
        for parent, _dirs, files in os.walk(directory):
            for name in files:
                if name.endswith(suffix):
                    found.append(os.path.join(parent, name))
    return found


def count_lines(paths):
    total = 0
    for path in paths:
        try:
            with open(path, encoding="utf8", errors="replace") as handle:
                total += sum(1 for _ in handle)
        except OSError:
            pass
    return total


# --------------------------------------------------------------------------
# The checklist. One function per section 10 item; each returns
# (status, evidence) where status is "met", "partial", or "unmet".
# --------------------------------------------------------------------------


def item_module_coverage():
    """Every upstream module has a counterpart or a written, reviewed omission."""
    upstream = os.environ.get("RXDB_UPSTREAM", UPSTREAM_DEFAULT)
    if not os.path.isdir(os.path.join(upstream, "src")):
        return "unmet", f"upstream checkout not found at {upstream}"
    core = [f for f in os.listdir(os.path.join(upstream, "src")) if f.endswith(".ts")]
    plugins_dir = os.path.join(upstream, "src", "plugins")
    plugins = sorted(
        d for d in os.listdir(plugins_dir) if os.path.isdir(os.path.join(plugins_dir, d))
    )

    status_text = read("docs/STATUS.md")
    accounted = 0
    for module in core:
        if f"`{module}`" in status_text:
            # A row exists. It is "accounted" if it is ported, or if it is marked
            # intentionally not ported with a reason.
            row = next(
                (line for line in status_text.splitlines() if f"`{module}`" in line), ""
            )
            if "ported" in row or "not ported (intentional)" in row:
                accounted += 1
    for module in plugins:
        row = next(
            (line for line in status_text.splitlines() if f"`{module}`" in line), ""
        )
        if row and ("ported" in row or "not ported (intentional)" in row):
            accounted += 1

    total = len(core) + len(plugins)
    if accounted == total:
        return "met", f"{accounted}/{total} modules ported or justified"
    return (
        "unmet",
        f"{accounted}/{total} modules ported or justified "
        f"({total - accounted} rows still say 'not started')",
    )


def item_ported_tests_named():
    """The ported test corpus passes, and each ported test names its upstream origin."""
    rust_files = glob_files(["crates"])
    total_provenance = 0
    files_with_provenance = 0
    for path in rust_files:
        text = open(path, encoding="utf8", errors="replace").read()
        hits = len(re.findall(r"test/unit/|src/plugins/|src/\w+\.ts", text))
        if hits:
            files_with_provenance += 1
            total_provenance += hits
    # `make test` runs on the shared build box through cargo-remote-3000. Never
    # invoke cargo directly from this script: a local workspace build is a hard
    # rule violation in this repo (docs/PLAN.md 7.4) and silently creates a
    # multi-hundred-MB target/ directory.
    status, output = run(["make", "test"])
    # A timeout is not a red test. `make test` queues on an flock because eight
    # other lanes may be compiling on the shared box, so a busy box looks like a
    # failure to a naive check. Distinguish the two: a timeout or a lock wait is
    # "cannot verify right now", a nonzero exit from cargo is a real failure.
    if status != 0 and ("Terminated" in output or "timed out" in output.lower()):
        return (
            "partial",
            "could not verify: `make test` timed out waiting for the shared build box "
            "(other lanes are compiling). Re-run when `make busy` reports IDLE.",
        )
    gate_ok = status == 0
    if gate_ok and total_provenance >= 50:
        return "met", f"cargo test green; {total_provenance} upstream references across {files_with_provenance} files"
    if not gate_ok:
        return "unmet", "cargo test is not green locally (run `make test` on the build box)"
    return (
        "partial",
        f"{total_provenance} upstream provenance references in {files_with_provenance} files; "
        "corpus is far short of the 55k-line upstream suite",
    )


def item_vectors_match():
    """The exported vector suite matches upstream byte for byte."""
    vectors = os.path.join(ROOT, "testdata", "vectors")
    if not os.path.isdir(vectors):
        return "unmet", "testdata/vectors does not exist"
    families = [f for f in os.listdir(vectors) if f.endswith(".json")]
    status, output = run(["node", "xtask/vectors/generate.mjs", "--check"])
    expected = {
        "jsnum",
        "index-strings",
        "schema-hashes",
        "normalised-queries",
        "bulk-write-categorization",
        "replication-transcripts",
        "selector-matching",
    }
    present = {f[:-5] for f in families}
    missing = expected - present
    if status == 0 and not missing:
        return "met", f"{len(families)} families regenerate identically"
    detail = f"{len(present)}/{len(expected)} families present"
    if missing:
        detail += f"; missing: {', '.join(sorted(missing))}"
    if status != 0:
        detail += "; --check failed"
    return "partial", detail


def item_dexie_migration():
    """A Dexie store migrates into dxdb's storage and back without semantic loss."""
    indexeddb = os.path.join(ROOT, "crates", "storages", "dxdb-indexeddb")
    if os.path.isdir(indexeddb):
        # Check for the test by reading it rather than by running cargo: the
        # target/ directory for a browser-storage test run is not something this
        # audit should create, and the presence of a green migration test is
        # verified by the lane's own gate and by the leader's merge check.
        files = glob_files(["crates/storages/dxdb-indexeddb"], ".rs")
        blob = "".join(open(f, encoding="utf8", errors="replace").read() for f in files)
        if "migration" in blob.lower() and "dexie" in blob.lower():
            return "partial", "dexie migration test present; green status is verified by the lane gate"
        return "partial", "dxdb-indexeddb exists but no dexie migration test found"
    return "unmet", "dxdb-indexeddb does not exist yet (M4)"


def item_replication_interop():
    """dxdb <-> upstream RxDB converges through the same endpoint; checkpoints survive restart."""
    directory = os.path.join(ROOT, "crates", "dxdb-replication")
    if not os.path.isdir(directory):
        return "unmet", "dxdb-replication does not exist yet (M3)"
    tests = glob_files(["crates/dxdb-replication"], ".rs")
    blob = "".join(open(p, encoding="utf8", errors="replace").read() for p in tests)
    has_convergence = "converge" in blob.lower()
    has_restart = "restart" in blob.lower() or "persist" in blob.lower()
    if has_convergence and has_restart:
        return "partial", "convergence and checkpoint-persistence tests present; interop with a real upstream client not yet demonstrated"
    return "unmet", "replication crate exists but lacks convergence/persistence tests"


def item_targets_pass():
    """Native, wasm32-unknown-unknown, wasip1, wasip2 all pass CI on stable."""
    # All four target checks go to the shared box. A local `cargo check
    # --workspace` would create a target/ directory of several hundred MB, which
    # is exactly what PLAN.md 7.4's remote-only rule exists to prevent.
    results = {}
    for label, target in [
        ("host", None),
        ("wasm32-unknown-unknown", "wasm32-unknown-unknown"),
        ("wasm32-wasip1", "wasm32-wasip1"),
        ("wasm32-wasip2", "wasm32-wasip2"),
    ]:
        cmd = cargo_check_cmd()
        if target:
            cmd += ["--target", target]
        status, output = run(cmd)
        if status != 0 and ("Terminated" in output or "timed out" in output.lower()):
            results[label] = None  # could not verify, distinct from failed
        else:
            results[label] = status == 0
    met = [k for k, v in results.items() if v]
    unverified = [k for k, v in results.items() if v is None]
    if len(met) == 4:
        return "met", "all four targets check on the shared box"
    if unverified and len(met) + len(unverified) == 4:
        return (
            "partial",
            f"could not verify {', '.join(unverified)}: the build box was busy. "
            f"{len(met)}/4 verified.",
        )
    return "unmet", f"{len(met)}/4 targets pass: {', '.join(met) or 'none'}"


def item_wasm_budgets():
    """WASM size budgets met and recorded."""
    text = read("docs/BENCHMARKS.md")
    rows = [line for line in text.splitlines() if line.startswith("| `dxdb-js`")]
    measured = [r for r in rows if "not yet measured" not in r and "pending" not in r]
    if len(measured) >= 4:
        return "met", f"{len(measured)} of 4 budgets measured"
    return "partial", f"{len(measured)}/4 budgets measured; the rest are still 'not yet measured'"


def item_benchmarks():
    """Benchmarks recorded, honest including losses."""
    text = read("docs/BENCHMARKS.md")
    if not text:
        return "unmet", "docs/BENCHMARKS.md does not exist"
    has_storage = "pending M2" not in text and "pending M4" not in text
    has_losses = "expected to lose" in text or "expected to be slower" in text
    if has_storage and has_losses:
        return "met", "storage benchmarks recorded, expected losses section present"
    if has_losses:
        return "partial", "losses recorded; storage benchmark rows still pending"
    return "unmet", "no storage measurements and no losses recorded"


def item_license_notice():
    """Apache-2.0 LICENSE, NOTICE crediting RxDB, unaffiliated README, no Co-Authored-By."""
    problems = []
    license_text = read("LICENSE")
    if "Apache License" not in license_text or "Version 2.0" not in license_text:
        problems.append("LICENSE is not Apache-2.0")
    notice = read("NOTICE")
    if "pubkey/rxdb" not in notice:
        problems.append("NOTICE does not credit pubkey/rxdb")
    if "3b763ee17d62bc96cfae556ea0ccf92f9891c8c1" not in notice:
        problems.append("NOTICE does not record the upstream pin")
    readme = read("README.md")
    if "unaffiliated" not in readme:
        problems.append("README does not state the project is unaffiliated")
    status, log = run(["git", "log", "--format=%B"])
    if "Co-Authored-By" in log:
        problems.append("Co-Authored-By trailers present in git history")
    if problems:
        return "unmet", "; ".join(problems)
    return "met", "LICENSE, NOTICE (with pin), unaffiliated README, clean trailers"


def item_status_matrix():
    """A published status matrix exists and is regenerable."""
    if not os.path.exists(os.path.join(ROOT, "docs", "STATUS.md")):
        return "unmet", "docs/STATUS.md does not exist"
    status, _ = run(["python3", "xtask/status.py"])
    if status != 0:
        return "partial", "docs/STATUS.md exists but `make status` cannot regenerate it"
    return "met", "docs/STATUS.md present and regenerable by `make status`"


CHECKS = [
    ("Every upstream module ported or justified", item_module_coverage),
    ("Ported tests pass and name their upstream origin", item_ported_tests_named),
    ("Vector suite matches upstream byte for byte", item_vectors_match),
    ("Dexie store migrates in and out without loss", item_dexie_migration),
    ("Replication interop with upstream converges", item_replication_interop),
    ("Native + wasm32 + wasip1 + wasip2 pass on stable", item_targets_pass),
    ("WASM size budgets met and recorded", item_wasm_budgets),
    ("Benchmarks recorded, honestly including losses", item_benchmarks),
    ("LICENSE, NOTICE, unaffiliated README, clean trailers", item_license_notice),
    ("Status matrix published and regenerable", item_status_matrix),
]


def main():
    report = []
    for title, check in CHECKS:
        try:
            status, evidence = check()
        except Exception as error:  # a broken check is not a passed check
            status, evidence = "unmet", f"check raised {type(error).__name__}: {error}"
        report.append({"item": title, "status": status, "evidence": evidence})

    counts = {s: sum(1 for r in report if r["status"] == s) for s in ("met", "partial", "unmet")}

    if "--json" in sys.argv:
        print(json.dumps({"items": report, "counts": counts}, indent=2))
    else:
        symbols = {"met": "PASS", "partial": "PART", "unmet": "FAIL"}
        for entry in report:
            print(f"[{symbols[entry['status']]}] {entry['item']}")
            print(f"       {entry['evidence']}")
        print()
        print(
            f"{counts['met']} met, {counts['partial']} partial, {counts['unmet']} unmet "
            f"of {len(report)} items"
        )
        if counts["unmet"] or counts["partial"]:
            print("\nPLAN.md section 10 is not satisfied. The unmet items are the work.")

    if "--strict" in sys.argv and (counts["unmet"] or counts["partial"]):
        sys.exit(1)


if __name__ == "__main__":
    main()
