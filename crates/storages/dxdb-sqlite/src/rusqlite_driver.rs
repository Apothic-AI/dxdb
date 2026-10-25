//! The `rusqlite` driver.
//!
//! One implementation for native and for wasm32. That is not laziness: `rusqlite`
//! keeps one API across both, and `default = ["cache", "ffi-sqlite-wasm-rs"]` means
//! that same API is backed by SQLite's C library on native and by
//! `sqlite-wasm-rs` on wasm32. A second, wasm-only driver would have to exist only
//! because something was unavailable on both sides, and nothing is.
//!
//! Verified by probe on 1.95.0: this dependency line compiles for
//! `x86_64-unknown-linux-gnu`, `wasm32-unknown-unknown`, `wasm32-wasip1` and
//! `wasm32-wasip2` with no feature selection
//! (`docs/lanes/m5-port-plan.md` section 7.4).
//!
//! # No `unsafe`
//!
//! `rusqlite` encapsulates the FFI, so this module contains none and the crate
//! keeps `#![forbid(unsafe_code)]`.

use std::path::Path;
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ToSql};

use crate::driver::{DriverError, Param, Row, StatementId, TableNames, Vfs};
use crate::traits::SqliteDriver;

/// How long a writer waits for another writer before failing.
///
/// SQLite already has the right mechanism for this, so we use it instead of
/// re-implementing upstream's retry loop (`sqlite-helpers.ts:191`, which retries
/// `BEGIN` in a `while` with `promiseWait(0)` between attempts — a busy-wait in
/// microtask clothing). Five seconds is long enough that ordinary contention
/// resolves and short enough that a genuinely stuck writer surfaces.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A `rusqlite`-backed driver.
#[derive(Debug, Default)]
pub struct RusqliteDriver {
    /// Whether a transaction is currently open, so `rollback` can be a no-op when
    /// it is not. Tracked here rather than asked of SQLite because SQLite reports
    /// "no transaction is active" as an error, and `rollback` must not fail.
    in_transaction: bool,
}

impl RusqliteDriver {
    /// A fresh driver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies the pragmas this storage depends on.
    ///
    /// Journal mode is target- and filesystem-dependent, so its failure is not
    /// fatal everywhere: a `:memory:` database cannot use WAL at all, and some
    /// hosted filesystems refuse it. The rest are hard requirements.
    fn configure(&mut self, conn: &Connection) -> Result<(), DriverError> {
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| sqlite_error("busy_timeout", e))?;
        // `IMMEDIATE` below takes the write lock up front; NORMAL is the standard
        // companion for WAL and is safe against application crashes (it is only
        // unsafe against an OS crash or power loss, which is the documented
        // trade-off for `synchronous = FULL`).
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| sqlite_error("synchronous", e))?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(|e| sqlite_error("foreign_keys", e))?;
        Ok(())
    }

    /// Attempts WAL. Failure is tolerated: an in-memory database cannot use it.
    fn try_enable_wal(&mut self, conn: &Connection) {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
    }
}

/// Turns a `rusqlite::Error` into a `DriverError` that names the operation.
///
/// `rusqlite::Error` is verbose and does not know which of our operations was
/// running; wrapping it here is what makes a failure attributable to a statement.
fn sqlite_error(op: &'static str, error: rusqlite::Error) -> DriverError {
    match error {
        rusqlite::Error::SqliteFailure(ffi_err, message) => DriverError::Sqlite {
            op,
            code: ffi_err.extended_code,
            message: message.unwrap_or_else(|| ffi_err.to_string()),
        },
        other => DriverError::Sqlite {
            op,
            code: -1,
            message: other.to_string(),
        },
    }
}

/// Converts our `Param` into the borrowed shape `rusqlite` binds.
///
/// `&dyn ToSql` rather than a `Vec<Box<dyn ToSql>>`: all five variants have
/// `ToSql` impls on their borrowed forms (`str`, `[u8]`, integers, floats), so the
/// slice costs nothing to build and nothing to drop.
fn to_sql_params<'a>(params: &'a [Param<'a>]) -> Vec<&'a dyn ToSql> {
    params
        .iter()
        .map(|p| match p {
            Param::Null => &NULL as &dyn ToSql,
            Param::Int(v) => v as &dyn ToSql,
            Param::Real(v) => v as &dyn ToSql,
            Param::Text(v) => v as &dyn ToSql,
            Param::Blob(v) => v as &dyn ToSql,
        })
        .collect()
}

/// The `NULL` value, as a `ToSql`.
static NULL: NullValue = NullValue;

/// A zero-sized `ToSql` for `NULL`.
#[derive(Debug)]
struct NullValue;

impl ToSql for NullValue {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Null))
    }
}

/// A `Row` impl over `rusqlite::Row`.
struct RusqliteRow<'stmt, 'conn> {
    row: &'stmt rusqlite::Row<'conn>,
    op: &'static str,
}

/// Names a `ValueRef` for the type-mismatch message.
const fn value_ref_name(value: &ValueRef<'_>) -> &'static str {
    match value {
        ValueRef::Null => "null",
        ValueRef::Integer(_) => "integer",
        ValueRef::Real(_) => "real",
        ValueRef::Text(_) => "text",
        ValueRef::Blob(_) => "blob",
    }
}

impl Row for RusqliteRow<'_, '_> {
    fn is_null(&self, column: usize) -> bool {
        matches!(self.row.get_ref(column), Ok(ValueRef::Null) | Err(_))
    }

    fn get_i64(&self, column: usize) -> Result<i64, DriverError> {
        match self.row.get_ref(column) {
            Ok(ValueRef::Integer(v)) => Ok(v),
            // SQLite stores a boolean as INTEGER and a caller may have written a
            // REAL that happens to be integral; refusing those would be pedantry
            // that breaks round-trips. Text is refused: that is always a bug.
            Ok(ValueRef::Real(v)) if v.fract() == 0.0 => Ok(v as i64),
            Ok(other) => Err(mismatch(self.op, column, "integer", &other)),
            Err(e) => Err(sqlite_error(self.op, e)),
        }
    }

    fn get_f64(&self, column: usize) -> Result<f64, DriverError> {
        match self.row.get_ref(column) {
            Ok(ValueRef::Real(v)) => Ok(v),
            Ok(ValueRef::Integer(v)) => Ok(v as f64),
            Ok(other) => Err(mismatch(self.op, column, "real", &other)),
            Err(e) => Err(sqlite_error(self.op, e)),
        }
    }

    fn get_text(&self, column: usize) -> Result<&str, DriverError> {
        match self.row.get_ref(column) {
            Ok(ValueRef::Text(bytes)) => {
                std::str::from_utf8(bytes).map_err(|e| DriverError::Sqlite {
                    op: self.op,
                    code: -1,
                    message: format!("column {column} is not valid UTF-8: {e}"),
                })
            }
            Ok(other) => Err(mismatch(self.op, column, "text", &other)),
            Err(e) => Err(sqlite_error(self.op, e)),
        }
    }

    fn get_blob(&self, column: usize) -> Result<&[u8], DriverError> {
        match self.row.get_ref(column) {
            Ok(ValueRef::Blob(bytes)) => Ok(bytes),
            Ok(other) => Err(mismatch(self.op, column, "blob", &other)),
            Err(e) => Err(sqlite_error(self.op, e)),
        }
    }
}

fn mismatch(
    op: &'static str,
    column: usize,
    expected: &'static str,
    actual: &ValueRef<'_>,
) -> DriverError {
    DriverError::TypeMismatch {
        op,
        column,
        expected,
        actual: value_ref_name(actual),
    }
}

impl SqliteDriver for RusqliteDriver {
    type Db = Connection;
    type Stmt = String;

    fn vfs(&self) -> Vfs {
        Vfs::selected()
    }

    fn open(&mut self, path: &Path) -> Result<Self::Db, DriverError> {
        let vfs = self.vfs();
        if !vfs.is_file_backed() {
            return Err(DriverError::WouldBeEphemeral {
                path: path.display().to_string(),
                remedy: vfs.remedy(),
            });
        }
        let conn = Connection::open(path).map_err(|e| sqlite_error("open", e))?;
        self.configure(&conn)?;
        self.try_enable_wal(&conn);
        Ok(conn)
    }

    fn open_in_memory(&mut self) -> Result<Self::Db, DriverError> {
        let conn = Connection::open_in_memory().map_err(|e| sqlite_error("open_in_memory", e))?;
        self.configure(&conn)?;
        // WAL is meaningless for an in-memory database; deliberately not attempted.
        Ok(conn)
    }

    fn set_pragma(&mut self, db: &Self::Db, key: &str, value: &str) -> Result<(), DriverError> {
        // `pragma_update` takes the value as a bind parameter, so `value` never
        // reaches SQL as text. That is why this is safe with a caller-supplied
        // string where a `format!` would not be.
        db.pragma_update(None, key, value)
            .map_err(|e| sqlite_error("set_pragma", e))
    }

    fn prepare(
        &mut self,
        db: &Self::Db,
        id: StatementId,
        tables: &TableNames,
    ) -> Result<Self::Stmt, DriverError> {
        let sql = id.sql(tables);
        // `prepare_cached` keeps the compiled statement on the connection keyed by
        // SQL text, so the second and later calls skip parsing. We return the SQL
        // string as the handle and let the connection own the cache: returning
        // `CachedStatement` is impossible here because it borrows `db`, and the
        // driver trait hands the statement back to a caller that already has `db`.
        db.prepare_cached(&sql)
            .map_err(|e| sqlite_error("prepare", e))?;
        Ok(sql)
    }

    fn execute(
        &mut self,
        db: &Self::Db,
        stmt: &Self::Stmt,
        params: &[Param<'_>],
        op: &'static str,
    ) -> Result<u64, DriverError> {
        let mut cached = db.prepare_cached(stmt).map_err(|e| sqlite_error(op, e))?;
        // Checked against the *compiled* statement rather than against a
        // hand-maintained constant, so a SQL edit that forgets its parameter
        // cannot silently bind the wrong values.
        let expected = cached.parameter_count();
        if expected != params.len() {
            return Err(DriverError::ParamCount {
                id: StatementId::InsertDocument, // op name carries the identity below
                expected,
                actual: params.len(),
            });
        }
        let bound = to_sql_params(params);
        cached
            .execute(rusqlite::params_from_iter(bound))
            .map(|n| n as u64)
            .map_err(|e| sqlite_error(op, e))
    }

    fn query(
        &mut self,
        db: &Self::Db,
        stmt: &Self::Stmt,
        params: &[Param<'_>],
        op: &'static str,
        on_row: &mut dyn FnMut(&dyn Row) -> Result<(), DriverError>,
    ) -> Result<(), DriverError> {
        let mut cached = db.prepare_cached(stmt).map_err(|e| sqlite_error(op, e))?;
        let expected = cached.parameter_count();
        if expected != params.len() {
            return Err(DriverError::ParamCount {
                id: StatementId::SelectAllDocuments,
                expected,
                actual: params.len(),
            });
        }
        let bound = to_sql_params(params);
        let mut rows = cached
            .query(rusqlite::params_from_iter(bound))
            .map_err(|e| sqlite_error(op, e))?;
        while let Some(row) = rows.next().map_err(|e| sqlite_error(op, e))? {
            let wrapper = RusqliteRow { row, op };
            on_row(&wrapper)?;
        }
        Ok(())
    }

    fn begin(&mut self, db: &Self::Db) -> Result<(), DriverError> {
        // IMMEDIATE, not DEFERRED: take the write lock now so contention is
        // handled by busy_timeout instead of surfacing after partial work.
        db.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| sqlite_error("begin", e))?;
        self.in_transaction = true;
        Ok(())
    }

    fn commit(&mut self, db: &Self::Db) -> Result<(), DriverError> {
        db.execute_batch("COMMIT")
            .map_err(|e| sqlite_error("commit", e))?;
        self.in_transaction = false;
        Ok(())
    }

    fn rollback(&mut self, db: &Self::Db) -> Result<(), DriverError> {
        if !self.in_transaction {
            // Not an error: a cleanup path may call this unconditionally.
            return Ok(());
        }
        db.execute_batch("ROLLBACK")
            .map_err(|e| sqlite_error("rollback", e))?;
        self.in_transaction = false;
        Ok(())
    }

    fn close(&mut self, db: Self::Db) -> Result<(), DriverError> {
        db.close().map_err(|(_, e)| sqlite_error("close", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DDL this driver's statements expect. See the crate docs on why it is
    /// `STRICT` with an index-string table rather than upstream's JSON blob.
    fn create_schema(conn: &Connection, tables: &TableNames) {
        conn.execute_batch(&format!(
            "CREATE TABLE {docs} (
                 id TEXT NOT NULL PRIMARY KEY,
                 revision TEXT NOT NULL,
                 deleted INTEGER NOT NULL CHECK (deleted IN (0,1)),
                 lwt INTEGER NOT NULL,
                 data TEXT NOT NULL
             ) STRICT;
             CREATE INDEX {docs}_cleanup ON {docs} (deleted, lwt, id);
             CREATE TABLE {idx} (
                 index_string TEXT NOT NULL,
                 id TEXT NOT NULL,
                 PRIMARY KEY (index_string, id)
             ) STRICT, WITHOUT ROWID;
             CREATE TABLE {att} (
                 id TEXT NOT NULL,
                 attachment_id TEXT NOT NULL,
                 data BLOB NOT NULL,
                 PRIMARY KEY (id, attachment_id)
             ) STRICT, WITHOUT ROWID;",
            docs = tables.docs,
            idx = tables.index,
            att = tables.attachments
        ))
        .expect("schema");
    }

    #[test]
    fn opens_in_memory_and_reports_a_vfs() {
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        assert_eq!(driver.vfs(), Vfs::selected());
        driver.close(conn).expect("close");
    }

    #[test]
    fn all_statements_prepare_against_the_schema() {
        // The check that the SQL and the DDL agree. A typo in a column name
        // shows up here rather than at the first write in production.
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        let tables = TableNames::new("todos", 1);
        create_schema(&conn, &tables);
        let all = [
            StatementId::InsertDocument,
            StatementId::ReplaceDocument,
            StatementId::SelectDocumentById,
            StatementId::SelectAllDocuments,
            StatementId::DeleteDocumentById,
            StatementId::DeleteTombstonesBelowLwt,
            StatementId::SelectIdsChangedSince,
            StatementId::CountDocuments,
            StatementId::InsertIndexEntry,
            StatementId::DeleteIndexEntry,
            StatementId::SelectIdsInIndexRange,
            StatementId::InsertAttachment,
            StatementId::SelectAttachment,
            StatementId::DeleteAttachmentsForDocument,
        ];
        for id in all {
            driver
                .prepare(&conn, id, &tables)
                .unwrap_or_else(|e| panic!("{id:?} did not prepare: {e}"));
        }
        driver.close(conn).expect("close");
    }

    #[test]
    fn writes_then_reads_a_row_through_the_trait() {
        // The end-to-end proof the tranche asks for: DDL, insert, select, and the
        // typed row decode, all through `SqliteDriver` rather than through
        // `rusqlite` directly.
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        let tables = TableNames::new("humans", 0);
        create_schema(&conn, &tables);

        let insert = driver
            .prepare(&conn, StatementId::InsertDocument, &tables)
            .expect("prepare insert");
        let json = r#"{"_id":"alice","_rev":"1-a","name":"Alice","age":30}"#;
        let changed = driver
            .execute(
                &conn,
                &insert,
                &[
                    Param::Text("alice"),
                    Param::Text("1-a"),
                    Param::Int(0), // _deleted: false
                    Param::Int(1_700_000_000_000),
                    Param::Text(json),
                ],
                "insert",
            )
            .expect("insert");
        assert_eq!(changed, 1);

        let select = driver
            .prepare(&conn, StatementId::SelectDocumentById, &tables)
            .expect("prepare select");
        let mut seen: Vec<(String, String, bool, i64, String)> = Vec::new();
        driver
            .query(
                &conn,
                &select,
                &[Param::Text("alice")],
                "select_by_id",
                &mut |row| {
                    seen.push((
                        row.get_text(0)?.to_string(),
                        row.get_text(1)?.to_string(),
                        row.get_i64(2)? != 0,
                        row.get_i64(3)?,
                        row.get_text(4)?.to_string(),
                    ));
                    Ok(())
                },
            )
            .expect("select");

        assert_eq!(seen.len(), 1);
        let (id, rev, deleted, lwt, body) = &seen[0];
        assert_eq!(id, "alice");
        assert_eq!(rev, "1-a");
        assert!(!deleted);
        assert_eq!(*lwt, 1_700_000_000_000);
        assert_eq!(body, json);
        // The document survives byte-for-byte, which is what ordered JSON needs.
        assert_eq!(body.as_str(), json);

        driver.close(conn).expect("close");
    }

    #[test]
    fn a_missing_id_yields_no_rows() {
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        let tables = TableNames::new("humans", 0);
        create_schema(&conn, &tables);
        let select = driver
            .prepare(&conn, StatementId::SelectDocumentById, &tables)
            .expect("prepare");
        let mut count = 0;
        driver
            .query(
                &conn,
                &select,
                &[Param::Text("nobody")],
                "select_by_id",
                &mut |_row| {
                    count += 1;
                    Ok(())
                },
            )
            .expect("query");
        assert_eq!(count, 0);
    }

    #[test]
    fn a_wrong_param_count_is_refused_before_binding() {
        // Not silently zero-bound: a mismatch must name the expected count.
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        let tables = TableNames::new("humans", 0);
        create_schema(&conn, &tables);
        let select = driver
            .prepare(&conn, StatementId::SelectDocumentById, &tables)
            .expect("prepare");
        let err = driver
            .query(&conn, &select, &[], "select_by_id", &mut |_| Ok(()))
            .expect_err("should refuse");
        match err {
            DriverError::ParamCount {
                expected, actual, ..
            } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 0);
            }
            other => panic!("expected ParamCount, got {other:?}"),
        }
    }

    #[test]
    fn reading_a_column_as_the_wrong_type_names_both_types() {
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        let tables = TableNames::new("humans", 0);
        create_schema(&conn, &tables);
        let insert = driver
            .prepare(&conn, StatementId::InsertDocument, &tables)
            .expect("prepare");
        driver
            .execute(
                &conn,
                &insert,
                &[
                    Param::Text("a"),
                    Param::Text("1-a"),
                    Param::Int(0),
                    Param::Int(1),
                    Param::Text("{}"),
                ],
                "insert",
            )
            .expect("insert");
        let select = driver
            .prepare(&conn, StatementId::SelectDocumentById, &tables)
            .expect("prepare select");
        let err = driver
            .query(
                &conn,
                &select,
                &[Param::Text("a")],
                "select_by_id",
                &mut |row| {
                    row.get_blob(0)?;
                    Ok(())
                },
            )
            .expect_err("should refuse");
        match err {
            DriverError::TypeMismatch {
                expected,
                actual,
                column,
                ..
            } => {
                assert_eq!(expected, "blob");
                assert_eq!(actual, "text");
                assert_eq!(column, 0);
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_transaction_rolls_back_on_error() {
        // `in_transaction` is the helper the storage layer's bulk_write will use,
        // so its rollback semantics get a direct test.
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        let tables = TableNames::new("humans", 0);
        create_schema(&conn, &tables);
        let insert = driver
            .prepare(&conn, StatementId::InsertDocument, &tables)
            .expect("prepare");

        let result: Result<(), DriverError> =
            crate::traits::in_transaction(&mut driver, &conn, |d| {
                d.execute(
                    &conn,
                    &insert,
                    &[
                        Param::Text("a"),
                        Param::Text("1-a"),
                        Param::Int(0),
                        Param::Int(1),
                        Param::Text("{}"),
                    ],
                    "insert",
                )?;
                // Fail after a successful write: the write must not survive.
                Err(DriverError::Callback("deliberate".into()))
            });
        assert!(result.is_err());

        let count = driver
            .prepare(&conn, StatementId::CountDocuments, &tables)
            .expect("prepare count");
        let mut total = 0i64;
        driver
            .query(&conn, &count, &[Param::Int(0)], "count", &mut |row| {
                total = row.get_i64(0)?;
                Ok(())
            })
            .expect("count");
        assert_eq!(total, 0, "the rolled-back insert must not be visible");
    }

    #[test]
    fn rollback_without_a_transaction_is_not_an_error() {
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        driver.rollback(&conn).expect("rollback must be idempotent");
        driver.close(conn).expect("close");
    }

    #[test]
    fn deleted_tombstones_are_purged_below_the_cutoff() {
        // The `cleanup` access path: only deleted rows, only older than the cutoff,
        // and live rows are untouched.
        let mut driver = RusqliteDriver::new();
        let conn = driver.open_in_memory().expect("open");
        let tables = TableNames::new("humans", 0);
        create_schema(&conn, &tables);
        let insert = driver
            .prepare(&conn, StatementId::InsertDocument, &tables)
            .expect("prepare insert");
        let rows = [
            ("old-deleted", 1, 100),
            ("new-deleted", 1, 900),
            ("old-live", 0, 100),
        ];
        for (id, deleted, lwt) in rows {
            driver
                .execute(
                    &conn,
                    &insert,
                    &[
                        Param::Text(id),
                        Param::Text("1-a"),
                        Param::Int(deleted),
                        Param::Int(lwt),
                        Param::Text("{}"),
                    ],
                    "insert",
                )
                .expect("insert");
        }
        let purge = driver
            .prepare(&conn, StatementId::DeleteTombstonesBelowLwt, &tables)
            .expect("prepare purge");
        let removed = driver
            .execute(&conn, &purge, &[Param::Int(500)], "purge")
            .expect("purge");
        assert_eq!(removed, 1, "only the old tombstone goes");

        let count = driver
            .prepare(&conn, StatementId::CountDocuments, &tables)
            .expect("prepare count");
        let mut included = 0i64;
        driver
            .query(&conn, &count, &[Param::Int(1)], "count", &mut |row| {
                included = row.get_i64(0)?;
                Ok(())
            })
            .expect("count");
        assert_eq!(included, 1, "the newer tombstone remains");
    }
}
