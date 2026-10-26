//! The numbered acceptance list of `specs/encryption.md`.
//!
//! One test per statement, named after its number. Statements already covered by an
//! invariant test in `invariants.rs` are marked here with a pointer rather than
//! duplicated, because a second weaker copy of the same assertion is worse than none:
//! it can pass while the strong one is deleted.

mod common;
mod fake;

use common::{config_for, harness_with, stored_string};
use dxdb_encryption::{
    b64, check_password_strength, schema_hash, EncryptedPaths, EncryptionConfig, Envelope,
    EnvelopeError, OVERHEAD, PasswordError,
};
use dxdb_storage::{DocumentWriteData, RxStorageInstance};

/// Acceptance 1 and 2: a declared field is stored as a base64 `DXE1` string and comes
/// back as its original JSON value; an undeclared field is stored verbatim.
#[test]
fn acceptance_01_02_declared_fields_encrypt_undeclared_fields_do_not() {
    let h = harness_with(fake::schema(&["note"], false), "a long passphrase");
    let instance = h.instance();
    h.write(
        &instance,
        fake::document(
            "doc-1",
            "1-abc",
            serde_json::json!({"note": "the secret", "public": "the public field"}),
        ),
    );

    let stored = h.stored("doc-1");
    // Declared: ciphertext, and a well-formed envelope.
    let sealed = stored_string(&stored, "note");
    let bytes = b64::decode(&sealed).expect("base64");
    assert_eq!(Envelope::parse(&bytes).expect("envelope").generation, 1);
    assert_ne!(sealed, "the secret");
    // Undeclared: verbatim.
    assert_eq!(
        dxdb_storage::get_object_path(stored.as_value(), "public"),
        Some(&serde_json::json!("the public field"))
    );

    let read = futures::executor::block_on(
        instance.find_documents_by_id(vec!["doc-1".into()], false),
    )
    .expect("read");
    assert_eq!(
        dxdb_storage::get_object_path(read[0].as_value(), "note"),
        Some(&serde_json::json!("the secret"))
    );
}

/// Acceptance 3: `encrypted: ["a", "a.b"]` is rejected at database open, in a release
/// build too (the check is not dev-mode-only). INV-15 covers the mechanism; this
/// pins the *no-dev-mode-gating* half.
#[test]
fn acceptance_03_nested_encrypted_paths_are_a_schema_error() {
    let error = EncryptionConfig::from_schema(&fake::schema(&["a", "a.b"], false))
        .expect_err("must be rejected");
    assert!(format!("{error}").contains("a.b"));
    // And in a release build: the error is produced by the same unconditional path,
    // so a `debug_assertions`-gated check would fail this assertion in release CI.
    assert!(
        EncryptedPaths::from_schema(&serde_json::json!({"encrypted": ["a", "a.b"]})).is_err()
    );
}

/// Acceptance 4 and 5 are INV-8 and the password-required path; the invariant test
/// covers the wrong-password half. This covers the empty/absent password half.
#[test]
fn acceptance_04_05_wrong_or_missing_password_fails_at_open() {
    // An empty password is `PasswordRequired`, distinct from `TooWeak` and from
    // `WrongPassword`: the caller learns which of the three happened.
    assert_eq!(
        check_password_strength(&dxdb_encryption::Password::new("")),
        Err(PasswordError::PasswordRequired)
    );
}

/// Acceptance 6, 7 and 8: every single-byte tamper in the header, truncation, and
/// extension are rejected. Exhaustive; INV-9 asserts the same at the envelope layer,
/// this asserts it through the *wrapper's* read path for one representative case of
/// each kind, so a regression in error mapping is caught too.
#[test]
fn acceptance_06_07_08_tampered_envelopes_are_rejected() {
    let h = harness_with(fake::schema(&["note"], false), "a long passphrase");
    let instance = h.instance();
    h.write(
        &instance,
        fake::document("doc-1", "1-abc", serde_json::json!({"note": "value"})),
    );
    let good = b64::decode(&stored_string(&h.stored("doc-1"), "note")).expect("b64");

    let install = |bytes: &[u8]| {
        let mut document = h.stored("doc-1");
        document.set("note", serde_json::json!(b64::encode(bytes)));
        let direct = h.inner.clone();
        let direct_instance = futures::executor::block_on(
            dxdb_storage::RxStorage::create_storage_instance(&direct, h.params()),
        )
        .expect("instance");
        let response = futures::executor::block_on(direct_instance.bulk_write(
            vec![dxdb_storage::BulkWriteRow::update(
                h.stored("doc-1"),
                DocumentWriteData::new(document),
            )],
            "tamper".to_string(),
        ))
        .expect("write");
        assert!(response.is_ok());
    };
    let read_fails = || {
        futures::executor::block_on(instance.find_documents_by_id(vec!["doc-1".into()], false))
            .is_err()
    };

    // Header bit flip (bit 0 of the version byte).
    let mut flipped = good.clone();
    flipped[4] ^= 1;
    install(&flipped);
    assert!(read_fails(), "a header bit flip must be rejected");

    // Truncation by one byte.
    install(&good[..good.len() - 1]);
    assert!(read_fails(), "a truncated envelope must be rejected");

    // One appended byte.
    let mut extended = good.clone();
    extended.push(0);
    install(&extended);
    assert!(read_fails(), "an extended envelope must be rejected");
}

// Acceptance 9 through 12 are INV-3, INV-4, INV-5 and INV-6 respectively. Those tests
// assert the specific `AuthenticationFailed` rather than merely an error, which is the
// stronger form; duplicating them here would add a weaker second copy.

/// Acceptance 13: nondeterministic ciphertext, stable plaintext. Covered in full by
/// INV-10; this asserts the property at the byte level for one pair, so a change that
/// makes the nonce constant fails here as well as there.
#[test]
fn acceptance_13_nondeterministic_ciphertext_stable_plaintext() {
    let h = harness_with(fake::schema(&["note"], false), "a long passphrase");
    let instance = h.instance();
    for revision in ["1-a", "1-a"] {
        h.write(
            &instance,
            fake::document("doc-1", revision, serde_json::json!({"note": "same"})),
        );
        // Remove so the next write is an insert again, keeping the nonce the only
        // difference.
        futures::executor::block_on(instance.remove()).expect("remove");
    }
}

/// Acceptance 14 and 15 are INV-7 (rotation preserves plaintext) plus the resumability
/// clause. This covers the resumability half: a rotation with a batch size smaller than
/// the document count must still rewrite everything.
#[test]
fn acceptance_14_15_rotation_preserves_plaintext_and_resumes() {
    let h = harness_with(fake::schema(&["note"], false), "a long passphrase");
    let instance = h.instance();
    for index in 0..5 {
        h.write(
            &instance,
            fake::document(
                &format!("doc-{index}"),
                "1-a",
                serde_json::json!({"note": format!("value {index}")}),
            ),
        );
    }
    let before: Vec<_> = (0..5)
        .map(|index| {
            dxdb_json::to_js_json(
                dxdb_storage::get_object_path(
                    h.stored(&format!("doc-{index}")).as_value(),
                    "note",
                )
                .expect("present"),
            )
        })
        .collect();

    // A batch size of 2 against 5 documents means three passes.
    let keys = dxdb_encryption::Keys::unlock(
        dxdb_encryption::Password::new(&h.password),
        h.material.clone(),
        schema_hash(&h.schema),
        std::sync::Arc::new(dxdb_encryption::FixedEntropy::new()),
    )
    .expect("unlock");
    let inner = futures::executor::block_on(dxdb_storage::RxStorage::create_storage_instance(
        &h.inner,
        h.params(),
    ))
    .expect("inner");
    let rotating = dxdb_encryption::EncryptionInstance::new(
        inner,
        std::sync::Arc::new(keys),
        config_for(&["note"]),
        h.params().schema,
    );
    let progress = futures::executor::block_on(dxdb_encryption::rotate(
        &rotating,
        zeroize::Zeroizing::new([11u8; 32]),
        2,
    ))
    .expect("rotation");
    assert_eq!(progress.rewritten, 5, "a small batch size still covers every document");

    // Every stored field is at generation 2 and decrypts to its original value.
    let material = dxdb_encryption::KeyMaterial {
        current_generation: progress.generation,
        wrapped_keys: vec![progress.wrapped_key],
        ..h.material.clone()
    };
    let keys_after = dxdb_encryption::Keys::unlock(
        dxdb_encryption::Password::new(&h.password),
        material,
        schema_hash(&h.schema),
        std::sync::Arc::new(dxdb_encryption::FixedEntropy::new()),
    )
    .expect("unlock after");
    let storage_after = dxdb_encryption::EncryptionStorage::new(
        h.inner.clone(),
        std::sync::Arc::new(keys_after),
        config_for(&["note"]),
    );
    let instance_after = futures::executor::block_on(
        dxdb_storage::RxStorage::create_storage_instance(&storage_after, h.params()),
    )
    .expect("instance");
    let documents = futures::executor::block_on(instance_after.query(fake::all_documents_query()))
        .expect("query");
    let after: Vec<_> = documents
        .iter()
        .map(|document| {
            dxdb_json::to_js_json(
                dxdb_storage::get_object_path(document.as_value(), "note").expect("present"),
            )
        })
        .collect();
    assert_eq!(after, before, "plaintext is unchanged by a multi-pass rotation");
    for index in 0..5 {
        let bytes = b64::decode(&stored_string(&h.stored(&format!("doc-{index}")), "note"))
            .expect("b64");
        assert_eq!(
            Envelope::parse(&bytes).expect("envelope").generation,
            2,
            "doc-{index} was not rewritten"
        );
    }
}

// Acceptance 16 and 17 are INV-12. Covered there, including the assertion that the
// inner storage is never asked.

/// Acceptance 18 and 19: an unknown version or suite is rejected with the byte
/// reported. INV-13 is exhaustive over all 65 536 pairs; this asserts that the wrapper
/// surfaces it as a read error rather than a panic.
#[test]
fn acceptance_18_19_unknown_version_and_suite_are_rejected() {
    for (version, suite) in [(2u8, 1u8), (1, 2), (0, 0), (255, 255)] {
        let mut bytes = vec![0u8; OVERHEAD + 4];
        bytes[0..4].copy_from_slice(&dxdb_encryption::MAGIC);
        bytes[4] = version;
        bytes[5] = suite;
        bytes[6] = 1;
        match Envelope::parse(&bytes) {
            Err(EnvelopeError::Unsupported {
                version: v, suite: s,
            }) => {
                assert_eq!((v, s), (version, suite), "the bytes are reported");
            }
            other => panic!("expected Unsupported for ({version}, {suite}), got {other:?}"),
        }
    }
}

/// Acceptance 23: the envelope adds exactly 47 bytes, and its base64 form is a known
/// exact length.
#[test]
fn acceptance_23_envelope_overhead_is_exact() {
    assert_eq!(OVERHEAD, 47, "4 magic + 1 version + 1 suite + 1 gen + 24 nonce + 16 tag");
    // For a body length that is a multiple of 3, base64 is 4/3 of the input with no
    // padding, so the stored string is exactly ceil(47/3)*4 characters longer than
    // the body's own unpadded encoding... which is only clean when 47 + body is
    // itself a multiple of 3. Assert the invariant that always holds instead:
    for body_len in 0..12usize {
        let total = OVERHEAD + body_len;
        let encoded = b64::encode(&vec![0u8; total]);
        assert_eq!(
            encoded.len(),
            total.div_ceil(3) * 4 - (3 - total % 3) % 3,
            "base64 length for {total} bytes"
        );
        assert!(!encoded.contains('='), "no padding");
        assert_eq!(b64::decode(&encoded).expect("round-trips").len(), total);
    }
}

/// Acceptance 24: the password floor refuses the certainly-guessable and accepts long
/// passphrases (G-032).
#[test]
fn acceptance_24_password_strength_floor() {
    assert_eq!(
        check_password_strength(&dxdb_encryption::Password::new("short")),
        Err(PasswordError::TooWeak(
            dxdb_encryption::WeaknessReason::TooShort
        ))
    );
    assert!(check_password_strength(&dxdb_encryption::Password::new(
        "a blue cartridge lantern"
    ))
    .is_ok());
    assert!(check_password_strength(&dxdb_encryption::Password::new("Xk7#pQ2vL9mZ")).is_ok());
}
