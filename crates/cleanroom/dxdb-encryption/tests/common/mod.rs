//! Shared helpers for the encryption tests.
#![allow(dead_code)]

use std::sync::Arc;

use dxdb_encryption::{
    schema_hash, EncryptedPaths, EncryptionConfig, EncryptionStorage, EntropySource, FixedEntropy,
    KdfParams, KeyMaterial, Keys, Password,
};
use dxdb_storage::{
    DocumentData, DynRxStorageInstance, InstanceCreationParams, RxStorage, RxStorageInstance,
    StorageSchema,
};

use crate::fake::FakeStorage;

/// Cheap Argon2id parameters for tests.
///
/// The cheap set exercises the *same* code path as the documented one; only the cost
/// differs. Using 64 MiB in every test would add ~200 ms per test for no additional
/// assurance, and the real parameters are exercised once, deliberately, in the KDF
/// timing probe.
pub fn cheap_params() -> KdfParams {
    KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    }
}

/// A wrapped storage plus the pieces a test needs to inspect it.
pub struct Harness {
    /// The wrapped storage.
    pub storage: EncryptionStorage<FakeStorage>,
    /// The inner fake, for asserting what actually got stored.
    pub inner: FakeStorage,
    /// The open keys.
    pub keys: Arc<Keys>,
    /// The persisted material.
    pub material: KeyMaterial,
    /// The schema value.
    pub schema: serde_json::Value,
    /// The password, so a test can reopen.
    pub password: String,
}

/// Builds a harness with cheap KDF parameters.
pub fn harness_with(schema_value: serde_json::Value, password: &str) -> Harness {
    build(schema_value, password, cheap_params())
}

/// Builds a harness with the documented parameters. Used by exactly one test.
pub fn harness_with_real_kdf(schema_value: serde_json::Value, password: &str) -> Harness {
    build(
        schema_value,
        password,
        KdfParams::RFC9106_SECOND_RECOMMENDED,
    )
}

fn build(schema_value: serde_json::Value, password: &str, params: KdfParams) -> Harness {
    let inner = FakeStorage::new();
    let config = EncryptionConfig::from_schema(&schema_value).expect("valid config");
    let entropy: Arc<dyn EntropySource> = Arc::new(FixedEntropy::new());

    // Derive with the requested parameters, then hand-build the material so the
    // verifier matches the derivation the test will reopen with.
    let salt = [42u8; 16];
    let master = params
        .derive_master_key(password.as_bytes(), &salt)
        .expect("derive");
    let material = KeyMaterial {
        salt,
        params,
        verifier: dxdb_encryption::config::verifier_for(&master),
        current_generation: 1,
        wrapped_keys: Vec::new(),
    };
    let keys = Keys::unlock(
        Password::new(password),
        material.clone(),
        schema_hash(&schema_value),
        Arc::clone(&entropy),
    )
    .expect("unlock");
    let keys = Arc::new(keys);
    let storage = EncryptionStorage::new(inner.clone(), Arc::clone(&keys), config);
    Harness {
        storage,
        inner,
        keys,
        material,
        schema: schema_value,
        password: password.to_string(),
    }
}

impl Harness {
    /// Creates the wrapped instance for the harness schema.
    pub fn instance(&self) -> Box<DynRxStorageInstance<'static>> {
        futures::executor::block_on(self.storage.create_storage_instance(self.params()))
            .expect("instance")
    }

    /// The creation params for the harness schema.
    pub fn params(&self) -> InstanceCreationParams {
        InstanceCreationParams {
            database_instance_token: "token".to_string(),
            database_name: "testdb".to_string(),
            collection_name: "humans".to_string(),
            schema: StorageSchema::from_value(&self.schema).expect("valid schema"),
            options: serde_json::json!({}),
            multi_instance: false,
            dev_mode: true,
        }
    }

    /// Writes one document through the wrapper.
    pub fn write(&self, instance: &DynRxStorageInstance<'_>, document: DocumentData) {
        let response = futures::executor::block_on(
            instance.bulk_write(vec![insert(document)], "ctx".to_string()),
        )
        .expect("write");
        assert!(response.is_ok(), "write failed: {response:?}");
    }

    /// The single stored document, as the inner storage holds it.
    pub fn stored(&self, id: &str) -> DocumentData {
        self.inner
            .documents()
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("document {id} is not stored"))
    }
}

/// An insert row.
pub fn insert(document: DocumentData) -> dxdb_storage::BulkWriteRow {
    dxdb_storage::BulkWriteRow::insert(dxdb_storage::DocumentWriteData::new(document))
}

/// Config with `paths` encrypted.
pub fn config_for(paths: &[&str]) -> EncryptionConfig {
    EncryptionConfig {
        encrypted: EncryptedPaths::new(paths.iter().map(|p| (*p).to_string())).expect("valid"),
        attachments_encrypted: false,
    }
}

/// The stored string at `path`.
pub fn stored_string(document: &DocumentData, path: &str) -> String {
    let value = dxdb_storage::get_object_path(document.as_value(), path)
        .cloned()
        .unwrap_or_else(|| panic!("no value at {path}"));
    match value {
        serde_json::Value::String(text) => text,
        other => panic!("expected a string at {path}, got {other:?}"),
    }
}

/// Unused re-exports kept so the module compiles when a test binary uses a subset.
const _: () = {
    let _ = std::mem::size_of::<KeyMaterial>();
    let _ = std::mem::size_of::<Password>();
};
