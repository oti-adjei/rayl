//! Mailboxes, and turning their URLs into names a person recognises.
//!
//! A mailbox URL embeds the account address and the full path, e.g.
//! `imap://me%40gmail.com@imap.gmail.com/%5BGmail%5D/All%20Mail`. Only the last segment is
//! useful in a result list, and it needs percent-decoding.

use std::path::Path;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct MailboxInfo {
    pub rowid: i64,
    pub url: String,
    /// Last path segment, percent-decoded. `None` when the URL has no usable segment.
    pub name: Option<String>,
    /// The account this mailbox belongs to, as an email address where the URL carries one.
    pub account: Option<String>,
    pub total: i64,
    pub unread: i64,
}

impl MailboxInfo {
    /// A name to show, falling back to the raw URL.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.url)
    }
}

/// Lists mailboxes with their counts, busiest first.
pub fn list(db: &Path) -> Result<Vec<MailboxInfo>> {
    let conn = rusqlite::Connection::open(db)?;
    let mut stmt = conn.prepare(
        "SELECT ROWID, url, total_count, unread_count FROM mailboxes ORDER BY total_count DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            let url: String = row.get(1)?;
            Ok(MailboxInfo {
                rowid: row.get(0)?,
                name: short_name(&url),
                account: account_of(&url),
                url,
                total: row.get(2)?,
                unread: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Mailbox ROWIDs whose decoded name, decoded path or raw URL contains `needle`,
/// case-insensitively.
///
/// Resolving here rather than in SQL is the point: mailbox URLs are percent-encoded, so a
/// `LIKE '%Sent Mail%'` against the stored URL can never match `Sent%20Mail`. Matching the
/// name the user was shown requires decoding first, which SQLite cannot do.
pub fn resolve_filter(db: &Path, needle: &str) -> Result<Vec<i64>> {
    let needle = needle.trim().to_lowercase();
    let conn = rusqlite::Connection::open(db)?;

    // Only ROWID and url: deliberately independent of the count columns, so resolving a
    // mailbox name keeps working on a store whose schema differs.
    let mut stmt = conn.prepare("SELECT ROWID, url FROM mailboxes")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .filter(|(_, url)| {
            let name_hit = short_name(url).is_some_and(|name| name.to_lowercase().contains(&needle));
            // Also match deeper path segments, so "Gmail" finds "[Gmail]/Sent Mail".
            let path_hit = percent_decode(url).to_lowercase().contains(&needle);
            let raw_hit = url.to_lowercase().contains(&needle);
            name_hit || path_hit || raw_hit
        })
        .map(|(rowid, _)| rowid)
        .collect())
}

/// The accounts present, as email addresses, with how many mailboxes each has.
///
/// Nothing else reports whose mail is loaded, which makes "no results" ambiguous between
/// "no such message" and "that account isn't in Mail at all".
pub fn accounts(db: &Path) -> Result<Vec<(String, usize)>> {
    let mut seen: Vec<(String, usize)> = Vec::new();
    for mailbox in list(db)? {
        let account = mailbox.account.unwrap_or_else(|| "(local)".to_string());
        match seen.iter_mut().find(|(name, _)| *name == account) {
            Some((_, count)) => *count += 1,
            None => seen.push((account, 1)),
        }
    }
    seen.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(seen)
}

/// The account address from a mailbox URL, e.g.
/// `imap://me%40gmail.com@imap.gmail.com/INBOX` -> `me@gmail.com`.
///
/// The userinfo holds a percent-encoded address, and a literal `@` separates it from the
/// host, so the split is on the *last* `@` of the authority.
pub fn account_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let (userinfo, _host) = authority.rsplit_once('@')?;
    if userinfo.is_empty() {
        return None;
    }
    Some(percent_decode(userinfo))
}

/// The trailing path segment of a mailbox URL, decoded. `None` when the tail is the account
/// host rather than a folder.
pub fn short_name(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit('/').next()?;
    if last.is_empty() || last.contains('@') {
        return None;
    }
    Some(percent_decode(last))
}

/// Decodes `%XX` escapes.
///
/// Hex digits are read as bytes rather than by slicing the string: a `%` followed by a
/// multi-byte character would otherwise slice mid-character and panic, which any mailbox
/// name containing an accent can produce.
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_mailbox_urls_to_the_folder_name() {
        assert_eq!(
            short_name("imap://me%40gmail.com@imap.gmail.com/INBOX"),
            Some("INBOX".into())
        );
        assert_eq!(
            short_name("imap://host/Sent%20Messages"),
            Some("Sent Messages".into())
        );
    }

    #[test]
    fn ignores_a_url_whose_tail_is_the_account() {
        assert_eq!(short_name("imap://me%40gmail.com@imap.gmail.com"), None);
    }

    #[test]
    fn decodes_percent_escapes_without_panicking_on_bad_input() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%2"), "%2");
    }

    #[test]
    fn percent_followed_by_multibyte_characters_does_not_panic() {
        assert_eq!(percent_decode("%\u{e9}x"), "%\u{e9}x");
        assert_eq!(percent_decode("Caf\u{e9}%20Mail"), "Caf\u{e9} Mail");
        assert_eq!(percent_decode("\u{1f600}%"), "\u{1f600}%");
    }

    #[test]
    fn decodes_a_utf8_encoded_mailbox_name() {
        assert_eq!(percent_decode("Caf%C3%A9"), "Caf\u{e9}");
        assert_eq!(
            short_name("imap://host/%5BGmail%5D/All%20Mail"),
            Some("All Mail".into())
        );
    }

    #[test]
    fn extracts_the_account_address_from_a_mailbox_url() {
        assert_eq!(
            account_of("imap://me%40gmail.com@imap.gmail.com/INBOX"),
            Some("me@gmail.com".into())
        );
        assert_eq!(
            account_of("imap://george.heavenson%40gmail.com@imap.gmail.com/%5BGmail%5D/Sent%20Mail"),
            Some("george.heavenson@gmail.com".into())
        );
        // Local stores carry no account.
        assert_eq!(account_of("file:///Users/x/Library/Mail/V10"), None);
        assert_eq!(account_of("not a url"), None);
    }

    #[test]
    fn lists_mailboxes_busiest_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE mailboxes (ROWID INTEGER PRIMARY KEY, url TEXT NOT NULL,
                total_count INTEGER NOT NULL, unread_count INTEGER NOT NULL);
             INSERT INTO mailboxes (url, total_count, unread_count) VALUES
                ('imap://host/INBOX', 483, 120),
                ('imap://host/%5BGmail%5D/All%20Mail', 989, 695),
                ('imap://host/Drafts', 2, 0);",
        )
        .unwrap();
        drop(conn);

        let boxes = list(&db).unwrap();
        assert_eq!(boxes.len(), 3);
        assert_eq!(boxes[0].display_name(), "All Mail");
        assert_eq!(boxes[0].total, 989);
        assert_eq!(boxes[1].display_name(), "INBOX");
        assert_eq!(boxes[2].display_name(), "Drafts");
    }

    /// A Gmail-shaped fixture, including the encoded space that caused the bug.
    fn gmail_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE mailboxes (ROWID INTEGER PRIMARY KEY, url TEXT NOT NULL,
                total_count INTEGER NOT NULL, unread_count INTEGER NOT NULL);
             INSERT INTO mailboxes (ROWID, url, total_count, unread_count) VALUES
                (1, 'imap://me%40gmail.com@imap.gmail.com/INBOX', 483, 120),
                (2, 'imap://me%40gmail.com@imap.gmail.com/%5BGmail%5D/Sent%20Mail', 13, 0),
                (3, 'imap://me%40gmail.com@imap.gmail.com/%5BGmail%5D/All%20Mail', 989, 695);",
        )
        .unwrap();
        drop(conn);
        (dir, db)
    }

    #[test]
    fn the_name_list_mailboxes_shows_is_a_name_search_mail_accepts() {
        // The reported bug: list_mailboxes displayed "Sent Mail", but filtering on that
        // string matched nothing because the stored URL spells it "Sent%20Mail".
        let (_dir, db) = gmail_fixture();

        let shown = list(&db).unwrap();
        let sent = shown
            .iter()
            .find(|m| m.display_name() == "Sent Mail")
            .expect("listed as 'Sent Mail'");

        let resolved = resolve_filter(&db, "Sent Mail").unwrap();
        assert!(
            resolved.contains(&sent.rowid),
            "the displayed name must resolve to the mailbox it names"
        );
    }

    #[test]
    fn mailbox_filter_is_case_insensitive_and_matches_partials() {
        let (_dir, db) = gmail_fixture();
        assert_eq!(resolve_filter(&db, "inbox").unwrap(), vec![1]);
        assert_eq!(resolve_filter(&db, "INBOX").unwrap(), vec![1]);
        assert_eq!(resolve_filter(&db, "sent").unwrap(), vec![2]);
        // A deeper path segment still matches, decoded.
        let gmail = resolve_filter(&db, "[Gmail]").unwrap();
        assert!(gmail.contains(&2) && gmail.contains(&3), "got {gmail:?}");
    }

    #[test]
    fn an_unknown_mailbox_resolves_to_nothing() {
        let (_dir, db) = gmail_fixture();
        assert!(resolve_filter(&db, "Nonexistent").unwrap().is_empty());
    }

    #[test]
    fn groups_mailboxes_by_account() {
        let (_dir, db) = gmail_fixture();
        let accounts = accounts(&db).unwrap();
        assert_eq!(accounts, vec![("me@gmail.com".to_string(), 3)]);
    }
}
