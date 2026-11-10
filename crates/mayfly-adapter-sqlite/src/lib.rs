//! `mayfly-adapter-sqlite` (control plane): the SQLite journal (ES-02), with receipts, inbox,
//! the event-kind manifest, inline projection rows and forward-only migrations, and the delivery
//! ledger (ES-05, [`SqliteLedger`]); projection generations (ES-04); snapshots join with ES-08.
//!
//! [`SqliteJournal`] implements `mayfly_control::journal::Journal` over one file through
//! `rusqlite` with its bundled SQLite (ADR 0008, amended by ADR 0021), following the adapter
//! mapping of `doc/event-store.md` §4:
//!
//! - **Schema** (`src/migrations/`, §4.1): `stream_head`, `journal_identity`, `journal_event`,
//!   `event_kind_manifest`, `command_receipt`, `inbox`, `projection_row` and `schema_meta`, with
//!   the constraint names the PostgreSQL adapter will share; triggers refuse `UPDATE`/`DELETE`
//!   of journal rows and any lowering or removal of a head (ES-I7, ES-I12; contract case JC-27).
//! - **Durability** (ES-I19): WAL, `synchronous = FULL` and foreign keys, read back on every
//!   connection at open; a store without them is refused (JC-26).
//! - **Append** (`append`): one `BEGIN IMMEDIATE` transaction, statements in the lock order of
//!   ES-I16, refusals in the port's reporting order, every refusal also guarded by the data (a
//!   head compare-and-set, a primary key), classified by extended code and the constraint the
//!   statement guards (`classify`), never by message text.
//! - **Delivery** (`ledger`, migration 0002): every unit marks each registered consumer for each
//!   stream it appends, in its own transaction; the ledger's claims, renewals, advances and
//!   releases are short `BEGIN IMMEDIATE` transactions on the same writers, fenced by the lease
//!   and judged by an injected store clock (`doc/event-store.md` §4.2, §5.1).
//! - **Projections** (`projections`, migration 0004): inline rows in each spec's active
//!   generation, written by the unit's transaction with the stream's new head as applied; the
//!   `ProjectionStore` reads and the offline rebuild's `ProjectionRebuild` writes (ES-04,
//!   `doc/event-store.md` §3.6).
//! - **Threads** (`writer`): each connection lives on its own thread; one writer is an
//!   implementation detail, never a premise (§4.3 rule 2).
//! - **Barriers** ([`StoreHooks`], [`NoHooks`]): where tests and the store lab strike.

mod append;
mod classify;
mod hooks;
mod journal;
mod ledger;
mod projections;
mod rows;
mod schema;
mod writer;

pub use hooks::{HookAction, NoHooks, StoreHooks};
pub use journal::{Durability, OpenError, SqliteJournal, SqliteOptions};
pub use ledger::SqliteLedger;
