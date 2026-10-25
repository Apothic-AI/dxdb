#!/usr/bin/env python3
"""Regenerate the module rows in docs/STATUS.md from the upstream checkout.

Only the row set and the upstream line counts are generated; the State, Crate,
and Evidence columns are human-maintained and preserved across runs by keying on
the module name. Run via `make status`.
"""
import os
import re
import sys

UPSTREAM = os.environ.get("RXDB_UPSTREAM", os.path.expanduser("~/Code/rxdb"))
# Default crate homes, so a fresh regeneration is not an empty column. A lane
# correcting its module's home edits docs/STATUS.md and the edit is preserved.
DEFAULTS = {
    "custom-index.ts": "dxdb-query", "query-planner.ts": "dxdb-query",
    "rx-query-helper.ts": "dxdb-query", "rx-query-mingo.ts": "dxdb-query",
    "rx-query.ts": "dxdb-query", "rx-query-single-result.ts": "dxdb-query",
    "rx-schema.ts": "dxdb-schema", "rx-schema-helper.ts": "dxdb-schema",
    "change-event-buffer.ts": "dxdb-reactivity", "event-reduce.ts": "dxdb-reactivity",
    "query-cache.ts": "dxdb-reactivity", "rx-change-event.ts": "dxdb-reactivity",
    "rx-storage-helper.ts": "dxdb-storage", "rx-storage-multiinstance.ts": "dxdb-storage",
}
STATUS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "docs", "STATUS.md")


def line_count(path):
    try:
        with open(path, encoding="utf8", errors="replace") as handle:
            return sum(1 for _ in handle)
    except OSError:
        return 0


def plugin_lines(name):
    total = 0
    root_dir = os.path.join(UPSTREAM, "src", "plugins", name)
    for root, _dirs, files in os.walk(root_dir):
        for filename in files:
            if filename.endswith(".ts"):
                total += line_count(os.path.join(root, filename))
    return total


def existing_rows():
    """State/Crate/Evidence keyed by module name, from the current file."""
    rows = {}
    if not os.path.exists(STATUS):
        return rows
    pattern = re.compile(r"^\| `([^`]+)` \| (\d+) \| ([^|]*) \| ([^|]*) \| (.*) \|$")
    with open(STATUS, encoding="utf8") as handle:
        for line in handle:
            match = pattern.match(line.rstrip("\n"))
            if match:
                module, _lines, crate, state, evidence = match.groups()
                rows[module] = (crate, state, evidence)
    return rows


def main():
    if not os.path.isdir(os.path.join(UPSTREAM, "src")):
        sys.exit(f"upstream checkout not found at {UPSTREAM}; set RXDB_UPSTREAM")
    core = sorted(f for f in os.listdir(os.path.join(UPSTREAM, "src")) if f.endswith(".ts"))
    plugins = sorted(
        d for d in os.listdir(os.path.join(UPSTREAM, "src", "plugins"))
        if os.path.isdir(os.path.join(UPSTREAM, "src", "plugins", d))
    )
    previous = existing_rows()
    text = open(STATUS, encoding="utf8").read() if os.path.exists(STATUS) else ""

    def row(module, lines):
        crate, state, evidence = previous.get(module, ("", "not started", ""))
        # An empty crate cell means "no editorial decision yet", so the default
        # home fills it in; a filled cell is preserved verbatim.
        crate = crate.strip() or DEFAULTS.get(module, "")
        return f"| `{module}` | {lines} | {crate} | {state} | {evidence} |"

    core_rows = [row(f, line_count(os.path.join(UPSTREAM, "src", f))) for f in core]
    plugin_rows = [row(d, plugin_lines(d)) for d in plugins]

    # Replace the core and plugin tables in place, leaving prose and the
    # milestone table alone.
    def replace_table(text, header, rows):
        marker = f"## {header}"
        start = text.index(marker)
        table_start = text.index("| Module |", start)
        table_end = text.index("\n\n", table_start)
        return text[:table_start] + "| Module | Upstream lines | Crate | State | Evidence (gate) |\n| --- | --- | --- | --- | --- |\n" + "\n".join(rows) + text[table_end:]

    text = replace_table(text, "Core", core_rows)
    text = replace_table(text, "Plugins", plugin_rows)
    with open(STATUS, "w", encoding="utf8") as handle:
        handle.write(text)
    print(f"status: {len(core_rows)} core rows, {len(plugin_rows)} plugin rows")


if __name__ == "__main__":
    main()
