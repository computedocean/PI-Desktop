//! Schema 35: the per-turn permission ceiling (remote-control spec §7.3).
//!
//! `turns.permission_ceiling` holds the ceiling a turn was admitted under and
//! `turn_queue.permission_ceiling` carries it while the turn waits. NULL means
//! no ceiling: every turn and queued entry that predates the column keeps its
//! existing behavior, except as noted below.

use super::{create_migration_backup, Connection, Context, Path, Result};
use rusqlite::Transaction;

const CEILING_CHECK: &str = "CHECK (permission_ceiling IN ('ask', 'accept-edits', 'auto'))";

/// Shared by fresh initialization and the additive v34 -> v35 migration.
pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    for table in ["turns", "turn_queue"] {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = 'permission_ceiling')",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            tx.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN permission_ceiling TEXT {CEILING_CHECK};"
            ))?;
        }
    }
    Ok(())
}

pub(super) fn migrate(conn: &Connection, path: &Path) -> Result<()> {
    let backup = create_migration_backup(conn, path, 34)?;
    let tx = conn.unchecked_transaction()?;
    initialize(&tx)?;
    // An entry queued before this version recorded the mode it was admitted
    // under but not whether a ceiling bound it. Treat that mode as its
    // ceiling so a restored remote entry cannot start broader than admitted.
    tx.execute_batch(
        "UPDATE turn_queue SET permission_ceiling = CASE
           WHEN permission_mode IN ('accept-edits', 'auto') THEN permission_mode
           ELSE 'ask' END
         WHERE permission_ceiling IS NULL;",
    )?;
    tx.pragma_update(None, "user_version", 35i64)?;
    tx.commit().with_context(|| {
        format!(
            "commit schema v34 to v35 turn permission ceiling migration; backup {} remains",
            backup.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::{migration_backup_path, Database, SCHEMA_VERSION};
    use rusqlite::Connection;

    fn version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn v34_upgrade_keeps_turns_and_bounds_restored_queue_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi.sqlite");
        let db = Database::open(&path).unwrap();
        db.conn()
            .execute_batch(
                "ALTER TABLE turns DROP COLUMN permission_ceiling;
                 ALTER TABLE turn_queue DROP COLUMN permission_ceiling;
                 INSERT INTO sessions(id, title, mode, created_at, updated_at)
                   VALUES ('s', 'legacy', 'agent', 1, 2);
                 INSERT INTO turns(id, session_id, status, started_at, ended_at)
                   VALUES ('old-turn', 's', 'completed', 1, 2);
                 INSERT INTO turn_queue(id, session_id, principal, input_hash, content,
                   permission_mode, position, created_at)
                   VALUES ('q-ask', 's', 'lan:device', 'h1', 'from phone', 'ask', 1, 2),
                          ('q-auto', 's', 'desktop', 'h2', 'from desktop', 'auto', 2, 3),
                          ('q-odd', 's', 'other', 'h3', 'unknown mode', 'inherit', 3, 4);
                 PRAGMA user_version = 34;",
            )
            .unwrap();
        drop(db);

        let upgraded = Database::open(&path).unwrap();
        assert_eq!(version(upgraded.conn()), SCHEMA_VERSION);
        let turn: (String, Option<String>) = upgraded
            .conn()
            .query_row(
                "SELECT status, permission_ceiling FROM turns WHERE id = 'old-turn'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(turn, ("completed".into(), None));
        let queue: Vec<(String, String, String)> = upgraded
            .conn()
            .prepare(
                "SELECT id, permission_mode, permission_ceiling FROM turn_queue ORDER BY position",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            queue,
            vec![
                ("q-ask".into(), "ask".into(), "ask".into()),
                ("q-auto".into(), "auto".into(), "auto".into()),
                ("q-odd".into(), "inherit".into(), "ask".into()),
            ]
        );
        assert!(upgraded
            .conn()
            .execute("UPDATE turns SET permission_ceiling = 'inherit'", [])
            .is_err());
        let backup = Connection::open(migration_backup_path(&path, 34)).unwrap();
        assert_eq!(version(&backup), 34);
    }

    #[test]
    fn a_fresh_database_starts_with_the_ceiling_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("pi.sqlite")).unwrap();
        for table in ["turns", "turn_queue"] {
            let exists: bool = db
                .conn()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = 'permission_ceiling')",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "{table}");
        }
    }
}
