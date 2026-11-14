//! Resolves which column a side table's `message_id` actually references.
//!
//! `messages` carries both a `ROWID` and a `message_id` hash, and the side tables are
//! inconsistent about which they key on — `message_global_data` uses the hash, verified on
//! V10. Rather than hardcode that per table and break on the next macOS release, each key is
//! resolved once by counting real matches.

use rusqlite::Connection;

/// The column of `messages` that a side table joins against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKey {
    Rowid,
    MessageIdHash,
}

impl MessageKey {
    /// The qualified column to use in a join, as SQL text. Never derived from user input.
    pub fn column(&self) -> &'static str {
        match self {
            MessageKey::Rowid => "m.ROWID",
            MessageKey::MessageIdHash => "m.message_id",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            MessageKey::Rowid => "messages.ROWID",
            MessageKey::MessageIdHash => "messages.message_id",
        }
    }
}

/// Counts how many rows of `table.message_id` match each candidate and picks the winner.
///
/// Defaults to the hash when the table is empty or the counts tie: that is what V10 uses for
/// every side table observed so far, so it is the better guess when evidence is absent.
pub fn resolve(conn: &Connection, table: &str) -> MessageKey {
    resolve_on(conn, table, "message_id")
}

/// As [`resolve`], for a side table whose foreign key is not called `message_id` —
/// `recipients` and `attachments` both call theirs `message`.
pub fn resolve_on(conn: &Connection, table: &str, column: &str) -> MessageKey {
    let count = |join_column: &str| -> i64 {
        // `table` and `column` come from our own constants and `join_column` from
        // MessageKey, so none of this is user input.
        let sql =
            format!("SELECT count(*) FROM {table} t JOIN messages m ON {join_column} = t.{column}");
        conn.query_row(&sql, [], |row| row.get::<_, i64>(0)).unwrap_or(0)
    };

    let by_hash = count("m.message_id");
    let by_rowid = count("m.ROWID");

    if by_rowid > by_hash {
        MessageKey::Rowid
    } else {
        MessageKey::MessageIdHash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(labels_use_rowid: bool) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, message_id INTEGER);
             CREATE TABLE labels (message_id INTEGER, mailbox_id INTEGER);
             INSERT INTO messages (ROWID, message_id) VALUES (1, 9001), (2, 9002), (3, 9003);",
        )
        .unwrap();
        for rowid in 1..=3 {
            let key = if labels_use_rowid { rowid } else { 9000 + rowid };
            conn.execute(
                "INSERT INTO labels (message_id, mailbox_id) VALUES (?1, 1)",
                [key],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn resolves_to_the_hash_column() {
        let conn = fixture(false);
        assert_eq!(resolve(&conn, "labels"), MessageKey::MessageIdHash);
    }

    #[test]
    fn resolves_to_rowid() {
        let conn = fixture(true);
        assert_eq!(resolve(&conn, "labels"), MessageKey::Rowid);
    }

    #[test]
    fn empty_table_falls_back_to_the_hash() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, message_id INTEGER);
             CREATE TABLE labels (message_id INTEGER, mailbox_id INTEGER);",
        )
        .unwrap();
        assert_eq!(resolve(&conn, "labels"), MessageKey::MessageIdHash);
    }

    #[test]
    fn resolves_a_table_whose_key_column_is_named_differently() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, message_id INTEGER);
             CREATE TABLE attachments (message INTEGER, name TEXT);
             INSERT INTO messages (ROWID, message_id) VALUES (1, 9001), (2, 9002);
             -- keyed on ROWID here
             INSERT INTO attachments (message, name) VALUES (1, 'a.pdf'), (2, 'b.pdf');",
        )
        .unwrap();
        assert_eq!(
            resolve_on(&conn, "attachments", "message"),
            MessageKey::Rowid
        );
    }

    #[test]
    fn missing_table_does_not_panic() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, message_id INTEGER);")
            .unwrap();
        assert_eq!(resolve(&conn, "nonexistent"), MessageKey::MessageIdHash);
    }
}
