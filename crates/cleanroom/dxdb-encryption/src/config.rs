//! Configuration: encrypted paths, the password floor, and the password verifier.
//!
//! Settles open question 1 in `specs/encryption.md`: the verifier is
//! `SHA-256("dxdb.enc.verifier.v1" || master_key)`. It is one-way, it reveals
//! nothing about the key, and testing a password guess against it still costs a
//! full Argon2id derivation, so it is not a cheaper oracle than trying to decrypt.

use serde_json::Value;
/// Domain separator for the verifier, so the verifier cannot be confused with any
/// other use of the master key.
const VERIFIER_DOMAIN: &[u8] = b"dxdb.enc.verifier.v1";

/// The schema's `encrypted` list, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedPaths {
    paths: Vec<String>,
}

/// Why `encrypted` was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The list is not an array of strings.
    #[error("schema.encrypted must be an array of strings")]
    NotAnArray,
    /// An entry is not a non-empty string.
    #[error("schema.encrypted contains an empty or non-string entry")]
    EmptyEntry,
    /// A child path of an already-encrypted parent. Encrypting both would encrypt
    /// the ciphertext again, producing data that cannot be read back
    /// (`specs/encryption.md` 1.2, INV-15).
    #[error("encrypted path {child:?} is a child of encrypted path {parent:?}")]
    NestedUnderEncrypted {
        /// The child path.
        child: String,
        /// The parent that already covers it.
        parent: String,
    },
}

impl EncryptedPaths {
    /// Reads and validates `schema.encrypted`.
    ///
    /// A missing or non-array value yields no paths (unencrypted), matching
    /// upstream. A present-but-malformed one is an error rather than being
    /// ignored, because silently not encrypting a field the schema asked to
    /// encrypt is the worst possible failure for this crate.
    pub fn from_schema(schema: &Value) -> Result<Self, PathError> {
        let Some(value) = schema.get("encrypted") else {
            return Ok(Self { paths: Vec::new() });
        };
        if value.is_null() {
            return Ok(Self { paths: Vec::new() });
        }
        let list = value.as_array().ok_or(PathError::NotAnArray)?;
        let mut paths = Vec::with_capacity(list.len());
        for entry in list {
            let path = entry.as_str().ok_or(PathError::EmptyEntry)?;
            if path.is_empty() {
                return Err(PathError::EmptyEntry);
            }
            paths.push(path.to_string());
        }
        Self::validated(paths)
    }

    /// Builds from an explicit list, validating the nesting rule.
    pub fn new(paths: impl IntoIterator<Item = String>) -> Result<Self, PathError> {
        Self::validated(paths.into_iter().collect())
    }

    fn validated(paths: Vec<String>) -> Result<Self, PathError> {
        for path in &paths {
            if path.is_empty() {
                return Err(PathError::EmptyEntry);
            }
        }
        // A child of an encrypted parent: check every pair. The list is tiny (a
        // schema never has many encrypted fields), so O(n^2) with a clear rule beats
        // a trie whose edge cases nobody reads.
        for child in &paths {
            for parent in &paths {
                if child != parent && is_child_path(child, parent) {
                    return Err(PathError::NestedUnderEncrypted {
                        child: child.clone(),
                        parent: parent.clone(),
                    });
                }
            }
        }
        Ok(Self { paths })
    }

    /// The paths.
    pub fn as_slice(&self) -> &[String] {
        &self.paths
    }

    /// True when nothing is encrypted.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Whether `field` is an encrypted path or a descendant of one.
    ///
    /// Descendants count because encrypting `assignee` makes `assignee.email`
    /// unreadable, so a query on the child is as impossible as one on the parent.
    pub fn covers(&self, field: &str) -> bool {
        self.paths
            .iter()
            .any(|path| field == path || is_child_path(field, path))
    }
}

/// True when `child` is `parent` followed by `.` and more path.
fn is_child_path(child: &str, parent: &str) -> bool {
    child.len() > parent.len()
        && child.starts_with(parent)
        && child.as_bytes().get(parent.len()) == Some(&b'.')
}

/// Reads the value at a dot path.
///
/// Mirrors `dxdb_storage::document::get_object_path`: object keys, with numeric
/// segments indexing arrays. Reimplemented here because the storage helper takes a
/// `DocumentData` and this crate needs the same traversal over a bare `Value` for
/// nested encrypted paths.
pub fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
        if current.is_null() {
            return None;
        }
    }
    Some(current)
}

/// Writes `value` at a dot path inside a JSON object.
///
/// Numeric segments index into arrays, matching [`get_path`]; a path that can be
/// read must be writable, or a nested encrypted path inside an array would seal and
/// then fail to open.
///
/// Returns `false` when a segment cannot be traversed (a scalar where a container is
/// expected, or an array index out of range), and in that case the document is left
/// exactly as it was: a partial write would silently truncate a document.
pub fn set_path(root: &mut serde_json::Map<String, Value>, path: &str, value: Value) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    let Some((last, parents)) = segments.split_last() else {
        return false;
    };
    // Build the new value bottom-up so a failure cannot leave a half-written path.
    // The returned option is the new *subtree*, and the caller installs it.
    fn write(container: &mut Value, segments: &[&str], last: &str, value: Value) -> Option<()> {
        if segments.is_empty() {
            match container {
                Value::Object(map) => {
                    map.insert(last.to_string(), value);
                    Some(())
                }
                Value::Array(items) => {
                    let index = last.parse::<usize>().ok()?;
                    if index >= items.len() {
                        return None;
                    }
                    items[index] = value;
                    Some(())
                }
                _ => None,
            }
        } else {
            let (segment, rest) = segments.split_first()?;
            let child = match container {
                Value::Object(map) => map
                    .entry((*segment).to_string())
                    .or_insert_with(|| Value::Object(serde_json::Map::new())),
                Value::Array(items) => {
                    let index = segment.parse::<usize>().ok()?;
                    items.get_mut(index)?
                }
                _ => return None,
            };
            write(child, rest, last, value)
        }
    }
    let mut holder = Value::Object(std::mem::take(root));
    let ok = write(&mut holder, parents, last, value).is_some();
    if let Value::Object(map) = holder {
        *root = map;
    }
    ok
}

/// A password, which this type will never print.
///
/// No `Debug`, no `Display`, no `Clone` into anything that could be logged. The
/// inner buffer is zeroed on drop.
pub struct Password(String);

impl Password {
    /// Wraps a password.
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }

    /// The password bytes, for the KDF.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// The length in bytes. Exposed so a strength report can be built without
    /// handing out the password itself.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when the password is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        // Best-effort wipe: `String` cannot guarantee the buffer was not copied by
        // the allocator, but leaving the bytes in freed memory is strictly worse.
        // Recorded as best-effort rather than claimed as a guarantee (T6 is out of
        // scope, `specs/encryption.md` 4.1).
        use zeroize::Zeroize as _;
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately useless: a password must never be printable, including
        // through a derived struct's `#[derive(Debug)]`.
        f.write_str("Password(<redacted>)")
    }
}

/// Why a password was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    /// An encrypted database was opened with no password.
    #[error("a password is required to open an encrypted collection")]
    PasswordRequired,
    /// The password is below the documented strength floor.
    #[error("password is too weak: {0}")]
    TooWeak(WeaknessReason),
}

/// Why a password is below the floor.
///
/// `specs/encryption.md` 1.5 and G-032: the floor is about guessability, not
/// length, so these are the properties that make a password trivially guessable
/// rather than an arbitrary character count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeaknessReason {
    /// Shorter than the entropy floor can reach with any character mix.
    TooShort,
    /// A single repeated character.
    RepeatedCharacter,
    /// A run of consecutive characters (`abcd`, `1234`).
    SequentialRun,
    /// Copies of one well-known password-shaped string.
    CommonPassword,
}

impl std::fmt::Display for WeaknessReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            WeaknessReason::TooShort => "shorter than the minimum length",
            WeaknessReason::RepeatedCharacter => "a single repeated character",
            WeaknessReason::SequentialRun => "a sequential run of characters",
            WeaknessReason::CommonPassword => "a well-known password",
        };
        f.write_str(text)
    }
}

/// The documented strength floor's minimum length.
///
/// Deliberately higher than upstream's `MINIMUM_PASSWORD_LENGTH = 8`
/// (`encryption-crypto-js/index.ts:32`). Twelve is not a magic security number;
/// it is the point below which the other checks stop helping, because a 7-character
/// string that is not repeated or sequential is still within reach of an offline
/// guesser that has a good dictionary. G-032 records why we diverge.
pub const MIN_PASSWORD_LEN: usize = 12;

/// A floor, not a strength meter.
///
/// This catches the passwords that are *certainly* guessable and passes everything
/// else, including long passphrases with no character-class variety. It is not a
/// zxcvbn-style estimator and the crate documentation says so: a password that
/// passes here can still be weak, and a caller that wants a real estimate should
/// run one above this layer.
pub fn check_password_strength(password: &Password) -> Result<(), PasswordError> {
    let text = password.as_bytes();
    if text.is_empty() {
        return Err(PasswordError::PasswordRequired);
    }
    if text.len() < MIN_PASSWORD_LEN {
        return Err(PasswordError::TooWeak(WeaknessReason::TooShort));
    }
    let bytes = text;
    if bytes.windows(2).all(|pair| pair[0] == pair[1]) {
        return Err(PasswordError::TooWeak(WeaknessReason::RepeatedCharacter));
    }
    // Order matters for the *reason* reported, not for the verdict: a password that
    // is both a well-known string and contains a sequential run is more usefully
    // described as well-known. The caller only ever acts on pass/fail, but a test
    // that names the reason should name the more specific one.
    if is_well_known(bytes) {
        return Err(PasswordError::TooWeak(WeaknessReason::CommonPassword));
    }
    if has_sequential_run(bytes, 6) {
        return Err(PasswordError::TooWeak(WeaknessReason::SequentialRun));
    }
    Ok(())
}

/// True when `run` consecutive bytes ascend or descend by one.
fn has_sequential_run(bytes: &[u8], run: usize) -> bool {
    if bytes.len() < run {
        return false;
    }
    let mut ascending = 1usize;
    let mut descending = 1usize;
    for window in bytes.windows(2) {
        if window[1] == window[0].wrapping_add(1) {
            ascending += 1;
        } else {
            ascending = 1;
        }
        if window[1].wrapping_add(1) == window[0] {
            descending += 1;
        } else {
            descending = 1;
        }
        if ascending >= run || descending >= run {
            return true;
        }
    }
    false
}

/// A short list of password shapes that appear at the top of every leak corpus.
///
/// Not a dictionary: a dictionary belongs in a caller's strength estimator. These
/// are the strings a *placeholder* uses, which no amount of KDF cost saves.
fn is_well_known(bytes: &[u8]) -> bool {
    const WELL_KNOWN: &[&[u8]] = &[
        b"password",
        b"passw0rd",
        b"password1",
        b"qwertyuiop",
        b"letmein",
        b"changeme",
        b"correcthorsebatterystaple",
        b"123456789012",
        b"administrator",
    ];
    let lowered: Vec<u8> = bytes.to_ascii_lowercase();
    // A well-known string with a short numeric suffix appended is the same password
    // shape with a digit on the end, which loses no meaningful entropy to an
    // attacker who is actually trying.
    let core = lowered
        .iter()
        .rev()
        .skip_while(|byte| byte.is_ascii_digit())
        .count();
    let trimmed = &lowered[..core];
    WELL_KNOWN
        .iter()
        .any(|known| trimmed == *known || lowered == *known)
}

/// The stored password check: `SHA-256("dxdb.enc.verifier.v1" || master_key)`.
///
/// One read, one hash, no KDF on the read side (the key is already derived by the
/// time this is compared).
pub fn verifier_for(master_key: &crate::keys::Key) -> [u8; 32] {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(VERIFIER_DOMAIN);
    hasher.update(master_key.as_ref());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Compares a stored verifier with the one `master_key` produces, in constant time.
///
/// A short-circuiting compare would leak how many leading bytes matched, which is
/// enough to reconstruct a verifier one byte at a time given enough attempts.
pub fn verifier_matches(master_key: &crate::keys::Key, stored: &[u8; 32]) -> bool {
    let computed = verifier_for(master_key);
    let mut difference = 0u8;
    for (a, b) in computed.iter().zip(stored.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::{
        check_password_strength, get_path, set_path, verifier_for, verifier_matches,
        EncryptedPaths, Password, PasswordError, WeaknessReason,
    };
    use serde_json::json;
    // Arc-15 in `specs/encryption.md`: declared fields encrypt, undeclared do not,
    // and the nesting rule is enforced in every build.
    #[test]
    fn nested_encrypted_paths_are_rejected() {
        let schema = json!({ "encrypted": ["assignee", "assignee.email"] });
        let error = EncryptedPaths::from_schema(&schema).expect_err("must be rejected");
        assert!(matches!(
            error,
            super::PathError::NestedUnderEncrypted { .. }
        ));
    }

    #[test]
    fn sibling_paths_are_allowed() {
        let schema = json!({ "encrypted": ["note", "assignee", "notebook"] });
        let paths = EncryptedPaths::from_schema(&schema).expect("valid");
        assert_eq!(paths.as_slice().len(), 3);
        // `notebook` is not a child of `note` even though it starts with it.
        assert!(paths.covers("note"));
        assert!(paths.covers("notebook"));
        assert!(!paths.covers("not"));
    }

    #[test]
    fn a_child_of_an_encrypted_parent_is_covered() {
        let paths = EncryptedPaths::new(["assignee".to_string()]).expect("valid");
        assert!(paths.covers("assignee"));
        assert!(paths.covers("assignee.email"));
        assert!(!paths.covers("assignees"));
    }

    #[test]
    fn a_missing_encrypted_list_means_no_encryption() {
        let paths = EncryptedPaths::from_schema(&json!({"primaryKey": "id"})).expect("valid");
        assert!(paths.is_empty());
    }

    #[test]
    fn a_malformed_encrypted_list_is_an_error_not_a_silent_skip() {
        assert!(EncryptedPaths::from_schema(&json!({"encrypted": "note"})).is_err());
        assert!(EncryptedPaths::from_schema(&json!({"encrypted": [1]})).is_err());
        assert!(EncryptedPaths::from_schema(&json!({"encrypted": [""]})).is_err());
    }

    fn root(value: &mut serde_json::Value) -> &mut serde_json::Map<String, serde_json::Value> {
        value.as_object_mut().expect("object")
    }

    #[test]
    fn dotted_paths_read_and_write() {
        let mut value = json!({"a": {"b": "x"}, "list": [{"c": 1}]});
        assert_eq!(get_path(&value, "a.b"), Some(&json!("x")));
        assert_eq!(get_path(&value, "list.0.c"), Some(&json!(1)));
        assert_eq!(get_path(&value, "a.missing"), None);
        assert!(set_path(root(&mut value), "a.d", json!(2)));
        assert!(set_path(root(&mut value), "list.0.e", json!(3)));
        assert_eq!(get_path(&value, "a.d"), Some(&json!(2)));
        assert_eq!(get_path(&value, "list.0.e"), Some(&json!(3)));
    }

    #[test]
    fn a_path_through_a_scalar_is_refused_rather_than_overwritten() {
        let mut value = json!({"a": "scalar"});
        assert!(!set_path(root(&mut value), "a.b", json!(1)));
        assert_eq!(value, json!({"a": "scalar"}));
    }

    #[test]
    fn the_password_floor_rejects_certainly_guessable_strings() {
        assert_eq!(
            check_password_strength(&Password::new("")),
            Err(PasswordError::PasswordRequired)
        );
        assert_eq!(
            check_password_strength(&Password::new("short")),
            Err(PasswordError::TooWeak(WeaknessReason::TooShort))
        );
        assert_eq!(
            check_password_strength(&Password::new("aaaaaaaaaaaaaaa")),
            Err(PasswordError::TooWeak(WeaknessReason::RepeatedCharacter))
        );
        assert_eq!(
            check_password_strength(&Password::new("abcdefghijklm")),
            Err(PasswordError::TooWeak(WeaknessReason::SequentialRun))
        );
        assert_eq!(
            check_password_strength(&Password::new("correcthorsebatterystaple")),
            Err(PasswordError::TooWeak(WeaknessReason::CommonPassword))
        );
        assert_eq!(
            check_password_strength(&Password::new("password123456")),
            Err(PasswordError::TooWeak(WeaknessReason::CommonPassword))
        );
    }

    #[test]
    fn the_password_floor_passes_a_passphrase_and_a_random_string() {
        assert!(check_password_strength(&Password::new("blue cartridge lantern")).is_ok());
        assert!(check_password_strength(&Password::new("Xk7#pQ2vL9mZ")).is_ok());
        // Long but with no character-class variety is still fine: the floor is about
        // guessability, not composition rules.
        assert!(check_password_strength(&Password::new("this is my long pass phrase")).is_ok());
    }

    // INV-11: a password must never be printable.
    #[test]
    fn a_password_does_not_print() {
        let password = Password::new("hunter2-but-longer");
        assert_eq!(format!("{password:?}"), "Password(<redacted>)");
    }

    // INV-8: the verifier distinguishes keys and nothing else.
    #[test]
    fn the_verifier_is_one_way_and_key_specific() {
        let a: crate::keys::Key = zeroize::Zeroizing::new([1u8; 32]);
        let b: crate::keys::Key = zeroize::Zeroizing::new([2u8; 32]);
        let verifier_a = verifier_for(&a);
        assert!(verifier_matches(&a, &verifier_a));
        assert!(!verifier_matches(&b, &verifier_a));
        // The verifier is not the key, and not a prefix of it.
        assert_ne!(verifier_a, *a);
    }
}
