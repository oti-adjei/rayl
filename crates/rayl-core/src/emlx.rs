//! Reading message bodies from `.emlx` files.
//!
//! Mail stores one file per message: a byte-count line, the RFC 5322 message, then an Apple
//! property list of local metadata. A `.partial.emlx` holds only headers, because the
//! account has not downloaded the body — 37% of messages on the store this was developed
//! against, so it is the normal case rather than an edge one.
//!
//! Locating a file is done by indexing the tree once rather than by deriving a path. Mail's
//! shard layout is undocumented and has changed before; a single walk costs milliseconds and
//! cannot be wrong about it.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use mail_parser::MimeHeaders;

use crate::error::{Error, Result};
use crate::store::MailStore;

#[derive(Debug, Clone)]
pub struct MessageFile {
    pub path: PathBuf,
    /// True for `.partial.emlx`: headers are present, the body is not.
    pub partial: bool,
}

/// Maps a message's numeric id to its file on disk.
#[derive(Debug, Default)]
pub struct EmlxIndex {
    by_id: HashMap<i64, MessageFile>,
    /// Directories that could not be read, usually a sign of partial access.
    pub unreadable_dirs: usize,
}

impl EmlxIndex {
    /// Walks the store once. Complete files win over partial ones when both exist for the
    /// same id, which happens while Mail is downloading a body.
    pub fn build(store: &MailStore) -> Result<Self> {
        let mut index = EmlxIndex::default();
        let mut stack = vec![store.path.clone()];

        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(Error::AccessDenied { path: dir });
                }
                Err(_) => {
                    index.unreadable_dirs += 1;
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(path);
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some((id, partial)) = parse_emlx_name(name) else {
                    continue;
                };
                // Prefer a complete file; never let a partial displace one.
                match index.by_id.get(&id) {
                    Some(existing) if !existing.partial => {}
                    _ => {
                        index.by_id.insert(id, MessageFile { path, partial });
                    }
                }
            }
        }

        Ok(index)
    }

    pub fn get(&self, id: i64) -> Option<&MessageFile> {
        self.by_id.get(&id)
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// (complete, partial) counts.
    pub fn coverage(&self) -> (usize, usize) {
        let partial = self.by_id.values().filter(|f| f.partial).count();
        (self.by_id.len() - partial, partial)
    }
}

/// `1234.emlx` -> (1234, false); `1234.partial.emlx` -> (1234, true).
fn parse_emlx_name(name: &str) -> Option<(i64, bool)> {
    let (stem, partial) = match name.strip_suffix(".partial.emlx") {
        Some(stem) => (stem, true),
        None => (name.strip_suffix(".emlx")?, false),
    };
    stem.parse().ok().map(|id| (id, partial))
}

/// One attachment's bytes, ready to be written out.
#[derive(Debug, Clone)]
pub struct ExtractedAttachment {
    /// Filename as the message declares it, already made safe to use as a leaf name.
    pub filename: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ParsedMessage {
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
    pub message_id: Option<String>,
    /// Plain text body, preferred for an agent to read.
    pub text: Option<String>,
    /// Set only when there is no plain text alternative; already converted from HTML.
    pub html_text: Option<String>,
    pub attachments: Vec<String>,
    /// True when the source was a `.partial.emlx`, so the body is genuinely unavailable
    /// rather than empty.
    pub partial: bool,
    /// How many URLs had tracking parameters removed, and how many bytes that saved.
    pub urls_compacted: usize,
    pub bytes_saved: usize,
}

impl ParsedMessage {
    /// The body an agent should read, with the source noted.
    pub fn body(&self) -> Option<&str> {
        self.text.as_deref().or(self.html_text.as_deref())
    }
}

/// Lists and extracts attachments from a message file.
///
/// `which` selects by zero-based position, or by filename when it parses as neither. Returns
/// every attachment when `which` is `None`.
pub fn extract_attachments(
    file: &MessageFile,
    which: Option<&str>,
) -> Result<Vec<ExtractedAttachment>> {
    let bytes = fs::read(&file.path).map_err(|e| Error::from_io(&file.path, e))?;
    let rfc822 = strip_length_prefix(&bytes);
    let Some(message) = mail_parser::MessageParser::default().parse(rfc822) else {
        return Ok(Vec::new());
    };

    let all: Vec<ExtractedAttachment> = message
        .attachments()
        .enumerate()
        .map(|(position, part)| ExtractedAttachment {
            filename: safe_filename(part.attachment_name(), position),
            content_type: part.content_type().map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            }),
            bytes: part.contents().to_vec(),
        })
        .collect();

    let Some(which) = which else { return Ok(all) };

    if let Ok(position) = which.parse::<usize>() {
        return Ok(all.into_iter().skip(position).take(1).collect());
    }
    let wanted = which.to_lowercase();
    Ok(all
        .into_iter()
        .filter(|a| a.filename.to_lowercase().contains(&wanted))
        .collect())
}

/// Reduces a declared filename to something safe to use as a single path component.
///
/// Attachment names come from the message, so they are attacker-controlled: a name of
/// `../../.ssh/authorized_keys` must not be allowed to escape the directory it is written to.
fn safe_filename(declared: Option<&str>, position: usize) -> String {
    let fallback = format!("attachment-{}", position + 1);
    let Some(name) = declared else { return fallback };

    // Keep only the last component, then strip anything that could traverse or confuse.
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = leaf
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\' && *c != '\0')
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();

    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        fallback
    } else {
        cleaned.chars().take(200).collect()
    }
}

pub fn read(file: &MessageFile) -> Result<ParsedMessage> {
    read_with(file, true)
}

/// `compact_urls` strips tracking parameters from links in the body. Worth leaving on: on
/// marketing mail it routinely removes the majority of the bytes.
pub fn read_with(file: &MessageFile, compact_urls: bool) -> Result<ParsedMessage> {
    let bytes = fs::read(&file.path).map_err(|e| Error::from_io(&file.path, e))?;
    let rfc822 = strip_length_prefix(&bytes);
    Ok(parse_rfc822_with(rfc822, file.partial, compact_urls))
}

/// An `.emlx` begins with the message's byte count on its own line, and ends with a plist of
/// Mail's local metadata. Both must be removed before the middle parses as a message.
fn strip_length_prefix(bytes: &[u8]) -> &[u8] {
    let Some(newline) = bytes.iter().position(|b| *b == b'\n') else {
        return bytes;
    };
    let Ok(header) = std::str::from_utf8(&bytes[..newline]) else {
        return bytes;
    };
    // A missing or malformed count means this is not the shape we expect; treating the whole
    // file as a message is the safer failure, since the parser tolerates trailing junk.
    let Ok(length) = header.trim().parse::<usize>() else {
        return bytes;
    };

    let start = newline + 1;
    let end = start.saturating_add(length).min(bytes.len());
    &bytes[start..end]
}

#[cfg(test)]
fn parse_rfc822(bytes: &[u8], partial: bool) -> ParsedMessage {
    parse_rfc822_with(bytes, partial, false)
}

fn parse_rfc822_with(bytes: &[u8], partial: bool, compact_urls: bool) -> ParsedMessage {
    let Some(message) = mail_parser::MessageParser::default().parse(bytes) else {
        return ParsedMessage {
            from: None,
            to: Vec::new(),
            cc: Vec::new(),
            subject: None,
            date: None,
            message_id: None,
            text: None,
            html_text: None,
            attachments: Vec::new(),
            partial,
            urls_compacted: 0,
            bytes_saved: 0,
        };
    };

    // Classify by content subtype rather than trusting which iterator a part arrives in:
    // for an HTML-only message `text_bodies()` yields the HTML part, and returning raw
    // markup as "text" would hand an agent a page of tags.
    let mut text: Option<String> = None;
    let mut html: Option<String> = None;

    for part in message.text_bodies().chain(message.html_bodies()) {
        let subtype = part
            .content_type()
            .and_then(|ct| ct.subtype())
            .unwrap_or("plain");

        if subtype.eq_ignore_ascii_case("plain") {
            if text.is_none() {
                text = part
                    .text_contents()
                    .map(normalise_whitespace)
                    .filter(|body| !body.is_empty());
            }
        } else if subtype.eq_ignore_ascii_case("html") && html.is_none() {
            html = part
                .text_contents()
                .map(|raw| normalise_whitespace(&strip_html(raw)))
                .filter(|body| !body.is_empty());
        }
    }

    // Only fall back to HTML when there is no plain alternative: most marketing mail carries
    // both, and the text part is far cheaper for an agent to read.
    let mut html_text = if text.is_some() { None } else { html };

    let mut urls_compacted = 0;
    let mut bytes_saved = 0;
    if compact_urls {
        for content in [&mut text, &mut html_text].into_iter().flatten() {
            let result = crate::urls::compact(content);
            urls_compacted += result.urls;
            bytes_saved += result.bytes_saved;
            *content = result.text;
        }
    }

    ParsedMessage {
        from: message
            .from()
            .and_then(|list| list.first())
            .map(format_address),
        to: addresses(message.to()),
        cc: addresses(message.cc()),
        subject: message.subject().map(str::to_string),
        date: message.date().map(|d| d.to_rfc3339()),
        message_id: message.message_id().map(str::to_string),
        text,
        html_text,
        attachments: message
            .attachments()
            .filter_map(|part| part.attachment_name().map(str::to_string))
            .collect(),
        partial,
        urls_compacted,
        bytes_saved,
    }
}

fn addresses(list: Option<&mail_parser::Address>) -> Vec<String> {
    list.and_then(|l| l.as_list())
        .map(|addrs| addrs.iter().map(format_address).collect())
        .unwrap_or_default()
}

fn format_address(addr: &mail_parser::Addr) -> String {
    match (addr.name(), addr.address()) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        (None, Some(email)) => email.to_string(),
        (Some(name), None) => name.to_string(),
        (None, None) => String::new(),
    }
}

/// Collapses the runs of blank lines and trailing spaces that HTML-to-text and quoted-
/// printable decoding leave behind. Keeps paragraph breaks.
fn normalise_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut blank_run = 0usize;

    for line in input.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
            out.push('\n');
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }

    out.trim().to_string()
}

/// Minimal tag stripping for the HTML-only case. Not a renderer: it exists so a message with
/// no plain-text part still yields something readable.
///
/// Walks characters, not bytes. Mail is full of non-ASCII — `©`, emoji, combining marks —
/// and both slicing at a byte offset and pushing a byte as a `char` corrupt or panic on it.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skip_until: Option<&str> = None;
    let mut i = 0;

    while i < html.len() {
        let Some(ch) = html[i..].chars().next() else { break };
        let ch_len = ch.len_utf8();

        // Inside <script>/<style>: discard everything up to the closing tag.
        if let Some(close) = skip_until {
            if starts_with_ignore_case(html, i, close) {
                i += close.len();
                skip_until = None;
            } else {
                i += ch_len;
            }
            continue;
        }
        if starts_with_ignore_case(html, i, "<script") {
            skip_until = Some("</script>");
            i += "<script".len();
            continue;
        }
        if starts_with_ignore_case(html, i, "<style") {
            skip_until = Some("</style>");
            i += "<style".len();
            continue;
        }

        match ch {
            '<' => {
                in_tag = true;
                // Block-level boundaries are paragraph breaks worth keeping.
                if ["<br", "</p", "</div", "</tr", "</h1", "</h2", "</li"]
                    .iter()
                    .any(|tag| starts_with_ignore_case(html, i, tag))
                {
                    out.push('\n');
                }
            }
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
        i += ch_len;
    }

    decode_entities(&out)
}

/// ASCII-case-insensitive prefix test at a byte offset. `needle` must be ASCII, so comparing
/// raw bytes cannot split a multi-byte character; `at` is always a character boundary
/// because callers advance by whole characters or by ASCII literal lengths.
fn starts_with_ignore_case(haystack: &str, at: usize, needle: &str) -> bool {
    debug_assert!(needle.is_ascii());
    haystack
        .as_bytes()
        .get(at..at + needle.len())
        .is_some_and(|slice| slice.eq_ignore_ascii_case(needle.as_bytes()))
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_emlx_filenames() {
        assert_eq!(parse_emlx_name("1234.emlx"), Some((1234, false)));
        assert_eq!(parse_emlx_name("1234.partial.emlx"), Some((1234, true)));
        assert_eq!(parse_emlx_name("notes.txt"), None);
        assert_eq!(parse_emlx_name("abc.emlx"), None);
        assert_eq!(parse_emlx_name(".emlx"), None);
    }

    #[test]
    fn strips_the_byte_count_and_trailing_plist() {
        let message = b"Subject: Hi\r\n\r\nBody text\r\n";
        let mut file = format!("{}\n", message.len()).into_bytes();
        file.extend_from_slice(message);
        file.extend_from_slice(b"<?xml version=\"1.0\"?><plist>junk</plist>");

        assert_eq!(strip_length_prefix(&file), message);
    }

    #[test]
    fn a_missing_byte_count_falls_back_to_the_whole_file() {
        let raw = b"Subject: Hi\n\nBody";
        assert_eq!(strip_length_prefix(raw), raw);
    }

    #[test]
    fn a_truncated_file_does_not_panic() {
        // Claims 9999 bytes but holds far fewer.
        let file = b"9999\nSubject: Hi\n\nshort".to_vec();
        let out = strip_length_prefix(&file);
        assert_eq!(out, b"Subject: Hi\n\nshort");
    }

    #[test]
    fn parses_headers_and_plain_body() {
        let raw = b"From: Alice <alice@example.test>\r\n\
                    To: Bob <bob@example.test>\r\n\
                    Cc: Carol <carol@example.test>\r\n\
                    Subject: Quarterly report\r\n\
                    Message-ID: <abc@example.test>\r\n\
                    \r\n\
                    Here is the report.\r\n";
        let parsed = parse_rfc822(raw, false);

        assert_eq!(parsed.from.as_deref(), Some("Alice <alice@example.test>"));
        assert_eq!(parsed.to, vec!["Bob <bob@example.test>"]);
        assert_eq!(parsed.cc, vec!["Carol <carol@example.test>"]);
        assert_eq!(parsed.subject.as_deref(), Some("Quarterly report"));
        assert_eq!(parsed.message_id.as_deref(), Some("abc@example.test"));
        assert_eq!(parsed.body(), Some("Here is the report."));
        assert!(!parsed.partial);
    }

    #[test]
    fn prefers_the_plain_part_of_a_multipart_message() {
        let raw = b"Subject: Both\r\n\
                    Content-Type: multipart/alternative; boundary=\"B\"\r\n\
                    \r\n\
                    --B\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    plain version\r\n\
                    --B\r\n\
                    Content-Type: text/html\r\n\
                    \r\n\
                    <p>html version</p>\r\n\
                    --B--\r\n";
        let parsed = parse_rfc822(raw, false);

        assert_eq!(parsed.text.as_deref(), Some("plain version"));
        assert!(parsed.html_text.is_none(), "no need to convert HTML here");
    }

    #[test]
    fn falls_back_to_html_when_there_is_no_plain_part() {
        let raw = b"Subject: HTML only\r\n\
                    Content-Type: text/html\r\n\
                    \r\n\
                    <html><body><p>Hello &amp; welcome</p><p>Second line</p></body></html>\r\n";
        let parsed = parse_rfc822(raw, false);

        assert!(parsed.text.is_none());
        let body = parsed.body().expect("html converted");
        assert!(body.contains("Hello & welcome"), "got {body:?}");
        assert!(body.contains("Second line"));
    }

    #[test]
    fn html_conversion_survives_non_ascii_content() {
        // '©' and a combining grapheme joiner are what actually panicked on real mail:
        // byte-indexed slicing landed inside them.
        let html = "<p>Copyright \u{a9} 2026 \u{34f} Acme</p><p>Caf\u{e9} \u{1f600}</p>";
        let text = strip_html(html);
        assert!(text.contains('\u{a9}'), "got {text:?}");
        assert!(text.contains("Caf\u{e9}"));
        assert!(text.contains('\u{1f600}'), "emoji must survive intact");
    }

    #[test]
    fn html_conversion_handles_non_ascii_inside_tags_and_skipped_blocks() {
        let html = "<style>p{content:\"\u{a9}\"}</style><div title=\"caf\u{e9}\">Visible \u{a9}</div>";
        let text = strip_html(html);
        assert!(text.contains("Visible \u{a9}"));
        assert!(!text.contains("content"), "style body must be dropped: {text:?}");
        assert!(!text.contains("title"), "attributes must be dropped: {text:?}");
    }

    #[test]
    fn uppercase_tags_are_recognised() {
        let html = "<SCRIPT>var x=1;</SCRIPT><P>Shown</P><BR>next";
        let text = strip_html(html);
        assert!(text.contains("Shown"));
        assert!(text.contains("next"));
        assert!(!text.contains("var x"));
    }

    #[test]
    fn unterminated_tag_does_not_loop_or_panic() {
        assert_eq!(strip_html("<div class=\"x"), "");
        assert_eq!(strip_html("<script>forever"), "");
        assert!(strip_html("text <").contains("text"));
    }

    #[test]
    fn html_conversion_drops_script_and_style_content() {
        let html = "<style>p{color:red}</style><script>var x=1;</script><p>Visible</p>";
        let text = strip_html(html);
        assert!(text.contains("Visible"));
        assert!(!text.contains("color"));
        assert!(!text.contains("var x"));
    }

    #[test]
    fn whitespace_normalisation_keeps_paragraphs_but_collapses_runs() {
        let input = "Line one   \n\n\n\nLine two\n   \n\nLine three";
        assert_eq!(
            normalise_whitespace(input),
            "Line one\n\nLine two\n\nLine three"
        );
    }

    #[test]
    fn compaction_shrinks_a_tracking_heavy_body_and_reports_it() {
        let raw = b"Subject: Jobs\r\n\r\nView roles\r\nhttps://www.linkedin.com/comm/company/acme/jobs?lipi=urn%3Ali&midToken=AQF&trk=eml-x&otpToken=MzhlMjIxZjE4NGVjNjk3NDMyYzA4Nzk0OGJkZTgzZGNj\r\n";
        let parsed = parse_rfc822_with(raw, false, true);

        let body = parsed.body().expect("body present");
        assert!(body.contains("https://www.linkedin.com/comm/company/acme/jobs"));
        assert!(!body.contains("otpToken"), "tracking must be gone: {body:?}");
        assert_eq!(parsed.urls_compacted, 1);
        assert!(parsed.bytes_saved > 50);
    }

    #[test]
    fn compaction_can_be_turned_off() {
        let raw = b"Subject: Jobs\r\n\r\nhttps://x.test/p?utm_source=mail\r\n";
        let parsed = parse_rfc822_with(raw, false, false);
        assert!(parsed.body().unwrap().contains("utm_source"));
        assert_eq!(parsed.urls_compacted, 0);
    }

    #[test]
    fn a_partial_message_is_flagged_rather_than_looking_empty() {
        let raw = b"Subject: Headers only\r\n\r\n";
        let parsed = parse_rfc822(raw, true);
        assert!(parsed.partial);
        assert_eq!(parsed.body(), None);
    }

    #[test]
    fn attachment_filenames_cannot_escape_their_directory() {
        assert_eq!(safe_filename(Some("report.pdf"), 0), "report.pdf");
        assert_eq!(
            safe_filename(Some("../../.ssh/authorized_keys"), 0),
            "authorized_keys"
        );
        assert_eq!(safe_filename(Some("/etc/passwd"), 0), "passwd");
        assert_eq!(safe_filename(Some("..\\..\\win.ini"), 0), "win.ini");
        assert_eq!(safe_filename(Some(".."), 0), "attachment-1");
        assert_eq!(safe_filename(Some("   "), 3), "attachment-4");
        assert_eq!(safe_filename(None, 1), "attachment-2");
        // A leading dot would hide the file; strip it but keep the name.
        assert_eq!(safe_filename(Some(".hidden.pdf"), 0), "hidden.pdf");
    }

    #[test]
    fn long_filenames_are_bounded() {
        let long = format!("{}.pdf", "a".repeat(500));
        assert_eq!(safe_filename(Some(&long), 0).chars().count(), 200);
    }

    #[test]
    fn extracts_an_attachment_by_position_and_by_name() {
        let raw = b"Subject: With files\r\nContent-Type: multipart/mixed; boundary=\"B\"\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"offer.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8gUERG\r\n--B--\r\n";

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.emlx");
        let mut file_bytes = format!("{}\n", raw.len()).into_bytes();
        file_bytes.extend_from_slice(raw);
        fs::write(&path, &file_bytes).unwrap();
        let file = MessageFile { path, partial: false };

        let all = extract_attachments(&file, None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].filename, "offer.pdf");
        assert_eq!(all[0].bytes, b"Hello PDF");
        assert_eq!(all[0].content_type.as_deref(), Some("application/pdf"));

        assert_eq!(extract_attachments(&file, Some("0")).unwrap().len(), 1);
        assert_eq!(extract_attachments(&file, Some("offer")).unwrap().len(), 1);
        assert!(extract_attachments(&file, Some("nope")).unwrap().is_empty());
        assert!(extract_attachments(&file, Some("7")).unwrap().is_empty());
    }

    #[test]
    fn a_message_with_no_attachments_yields_none() {
        let raw = b"Subject: Plain\r\n\r\njust text\r\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2.emlx");
        fs::write(&path, raw).unwrap();
        let file = MessageFile { path, partial: false };
        assert!(extract_attachments(&file, None).unwrap().is_empty());
    }

    #[test]
    fn index_prefers_complete_files_over_partial_ones() {
        let dir = tempfile::tempdir().unwrap();
        let messages = dir.path().join("Inbox.mbox/UUID/Data/1/Messages");
        fs::create_dir_all(&messages).unwrap();
        fs::write(messages.join("7.partial.emlx"), b"0\n").unwrap();
        fs::write(messages.join("7.emlx"), b"0\n").unwrap();
        fs::write(messages.join("8.partial.emlx"), b"0\n").unwrap();

        let store = MailStore { root: dir.path().into(), version: 10, path: dir.path().into() };
        let index = EmlxIndex::build(&store).unwrap();

        assert_eq!(index.len(), 2);
        assert!(!index.get(7).unwrap().partial, "complete must win");
        assert!(index.get(8).unwrap().partial);
        assert_eq!(index.coverage(), (1, 1));
    }

    #[test]
    fn index_reports_nothing_for_an_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = MailStore { root: dir.path().into(), version: 10, path: dir.path().into() };
        let index = EmlxIndex::build(&store).unwrap();
        assert!(index.get(1).is_none());
        assert!(index.is_empty());
    }
}
