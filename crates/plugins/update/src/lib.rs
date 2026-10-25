//! MongoDB-style update operators, applied to a document.
//!
//! # Status
//!
//! Scaffold only. This plugin is 83 lines upstream and its whole behaviour is a
//! call into `mingo`'s updater
//! (`src/plugins/update/mingo-updater.ts:4`, `mingo@7.2.5`), so the port has to
//! re-implement the operator set rather than delegate. That work, and the
//! `update` / `incrementalUpdate` document methods it backs, are gated on
//! `dxdb-core`'s document write path (lane D). See
//! `docs/lanes/m6-port-plan.md`.
//!
//! # The operator set is a closed list, not "mingo"
//!
//! The port targets exactly the fourteen operators mingo ships
//! (`node_modules/mingo/cjs/operators/update/index.js`): `$addToSet`, `$bit`,
//! `$currentDate`, `$inc`, `$max`, `$min`, `$mul`, `$pop`, `$pull`, `$pullAll`,
//! `$push`, `$rename`, `$set`, `$unset`.
//!
//! Upstream then narrows that further in its *typings*
//! (`src/types/plugins/update.d.ts:12`), which list `$min $max $inc $set
//! $unset $push $addToSet $pop $pullAll $rename` and omit `$bit`,
//! `$currentDate`, `$mul`, `$pull` and `$setOnInsert`. It is a typings gap, not
//! a runtime restriction: `mingoUpdater` forwards the whole object to mingo, so
//! a document `update()` carrying `$bit` works upstream today. This crate
//! therefore implements the runtime set and records the typings gap as a
//! deliberate, documented widening — dropping five operators to match a
//! TypeScript declaration would be a behaviour change disguised as parity.
//!
//! # Rules that must survive the port
//!
//! - The input document is cloned and mutated, never written through, and the
//!   caller gets the clone back (`mingo-updater.ts:15-25`; `cloneMode: "none"`
//!   means mingo does not clone a second time).
//! - `$set` on a dotted path creates intermediate objects; `$unset` removes the
//!   key rather than setting it to null.
//! - `$inc` and `$mul` on a missing field start from zero (and from one, for
//!   `$mul`), and on a non-numeric field they are an error, not a coercion.
//! - `$push` and `$addToSet` respect `$each`, `$slice`, `$sort` and
//!   `$position`, and `$addToSet` compares with RxDB's deep-equality rules,
//!   which is the JS `deep-equal` in `src/plugins/utils/utils-object-deep-equal.ts`,
//!   not Rust `PartialEq` on the JSON value.
//! - Numbers are JS numbers: `dxdb_jsnum` semantics, because `$inc` on
//!   `9007199254740993` and on `0.1 + 0.2` are observably different from Rust
//!   `f64` and `i64` arithmetic.
//! - Applying an operator set to a frozen (dev-mode) document must not panic;
//!   the port operates on owned data so this is structural, but the dev-mode
//!   error code for an update that changes nothing must still match.
//!
//! # Interface, once lane D publishes
//!
//! Upstream exposes three entry points (`src/plugins/update/index.ts`):
//! `doc.update(obj)` writes directly, `doc.incrementalUpdate(obj)` runs the
//! update inside `incrementalModify` so it retries against the latest revision,
//! and `query.update(obj)` maps the document method over the query result.
//! In Rust these become inherent methods on the document handle plus an
//! extension trait for the query form, per PLAN.md 4.3.

#![forbid(unsafe_code)]
