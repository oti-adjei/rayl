//! `rayl read` — fetches one message body from its `.emlx` file.

use anyhow::{Context, Result, bail};
use rayl_core::MailStore;
use rayl_core::emlx::{EmlxIndex, ParsedMessage};

pub fn run(id: Option<i64>, max_bytes: usize, coverage: bool, raw_urls: bool) -> Result<()> {
    let store = MailStore::discover().context("locating the mail store")?;
    let index = EmlxIndex::build(&store).context("indexing message files")?;

    if coverage {
        let (complete, partial) = index.coverage();
        println!("Message files: {complete} complete, {partial} headers-only");
        if index.unreadable_dirs > 0 {
            println!("Unreadable directories: {}", index.unreadable_dirs);
        }
        return Ok(());
    }

    let Some(id) = id else {
        bail!("give a message id (from `rayl search`), or pass --coverage");
    };

    let Some(file) = index.get(id) else {
        bail!(
            "no message file for #{id} ({} files indexed); the id comes from `rayl search`",
            index.len()
        );
    };

    let message =
        rayl_core::emlx::read_with(file, !raw_urls).context("reading the message file")?;
    render(id, &message, max_bytes);
    Ok(())
}

fn render(id: i64, message: &ParsedMessage, max_bytes: usize) {
    if let Some(subject) = &message.subject {
        println!("Subject: {subject}");
    }
    if let Some(from) = &message.from {
        println!("From:    {from}");
    }
    if !message.to.is_empty() {
        println!("To:      {}", message.to.join(", "));
    }
    if !message.cc.is_empty() {
        println!("Cc:      {}", message.cc.join(", "));
    }
    if let Some(date) = &message.date {
        println!("Date:    {date}");
    }
    if let Some(message_id) = &message.message_id {
        println!("Id:      <{message_id}>  (rayl #{id})");
    }
    if !message.attachments.is_empty() {
        println!("Files:   {}", message.attachments.join(", "));
    }
    println!();

    match message.body() {
        None if message.partial => {
            println!("No body available: Mail has downloaded only the headers for this");
            println!("message. Its account is set not to keep full copies locally.");
        }
        None => println!("No readable body found."),
        Some(body) => {
            let (shown, truncated) = truncate_on_boundary(body, max_bytes);
            print!("{shown}");
            if !shown.ends_with('\n') {
                println!();
            }
            if truncated {
                println!();
                println!(
                    "[truncated at {max_bytes} bytes of {}; raise with --max-bytes]",
                    body.len()
                );
            }
            if message.text.is_none() {
                println!();
                println!("[converted from HTML; no plain-text part was present]");
            }
            if message.urls_compacted > 0 {
                println!();
                println!(
                    "[stripped tracking from {} URL{}, saving {} bytes; --raw-urls to keep them]",
                    message.urls_compacted,
                    if message.urls_compacted == 1 { "" } else { "s" },
                    message.bytes_saved
                );
            }
        }
    }
}

/// Truncates without splitting a UTF-8 character, which would otherwise corrupt the tail of
/// any message containing non-ASCII text.
fn truncate_on_boundary(body: &str, max_bytes: usize) -> (&str, bool) {
    if body.len() <= max_bytes {
        return (body, false);
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    (&body[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_bodies_are_untouched() {
        let (shown, truncated) = truncate_on_boundary("hello", 100);
        assert_eq!(shown, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        // Three-byte characters: a naive slice at 4 bytes would panic or corrupt.
        let body = "日本語テキスト";
        let (shown, truncated) = truncate_on_boundary(body, 4);
        assert!(truncated);
        assert_eq!(shown, "日");
        assert!(body.starts_with(shown));
    }

    #[test]
    fn truncating_to_zero_yields_nothing_rather_than_panicking() {
        let (shown, truncated) = truncate_on_boundary("日本", 1);
        assert_eq!(shown, "");
        assert!(truncated);
    }
}
