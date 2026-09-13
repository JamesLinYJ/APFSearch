//! A bounded, deduplicated changed-ID journal. The singleton counter is updated
//! in the same SQLite transaction as each distinct ID. Repeated observations
//! do not increment it, and rollback restores both the counter and the rows.
use rusqlite::Connection;

pub(super) const MAX_IDS: usize = 65_536;

pub(super) fn install(connection: &Connection) -> Result<(), String> {
    // Installed once with an empty index. Normal opens perform no schema writes.
    connection
        .execute_batch(&format!(
            "CREATE TABLE cache_journal_state(
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                 pending_count INTEGER NOT NULL CHECK(pending_count>=0)
             );
             INSERT INTO cache_journal_state VALUES(1,0);
             CREATE TRIGGER cache_change_count_delete AFTER DELETE ON cache_changes BEGIN
                 UPDATE cache_journal_state SET pending_count=pending_count-1 WHERE singleton=1;
             END;
             CREATE TRIGGER cache_change_count_insert AFTER INSERT ON cache_changes BEGIN
                 UPDATE cache_journal_state SET pending_count=pending_count+1 WHERE singleton=1;
             END;
             CREATE TRIGGER cache_change_limit AFTER UPDATE OF pending_count ON cache_journal_state
                 WHEN new.pending_count>{MAX_IDS} BEGIN
                 INSERT INTO settings(key,value) VALUES('cache_journal_overflow','true')
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value;
                 DELETE FROM cache_changes;
             END;"
        ))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                 CREATE TABLE cache_changes(id INTEGER PRIMARY KEY);",
            )
            .unwrap();
        install(&connection).unwrap();
        connection
    }

    fn count(connection: &Connection) -> i64 {
        let tracked: i64 = connection
            .query_row("SELECT pending_count FROM cache_journal_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        let actual: i64 = connection
            .query_row("SELECT count(*) FROM cache_changes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tracked, actual);
        tracked
    }

    fn overflow(connection: &Connection) -> bool {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true')",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn deduplication_deletion_and_rollback_preserve_the_counter() {
        let connection = fixture();
        for _ in 0..100 {
            connection
                .execute(
                    "INSERT INTO cache_changes VALUES(42) ON CONFLICT(id) DO NOTHING",
                    [],
                )
                .unwrap();
        }
        assert_eq!(count(&connection), 1);
        connection
            .execute_batch(
                "BEGIN; DELETE FROM cache_changes; INSERT INTO cache_changes VALUES(7),(8);",
            )
            .unwrap();
        assert_eq!(count(&connection), 2);
        connection.execute_batch("ROLLBACK").unwrap();
        assert_eq!(count(&connection), 1);
        connection.execute("DELETE FROM cache_changes", []).unwrap();
        assert_eq!(count(&connection), 0);
    }

    #[test]
    fn overflow_is_bounded_atomic_and_resettable() {
        let connection = fixture();
        connection.execute_batch("BEGIN").unwrap();
        {
            let mut insert = connection
                .prepare("INSERT INTO cache_changes VALUES(?1)")
                .unwrap();
            for id in 1..=MAX_IDS as i64 {
                insert.execute([id]).unwrap();
            }
        }
        connection.execute_batch("COMMIT").unwrap();
        assert_eq!(count(&connection), MAX_IDS as i64);
        assert!(!overflow(&connection));
        connection.execute_batch("BEGIN").unwrap();
        connection
            .execute("INSERT INTO cache_changes VALUES(?1)", [MAX_IDS as i64 + 1])
            .unwrap();
        assert!(overflow(&connection));
        assert_eq!(count(&connection), 0);
        connection.execute_batch("ROLLBACK").unwrap();
        assert!(!overflow(&connection));
        assert_eq!(count(&connection), MAX_IDS as i64);
        connection
            .execute("INSERT INTO cache_changes VALUES(?1)", [MAX_IDS as i64 + 1])
            .unwrap();
        assert!(overflow(&connection));
        assert_eq!(count(&connection), 0);
        connection.execute_batch("UPDATE settings SET value='false' WHERE key='cache_journal_overflow'; INSERT INTO cache_changes VALUES(9);").unwrap();
        assert_eq!(count(&connection), 1);
    }

    #[test]
    fn trigger_has_no_per_insert_count_or_journal_scan() {
        let connection = fixture();
        let trigger: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='cache_change_limit'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!trigger.to_ascii_lowercase().contains("count("));
        assert!(!trigger.contains("SELECT id FROM cache_changes"));
    }
}
