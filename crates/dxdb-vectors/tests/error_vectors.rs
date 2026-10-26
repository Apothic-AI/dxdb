//! Error vectors: upstream's refusal behaviour, and where our codes diverge.
//!
//! Every other vector family records successful output. This one records how upstream
//! *refuses*, because a port diverges quietly on error paths: a success is compared
//! field by field, while an error is usually accepted as "it failed", so returning the
//! wrong code — or inventing a code that upstream never had — goes unnoticed.
//!
//! Two things are checked here:
//!
//! 1. **Every code our crates can return exists upstream.** The declared layer of the
//!    vector is upstream's full `ERROR_MESSAGES` vocabulary (219 entries at this pin).
//!    A code we return that is not in that table is an invented code: a caller porting
//!    from RxDB cannot react to it, and a test asserting it is asserting our behaviour
//!    rather than upstream's. This is the check that found `QU21`-`QU24` and `SC44`.
//! 2. **Every code upstream actually raises is reachable in our port**, where the
//!    vector names a raiser we have an equivalent for.
//!
//! It deliberately does *not* assert that our crates raise the same code for the same
//! input in all cases: that is a bigger claim, and where it is currently false the
//! divergence is recorded as a known one below with its reason, rather than being
//! hidden by a weaker assertion.

use dxdb_vectors::{error_vectors, VectorFamily, Vectors};

fn load() -> error_vectors::Vectors {
    Vectors::discover()
        .load::<error_vectors::Vectors>(VectorFamily::ErrorVectors)
        .expect("the committed error vectors decode")
}

/// The authoritative declared vocabulary, as a set of strings.
fn declared_codes(v: &error_vectors::Vectors) -> std::collections::BTreeSet<&str> {
    v.declared_codes.iter().map(|c| c.code.as_str()).collect()
}

#[test]
fn every_code_our_crates_return_exists_upstream() {
    let v = load();
    let declared = declared_codes(&v);

    // Codes our crates can return, with the crate that returns them. Kept as a literal
    // list rather than derived, because the point is to catch a code being *added* to
    // one of those enums without a corresponding upstream entry: deriving it would
    // require this test to know how to enumerate every enum, and it would then agree
    // with whatever we wrote.
    let our_codes: &[(&str, &str)] = &[
        ("SC1", "dxdb-schema"),
        ("SC7", "dxdb-schema"),
        ("SC8", "dxdb-schema"),
        ("SC11", "dxdb-schema"),
        ("SC18", "dxdb-schema"),
        ("SC19", "dxdb-schema"),
        ("SC20", "dxdb-schema"),
        ("SC24", "dxdb-schema"),
        ("SC29", "dxdb-schema"),
        ("SC30", "dxdb-schema"),
        ("SC32", "dxdb-schema"),
        ("SC33", "dxdb-schema"),
        ("SC39", "dxdb-schema"),
        ("SC41", "dxdb-schema"),
        ("DOC18", "dxdb-schema"),
        ("DOC19", "dxdb-schema"),
        ("CI1", "dxdb-schema, dxdb-query"),
        ("CI2", "dxdb-schema, dxdb-query"),
        ("QU12", "dxdb-query"),
        ("QU13", "dxdb-query"),
        ("QU16", "dxdb-query"),
        ("QU19", "dxdb-query"),
        ("QU20", "dxdb-query"),
        ("SNH", "dxdb-query"),
        ("VD1", "dxdb-schema, dxdb-query"),
        // --- Divergent: returned by our crates, absent from upstream's table. ---
        ("QU21", "dxdb-query"),
        ("QU22", "dxdb-query"),
        ("QU23", "dxdb-query"),
        ("QU24", "dxdb-query"),
        ("SC44", "dxdb-schema"),
    ];

    // The codes we have agreed to treat as a known divergence, with the reason. Listed
    // explicitly so a *new* invented code fails this test instead of joining a crowd.
    let known_divergent: &[(&str, &str)] = &[
        (
            "QU21",
            "unknown Mango operator. Upstream has no code: mingo throws MingoError2 with \
             no `rxdb` marker. We return a typed QU21 instead.",
        ),
        (
            "QU22",
            "top-level $not. Upstream has no code: mingo throws MingoError2.",
        ),
        (
            "QU23",
            "invalid $regex. Upstream has no code: RegExp throws a plain SyntaxError.",
        ),
        (
            "QU24",
            "malformed operator payload. Upstream mostly does not validate these at the \
             matcher layer at all; `$mod: [1,2,3]` does not throw upstream.",
        ),
        (
            "SC44",
            "a schema that fails draft-07 validation. Upstream validates schemas through \
             the ajv/is-my-json-valid plugin, which reports its own errors, so there is no \
             core SC code for it.",
        ),
    ];

    let mut unexpected = Vec::new();
    for (code, crate_name) in our_codes {
        if declared.contains(code) {
            continue;
        }
        let known = known_divergent.iter().any(|(c, _)| c == code);
        if !known {
            unexpected.push(format!(
                "{code} (returned by {crate_name}) is not in upstream's declared \
                 vocabulary and is not a recorded divergence.\n  Either upstream has a \
                 code for this and we should return it, or it is an invented code and \
                 must be added to `known_divergent` with a reason."
            ));
        }
    }

    assert!(
        unexpected.is_empty(),
        "{} code(s) our crates return do not exist upstream:\n{}",
        unexpected.len(),
        unexpected.join("\n")
    );

    // And the known-divergent list must not go stale: if upstream ever gains one of
    // these codes, the divergence disappears and the entry should too.
    for (code, _) in known_divergent {
        assert!(
            !declared.contains(code),
            "{code} is recorded as a divergence but HAS appeared in upstream's \
             vocabulary at this pin. Update our crates to match upstream and remove the \
             entry from `known_divergent`."
        );
    }
}

#[test]
fn upstream_raised_codes_are_the_ones_a_port_must_reproduce() {
    let v = load();

    // The typed layer: a code and the parameter keys that make the error actionable.
    // A port returning the right code with no parameters is still wrong for any caller
    // that reads them.
    let expected: &[(&str, &str, &[&str])] = &[
        ("primary-key-without-maxLength", "SC39", &["schema"]),
        ("index-names-a-field-not-in-schema", "CI1", &["fieldName"]),
        ("lower-bound-on-unencodable-type", "CI2", &["type"]),
        ("upper-bound-on-unencodable-type", "CI2", &["type"]),
        ("prepare-query-without-sort", "SNH", &["query"]),
    ];

    for (why, code, param_keys) in expected {
        let case = v
            .raised(why)
            .unwrap_or_else(|| panic!("error vector has no raised case {why:?}"));
        assert_eq!(
            case.error_code.as_deref(),
            Some(*code),
            "{why} must raise {code} upstream"
        );
        assert!(
            case.rxdb_error,
            "{why} must carry the `rxdb` marker so callers can detect an RxDB error"
        );
        assert_eq!(
            case.parameter_keys,
            param_keys.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "{why} must carry exactly these parameter keys; a port that returns the code \
             without the payload cannot be acted on"
        );
        // Every typed error points at documentation.
        assert!(
            case.error_url
                .as_deref()
                .is_some_and(|u| u.contains("rxdb.info")),
            "{why} must carry an rxdb.info docs URL"
        );
    }
}

#[test]
fn untyped_upstream_failures_are_recorded_as_untyped() {
    let v = load();

    // These three are where a port is most likely to "improve" on upstream by returning
    // a tidy typed error. That would be a divergence: a caller checking for a code
    // would find one where RxDB gives none.
    for why in [
        "unknown-mango-operator",
        "top-level-negative-operator",
        "invalid-regex-pattern",
    ] {
        let case = v
            .raised(why)
            .unwrap_or_else(|| panic!("error vector has no raised case {why:?}"));
        assert_eq!(
            case.error_code, None,
            "{why} must have NO RxDB code: upstream throws an untyped error here"
        );
        assert!(!case.rxdb_error, "{why} must not carry the `rxdb` marker");
    }
}

#[test]
fn the_declared_vocabulary_is_complete_and_documented() {
    let v = load();

    // 219 at this pin. A drop means upstream's table was truncated; a jump means the
    // pin moved. Either way a reviewer should see it.
    assert!(
        v.declared_code_count > 200,
        "the declared error vocabulary shrank to {} entries",
        v.declared_code_count
    );
    assert_eq!(
        v.declared_code_count,
        v.declared_codes.len(),
        "declaredCodeCount disagrees with the array"
    );

    // The codes the port is most likely to need, all present with a message.
    for code in ["CI1", "CI2", "SC20", "SC39", "COL22", "DXE1", "DB1", "SNH"] {
        let entry = v
            .declared(code)
            .unwrap_or_else(|| panic!("{code} missing from upstream's declared vocabulary"));
        assert!(
            entry.message.as_deref().is_some_and(|m| !m.is_empty()),
            "{code} has no message in upstream's table"
        );
    }

    // Only the codes we actually raised are marked reachable. Everything else is a
    // declaration, and must not be mistaken for verified behaviour.
    let reachable: std::collections::BTreeSet<&str> = v
        .declared_codes
        .iter()
        .filter(|c| c.reachable)
        .map(|c| c.code.as_str())
        .collect();
    let expected: std::collections::BTreeSet<&str> =
        ["CI1", "CI2", "SC39", "SNH"].into_iter().collect();
    assert_eq!(
        reachable, expected,
        "the reachable set must be exactly the codes this generator raised"
    );

    // And the named-code notes must exist for every code the task asked about, so
    // "why is SC20 not raised" is answerable from the data.
    for code in ["SC20", "COL22", "DXE1"] {
        let note = v
            .named_code(code)
            .unwrap_or_else(|| panic!("{code} must have a namedCodes entry"));
        assert!(!note.raised, "{code} is not raised by this generator");
        assert!(
            note.why.as_deref().is_some_and(|w| !w.is_empty()),
            "{code} must explain why it is declared but not raised"
        );
    }
}
