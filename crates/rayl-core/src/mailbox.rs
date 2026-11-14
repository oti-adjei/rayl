//! Mailboxes, and turning their URLs into names a person recognises.
//!
//! A mailbox URL embeds the account address and the full path, e.g.
//! `imap://me%40gmail.com@imap.gmail.com/%5BGmail%5D/All%20Mail`. Only the last segment is
//! useful in a result list, and it needs percent-decoding.

use std::path::Path;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct MailboxInfo {
    pub url: String,
    /// Last path segment, percent-decoded. `None` when the URL has no usable segment.
    pub name: Option<String>,
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
        "SELECT url, total_count, unread_count FROM mailboxes ORDER BY total_count DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            let url: String = row.get(0)?;
            Ok(MailboxInfo {
                name: short_name(&url),
                url,
                total: row.get(1)?,
                unread: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
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
}
