//! Core errors.
//!
//! Upstream has a 196-code catalogue (`src/rx-error.ts`), and a few of those codes are
//! load-bearing because callers and tests match on them: `COL21` (collection closed),
//! `COL22` (duplicate ids in one `bulkInsert`), `CONFLICT` (409), and `SNH` ("should
//! never happen", an internal invariant). Rather than invent a parallel scheme, each
//! variant carries the upstream code where one exists, so a ported test that asserts
//! `.code === 'COL22'` has something to assert against.

/// An error raised by the core.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CoreError {
    /// A document is not shaped like a document.
    #[error("{0}")]
    InvalidDocument(String),
    /// The schema could not be built or read.
    #[error("{0}")]
    Schema(String),
    /// The storage failed in a way that is not a per-row write error.
    #[error("{0}")]
    Storage(String),
    /// A write conflicted: the stored revision is not the one the caller assumed.
    ///
    /// Upstream code `CONFLICT`; the storage reports it as a `409` per-row error and
    /// the collection throws it.
    #[error("conflict: document '{id}' has a different revision in the database")]
    Conflict {
        /// The document that conflicted.
        id: String,
    },
    /// A `bulkInsert` was given the same primary value twice (`COL22`).
    #[error("bulkInsert() called with the same primary key more than once: {ids:?}")]
    DuplicateIds {
        /// The ids that appeared more than once.
        ids: Vec<String>,
    },
    /// A write was attempted on a closed collection (`COL21`).
    #[error("collection '{collection}' is closed")]
    CollectionClosed {
        /// The collection name.
        collection: String,
    },
    /// A write was attempted while a schema migration is running (`COL20`).
    #[error("collection '{collection}' has a migration in progress")]
    MigrationInProgress {
        /// The collection name.
        collection: String,
    },
    /// The collection does not exist on this database.
    #[error("collection '{collection}' does not exist")]
    UnknownCollection {
        /// The collection name.
        collection: String,
    },
    /// Adding a collection that already exists.
    #[error("collection '{collection}' already exists")]
    DuplicateCollection {
        /// The collection name.
        collection: String,
    },
    /// A query named an operator outside the supported Mango set.
    #[error("query error: {0}")]
    Query(String),
    /// A document requested by id was not found (used by `findOne`-shaped helpers).
    #[error("document '{id}' not found in collection '{collection}'")]
    DocumentNotFound {
        /// The collection name.
        collection: String,
        /// The document id.
        id: String,
    },
    /// A final field was changed by an update (`DOC9` upstream: "final fields cannot be
    /// changed").
    #[error("field '{field}' is final and cannot be changed")]
    FinalFieldChanged {
        /// The field path.
        field: String,
    },
    /// An internal invariant broke (`SNH`).
    #[error("should never happen: {0}")]
    ShouldNeverHappen(String),
}

impl CoreError {
    /// The upstream error code this variant corresponds to, when it has one.
    ///
    /// `None` means the error is a dxdb-side refinement with no upstream code — for
    /// example a schema-build failure that upstream would surface from a different
    /// layer.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            CoreError::Conflict { .. } => Some("CONFLICT"),
            CoreError::DuplicateIds { .. } => Some("COL22"),
            CoreError::CollectionClosed { .. } => Some("COL21"),
            CoreError::MigrationInProgress { .. } => Some("COL20"),
            CoreError::UnknownCollection { .. } => Some("DB8"),
            CoreError::DuplicateCollection { .. } => Some("DB2"),
            CoreError::DocumentNotFound { .. } => Some("DOC1"),
            CoreError::FinalFieldChanged { .. } => Some("DOC9"),
            CoreError::ShouldNeverHappen(_) => Some("SNH"),
            CoreError::InvalidDocument(_)
            | CoreError::Schema(_)
            | CoreError::Storage(_)
            | CoreError::Query(_) => None,
        }
    }

    /// True for a `409`-shaped failure, which a caller may resolve by re-reading.
    pub fn is_conflict(&self) -> bool {
        matches!(self, CoreError::Conflict { .. })
    }
}

/// The core's result type.
pub type CoreResult<T> = Result<T, CoreError>;
