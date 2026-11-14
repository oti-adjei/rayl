//! Metadata search over the envelope index.
//!
//! Everything here comes from the index alone — sender, subject, date, mailbox, flags,
//! thread — so it serves the `direct+live` mode with nothing persisted. Body text is not
//! available at this layer; it lives in `.emlx` files and needs the indexed strategy.
//!
//! Every column name and foreign key below was confirmed against a real V10 store rather
//! than assumed. See `02-Design/envelope-index-v10.md`.

use std::path::Path;

use rusqlite::Connection;
use rusqlite::types::Value;

use crate::epoch::{Epoch, detect};
use crate::error::Result;
use crate::keys::{MessageKey, resolve};

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    /// Matched against the displayed subject.
    pub text: Option<String>,
    /// Matched against the sender's address or display name.
    pub from: Option<String>,
    /// Matched against the mailbox URL.
    pub mailbox: Option<String>,
    pub unread_only: bool,
    pub flagged_only: bool,
    /// Inclusive bounds in Unix seconds; converted to the store's own epoch internally.
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub rowid: i64,
    /// RFC 5322 Message-ID. Absent for a small number of messages, so never assume it.
    pub message_id_header: Option<String>,
    pub subject: String,
    pub sender_address: Option<String>,
    pub sender_name: Option<String>,
    pub date_sent_unix: Option<i64>,
    pub read: bool,
    pub flagged: bool,
    pub size: i64,
    pub conversation_id: i64,
    /// The mailbox `messages.mailbox` points at. On Gmail this is almost always "All Mail"
    /// regardless of where the message appears, so prefer [`SearchHit::mailboxes`].
    pub mailbox_url: String,
    /// Every mailbox the message is filed under, from `labels`. This is where INBOX, Sent
    /// and starred actually live on a Gmail account.
    pub mailboxes: Vec<String>,
}

impl SearchHit {
    /// How many mailboxes the message is filed under; at least one.
    pub fn mailbox_count(&self) -> usize {
        self.mailboxes.len().max(1)
    }
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    /// Distinct messages matching, before the limit. Lets a caller say "showing 20 of 340".
    pub total_matching: i64,
    pub epoch: Epoch,
    pub epoch_confident: bool,
}

/// Joins verified on V10; the key columns are resolved at runtime by [`crate::keys`] rather
/// than assumed, since side tables differ in whether they reference `ROWID` or the hash.
///
/// The label subquery is what makes mailbox reporting correct on Gmail, where
/// `messages.mailbox` is "All Mail" for nearly everything and the real filing lives in
/// `labels`.
fn select_clause(global_key: MessageKey, label_key: MessageKey) -> String {
    format!(
        "SELECT m.ROWID,
                g.message_id_header,
                COALESCE(m.subject_prefix, '') || s.subject AS display_subject,
                a.address,
                a.comment,
                m.date_sent,
                m.read,
                m.flagged,
                m.size,
                m.conversation_id,
                mb.url,
                (SELECT group_concat(lm.url, char(10))
                   FROM labels l
                   JOIN mailboxes lm ON lm.ROWID = l.mailbox_id
                  WHERE l.message_id = {label}) AS label_urls
         FROM messages m
         JOIN subjects s ON s.ROWID = m.subject
         JOIN mailboxes mb ON mb.ROWID = m.mailbox
         LEFT JOIN addresses a ON a.ROWID = m.sender
         LEFT JOIN message_global_data g ON g.message_id = {global}",
        label = label_key.column(),
        global = global_key.column()
    )
}

/// Messages not in the trash. The denominator for any coverage statement.
pub fn live_message_count(db: &Path) -> Result<i64> {
    let conn = Connection::open(db)?;
    Ok(conn
        .query_row("SELECT count(*) FROM messages WHERE deleted = 0", [], |row| {
            row.get(0)
        })
        .unwrap_or(0))
}

pub fn search(db: &Path, query: &SearchQuery, now_unix: i64) -> Result<SearchResults> {
    let conn = Connection::open(db)?;

    // Date filters are expressed in Unix seconds but stored in the index's own epoch, so the
    // epoch has to be settled before any bound can be compared.
    let newest: Option<i64> = conn
        .query_row("SELECT max(date_sent) FROM messages WHERE date_sent > 0", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .ok()
        .flatten();
    let detection = newest.map(|sample| detect(sample, now_unix));
    let epoch = detection.as_ref().map(|d| d.epoch).unwrap_or(Epoch::Unix);
    let epoch_confident = detection.as_ref().map(|d| d.confident).unwrap_or(false);

    let global_key = resolve(&conn, "message_global_data");
    let label_key = resolve(&conn, "labels");

    let (predicates, params) = build_filters(query, epoch, label_key);
    let where_clause = predicates.join(" AND ");

    // Count distinct messages, not rows: the label subquery and the global-data join can
    // each produce more rows than there are messages.
    let count_sql = format!(
        "SELECT count(DISTINCT m.global_message_id) FROM messages m
         JOIN subjects s ON s.ROWID = m.subject
         JOIN mailboxes mb ON mb.ROWID = m.mailbox
         LEFT JOIN addresses a ON a.ROWID = m.sender
         WHERE {where_clause}"
    );
    let total_matching: i64 = conn
        .query_row(
            &count_sql,
            rusqlite::params_from_iter(params.iter()),
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Grouping by global id collapses the handful of rows that share one, which is how the
    // same message filed under several mailboxes avoids appearing twice.
    let sql = format!(
        "{select} WHERE {where_clause}
         GROUP BY m.global_message_id
         ORDER BY m.date_sent DESC
         LIMIT ?{limit}",
        select = select_clause(global_key, label_key),
        limit = params.len() + 1
    );

    let mut limited = params.clone();
    limited.push(Value::Integer(query.limit.max(1) as i64));

    let mut stmt = conn.prepare(&sql)?;
    let hits = stmt
        .query_map(rusqlite::params_from_iter(limited.iter()), |row| {
            let stored_date: Option<i64> = row.get(5)?;
            Ok(SearchHit {
                rowid: row.get(0)?,
                message_id_header: row.get(1)?,
                subject: row.get(2)?,
                sender_address: row.get(3)?,
                sender_name: row.get::<_, Option<String>>(4)?.filter(|c| !c.is_empty()),
                date_sent_unix: stored_date.filter(|d| *d > 0).map(|d| epoch.to_unix(d)),
                read: row.get::<_, i64>(6)? != 0,
                flagged: row.get::<_, i64>(7)? != 0,
                size: row.get(8)?,
                conversation_id: row.get(9)?,
                mailbox_url: row.get(10)?,
                mailboxes: row
                    .get::<_, Option<String>>(11)?
                    .map(|joined| joined.split('\n').map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(SearchResults { hits, total_matching, epoch, epoch_confident })
}

/// Builds the shared WHERE clause. Positional parameters keep user input out of the SQL
/// text; only the parameter *indices* are interpolated.
fn build_filters(
    query: &SearchQuery,
    epoch: Epoch,
    label_key: MessageKey,
) -> (Vec<String>, Vec<Value>) {
    let mut predicates = vec!["m.deleted = 0".to_string()];
    let mut params: Vec<Value> = Vec::new();

    let next = |params: &mut Vec<Value>, value: Value| -> usize {
        params.push(value);
        params.len()
    };

    if let Some(text) = &query.text {
        let i = next(&mut params, Value::Text(like_pattern(text)));
        predicates.push(format!(
            "(COALESCE(m.subject_prefix, '') || s.subject) LIKE ?{i} ESCAPE '\\'"
        ));
    }
    if let Some(from) = &query.from {
        let i = next(&mut params, Value::Text(like_pattern(from)));
        predicates.push(format!(
            "(a.address LIKE ?{i} ESCAPE '\\' OR a.comment LIKE ?{i} ESCAPE '\\')"
        ));
    }
    if let Some(mailbox) = &query.mailbox {
        let i = next(&mut params, Value::Text(like_pattern(mailbox)));
        // Match the primary mailbox *or* any label. Checking only the former silently
        // returns nothing on Gmail, where every message's primary mailbox is "All Mail".
        predicates.push(format!(
            "(mb.url LIKE ?{i} ESCAPE '\\'
              OR EXISTS (SELECT 1 FROM labels l
                           JOIN mailboxes lm ON lm.ROWID = l.mailbox_id
                          WHERE l.message_id = {label} AND lm.url LIKE ?{i} ESCAPE '\\'))",
            label = label_key.column()
        ));
    }
    if query.unread_only {
        predicates.push("m.read = 0".to_string());
    }
    if query.flagged_only {
        predicates.push("m.flagged = 1".to_string());
    }
    if let Some(since) = query.since {
        let i = next(&mut params, Value::Integer(epoch.from_unix(since)));
        predicates.push(format!("m.date_sent >= ?{i}"));
    }
    if let Some(until) = query.until {
        let i = next(&mut params, Value::Integer(epoch.from_unix(until)));
        predicates.push(format!("m.date_sent <= ?{i}"));
    }

    (predicates, params)
}

/// Wraps a term for substring matching, neutralising LIKE's own wildcards so a user
/// searching for `100%` or `a_b` gets a literal match.
fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_528_000; // 2026-11-13

    /// A miniature V10-shaped index, including a Gmail-style message filed twice.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, message_id INTEGER,
                global_message_id INTEGER, sender INTEGER, subject_prefix TEXT,
                subject INTEGER NOT NULL, date_sent INTEGER, mailbox INTEGER NOT NULL,
                read INTEGER NOT NULL, flagged INTEGER NOT NULL, deleted INTEGER NOT NULL,
                size INTEGER NOT NULL, conversation_id INTEGER NOT NULL);
             CREATE TABLE subjects (ROWID INTEGER PRIMARY KEY, subject TEXT NOT NULL);
             CREATE TABLE addresses (ROWID INTEGER PRIMARY KEY, address TEXT NOT NULL, comment TEXT NOT NULL);
             CREATE TABLE mailboxes (ROWID INTEGER PRIMARY KEY, url TEXT NOT NULL);
             CREATE TABLE message_global_data (ROWID INTEGER PRIMARY KEY, message_id INTEGER, message_id_header TEXT);
             CREATE TABLE labels (message_id INTEGER, mailbox_id INTEGER);

             INSERT INTO subjects (ROWID, subject) VALUES (1, 'Invoice 100% due'), (2, 'Weekly report');
             INSERT INTO addresses (ROWID, address, comment) VALUES
                (1, 'billing@vendor.test', 'Vendor Billing'),
                (2, 'boss@work.test', '');
             INSERT INTO mailboxes (ROWID, url) VALUES
                (1, 'imap://me%40gmail.com@imap.gmail.com/INBOX'),
                (2, 'imap://me%40gmail.com@imap.gmail.com/Archive');
             INSERT INTO message_global_data (message_id, message_id_header) VALUES
                (9001, '<a@vendor.test>'), (9002, '<b@work.test>');",
        )
        .unwrap();

        // 1: unread, flagged, from vendor, prefixed subject, filed under two mailboxes.
        // 2: read, from boss, plain subject.
        // 3: a second row sharing message 1's global id, as a duplicate filing produces.
        conn.execute_batch(&format!(
            "INSERT INTO messages VALUES
                (1, 9001, 7001, 1, 'Re: ', 1, {d1}, 1, 0, 1, 0, 2048, 500),
                (2, 9002, 7002, 2, NULL,  2, {d2}, 1, 1, 0, 0, 1024, 501),
                (3, 9001, 7001, 1, 'Re: ', 1, {d1}, 2, 0, 1, 0, 2048, 500);
             INSERT INTO labels (message_id, mailbox_id) VALUES (9001, 1), (9001, 2), (9002, 1);",
            d1 = NOW - 3600,
            d2 = NOW - 86_400 * 10
        ))
        .unwrap();
        drop(conn);
        (dir, db)
    }

    fn query() -> SearchQuery {
        SearchQuery { limit: 50, ..Default::default() }
    }

    #[test]
    fn counts_live_messages_excluding_deleted() {
        let (_dir, db) = fixture();
        // Three rows, none deleted.
        assert_eq!(live_message_count(&db).unwrap(), 3);
    }

    #[test]
    fn returns_newest_first_and_builds_the_display_subject() {
        let (_dir, db) = fixture();
        let results = search(&db, &query(), NOW).unwrap();

        assert_eq!(results.hits.len(), 2, "the duplicate filing must collapse");
        assert_eq!(results.hits[0].subject, "Re: Invoice 100% due");
        assert_eq!(results.hits[1].subject, "Weekly report");
    }

    #[test]
    fn collapses_messages_filed_under_several_mailboxes() {
        let (_dir, db) = fixture();
        let results = search(&db, &query(), NOW).unwrap();

        // Three rows exist, two distinct messages.
        assert_eq!(results.total_matching, 2);
        let first = &results.hits[0];
        assert_eq!(first.mailbox_count(), 2, "Gmail labels reported, not duplicated");
        // The label mailboxes are what a reader needs; the primary is All-Mail-ish.
        assert!(first.mailboxes.iter().any(|m| m.ends_with("/INBOX")));
        assert!(first.mailboxes.iter().any(|m| m.ends_with("/Archive")));
    }

    #[test]
    fn resolves_sender_and_omits_an_empty_display_name() {
        let (_dir, db) = fixture();
        let results = search(&db, &query(), NOW).unwrap();

        assert_eq!(results.hits[0].sender_address.as_deref(), Some("billing@vendor.test"));
        assert_eq!(results.hits[0].sender_name.as_deref(), Some("Vendor Billing"));
        assert_eq!(results.hits[1].sender_name, None, "empty comment is not a name");
    }

    #[test]
    fn carries_the_rfc_message_id() {
        let (_dir, db) = fixture();
        let results = search(&db, &query(), NOW).unwrap();
        assert_eq!(results.hits[0].message_id_header.as_deref(), Some("<a@vendor.test>"));
    }

    #[test]
    fn text_filter_matches_the_prefix_and_the_subject_together() {
        let (_dir, db) = fixture();

        let mut q = query();
        q.text = Some("Re: Invoice".into());
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 1);

        // The prefix is stored apart from the subject, so this only works if they are
        // concatenated before matching.
        let mut q = query();
        q.text = Some("report".into());
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 1);
    }

    #[test]
    fn like_wildcards_in_user_input_are_literal() {
        let (_dir, db) = fixture();

        let mut q = query();
        q.text = Some("100%".into());
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 1);

        // A bare % must match the subject containing a literal percent sign, and only
        // that one. Unescaped it would behave as a wildcard and match both messages.
        let mut q = query();
        q.text = Some("%".into());
        let results = search(&db, &q, NOW).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].subject, "Re: Invoice 100% due");

        let mut q = query();
        q.text = Some("_".into());
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 0);
    }

    #[test]
    fn sender_filter_matches_address_or_display_name() {
        let (_dir, db) = fixture();

        let mut q = query();
        q.from = Some("vendor.test".into());
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 1);

        let mut q = query();
        q.from = Some("Vendor Billing".into());
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 1);
    }

    #[test]
    fn flag_filters_apply() {
        let (_dir, db) = fixture();

        let mut q = query();
        q.unread_only = true;
        let unread = search(&db, &q, NOW).unwrap();
        assert_eq!(unread.hits.len(), 1);
        assert!(!unread.hits[0].read);

        let mut q = query();
        q.flagged_only = true;
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 1);
    }

    #[test]
    fn date_bounds_are_given_in_unix_and_compared_in_store_units() {
        let (_dir, db) = fixture();

        let mut q = query();
        q.since = Some(NOW - 7200);
        let recent = search(&db, &q, NOW).unwrap();
        assert_eq!(recent.hits.len(), 1, "only the message from an hour ago");

        let mut q = query();
        q.until = Some(NOW - 86_400);
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 1, "only the older one");
    }

    #[test]
    fn mailbox_filter_narrows_without_multiplying_rows() {
        let (_dir, db) = fixture();
        let mut q = query();
        q.mailbox = Some("Archive".into());

        let results = search(&db, &q, NOW).unwrap();
        assert_eq!(results.hits.len(), 1, "one hit, not one per matching label");
    }

    #[test]
    fn mailbox_filter_finds_messages_by_label_not_only_primary_mailbox() {
        // The Gmail shape: every message's primary mailbox is All Mail, and INBOX exists
        // only as a label. Filtering on the primary mailbox alone would find nothing.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, message_id INTEGER,
                global_message_id INTEGER, sender INTEGER, subject_prefix TEXT,
                subject INTEGER NOT NULL, date_sent INTEGER, mailbox INTEGER NOT NULL,
                read INTEGER NOT NULL, flagged INTEGER NOT NULL, deleted INTEGER NOT NULL,
                size INTEGER NOT NULL, conversation_id INTEGER NOT NULL);
             CREATE TABLE subjects (ROWID INTEGER PRIMARY KEY, subject TEXT NOT NULL);
             CREATE TABLE addresses (ROWID INTEGER PRIMARY KEY, address TEXT NOT NULL, comment TEXT NOT NULL);
             CREATE TABLE mailboxes (ROWID INTEGER PRIMARY KEY, url TEXT NOT NULL);
             CREATE TABLE message_global_data (ROWID INTEGER PRIMARY KEY, message_id INTEGER, message_id_header TEXT);
             CREATE TABLE labels (message_id INTEGER, mailbox_id INTEGER);

             INSERT INTO subjects (ROWID, subject) VALUES (1, 'Hello');
             INSERT INTO addresses (ROWID, address, comment) VALUES (1, 'a@b.test', '');
             INSERT INTO mailboxes (ROWID, url) VALUES
                (1, 'imap://me%40gmail.com@imap.gmail.com/%5BGmail%5D/All%20Mail'),
                (2, 'imap://me%40gmail.com@imap.gmail.com/INBOX');
             INSERT INTO messages VALUES (1, 9001, 7001, 1, NULL, 1, {d}, 1, 0, 0, 0, 10, 1);
             INSERT INTO labels (message_id, mailbox_id) VALUES (9001, 1), (9001, 2);",
            d = NOW - 60
        ))
        .unwrap();
        drop(conn);

        let mut q = query();
        q.mailbox = Some("INBOX".into());
        let results = search(&db, &q, NOW).unwrap();
        assert_eq!(results.hits.len(), 1, "INBOX exists only as a label here");
        assert_eq!(results.hits[0].mailbox_count(), 2);
    }

    #[test]
    fn limit_caps_hits_but_total_reports_the_full_match_count() {
        let (_dir, db) = fixture();
        let mut q = query();
        q.limit = 1;

        let results = search(&db, &q, NOW).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.total_matching, 2, "callers need the real count to page");
    }

    #[test]
    fn dates_are_normalised_to_unix() {
        let (_dir, db) = fixture();
        let results = search(&db, &query(), NOW).unwrap();
        assert_eq!(results.epoch, Epoch::Unix);
        assert_eq!(results.hits[0].date_sent_unix, Some(NOW - 3600));
    }

    #[test]
    fn escaping_leaves_ordinary_terms_untouched() {
        assert_eq!(like_pattern("hello"), "%hello%");
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern("c:\\path"), "%c:\\\\path%");
    }
}
