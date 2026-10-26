//! Revisions: `{height}-{databaseInstanceToken}` (PLAN.md 4.3).
//!
//! Upstream `src/plugins/utils/utils-revision.ts`:
//!
//! ```text
//! createRevision:       1 for a new document, previous height + 1 otherwise
//! parseRevision:        splits on the FIRST '-'  -> { height, hash }
//! getHeightOfRevision:  same parse, but allocation-free
//! ```
//!
//! The "hash" half is *not* a hash: upstream passes the database instance token,
//! so two databases writing the same document produce different but
//! height-comparable revisions. A port that emitted a content hash would break
//! conflict detection, so the field is named after what it is here.
//!
//! Heights are `u32`, not `usize`: the value comes off the wire as a decimal
//! string, and a height that does not fit in 32 bits is corrupt data, not a
//! platform-dependent edge.

use std::fmt;

/// The two halves of a revision string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedRevision<'a> {
    /// The revision height: 1 for a new document, `previous + 1` afterwards.
    pub height: u32,
    /// The database instance token the revision was created with.
    pub token: &'a str,
}

/// A revision string that could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RevisionError {
    /// No `-` separator.
    #[error("malformatted revision (no '-' separator): {0:?}")]
    MissingSeparator(String),
    /// The height half was empty, non-numeric, or out of range.
    #[error("malformatted revision height in {revision:?}")]
    BadHeight {
        /// The revision that failed.
        revision: String,
    },
}

/// Creates the revision for a write: `1-{token}` for a new document,
/// `{previous_height + 1}-{token}` when updating.
///
/// Mirrors `createRevision` (`utils-revision.ts:59`).
///
/// # Examples
///
/// ```
/// # use dxdb_storage::create_revision;
/// assert_eq!(create_revision("token", None), "1-token");
/// assert_eq!(create_revision("token", Some("1-x")), "2-token");
/// assert_eq!(create_revision("token", Some("9-x")), "10-token");
/// ```
pub fn create_revision(database_instance_token: &str, previous_revision: Option<&str>) -> String {
    match previous_revision {
        None => format!("1-{database_instance_token}"),
        Some(previous) => {
            // `get_height_of_revision` on a malformed previous revision would
            // panic in upstream too; the caller owns the invariant that
            // `previous` came out of a storage.
            let height = get_height_of_revision(previous);
            format!("{}-{}", height.saturating_add(1), database_instance_token)
        }
    }
}

/// Parses a revision into `(height, token)`.
///
/// Accepts multi-digit heights and splits on the *first* `-`, so a token that
/// itself contains `-` (some storages use uuids-with-dashes) survives.
pub fn parse_revision(revision: &str) -> Result<ParsedRevision<'_>, RevisionError> {
    let dash = revision
        .find('-')
        .ok_or_else(|| RevisionError::MissingSeparator(revision.to_string()))?;
    let height = parse_height(&revision[..dash]).ok_or_else(|| RevisionError::BadHeight {
        revision: revision.to_string(),
    })?;
    Ok(ParsedRevision {
        height,
        token: &revision[dash + 1..],
    })
}

/// The revision height, without allocating.
///
/// This is the hot path (`utils-revision.ts:40`): every conflict check and every
/// replicate comparison asks for it. Panics on a malformed revision, matching
/// upstream's `newRxError('SNH')` — a malformed revision reaching here is a
/// programming error, and returning a sentinel height would silently corrupt
/// conflict resolution. Use [`try_get_height_of_revision`] at an untrusted
/// boundary such as replication input.
///
/// # Panics
///
/// Panics if `revision` is malformed.
pub fn get_height_of_revision(revision: &str) -> u32 {
    try_get_height_of_revision(revision)
        .unwrap_or_else(|| panic!("malformatted revision: {revision:?}"))
}

/// The revision height, or `None` if the revision is malformed.
pub fn try_get_height_of_revision(revision: &str) -> Option<u32> {
    let dash = revision.find('-')?;
    parse_height(&revision[..dash])
}

fn parse_height(digits: &str) -> Option<u32> {
    // Upstream loops `height = height * 10 + (charCode - 48)` with no validation,
    // so a leading '+' or an empty prefix would produce garbage rather than an
    // error. Rejecting is the only safe reading of the same intent.
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // u32 overflow is corrupt data rather than a panic.
    digits.parse::<u32>().ok()
}

impl fmt::Display for ParsedRevision<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.height, self.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_height_one_for_a_new_document() {
        assert_eq!(create_revision("db1", None), "1-db1");
    }

    #[test]
    fn increments_the_previous_height() {
        assert_eq!(create_revision("db1", Some("1-abc")), "2-db1");
        assert_eq!(create_revision("db1", Some("9-abc")), "10-db1");
        assert_eq!(create_revision("db1", Some("99-abc")), "100-db1");
    }

    #[test]
    fn multi_digit_heights_parse() {
        // PLAN.md 4.3: "parseRevision accepts multi-digit heights".
        assert_eq!(
            parse_revision("1-abc").expect("valid"),
            ParsedRevision {
                height: 1,
                token: "abc"
            }
        );
        assert_eq!(parse_revision("123-abc").expect("valid").height, 123);
        assert_eq!(get_height_of_revision("4294967295-x"), u32::MAX);
    }

    #[test]
    fn splits_on_the_first_dash_so_dashed_tokens_survive() {
        let parsed = parse_revision("7-abc-def-ghi").expect("valid");
        assert_eq!(parsed.height, 7);
        assert_eq!(parsed.token, "abc-def-ghi");
    }

    #[test]
    fn malformed_revisions_are_errors_not_garbage_heights() {
        assert_eq!(
            parse_revision("nodash"),
            Err(RevisionError::MissingSeparator("nodash".to_string()))
        );
        assert!(parse_revision("-abc").is_err());
        assert!(parse_revision("1x-abc").is_err());
        assert!(parse_revision("+1-abc").is_err());
        assert_eq!(try_get_height_of_revision("nope"), None);
    }

    #[test]
    fn height_parse_does_not_allocate_a_token() {
        // Behavioural stand-in for the allocation claim: the returned token is a
        // borrow into the input, so no copy happens.
        let revision = String::from("12-some-token");
        let parsed = parse_revision(&revision).expect("valid");
        assert!(std::ptr::eq(parsed.token.as_ptr(), revision[3..].as_ptr()));
    }
}
