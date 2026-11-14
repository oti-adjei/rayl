//! Settles, against real data, the things a schema dump cannot tell us.
//!
//! Every check reports counts and distributions only. No subjects, addresses or bodies are
//! read, so the output is safe to share when reporting a schema that differs.

use std::path::Path;

use rusqlite::Connection;

use crate::epoch::{EpochDetection, detect, format_unix};
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct Verification {
    pub epoch: Option<EpochDetection>,
    pub newest_message: Option<String>,
    pub oldest_message: Option<String>,
    /// Which column `message_global_data.message_id` actually references.
    pub global_data_join: JoinResolution,
    pub download_states: Vec<(i64, i64)>,
    pub recipient_types: Vec<(i64, i64)>,
    pub body_indexed: Vec<(i64, i64)>,
    pub message_id_header_present: i64,
    pub messages_total: i64,
    pub messages_live: i64,
    pub distinct_global_ids: i64,
    /// Messages mapped into more than one mailbox, i.e. the Gmail duplicate case.
    pub multi_mailbox_messages: i64,
    pub summaries_linked: i64,
}

/// `message_global_data.message_id` could reference either `messages.ROWID` or
/// `messages.message_id`; both have compatible cardinality, so we count actual matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinResolution {
    Rowid { matches: i64, rival: i64 },
    MessageIdColumn { matches: i64, rival: i64 },
    Ambiguous { rowid: i64, message_id: i64 },
    Unavailable(String),
}

pub fn run(db: &Path, now_unix: i64) -> Result<Verification> {
    let conn = Connection::open(db)?;

    let messages_total = scalar(&conn, "SELECT count(*) FROM messages").unwrap_or(0);
    let messages_live = scalar(&conn, "SELECT count(*) FROM messages WHERE deleted = 0").unwrap_or(0);
    let distinct_global_ids =
        scalar(&conn, "SELECT count(DISTINCT global_message_id) FROM messages WHERE deleted = 0")
            .unwrap_or(0);

    // The newest message should land near today under exactly one epoch interpretation.
    let newest = scalar(&conn, "SELECT max(date_sent) FROM messages WHERE date_sent > 0");
    let oldest = scalar(&conn, "SELECT min(date_sent) FROM messages WHERE date_sent > 0");

    let epoch = newest.map(|sample| detect(sample, now_unix));
    let newest_message = epoch
        .as_ref()
        .zip(newest)
        .map(|(d, v)| format_unix(d.epoch.to_unix(v)));
    let oldest_message = epoch
        .as_ref()
        .zip(oldest)
        .map(|(d, v)| format_unix(d.epoch.to_unix(v)));

    Ok(Verification {
        epoch,
        newest_message,
        oldest_message,
        global_data_join: resolve_join(&conn),
        download_states: distribution(
            &conn,
            "SELECT download_state, count(*) FROM message_global_data GROUP BY download_state ORDER BY download_state",
        ),
        recipient_types: distribution(
            &conn,
            "SELECT type, count(*) FROM recipients GROUP BY type ORDER BY type",
        ),
        body_indexed: distribution(
            &conn,
            "SELECT message_body_indexed, count(*) FROM searchable_messages GROUP BY message_body_indexed ORDER BY message_body_indexed",
        ),
        message_id_header_present: scalar(
            &conn,
            "SELECT count(*) FROM message_global_data WHERE message_id_header IS NOT NULL AND message_id_header != ''",
        )
        .unwrap_or(0),
        messages_total,
        messages_live,
        distinct_global_ids,
        multi_mailbox_messages: scalar(
            &conn,
            "SELECT count(*) FROM (SELECT message_id FROM labels GROUP BY message_id HAVING count(*) > 1)",
        )
        .unwrap_or(0),
        summaries_linked: scalar(
            &conn,
            "SELECT count(*) FROM messages WHERE summary IS NOT NULL AND summary != 0",
        )
        .unwrap_or(0),
    })
}

fn resolve_join(conn: &Connection) -> JoinResolution {
    let by_rowid = scalar(
        conn,
        "SELECT count(*) FROM message_global_data g JOIN messages m ON m.ROWID = g.message_id",
    );
    let by_message_id = scalar(
        conn,
        "SELECT count(*) FROM message_global_data g JOIN messages m ON m.message_id = g.message_id",
    );

    match (by_rowid, by_message_id) {
        (Some(rowid), Some(message_id)) => {
            // A real foreign key matches nearly every row; the wrong guess matches few, or
            // matches many but duplicated. Require a clear margin before claiming either.
            if rowid > message_id * 2 {
                JoinResolution::Rowid { matches: rowid, rival: message_id }
            } else if message_id > rowid * 2 {
                JoinResolution::MessageIdColumn { matches: message_id, rival: rowid }
            } else {
                JoinResolution::Ambiguous { rowid, message_id }
            }
        }
        _ => JoinResolution::Unavailable("message_global_data is not queryable".into()),
    }
}

fn scalar(conn: &Connection, sql: &str) -> Option<i64> {
    conn.query_row(sql, [], |row| row.get::<_, Option<i64>>(0))
        .ok()
        .flatten()
}

fn distribution(conn: &Connection, sql: &str) -> Vec<(i64, i64)> {
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<i64>>(0)?.unwrap_or(-1),
            row.get::<_, i64>(1)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_614_400; // 2026-11-13

    /// Builds a miniature index with the V10 shape, so the checks are exercised end to end.
    fn fixture(global_uses_rowid: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, message_id INTEGER,
                 global_message_id INTEGER, date_sent INTEGER, deleted INTEGER, summary INTEGER);
             CREATE TABLE message_global_data (ROWID INTEGER PRIMARY KEY, message_id INTEGER,
                 download_state INTEGER, message_id_header TEXT);
             CREATE TABLE recipients (ROWID INTEGER PRIMARY KEY, type INTEGER);
             CREATE TABLE searchable_messages (message_id INTEGER PRIMARY KEY, message_body_indexed INTEGER);
             CREATE TABLE labels (message_id INTEGER, mailbox_id INTEGER);",
        )
        .unwrap();

        // Three messages; ROWIDs 1..3, message_id hashes deliberately different.
        for (rowid, hash) in [(1, 9001), (2, 9002), (3, 9003)] {
            conn.execute(
                "INSERT INTO messages (ROWID, message_id, global_message_id, date_sent, deleted, summary)
                 VALUES (?1, ?2, ?3, ?4, 0, NULL)",
                rusqlite::params![rowid, hash, 7000 + rowid, NOW - 3600],
            )
            .unwrap();
            let fk = if global_uses_rowid { rowid } else { hash };
            conn.execute(
                "INSERT INTO message_global_data (message_id, download_state, message_id_header)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![fk, rowid % 2, format!("<{rowid}@example.test>")],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO recipients (type) VALUES (0), (0), (1);
             INSERT INTO searchable_messages (message_id, message_body_indexed) VALUES (1,1),(2,1),(3,0);
             -- message 1 lives in two mailboxes, as Gmail labels produce
             INSERT INTO labels (message_id, mailbox_id) VALUES (1,10),(1,11),(2,10);",
        )
        .unwrap();
        drop(conn);
        (dir, db)
    }

    #[test]
    fn resolves_global_data_join_to_rowid() {
        let (_dir, db) = fixture(true);
        let report = run(&db, NOW).unwrap();
        assert!(matches!(report.global_data_join, JoinResolution::Rowid { .. }));
    }

    #[test]
    fn resolves_global_data_join_to_message_id_column() {
        let (_dir, db) = fixture(false);
        let report = run(&db, NOW).unwrap();
        assert!(matches!(
            report.global_data_join,
            JoinResolution::MessageIdColumn { .. }
        ));
    }

    #[test]
    fn counts_distributions_and_duplicates() {
        let (_dir, db) = fixture(true);
        let report = run(&db, NOW).unwrap();

        assert_eq!(report.messages_total, 3);
        assert_eq!(report.messages_live, 3);
        assert_eq!(report.distinct_global_ids, 3);
        assert_eq!(report.recipient_types, vec![(0, 2), (1, 1)]);
        assert_eq!(report.body_indexed, vec![(0, 1), (1, 2)]);
        assert_eq!(report.message_id_header_present, 3);
        // Only message 1 appears under two mailboxes.
        assert_eq!(report.multi_mailbox_messages, 1);
        assert_eq!(report.summaries_linked, 0);
    }

    #[test]
    fn detects_the_epoch_from_real_timestamps() {
        let (_dir, db) = fixture(true);
        let report = run(&db, NOW).unwrap();
        let epoch = report.epoch.expect("timestamps present");
        assert!(epoch.confident);
        assert_eq!(epoch.epoch, crate::epoch::Epoch::Unix);
    }

    #[test]
    fn missing_tables_degrade_to_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, deleted INTEGER);")
            .unwrap();
        drop(conn);

        let report = run(&db, NOW).unwrap();
        assert!(report.download_states.is_empty());
        assert!(matches!(
            report.global_data_join,
            JoinResolution::Unavailable(_)
        ));
    }
}
