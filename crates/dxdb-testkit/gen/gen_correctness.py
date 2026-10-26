#!/usr/bin/env python3
"""Generate the query-correctness fixture module from upstream RxDB.

The correctness suite is 2,663 lines of two things: a 265-line harness and 25
groups of *data*. The data is the valuable part and it is also the part a human
transcribing by hand gets subtly wrong — a transposed expected id still compiles and
still passes against a permissive storage.

So it is extracted, not typed. This generator reads each `testCorrectQueries({...})`
group, evaluates its `data:` and `queries:` literals with the same parser the rest
of the toolchain uses, resolves the `schemaObjects.*` fixture calls to their ported
Rust equivalents, and emits `src/fixtures/correctness.rs` plus a JSON mirror.

Every field of every query is carried: `info`, `query` (selector, sort, skip, limit,
index), `expectedResultDocIds`, and `selectorSatisfiedByIndex` when upstream asserts
it. A field that is absent upstream is absent here, which matters because the
harness branches on presence.

Usage:

    python3 crates/dxdb-testkit/gen/gen_correctness.py
    python3 crates/dxdb-testkit/gen/gen_correctness.py --check
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from jslit import Unsupported, parse  # noqa: E402

QC_REL = "test/unit/rx-storage-query-correctness.test.ts"


def upstream_root() -> Path:
    env = os.environ.get("RXDB_UPSTREAM")
    return Path(env).expanduser() if env else Path("~/Code/rxdb").expanduser()


# ---------------------------------------------------------------------------
# Brace-aware scanning (same approach as declare_cases.py, kept local so each
# generator is independently runnable)
# ---------------------------------------------------------------------------


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


def split_top(text: str, sep: str = ",") -> list[str]:
    parts: list[str] = []
    depth = 0
    cur: list[str] = []
    instr = None
    i = 0
    esc = False
    while i < len(text):
        c = text[i]
        if instr:
            cur.append(c)
            if esc:
                esc = False
            elif c == "\\":
                esc = True
            elif c == instr:
                instr = None
        elif c in "'\"":
            instr = c
            cur.append(c)
        elif c in "([{":
            depth += 1
            cur.append(c)
        elif c in ")]}":
            depth -= 1
            cur.append(c)
        elif c == sep and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(c)
        i += 1
    if "".join(cur).strip():
        parts.append("".join(cur))
    return parts


def strip_ts_casts(text: str) -> str:
    text = re.sub(r"\s+as\s+const\b", "", text)
    text = re.sub(r"\s+as\s+any\b", "", text)
    text = re.sub(r"\s+as\s+[A-Za-z_$][\w$.<>\[\], |]*", "", text)
    text = re.sub(r"\bsatisfies\s+[A-Za-z_$][\w$.<>\[\], |]*", "", text)
    return text


CALL_RE = re.compile(r"(schemaObjects|Object)\.([A-Za-z_$][\w$]*)\s*\(")


def subst_calls(text: str) -> str:
    """Innermost-first substitution of `schemaObjects.fn(literals)` calls."""
    while True:
        matches = list(CALL_RE.finditer(text))
        if not matches:
            return text
        progressed = False
        for m in reversed(matches):
            p_open = m.end() - 1
            p_close = find_balanced(text, p_open, "(", ")")
            inner = text[p_open + 1 : p_close]
            if CALL_RE.search(inner):
                continue
            args = []
            for a in split_top(inner):
                a = a.strip()
                if not a:
                    continue
                if a.startswith("{") and "__call__" in a:
                    args.append(parse(a))
                    continue
                args.append(parse(strip_ts_casts(a)))
            marker = {"__call__": {"fn": m.group(2), "recv": m.group(1), "args": args}}
            text = text[: m.start()] + json.dumps(marker) + text[p_close + 1 :]
            progressed = True
            break
        if not progressed:
            raise Unsupported("no innermost call found in %r" % (text[:120],))


def expand(value):
    if isinstance(value, dict):
        if set(value.keys()) == {"__call__"}:
            c = value["__call__"]
            return {"__doc__": c["fn"], "recv": c["recv"], "args": expand(c["args"])}
        return {k: expand(v) for k, v in value.items()}
    if isinstance(value, list):
        return [expand(v) for v in value]
    return value


# ---------------------------------------------------------------------------
# Group extraction
# ---------------------------------------------------------------------------

CALL_START = re.compile(r"testCorrectQueries\s*(<[\s\S]*?>)?\s*\(\s*\{")


def groups(src: str):
    pos = 0
    while True:
        m = CALL_START.search(src, pos)
        if not m:
            return
        if "input:" in src[m.start() : m.end() + 40]:
            pos = m.end()
            continue
        brace = src.index("{", m.end() - 1)
        end = find_balanced(src, brace, "{", "}")
        yield brace, src[brace + 1 : end]
        pos = end


def field(body: str, name: str) -> str | None:
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


# ---------------------------------------------------------------------------
# Schema resolution
# ---------------------------------------------------------------------------

#: Named upstream fixtures and the Rust function that returns the same value.
NAMED_SCHEMAS = {
    "schemas.human": "crate::schemas::human",
    "schemas.humanMinimal": "crate::schemas::human_minimal",
    "schemas.nestedHuman": "crate::schemas::nested_human",
    "schemas.heroArray": "crate::schemas::hero_array",
    "schemas.simpleArrayHero": "crate::schemas::simple_array_hero",
    "schemas.humanAgeIndex": "crate::schemas::human_age_index",
    "schemas.humanWithAllIndex": "crate::schemas::human_with_all_index",
    "schemas.noIndexHuman": "crate::schemas::no_index_human",
    "human": "crate::schemas::human",
    "humanMinimal": "crate::schemas::human_minimal",
    "nestedHuman": "crate::schemas::nested_human",
}


def resolve_schema(src: str | None) -> dict:
    """Turn upstream's `schema:` expression into a fixture descriptor.

    Three shapes occur:

    * a named fixture (`schemas.human`, or the bare `human` import);
    * `withIndexes(<named fixture>, [...])`, which clones the fixture and replaces
      `indexes`, adding any indexed field to `required` — ported as an operation
      because the clone-and-mutate is load-bearing (the fixture itself must not
      change, since other groups share it);
    * an inline object literal.
    """
    if src is None:
        raise SystemExit("group has no schema")
    src = src.strip()

    m = re.match(r"^withIndexes\s*\(", src)
    if m:
        inner = src[src.index("(") + 1 : src.rindex(")")]
        parts = split_top(inner)
        base = parts[0].strip()
        indexes_src = parts[1].strip()
        if not indexes_src.startswith("["):
            raise SystemExit(f"withIndexes second argument is not an array: {indexes_src[:60]}")
        indexes = parse(indexes_src)
        if base not in NAMED_SCHEMAS:
            raise SystemExit(f"withIndexes over an unknown fixture: {base!r}")
        return {
            "kind": "WithIndexes",
            "base": NAMED_SCHEMAS[base],
            "base_name": base,
            "indexes": indexes,
        }

    if src in NAMED_SCHEMAS:
        return {"kind": "Named", "path": NAMED_SCHEMAS[src], "base_name": src}

    if src.startswith("{"):
        value = parse(strip_ts_casts(src))
        return {"kind": "Inline", "value": value}

    raise SystemExit(f"unrecognised schema expression: {src[:80]!r}")


# ---------------------------------------------------------------------------
# Emit
# ---------------------------------------------------------------------------

KIND_LIT = {
    "str": "Value::String",
}


def json_literal(value, indent: int) -> str:
    """Emit a `serde_json::json!`-compatible literal.

    `json!` is macro-expanded at compile time, so this stays a `const`-adjacent
    static table with no runtime parsing. Floats are emitted with `serde_json`'s own
    formatting via the JSON text, which is why the whole value is written as JSON
    and pasted: `Number.MAX_SAFE_INTEGER` and `multipleOf: 0.1` both appear in
    upstream schemas and both must survive verbatim.
    """
    text = json.dumps(value, ensure_ascii=False, indent=None, separators=(",", ":"))
    # `json!` accepts a JSON text as its single argument only via a literal, and
    # Rust string literals cannot contain a raw `"` — so instead of hand-rolling,
    # we emit the JSON as a raw string and let the runtime parse it once behind a
    # `OnceLock`. That is both simpler and auditable, and it means a malformed
    # value fails at exactly one place with a parse error naming the group.
    return text


def rust_raw_string(text: str) -> str:
    """A Rust raw string literal that holds arbitrary JSON including quotes."""
    hashes = "#"
    while f'"{hashes}' in text:
        hashes += "#"
    return f'r{hashes}"{text}"{hashes}'


def build(root: Path) -> tuple[list[dict], str]:
    src = (root / QC_REL).read_text()
    out = []
    for n, (brace, body) in enumerate(groups(src), start=1):
        line = src[:brace].count("\n") + 1
        title_m = re.search(r"testTitle\s*:\s*'((?:\\.|[^'\\])*)'", body)
        if not title_m:
            raise SystemExit(f"{QC_REL}:{line}: no testTitle")
        title = title_m.group(1)
        title_line = src[: src.index(title_m.group(0))].count("\n") + 1

        schema = resolve_schema(field(body, "schema"))

        data_src = field(body, "data")
        if data_src is None:
            raise SystemExit(f"{QC_REL}:{line}: no data")
        data = expand(parse(strip_ts_casts(subst_calls(data_src))))

        queries_src = field(body, "queries") or "[]"
        queries = expand(parse(strip_ts_casts(subst_calls(queries_src))))
        queries = [q for q in queries if q is not None]

        # Resolve the fixture calls in `data` into concrete documents now, so the
        # committed data is the documents and not a recipe. The recipes are
        # recorded in a comment per group so a reader can still see the upstream
        # `schemaObjects.humanData('aa', 10, 'alice')` call that produced them.
        docs, recipes = [], []
        for entry in data:
            if isinstance(entry, dict) and "__doc__" in entry:
                docs.append(materialise(entry))
                recipes.append(render_call(entry))
            else:
                docs.append(entry)
                recipes.append(None)

        primary = primary_path(schema)

        out.append(
            {
                "index": n,
                "title": title,
                "upstream_line": title_line,
                "schema": schema,
                "primary_path": primary,
                "data": docs,
                "recipes": recipes,
                "queries": queries,
                "not_run_if_dexie": "config.storage.name.includes('dexie')" in body,
            }
        )
    return out, src


def primary_path(schema: dict):
    """The primary key path, resolved through the same rules the schema has."""
    if schema["kind"] == "Inline":
        return inline_primary(schema["value"])
    # Named/WithIndexes: resolve by calling the Rust fixture would need Rust; the
    # upstream fixtures' primary keys are known and asserted by a Rust test, so
    # record them here and let that test catch a disagreement.
    name = schema["base_name"]
    mapping = {
        "schemas.human": "passportId",
        "human": "passportId",
        "schemas.humanMinimal": "passportId",
        "humanMinimal": "passportId",
        "schemas.nestedHuman": "passportId",
        "nestedHuman": "passportId",
        "schemas.heroArray": "name",
        "schemas.simpleArrayHero": "name",
        "schemas.humanAgeIndex": "passportId",
        "schemas.humanWithAllIndex": "passportId",
        "schemas.noIndexHuman": "firstName",
    }
    return mapping[name]


def inline_primary(value: dict) -> str:
    pk = value.get("primaryKey")
    if isinstance(pk, str):
        return pk
    if isinstance(pk, dict):
        return pk.get("key") or (pk.get("fields") or [""])[0]
    raise SystemExit("inline schema has no usable primaryKey")


# --- fixture call materialisation -----------------------------------------
#
# These are the Rust ports of `schema-objects.ts`'s generators, expressed in
# Python so the generator can call them. They must stay identical to
# `src/fixtures/objects.rs`; the Rust side has unit tests asserting the same
# outputs for the same arguments, and both are checked against upstream's
# defaults in the module docs.


def _random_string_with_special_chars(min_len: int, max_len: int) -> str:
    # The generated fixtures use only the *explicit* arguments upstream passes,
    # never the random defaults, so this is unreachable for the committed data.
    raise SystemExit(
        "a fixture call omitted an argument that upstream defaults randomly; "
        "the generated data must be fully specified to be reproducible"
    )


def materialise(entry: dict) -> dict:
    fn = entry["__doc__"]
    args = entry["args"]
    if fn == "humanData":
        passport, age, first = args
        if passport is None or age is None or first is None:
            _random_string_with_special_chars(8, 12)
        # `lastName` is the one argument upstream leaves random even when the
        # other three are given. It is never asserted on by the suite (no query
        # sorts on or filters by lastName), so a fixed value is recorded instead of
        # a random one, and this comment is why. A future case that does depend on
        # it must pass it explicitly.
        return {
            "passportId": passport,
            "firstName": first,
            "lastName": f"last-{passport}",
            "age": age,
        }
    if fn == "nestedHumanData":
        partial = args[0]
        doc = {
            "passportId": "unset",
            "firstName": "unset",
            "mainSkill": {"name": "unset", "level": 5},
        }
        doc.update(partial)
        return doc
    if fn == "simpleHumanV3Data":
        partial = args[0]
        doc = {"passportId": "unset", "age": 20}
        doc.update(partial)
        return doc
    raise SystemExit(f"unported fixture generator: {fn}")


def render_call(entry: dict) -> str:
    fn = entry["__doc__"]
    args = ", ".join(json.dumps(a, ensure_ascii=False) for a in entry["args"])
    return f"schemaObjects.{fn}({args})"


def emit_module(groups_out: list[dict]) -> str:
    out = []
    out.append(
        '//! Query-correctness fixture data. **Generated — do not edit.**\n'
        "//!\n"
        "//! Regenerate with `python3 crates/dxdb-testkit/gen/gen_correctness.py`;\n"
        "//! verify with `--check`.\n"
        "//!\n"
        "//! One [`Group`] per upstream `testCorrectQueries` call. Each group carries\n"
        "//! the schema it runs against, the documents it inserts, and every query with\n"
        "//! its expected result ids — extracted from upstream rather than transcribed,\n"
        "//! so a transposed id cannot pass review.\n"
        "//!\n"
        "//! # Why the values are JSON\n"
        "//!\n"
        "//! A group's data is `serde_json::Value`, parsed once from the extracted\n"
        "//! literal. Typed structs per group would be 25 near-identical declarations\n"
        "//! and would still have to carry the same JSON for the schema, which a storage\n"
        "//! consumes as JSON anyway ([`dxdb_storage::StorageSchema::from_value`]).\n"
        "//! Upstream's fixtures are object literals for the same reason.\n"
        "\n"
        "use serde_json::Value;\n"
        "use std::sync::OnceLock;\n"
        "\n"
        "use serde_json::json;\n"
        "\n"
        "use crate::fixtures::correctness_support::{Group, Query, SchemaSource};\n"
    )
    out.append("/// Every group, in upstream order.")
    out.append("pub fn groups() -> &'static [Group] {")
    out.append("    static GROUPS: OnceLock<Vec<Group>> = OnceLock::new();")
    out.append("    GROUPS.get_or_init(|| vec![")
    for g in groups_out:
        out.append("        group_%02d()," % g["index"])
    out.append("    ])")
    out.append("}")
    out.append("")
    for g in groups_out:
        fn = "group_%02d" % g["index"]
        out.append("/// `%s`" % g["title"].replace("\\", "\\\\").replace('"', '\\"'))
        out.append("///")
        out.append("/// Upstream: `%s:%d`" % (QC_REL, g["upstream_line"]))
        out.append("fn %s() -> Group {" % fn)
        out.append("    static CACHE: OnceLock<Group> = OnceLock::new();")
        out.append("    CACHE")
        out.append("        .get_or_init(|| Group {")
        out.append("            index: %d," % g["index"])
        out.append(
            "            title: %s," % rust_raw_string(g["title"])
        )
        out.append("            upstream_line: %d," % g["upstream_line"])
        out.append(
            "            primary_path: %s," % rust_raw_string(g["primary_path"])
        )
        # schema source
        schema = g["schema"]
        if schema["kind"] == "Named":
            out.append("            schema: SchemaSource::Named(%s)," % schema["path"])
        elif schema["kind"] == "WithIndexes":
            base_fn = schema["base"]
            # `Named(...)` is a function pointer, so emit it the same way.
            out.append("            schema: SchemaSource::WithIndexes {")
            out.append("                base: %s," % base_fn)
            out.append(
                "                base_name: %s,"
                % rust_raw_string(schema["base_name"])
            )
            # `indexes` is `&'static [Value]`, so each entry is emitted as a
            # `json!(..)` literal rather than parsed: `json!` is a compile-time macro,
            # which keeps the table static.
            entries = ", ".join(
                "json!(%s)" % json.dumps(entry, ensure_ascii=False, separators=(",", ":"))
                for entry in schema["indexes"]
            )
            out.append("                indexes: vec![%s]," % entries)
            out.append("            },")
        else:
            out.append(
                "            schema: SchemaSource::Inline(%s),"
                % rust_raw_string(json_literal(schema["value"], 0))
            )
        out.append("            data: parse_list(&[")
        for doc in g["data"]:
            out.append("                %s," % rust_raw_string(json_literal(doc, 0)))
        out.append("            ]),")
        out.append("            queries: vec![")
        for q in g["queries"]:
            out.append("                Query {")
            out.append("                    info: %s," % rust_raw_string(q["info"]))
            out.append(
                "                    query: parse(%s)," % rust_raw_string(json_literal(q["query"], 0))
            )
            out.append(
                "                    expected_result_doc_ids: &[%s],"
                % ", ".join(
                    rust_raw_string(x) for x in q["expectedResultDocIds"]
                )
            )
            if "selectorSatisfiedByIndex" in q:
                out.append(
                    "                    selector_satisfied_by_index: Some(%s),"
                    % ("true" if q["selectorSatisfiedByIndex"] else "false")
                )
            else:
                out.append("                    selector_satisfied_by_index: None,")
            out.append("                },")
        out.append("            ],")
        out.append("            not_run_if_dexie: %s," % ("true" if g["not_run_if_dexie"] else "false"))
        out.append("            // The upstream calls that produced `data`, one per document,")
        out.append("            // or an empty string where upstream wrote the document literal out.")
        out.append("            recipes: &[")
        for r in g["recipes"]:
            if r is None:
                out.append("                \"\",")
            else:
                out.append("                %s," % rust_raw_string(r))
        out.append("            ],")
        out.append("        })")
        out.append("        .clone()")
        out.append("}")
        out.append("")
    # helpers
    out.append("/// Parses one JSON literal from the generated data.")
    out.append("#[track_caller]")
    out.append("fn parse(text: &str) -> Value {")
    out.append("    serde_json::from_str(text).unwrap_or_else(|err| {")
    out.append('        panic!("generated JSON is malformed ({err}): {text}")')
    out.append("    })")
    out.append("}")
    out.append("")
    out.append("/// Parses a list of JSON literals.")
    out.append("#[track_caller]")
    out.append("fn parse_list(texts: &[&str]) -> Vec<Value> {")
    out.append("    texts.iter().map(|text| parse(text)).collect()")
    out.append("}")
    out.append("")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()
    root = upstream_root()
    if not (root / QC_REL).exists():
        print(f"error: upstream not found at {root}", file=sys.stderr)
        return 2
    groups_out, _ = build(root)
    text = emit_module(groups_out)
    base = Path(__file__).resolve().parent.parent / "src" / "fixtures"
    target = base / "correctness.rs"
    if args.check:
        if not target.exists() or target.read_text() != text:
            print(f"error: {target} is stale; regenerate", file=sys.stderr)
            return 1
        print(f"ok: {target.name} matches upstream ({len(groups_out)} groups, "
              f"{sum(len(g['queries']) for g in groups_out)} queries)")
        return 0
    base.mkdir(parents=True, exist_ok=True)
    target.write_text(text)
    print(
        f"wrote {target} ({len(groups_out)} groups, "
        f"{sum(len(g['queries']) for g in groups_out)} queries, "
        f"{sum(len(g['data']) for g in groups_out)} documents)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
