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
use crate::keys::{MessageKey, resolve, resolve_on};

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    /// Matched against the displayed subject.
    pub text: Option<String>,
    /// Matched against the sender's address or display name.
    pub from: Option<String>,
    /// Matched against the mailbox URL.
    pub mailbox: Option<String>,
    /// Matched against any recipient's address or display name (To or Cc).
    pub to: Option<String>,
    /// Matched against message body text. Requires local bodies; see [`search_bodies`].
    pub body: Option<String>,
    pub unread_only: bool,
    pub flagged_only: bool,
    pub has_attachment: bool,
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
    /// Attachment filenames recorded in the index.
    pub attachments: Vec<String>,
    /// Messages sharing this `conversation_id`, so a caller can tell a standalone message
    /// from one reply in a long thread.
    pub thread_size: i64,
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
    /// Present when a body filter ran, so a caller can state its coverage honestly.
    pub body_scan: Option<BodyScan>,
}

/// What a body search actually managed to look at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyScan {
    /// Messages whose body text was read and searched.
    pub searched: usize,
    /// Messages skipped because Mail has not downloaded the body. These are not misses —
    /// they are unsearchable, and a caller should say so rather than imply absence.
    pub unavailable: usize,
    /// True when the candidate set was larger than the scan budget.
    pub capped: bool,
}

/// Joins verified on V10; the key columns are resolved at runtime by [`crate::keys`] rather
/// than assumed, since side tables differ in whether they reference `ROWID` or the hash.
///
/// The label subquery is what makes mailbox reporting correct on Gmail, where
/// `messages.mailbox` is "All Mail" for nearly everything and the real filing lives in
/// `labels`.
fn select_clause(keys: &Keys) -> String {
    let message_id_header = if keys.has_global_data {
        "g.message_id_header"
    } else {
        "NULL"
    };
    let global_join = if keys.has_global_data {
        format!(
            "LEFT JOIN message_global_data g ON g.message_id = {}",
            keys.global_data.column()
        )
    } else {
        String::new()
    };
    let label_urls = if keys.has_labels {
        format!(
            "(SELECT group_concat(lm.url, char(10)) FROM labels l
                JOIN mailboxes lm ON lm.ROWID = l.mailbox_id
               WHERE l.message_id = {})",
            keys.label.column()
        )
    } else {
        "NULL".to_string()
    };
    let attachment_names = if keys.has_attachments {
        format!(
            "(SELECT group_concat(at.name, char(10)) FROM attachments at
               WHERE at.message = {} AND at.name IS NOT NULL)",
            keys.attachment.column()
        )
    } else {
        "NULL".to_string()
    };

    format!(
        "SELECT m.ROWID,
                {message_id_header},
                COALESCE(m.subject_prefix, '') || s.subject AS display_subject,
                a.address,
                a.comment,
                m.date_sent,
                m.read,
                m.flagged,
                m.size,
                m.conversation_id,
                mb.url,
                {label_urls} AS label_urls,
                {attachment_names} AS attachment_names,
                (SELECT count(*) FROM messages mt
                  WHERE mt.conversation_id = m.conversation_id AND mt.deleted = 0) AS thread_size
         FROM messages m
         JOIN subjects s ON s.ROWID = m.subject
         JOIN mailboxes mb ON mb.ROWID = m.mailbox
         LEFT JOIN addresses a ON a.ROWID = m.sender
         {global_join}"
    )
}

/// Which column of `messages` each side table joins against, resolved per query because the
/// index is undocumented and inconsistent about it.
#[derive(Debug, Clone, Copy)]
struct Keys {
    global_data: MessageKey,
    label: MessageKey,
    attachment: MessageKey,
    recipient: MessageKey,
    /// Which optional side tables this store actually has. A release that drops one should
    /// cost the feature that needs it, not every search.
    has_global_data: bool,
    has_labels: bool,
    has_attachments: bool,
    has_recipients: bool,
}

impl Keys {
    fn resolve(conn: &Connection) -> Self {
        Self {
            global_data: resolve(conn, "message_global_data"),
            label: resolve(conn, "labels"),
            attachment: resolve_on(conn, "attachments", "message"),
            recipient: resolve_on(conn, "recipients", "message"),
            has_global_data: table_exists(conn, "message_global_data"),
            has_labels: table_exists(conn, "labels"),
            has_attachments: table_exists(conn, "attachments"),
            has_recipients: table_exists(conn, "recipients"),
        }
    }
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

/// How many candidate bodies a single search will open. Scanning files is cheap per message
/// but linear, so a very large mailbox gets a bounded answer rather than a slow one.
pub const BODY_SCAN_BUDGET: usize = 3_000;

/// Searches message bodies as well as metadata.
///
/// There is no index behind this: candidates are narrowed by the metadata filters, then their
/// `.emlx` files are read and scanned. That keeps the `direct+live` promise — nothing is
/// persisted — and for a mailbox of a few thousand messages it is fast enough that an index
/// would be premature. It is linear, so [`BODY_SCAN_BUDGET`] bounds it.
pub fn search_bodies(
    db: &Path,
    index: &crate::emlx::EmlxIndex,
    query: &SearchQuery,
    now_unix: i64,
) -> Result<SearchResults> {
    let Some(needle) = query.body.as_deref().map(str::to_lowercase) else {
        return search(db, query, now_unix);
    };

    // Gather candidates using every filter except the body one, with a wide limit: the body
    // test decides the final set, so the metadata query must not pre-truncate it.
    let candidate_query = SearchQuery {
        body: None,
        limit: BODY_SCAN_BUDGET,
        ..query.clone()
    };
    let candidates = search(db, &candidate_query, now_unix)?;
    let capped = candidates.hits.len() >= BODY_SCAN_BUDGET;

    let mut scan = BodyScan { capped, ..Default::default() };
    let mut hits = Vec::new();

    for hit in candidates.hits {
        let Some(file) = index.get(hit.rowid) else {
            scan.unavailable += 1;
            continue;
        };
        if file.partial {
            scan.unavailable += 1;
            continue;
        }
        // Compaction off: searching should match what the message says, not what we would
        // have shown after rewriting its links.
        let Ok(message) = crate::emlx::read_with(file, false) else {
            scan.unavailable += 1;
            continue;
        };
        scan.searched += 1;

        if message
            .body()
            .is_some_and(|body| body.to_lowercase().contains(&needle))
        {
            hits.push(hit);
        }
    }

    let total_matching = hits.len() as i64;
    hits.truncate(query.limit.max(1));

    Ok(SearchResults {
        hits,
        total_matching,
        epoch: candidates.epoch,
        epoch_confident: candidates.epoch_confident,
        body_scan: Some(scan),
    })
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

/// One message by the id search reports, so a caller holding an id need not re-search.
pub fn hit_by_rowid(db: &Path, rowid: i64, now_unix: i64) -> Result<Option<SearchHit>> {
    let query = SearchQuery { limit: 1, ..Default::default() };
    let mut results = search_where(db, &query, now_unix, Some(rowid))?;
    Ok(results.hits.pop())
}

pub fn search(db: &Path, query: &SearchQuery, now_unix: i64) -> Result<SearchResults> {
    search_where(db, query, now_unix, None)
}

fn search_where(
    db: &Path,
    query: &SearchQuery,
    now_unix: i64,
    only_rowid: Option<i64>,
) -> Result<SearchResults> {
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

    let keys = Keys::resolve(&conn);

    // Mailbox names must be resolved to ids before the query: the stored URLs are
    // percent-encoded, so matching a displayed name like "Sent Mail" against the raw URL
    // can never hit `Sent%20Mail`.
    let mailbox_ids = match &query.mailbox {
        Some(needle) => Some(crate::mailbox::resolve_filter(db, needle)?),
        None => None,
    };

    let (mut predicates, params) = build_filters(query, epoch, &keys, mailbox_ids.as_deref());
    if let Some(rowid) = only_rowid {
        predicates.push(format!("m.ROWID = {rowid}"));
    }
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
        select = select_clause(&keys),
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
                mailboxes: split_lines(row.get::<_, Option<String>>(11)?),
                attachments: split_lines(row.get::<_, Option<String>>(12)?),
                thread_size: row.get::<_, Option<i64>>(13)?.unwrap_or(1),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(SearchResults {
        hits,
        total_matching,
        epoch,
        epoch_confident,
        body_scan: None,
    })
}

/// Builds the shared WHERE clause. Positional parameters keep user input out of the SQL
/// text; only the parameter *indices* are interpolated.
/// Splits a `group_concat` result, dropping blanks.
fn split_lines(joined: Option<String>) -> Vec<String> {
    joined
        .map(|text| {
            text.split('\n')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn build_filters(
    query: &SearchQuery,
    epoch: Epoch,
    keys: &Keys,
    mailbox_ids: Option<&[i64]>,
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
    if let Some(ids) = mailbox_ids {
        if ids.is_empty() {
            // The name matched no mailbox. Say "nothing" rather than "everything".
            predicates.push("0 = 1".to_string());
        } else {
            // Match the primary mailbox *or* any label. Checking only the former silently
            // returns nothing on Gmail, where every message's primary mailbox is All Mail.
            let list = ids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let label_clause = if keys.has_labels {
                format!(
                    " OR EXISTS (SELECT 1 FROM labels l
                                  WHERE l.message_id = {} AND l.mailbox_id IN ({list}))",
                    keys.label.column()
                )
            } else {
                String::new()
            };
            predicates.push(format!("(m.mailbox IN ({list}){label_clause})"));
        }
    }
    if let Some(recipient) = query.to.as_ref().filter(|_| keys.has_recipients) {
        let i = next(&mut params, Value::Text(like_pattern(recipient)));
        // Recipients are normalised into their own table, so this is an EXISTS rather than a
        // join: a message with four recipients must still appear once.
        predicates.push(format!(
            "EXISTS (SELECT 1 FROM recipients r
                       JOIN addresses ra ON ra.ROWID = r.address
                      WHERE r.message = {recipient_key}
                        AND (ra.address LIKE ?{i} ESCAPE '\\'
                             OR ra.comment LIKE ?{i} ESCAPE '\\'))",
            recipient_key = keys.recipient.column()
        ));
    }
    if query.has_attachment && keys.has_attachments {
        predicates.push(format!(
            "EXISTS (SELECT 1 FROM attachments at WHERE at.message = {attachment})",
            attachment = keys.attachment.column()
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
    use crate::store::MailStore;

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
             CREATE TABLE recipients (ROWID INTEGER PRIMARY KEY, message INTEGER, address INTEGER, type INTEGER);
             CREATE TABLE attachments (ROWID INTEGER PRIMARY KEY, message INTEGER, name TEXT);

             INSERT INTO subjects (ROWID, subject) VALUES (1, 'Invoice 100% due'), (2, 'Weekly report');
             INSERT INTO addresses (ROWID, address, comment) VALUES
                (1, 'billing@vendor.test', 'Vendor Billing'),
                (2, 'boss@work.test', ''),
                (3, 'me@home.test', 'Me'),
                (4, 'work.me@company.test', 'Me At Work');
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
             INSERT INTO labels (message_id, mailbox_id) VALUES (9001, 1), (9001, 2), (9002, 1);
             -- message 1 (hash 9001) went to the home address; message 2 to the work one
             INSERT INTO recipients (message, address, type) VALUES (9001, 3, 0), (9002, 4, 0);
             INSERT INTO attachments (message, name) VALUES (9001, 'invoice.pdf');",
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
    fn finds_one_message_by_its_reported_id() {
        let (_dir, db) = fixture();
        let hit = hit_by_rowid(&db, 1, NOW).unwrap().expect("message 1 exists");
        assert_eq!(hit.rowid, 1);
        assert_eq!(hit.message_id_header.as_deref(), Some("<a@vendor.test>"));
        assert_eq!(hit.subject, "Re: Invoice 100% due");
        assert!(hit.mailboxes.iter().any(|m| m.ends_with("/INBOX")));
    }

    #[test]
    fn an_unknown_id_is_none_rather_than_an_error() {
        let (_dir, db) = fixture();
        assert!(hit_by_rowid(&db, 9999, NOW).unwrap().is_none());
    }

    #[test]
    fn recipient_filter_matches_to_addresses() {
        let (_dir, db) = fixture();

        let mut q = query();
        q.to = Some("me@home.test".into());
        let results = search(&db, &q, NOW).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].rowid, 1);

        // Two of the user's addresses route into one mailbox; this is the question that
        // could not be asked before.
        let mut q = query();
        q.to = Some("work.me".into());
        let results = search(&db, &q, NOW).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].rowid, 2);

        // Display names are matched too.
        let mut q = query();
        q.to = Some("Me At Work".into());
        assert_eq!(search(&db, &q, NOW).unwrap().hits.len(), 1);
    }

    #[test]
    fn attachment_filter_and_names() {
        let (_dir, db) = fixture();

        let mut q = query();
        q.has_attachment = true;
        let results = search(&db, &q, NOW).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].attachments, vec!["invoice.pdf".to_string()]);

        // The message without one reports an empty list, not a phantom entry.
        let all = search(&db, &query(), NOW).unwrap();
        let plain = all.hits.iter().find(|h| h.rowid == 2).unwrap();
        assert!(plain.attachments.is_empty());
    }

    #[test]
    fn thread_size_counts_messages_sharing_a_conversation() {
        let (_dir, db) = fixture();
        let results = search(&db, &query(), NOW).unwrap();

        // Rows 1 and 3 share conversation 500; row 2 is alone in 501.
        let threaded = results.hits.iter().find(|h| h.rowid == 1).unwrap();
        assert_eq!(threaded.thread_size, 2);
        let lone = results.hits.iter().find(|h| h.rowid == 2).unwrap();
        assert_eq!(lone.thread_size, 1);
    }

    #[test]
    fn a_store_missing_optional_tables_still_searches() {
        // A macOS release that drops a side table should cost that feature, not everything.
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
             INSERT INTO subjects (ROWID, subject) VALUES (1, 'Bare store');
             INSERT INTO addresses (ROWID, address, comment) VALUES (1, 'a@b.test', '');
             INSERT INTO mailboxes (ROWID, url) VALUES (1, 'imap://host/INBOX');
             INSERT INTO messages VALUES (1, 9001, 7001, 1, NULL, 1, {d}, 1, 1, 0, 0, 10, 1);",
            d = NOW - 60
        ))
        .unwrap();
        drop(conn);

        let results = search(&db, &query(), NOW).unwrap();
        assert_eq!(results.hits.len(), 1, "search must still work");
        assert_eq!(results.hits[0].subject, "Bare store");
        // Features that need the absent tables degrade quietly.
        assert!(results.hits[0].message_id_header.is_none());
        assert!(results.hits[0].mailboxes.is_empty());
        assert!(results.hits[0].attachments.is_empty());
    }

    /// Writes a real `.emlx` so the body scanner has files to open.
    fn write_emlx(dir: &std::path::Path, id: i64, body: &str) {
        let messages = dir.join("Inbox.mbox/UUID/Data/1/Messages");
        std::fs::create_dir_all(&messages).unwrap();
        let raw = format!("Subject: Message {id}\r\n\r\n{body}\r\n");
        let mut bytes = format!("{}\n", raw.len()).into_bytes();
        bytes.extend_from_slice(raw.as_bytes());
        std::fs::write(messages.join(format!("{id}.emlx")), bytes).unwrap();
    }

    #[test]
    fn body_search_matches_text_the_subject_does_not_contain() {
        let (dir, db) = fixture();

        // The real motivating case: a generic subject, with the useful detail only in the
        // body. Message 1's subject is "Invoice 100% due"; nothing says "Ghana".
        write_emlx(dir.path(), 1, "The deadline is 3 March and the office is in Ghana.");
        write_emlx(dir.path(), 2, "Nothing interesting here.");

        let store = MailStore {
            root: dir.path().into(),
            version: 10,
            path: dir.path().into(),
        };
        let index = crate::emlx::EmlxIndex::build(&store).unwrap();

        let mut q = query();
        q.body = Some("ghana".into());
        let results = search_bodies(&db, &index, &q, NOW).unwrap();

        assert_eq!(results.hits.len(), 1, "matched on body text alone");
        assert_eq!(results.hits[0].rowid, 1);

        let scan = results.body_scan.expect("coverage reported");
        assert_eq!(scan.searched, 2);
        assert_eq!(scan.unavailable, 0);
        assert!(!scan.capped);
    }

    #[test]
    fn body_search_is_case_insensitive_and_reports_no_match_honestly() {
        let (dir, db) = fixture();
        write_emlx(dir.path(), 1, "Deadline is 3 March.");
        write_emlx(dir.path(), 2, "Nothing here.");
        let store = MailStore { root: dir.path().into(), version: 10, path: dir.path().into() };
        let index = crate::emlx::EmlxIndex::build(&store).unwrap();

        let mut q = query();
        q.body = Some("DEADLINE".into());
        assert_eq!(search_bodies(&db, &index, &q, NOW).unwrap().hits.len(), 1);

        let mut q = query();
        q.body = Some("nonexistent phrase".into());
        let results = search_bodies(&db, &index, &q, NOW).unwrap();
        assert!(results.hits.is_empty());
        assert_eq!(results.body_scan.unwrap().searched, 2);
    }

    #[test]
    fn body_search_counts_undownloaded_bodies_as_unsearchable_not_as_misses() {
        let (dir, db) = fixture();
        // Message 1 has a real body; message 2 has only headers, as an IMAP account that
        // keeps no local copies produces.
        write_emlx(dir.path(), 1, "contains the term widget");
        let messages = dir.path().join("Inbox.mbox/UUID/Data/1/Messages");
        std::fs::create_dir_all(&messages).unwrap();
        std::fs::write(messages.join("2.partial.emlx"), b"0\n").unwrap();

        let store = MailStore { root: dir.path().into(), version: 10, path: dir.path().into() };
        let index = crate::emlx::EmlxIndex::build(&store).unwrap();

        let mut q = query();
        q.body = Some("widget".into());
        let results = search_bodies(&db, &index, &q, NOW).unwrap();

        assert_eq!(results.hits.len(), 1);
        let scan = results.body_scan.expect("coverage reported");
        assert_eq!(scan.searched, 1);
        assert_eq!(scan.unavailable, 1, "the partial must be reported, not ignored");
    }

    #[test]
    fn body_search_combines_with_metadata_filters() {
        let (dir, db) = fixture();
        write_emlx(dir.path(), 1, "shared term");
        write_emlx(dir.path(), 2, "shared term");
        let store = MailStore { root: dir.path().into(), version: 10, path: dir.path().into() };
        let index = crate::emlx::EmlxIndex::build(&store).unwrap();

        // Both bodies match, but only one is from the vendor.
        let mut q = query();
        q.body = Some("shared term".into());
        assert_eq!(search_bodies(&db, &index, &q, NOW).unwrap().hits.len(), 2);

        let mut q = query();
        q.body = Some("shared term".into());
        q.from = Some("vendor.test".into());
        let narrowed = search_bodies(&db, &index, &q, NOW).unwrap();
        assert_eq!(narrowed.hits.len(), 1);
        assert_eq!(narrowed.hits[0].rowid, 1);
        assert_eq!(
            narrowed.body_scan.unwrap().searched,
            1,
            "metadata filters should narrow before any file is opened"
        );
    }

    #[test]
    fn a_query_without_a_body_filter_skips_scanning_entirely() {
        let (dir, db) = fixture();
        let store = MailStore { root: dir.path().into(), version: 10, path: dir.path().into() };
        let index = crate::emlx::EmlxIndex::build(&store).unwrap();

        let results = search_bodies(&db, &index, &query(), NOW).unwrap();
        assert!(results.body_scan.is_none(), "no scan should be claimed");
        assert_eq!(results.hits.len(), 2);
    }

    #[test]
    fn body_search_respects_the_limit_while_reporting_the_true_total() {
        let (dir, db) = fixture();
        write_emlx(dir.path(), 1, "common");
        write_emlx(dir.path(), 2, "common");
        let store = MailStore { root: dir.path().into(), version: 10, path: dir.path().into() };
        let index = crate::emlx::EmlxIndex::build(&store).unwrap();

        let mut q = query();
        q.body = Some("common".into());
        q.limit = 1;
        let results = search_bodies(&db, &index, &q, NOW).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.total_matching, 2, "the count must not be truncated too");
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
    fn a_percent_encoded_mailbox_name_matches_the_name_shown_to_the_user() {
        // Regression: filtering on the displayed name "Sent Mail" matched nothing, because
        // the stored URL spells it "Sent%20Mail" and the predicate compared raw URLs.
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
             CREATE TABLE mailboxes (ROWID INTEGER PRIMARY KEY, url TEXT NOT NULL,
                total_count INTEGER NOT NULL, unread_count INTEGER NOT NULL);
             CREATE TABLE message_global_data (ROWID INTEGER PRIMARY KEY, message_id INTEGER, message_id_header TEXT);
             CREATE TABLE labels (message_id INTEGER, mailbox_id INTEGER);

             INSERT INTO subjects (ROWID, subject) VALUES (1, 'Thanks');
             INSERT INTO addresses (ROWID, address, comment) VALUES (1, 'me@x.test', '');
             INSERT INTO mailboxes (ROWID, url, total_count, unread_count) VALUES
                (1, 'imap://me%40gmail.com@imap.gmail.com/%5BGmail%5D/All%20Mail', 1, 0),
                (2, 'imap://me%40gmail.com@imap.gmail.com/%5BGmail%5D/Sent%20Mail', 1, 0);
             INSERT INTO messages VALUES (1, 9001, 7001, 1, NULL, 1, {d}, 1, 1, 0, 0, 10, 1);
             INSERT INTO labels (message_id, mailbox_id) VALUES (9001, 1), (9001, 2);",
            d = NOW - 60
        ))
        .unwrap();
        drop(conn);

        for needle in ["Sent Mail", "sent mail", "Sent", "[Gmail]/Sent Mail"] {
            let mut q = query();
            q.mailbox = Some(needle.into());
            let results = search(&db, &q, NOW).unwrap();
            assert_eq!(results.hits.len(), 1, "{needle:?} should match");
        }
    }

    #[test]
    fn an_unknown_mailbox_returns_nothing_rather_than_everything() {
        let (_dir, db) = fixture();
        let mut q = query();
        q.mailbox = Some("NoSuchMailbox".into());

        let results = search(&db, &q, NOW).unwrap();
        assert!(results.hits.is_empty());
        assert_eq!(results.total_matching, 0);
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
