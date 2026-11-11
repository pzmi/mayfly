//! Forward-only raw-SQL migrations (fnd §6 rule 6, `doc/event-store.md` §6.1, §6.3 rule 3).
//!
//! Each migration is one `.sql` file under `src/migrations/`, applied in one transaction with
//! its row in `schema_meta` (`version`, `applied_at`, `compat_floor`). There is no downgrade.
//! Before a migration is applied to a store that held a schema when it was opened, the file is copied
//! beside itself (`<file>.pre-<version>`, by `VACUUM INTO`), so an operator can go back to the
//! store as it was. A store whose `compat_floor` is above the newest migration this build knows
//! was written by a build this one cannot run against: it refuses to open, naming both
//! versions. A store at a newer version whose floor this build meets opens and ignores the
//! additions (they are expand-only).

use std::path::{Path, PathBuf};

use mayfly_control::journal::StorageError;
use mayfly_domain::Millis;
use rusqlite::{Connection, OptionalExtension, params};

use crate::OpenError;
use crate::classify::read_error;

/// One migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Migration {
    /// Its number, from 1, consecutive.
    pub(crate) version: u32,
    /// The lowest schema version a binary must know to run against a store that has it.
    pub(crate) compat_floor: u32,
    /// Its statements.
    pub(crate) sql: &'static str,
}

/// Every migration of this build, in order.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        compat_floor: 1,
        sql: include_str!("migrations/0001_journal.sql"),
    },
    // Expand-only, yet a build without it must not run on a store that has it: its units would
    // write no dispatch marks, and a consumer already bootstrapped would never deliver their
    // events. So the floor rises to 2.
    Migration {
        version: 2,
        compat_floor: 2,
        sql: include_str!("migrations/0002_delivery.sql"),
    },
    // A trigger that refuses what no build does (lowering a dispatch mark): a build at 2 that
    // runs on a store at 3 never fires it, so the floor stays at 2.
    Migration {
        version: 3,
        compat_floor: 2,
        sql: include_str!("migrations/0003_dispatch_mark_rises.sql"),
    },
    // Expand-only. A build before it keeps no projection, so its units append events without
    // the rows: a missed write, not a wrong one. A build that keeps projections heals it: each
    // of its units puts the stream's whole row set in place of what is stored, and its start
    // rebuilds a projection with a stream whose applied revision is not the head
    // (`ProjectionRebuild::behind`, PC-10). So an older build may run on this store and the
    // floor stays at 2 (ES-04F; `doc/event-store.md` section 6.4).
    Migration {
        version: 4,
        compat_floor: 2,
        sql: include_str!("migrations/0004_projection_generations.sql"),
    },
    // Expand-only: a build at 4 ignores the claims. Its rebuild takes the active generation
    // plus one and never removes a claim, but a build at 5 claims above every generation used
    // and supersedes a stale claim, and the build at 4 runs alone (one control plane per
    // store, section 4.5): the floor stays at 2.
    Migration {
        version: 5,
        compat_floor: 2,
        sql: include_str!("migrations/0005_projection_rebuild_claims.sql"),
    },
];

fn engine(stage: impl Fn(StorageError) -> OpenError) -> impl Fn(rusqlite::Error) -> OpenError {
    move |e| stage(read_error(&e))
}

/// The store's schema version and compatibility floor; `(0, 0)` for an empty store.
fn stored_version(conn: &Connection) -> Result<(u32, u32), OpenError> {
    let open = engine(OpenError::Engine);
    let has_meta = conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'schema_meta'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(&open)?
        .is_some();
    if !has_meta {
        return Ok((0, 0));
    }
    let row = conn
        .query_row(
            "SELECT version, compat_floor FROM schema_meta ORDER BY version DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(&open)?;
    let unreadable = || OpenError::Engine(StorageError::new("row_unreadable"));
    match row {
        None => Ok((0, 0)),
        Some((version, floor)) => Ok((
            u32::try_from(version).map_err(|_| unreadable())?,
            u32::try_from(floor).map_err(|_| unreadable())?,
        )),
    }
}

/// Where the copy taken before `version` is applied goes.
pub(crate) fn backup_path(store: &Path, version: u32) -> PathBuf {
    let mut name = store.as_os_str().to_owned();
    name.push(format!(".pre-{version:04}"));
    PathBuf::from(name)
}

/// Brings the store on `conn` up to the last of `migrations`, backing it up to `backup` (the
/// store's file; `None` for a store with no file) before each migration applied over a schema.
pub(crate) fn migrate(
    conn: &Connection,
    migrations: &[Migration],
    backup: Option<&Path>,
    applied_at: Millis,
) -> Result<(), OpenError> {
    let known = migrations.last().map_or(0, |m| m.version);
    let (version, floor) = stored_version(conn)?;
    if floor > known {
        return Err(OpenError::SchemaAboveBuild {
            store: version,
            compat_floor: floor,
            build: known,
        });
    }
    let at = i64::try_from(applied_at.get())
        .map_err(|_| OpenError::Engine(StorageError::new("instant_out_of_range")))?;
    let pending: Vec<&Migration> = migrations.iter().filter(|m| m.version > version).collect();
    // Only a store that held a schema when it was opened is copied: a store created now has
    // nothing to go back to between its own first migrations.
    let held_schema = version > 0;
    for migration in pending {
        if let Some(store) = backup.filter(|_| held_schema) {
            let copy = backup_path(store, migration.version);
            if !copy.exists() {
                conn.execute(
                    "VACUUM INTO ?1",
                    params![copy.to_string_lossy().into_owned()],
                )
                .map_err(engine(|error| OpenError::Backup {
                    version: migration.version,
                    error,
                }))?;
            }
        }
        let failed = |error| OpenError::Migration {
            version: migration.version,
            error,
        };
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(engine(failed))?;
        let applied = conn.execute_batch(migration.sql).and_then(|()| {
            conn.execute(
                "INSERT INTO schema_meta (version, applied_at, compat_floor) VALUES (?1, ?2, ?3)",
                params![
                    i64::from(migration.version),
                    at,
                    i64::from(migration.compat_floor)
                ],
            )
        });
        if let Err(e) = applied.and_then(|_| conn.execute_batch("COMMIT")) {
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            return Err(engine(failed)(e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXTRA: Migration = Migration {
        version: 6,
        compat_floor: 2,
        sql: "CREATE TABLE extra_fact (id TEXT NOT NULL, CONSTRAINT extra_fact_pkey PRIMARY KEY (id)) STRICT;",
    };

    const BREAKING: Migration = Migration {
        version: 6,
        compat_floor: 6,
        sql: "CREATE TABLE breaking_fact (id TEXT NOT NULL, CONSTRAINT breaking_fact_pkey PRIMARY KEY (id)) STRICT;",
    };

    fn store() -> (tempfile::TempDir, PathBuf, Connection) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("journal.sqlite");
        let conn = Connection::open(&path).unwrap_or_else(|e| panic!("{e}"));
        (dir, path, conn)
    }

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap_or_else(|e| panic!("{e}"));
        stmt.query_map([], |r| r.get::<_, String>(0))
            .and_then(Iterator::collect)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn should_create_every_table_without_a_backup_when_the_store_is_empty() {
        // given
        let (_dir, path, conn) = store();

        // when
        let migrated = migrate(&conn, MIGRATIONS, Some(&path), Millis::new(7));

        // then
        assert_eq!(migrated, Ok(()));
        assert_eq!(
            tables(&conn),
            [
                "command_receipt",
                "consumer_checkpoint",
                "consumer_lease",
                "delivery_consumer",
                "event_kind_manifest",
                "inbox",
                "journal_event",
                "journal_identity",
                "projection_meta",
                "projection_rebuild",
                "projection_row",
                "projection_stream",
                "schema_meta",
                "stream_dispatch",
                "stream_head"
            ]
        );
        assert_eq!(stored_version(&conn), Ok((5, 2)));
        assert!(!backup_path(&path, 1).exists());
        assert!(!backup_path(&path, 2).exists());
        assert!(!backup_path(&path, 3).exists());
        assert!(!backup_path(&path, 4).exists());
        assert!(!backup_path(&path, 5).exists());
    }

    #[test]
    fn should_back_up_the_store_before_a_migration_when_it_holds_a_schema() {
        // given
        let (_dir, path, conn) = store();
        migrate(&conn, MIGRATIONS, Some(&path), Millis::new(7)).unwrap_or_else(|e| panic!("{e}"));
        let with_extra = [
            MIGRATIONS[0],
            MIGRATIONS[1],
            MIGRATIONS[2],
            MIGRATIONS[3],
            MIGRATIONS[4],
            EXTRA,
        ];

        // when
        let migrated = migrate(&conn, &with_extra, Some(&path), Millis::new(8));

        // then
        assert_eq!(migrated, Ok(()));
        assert_eq!(stored_version(&conn), Ok((6, 2)));
        let backup = Connection::open(backup_path(&path, 6)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(stored_version(&backup), Ok((5, 2)));
        assert!(!tables(&backup).contains(&"extra_fact".to_owned()));
        assert_eq!(
            migrate(&conn, &with_extra, Some(&path), Millis::new(9)),
            Ok(())
        );
    }

    #[test]
    fn should_open_a_newer_store_when_its_floor_is_one_this_build_knows() {
        // given
        let (_dir, path, conn) = store();
        migrate(
            &conn,
            &[
                MIGRATIONS[0],
                MIGRATIONS[1],
                MIGRATIONS[2],
                MIGRATIONS[3],
                MIGRATIONS[4],
                EXTRA,
            ],
            Some(&path),
            Millis::new(7),
        )
        .unwrap_or_else(|e| panic!("{e}"));

        // when
        let older = migrate(&conn, MIGRATIONS, Some(&path), Millis::new(8));

        // then
        assert_eq!(older, Ok(()));
        assert_eq!(stored_version(&conn), Ok((6, 2)));
    }

    #[test]
    fn should_refuse_a_store_when_its_floor_is_above_this_build() {
        // given
        let (_dir, path, conn) = store();
        migrate(
            &conn,
            &[
                MIGRATIONS[0],
                MIGRATIONS[1],
                MIGRATIONS[2],
                MIGRATIONS[3],
                MIGRATIONS[4],
                BREAKING,
            ],
            Some(&path),
            Millis::new(7),
        )
        .unwrap_or_else(|e| panic!("{e}"));

        // when
        let older = migrate(&conn, MIGRATIONS, Some(&path), Millis::new(8));

        // then
        assert_eq!(
            older,
            Err(OpenError::SchemaAboveBuild {
                store: 6,
                compat_floor: 6,
                build: 5
            })
        );
    }

    #[test]
    fn should_roll_back_and_name_the_migration_when_its_sql_fails() {
        // given
        let (_dir, path, conn) = store();
        let broken = Migration {
            version: 1,
            compat_floor: 1,
            sql: "CREATE TABLE half (id TEXT) STRICT; CREATE TABLE half (id TEXT) STRICT;",
        };

        // when
        let migrated = migrate(&conn, &[broken], Some(&path), Millis::new(7));

        // then
        assert_eq!(
            migrated,
            Err(OpenError::Migration {
                version: 1,
                error: StorageError::new("engine_error")
            })
        );
        assert_eq!(tables(&conn), Vec::<String>::new());
        assert!(conn.is_autocommit());
    }

    #[test]
    fn should_refuse_an_update_when_it_lowers_a_dispatch_mark() {
        // given
        let (_dir, path, conn) = store();
        migrate(&conn, MIGRATIONS, Some(&path), Millis::new(7)).unwrap_or_else(|e| panic!("{e}"));
        conn.execute(
            "INSERT INTO stream_dispatch (consumer, stream_id, marked, retry_at) \
             VALUES ('c', 'attempt-1', 5, NULL)",
            [],
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let set = |sql: &str| conn.execute(sql, []).map_err(|e| e.to_string());

        // when
        let lowered = set("UPDATE stream_dispatch SET marked = 4");
        let moved = set("UPDATE stream_dispatch SET stream_id = 'attempt-2'");
        let backed_off = set("UPDATE stream_dispatch SET retry_at = 9");
        let raised = set("UPDATE stream_dispatch SET marked = 6");

        // then
        assert!(
            lowered
                .as_ref()
                .is_err_and(|e| e.contains("stream_dispatch mark never falls")),
            "{lowered:?}"
        );
        assert!(moved.is_err(), "{moved:?}");
        assert_eq!((backed_off, raised), (Ok(1), Ok(1)));
    }
}
