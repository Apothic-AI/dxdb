//! The `SqliteDriver` trait itself.
//!
//! See the crate docs for why the trait looks like this rather than like
//! upstream's `sqliteBasics`. The short version: we own both sides, so the trait
//! is typed on the operation and one implementation owns the SQL text.
//!
//! # Associated types and object safety
//!
//! The trait is **not** object-safe, deliberately. `Self::Db` and `Self::Stmt` are
//! the backend's own connection and statement types, which is the whole point:
//! `rusqlite::Connection` is `!Sync` and its `Statement` borrows it, and erasing
//! those to `dyn Any` would buy nothing but dynamic dispatch in the hot loop. The
//! storage layer is generic over `D: SqliteDriver`, so dispatch is static.
//!
//! # Async
//!
//! Every method is synchronous. SQLite's C API is synchronous, and a driver that
//! pretended otherwise would just be a future wrapping a blocking call, which is
//! the kind of lie that hides a worker-thread stall. The *storage* layer is where
//! async lives; it decides where these calls run (see `dxdb-storage`'s
//! `Spawner`).

use std::path::Path;

use crate::driver::{DriverError, Param, Row, StatementId, TableNames, Vfs};

/// A SQLite backend.
///
/// `&mut self` throughout because both `rusqlite::Connection` and the wasm
/// handles are single-writer and `!Sync`; taking `&self` would invite
/// concurrent use that the backend cannot actually support.
pub trait SqliteDriver {
    /// The backend's connection handle.
    type Db;
    /// The backend's prepared statement handle.
    type Stmt;

    /// Which VFS this driver's file paths resolve through.
    ///
    /// A driver reports what it actually got, not what the caller hoped for, so
    /// [`crate::open`] can refuse a file path before any data is written.
    fn vfs(&self) -> Vfs;

    /// Opens a database at `path`.
    ///
    /// Implementations must return [`DriverError::WouldBeEphemeral`] rather than
    /// opening a scratch database when [`Self::vfs`] is not file-backed. The
    /// caller cannot tell the difference afterwards, which is exactly why the
    /// check belongs here.
    fn open(&mut self, path: &Path) -> Result<Self::Db, DriverError>;

    /// Opens a private in-memory database.
    fn open_in_memory(&mut self) -> Result<Self::Db, DriverError>;

    /// Sets `PRAGMA <key> = <value>`.
    ///
    /// Separate from [`Self::execute`] because several SQLite bindings return no
    /// rows for a pragma and some raise if asked for them — upstream has a
    /// dedicated `setPragma` for this reason (`sqlite-types.ts:48`).
    fn set_pragma(&mut self, db: &Self::Db, key: &str, value: &str) -> Result<(), DriverError>;

    /// Compiles (or fetches from cache) the statement for `id`.
    fn prepare(
        &mut self,
        db: &Self::Db,
        id: StatementId,
        tables: &TableNames,
    ) -> Result<Self::Stmt, DriverError>;

    /// Runs a statement and returns the number of rows it changed.
    fn execute(
        &mut self,
        db: &Self::Db,
        stmt: &Self::Stmt,
        params: &[Param<'_>],
        op: &'static str,
    ) -> Result<u64, DriverError>;

    /// Runs a statement, delivering each row to `on_row`.
    ///
    /// The callback returns `Result<(), DriverError>` so a scan can stop early
    /// (a `limit` reached, a bad type) without materialising the rest of the
    /// table. Returning `Err` aborts the scan and surfaces the error.
    fn query(
        &mut self,
        db: &Self::Db,
        stmt: &Self::Stmt,
        params: &[Param<'_>],
        op: &'static str,
        on_row: &mut dyn FnMut(&dyn Row) -> Result<(), DriverError>,
    ) -> Result<(), DriverError>;

    /// Begins a write transaction (`BEGIN IMMEDIATE`).
    fn begin(&mut self, db: &Self::Db) -> Result<(), DriverError>;

    /// Commits the open transaction.
    fn commit(&mut self, db: &Self::Db) -> Result<(), DriverError>;

    /// Rolls back the open transaction.
    ///
    /// Must not error when no transaction is open, so a cleanup path can call it
    /// unconditionally.
    fn rollback(&mut self, db: &Self::Db) -> Result<(), DriverError>;

    /// Closes the connection.
    fn close(&mut self, db: Self::Db) -> Result<(), DriverError>;
}

/// Runs `body` inside a transaction, rolling back if it fails.
///
/// The rollback is best-effort: if `body` failed and the rollback also fails, the
/// `body` error is the one returned, because it is the one that explains the
/// failure. A rollback that fails on top of a broken connection adds nothing.
pub fn in_transaction<D, T>(
    driver: &mut D,
    db: &D::Db,
    body: impl FnOnce(&mut D) -> Result<T, DriverError>,
) -> Result<T, DriverError>
where
    D: SqliteDriver + ?Sized,
{
    driver.begin(db)?;
    match body(driver) {
        Ok(value) => {
            driver.commit(db)?;
            Ok(value)
        }
        Err(e) => {
            let _ = driver.rollback(db);
            Err(e)
        }
    }
}
